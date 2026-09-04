use crate::{
    interceptor::{InterceptAction, MessageDirection},
    message_aggregator::MessagesAggregator,
    sniffer_error::SnifferError,
    types::{MessageFrame, MsgType},
};
use async_channel::{Receiver, Sender};
use once_cell::sync::Lazy;
use std::{
    convert::TryInto,
    fs, io,
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use stratum_apps::{
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::noise_connection::Connection,
    stratum_core::{
        framing_sv2::framing::Sv2Frame,
        noise_sv2::{Initiator, Responder},
        parsers_sv2::{
            AnyMessage, AnyMessageOwned, ExtensionsNegotiationOwned, ExtensionsOwned, IsSv2Message,
            Tlv, message_type_to_name, parse_message_frame_with_tlvs,
        },
    },
    utils::types::OutboundFrame,
};
use tokio_util::sync::CancellationToken;

/// Advisory per-port lockfiles held for the process lifetime so no two concurrent
/// test processes can claim the same port. The kernel releases flock on exit (even
/// after `kill -9`), so stale lockfiles are harmless.
static HELD_LOCKS: Lazy<Mutex<Vec<std::fs::File>>> = Lazy::new(|| Mutex::new(Vec::new()));

fn lockfile_for(port: u16) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("sv2-it-ports")
        .join(format!("{port}.lock"))
}

fn try_lock_port(port: u16) -> io::Result<std::fs::File> {
    let path = lockfile_for(port);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // Non-blocking exclusive lock: EAGAIN → another process holds this port.
    // SAFETY: fd is valid, flock is async-signal-safe on both Linux and macOS.
    let fd = file.as_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Acquires a blocking exclusive file lock, runs `f`, and releases the lock.
///
/// This serializes initialization of artifacts shared by concurrent nextest processes. Callers
/// must check whether their artifact exists again inside `f`: another process may have created it
/// while this process was waiting for the lock.
pub fn with_exclusive_lock(lock_path: &Path, f: impl FnOnce()) {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| {
            panic!(
                "failed to create lockfile directory {}: {e}",
                parent.display()
            )
        });
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap_or_else(|e| panic!("failed to open lockfile {}: {e}", lock_path.display()));
    let fd = file.as_raw_fd();
    loop {
        // SAFETY: `fd` remains valid for the lifetime of `file`; `flock` is available on every
        // platform supported by the integration-test harness (Linux and macOS).
        if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            panic!("failed to lock {}: {e}", lock_path.display());
        }
    }
    f();
}

/// How often readiness gates and message-wait loops re-check their condition.
///
/// At 200ms each waiter checks five times per second. This materially reduces residual latency
/// across the suite's hundreds of waits compared with the previous one-second cadence, without
/// making every single-threaded test runtime wake twenty or more times per second.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Retry cadence for the *unbounded* connect loops that wait for a peer to come up.
///
/// Deliberately much slower than [`POLL_INTERVAL`]. Those loops have no timeout, so when a peer
/// never appears — which several tests arrange on purpose — they spin for the whole test. Every
/// test runs on a bare `#[tokio::test]`, i.e. a single-threaded runtime, so even a 5 Hz connect
/// loop competes with the test's own work on the one thread it has. The message-wait loops are safe
/// at [`POLL_INTERVAL`] because they exit as soon as their message lands.
pub const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Ceiling for readiness gates on spawned child processes (Bitcoin Core, sv2-tp).
///
/// This is never paid on the happy path — only when something is genuinely wrong — so it is set
/// far above the observed startup time rather than tuned tightly. It stays well under nextest's
/// `terminate-after` (120s) so the gate's own panic fires before nextest kills the test, which
/// keeps the failure message legible.
pub const PROCESS_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Fixed startup grace period for in-process pool roles.
///
/// A TCP readiness probe is unsafe here because the pool treats every accepted connection as a
/// protocol session. Probing created phantom downstreams and hung tests that intentionally
/// intercept setup, so startup retains the original one-second delay until the role exposes an
/// internal readiness signal.
pub const ROLE_STARTUP_DELAY: Duration = Duration::from_secs(1);

/// Blocks until `path` exists, polling every [`POLL_INTERVAL`].
///
/// Deliberately stats the path rather than connecting to it. This gates on Bitcoin Core's IPC
/// socket, and a capnp RPC endpoint ascribes meaning to a bare connection: Core's libmultiprocess
/// layer sets TCP_NODELAY on each accepted connection, so a probe that connects and immediately
/// drops makes that setsockopt run against a socket that is already gone. On macOS that returns
/// EINVAL and takes down Core's IPC listener with an "Uncaught exception in daemonized task",
/// after which sv2-tp can never connect. Linux tolerates the same call, so this only reproduces
/// on macOS.
///
/// Stat-ing is sound provided callers remove any datadir left over from an earlier test on the
/// same port, so the socket that appears can only belong to the node just started.
///
/// Panics with `what` in the message if `timeout` elapses first.
pub fn wait_for_path(path: &std::path::Path, timeout: Duration, what: &str) {
    let start = std::time::Instant::now();
    while !path.exists() {
        if start.elapsed() > timeout {
            panic!(
                "timeout after {timeout:?} waiting for {what} (path {} never appeared)",
                path.display()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    tracing::debug!(
        target: "readiness",
        "ready: {what} after {:?}",
        start.elapsed()
    );
}

/// Blocks until a TCP connection to `addr` succeeds, polling every [`POLL_INTERVAL`].
///
/// Used from synchronous process-startup contexts.
///
/// Panics with `what` in the message if `timeout` elapses first.
pub fn wait_for_listener(addr: SocketAddr, timeout: Duration, what: &str) {
    let start = std::time::Instant::now();
    loop {
        if TcpStream::connect_timeout(&addr, POLL_INTERVAL).is_ok() {
            tracing::debug!(
                target: "readiness",
                "ready: {what} after {:?}",
                start.elapsed()
            );
            return;
        }
        if start.elapsed() > timeout {
            panic!("timeout after {timeout:?} waiting for {what} to listen on {addr}");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Allocate a loopback address whose port is exclusively reserved across
/// concurrent test processes.
///
/// Probes with `bind(0)` then holds a per-port `flock` lockfile in
/// `$TMPDIR/sv2-it-ports/` for the process lifetime. Every integration-test process follows this
/// protocol, so another test cannot claim the port after the probe is dropped and before its role
/// binds. Lockfiles are released by the kernel on exit.
pub fn get_available_address() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], get_available_port()))
}

fn get_available_port() -> u16 {
    loop {
        let probe = TcpListener::bind("127.0.0.1:0").expect("bind(0) for port probe");
        let port = probe.local_addr().expect("probe local_addr").port();
        match try_lock_port(port) {
            Ok(lock_handle) => {
                HELD_LOCKS.lock().expect("ports lock").push(lock_handle);
                drop(probe);
                return port;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                drop(probe);
            }
            Err(e) => {
                drop(probe);
                panic!("failed to lock port {port}: {e}");
            }
        }
    }
}
pub async fn wait_for_client(listen_socket: SocketAddr) -> tokio::net::TcpStream {
    let listener = tokio::net::TcpListener::bind(listen_socket)
        .await
        .expect("Impossible to listen on given address");
    let (stream, _) = listener
        .accept()
        .await
        .expect("Impossible to accept dowsntream connection");
    stream
}

pub async fn create_downstream(
    stream: tokio::net::TcpStream,
) -> Option<(Receiver<MessageFrame>, Sender<OutboundFrame>)> {
    let pub_key = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"
        .to_string()
        .parse::<Secp256k1PublicKey>()
        .unwrap()
        .into_bytes();
    let prv_key = "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n"
        .to_string()
        .parse::<Secp256k1SecretKey>()
        .unwrap()
        .into_bytes();
    let responder =
        Responder::from_authority_kp(&pub_key, &prv_key, std::time::Duration::from_secs(10000))
            .unwrap();

    if let Ok((receiver_from_client, sender_to_client)) =
        Connection::accept::<OutboundFrame>(stream, responder, CancellationToken::new()).await
    {
        Some((receiver_from_client, sender_to_client))
    } else {
        None
    }
}

pub async fn create_upstream(
    stream: tokio::net::TcpStream,
) -> Option<(Receiver<MessageFrame>, Sender<OutboundFrame>)> {
    let initiator = Initiator::without_pk().expect("This fn call can not fail");
    Connection::connect::<OutboundFrame>(stream, initiator, CancellationToken::new())
        .await
        .ok()
}

pub async fn recv_from_down_send_to_up(
    recv: Receiver<MessageFrame>,
    send: Sender<OutboundFrame>,
    downstream_messages: MessagesAggregator,
    action: Vec<InterceptAction>,
    identifier: &str,
    negotiated_extensions: Arc<Mutex<Vec<u16>>>,
) -> Result<(), SnifferError> {
    while let Ok(mut frame) = recv.recv().await {
        let extensions = negotiated_extensions.lock().unwrap().clone();
        let (msg_type, msg, tlv_fields) = message_from_frame_with_tlvs(&mut frame, &extensions);

        // Track extension negotiation
        if let AnyMessageOwned::Extensions(ExtensionsOwned::ExtensionsNegotiation(
            ExtensionsNegotiationOwned::RequestExtensionsSuccess(success),
        )) = &msg
        {
            let mut exts = negotiated_extensions.lock().unwrap();
            *exts = success.supported_extensions.clone().into_inner();
            tracing::info!(
                "🔍 Sniffer {} | Tracked negotiated extensions: {:?}",
                identifier,
                *exts
            );
        }

        let action = action.iter().find(|action| {
            action
                .find_matching_action(msg_type, MessageDirection::ToUpstream)
                .is_some()
        });
        if let Some(action) = action {
            match action {
                InterceptAction::IgnoreMessage(_) => {
                    tracing::info!(
                        "🔍 Sv2 Sniffer {} | Ignored: {} | Direction: ⬆",
                        identifier,
                        message_type_to_name(msg_type)
                    );
                    continue;
                }
                InterceptAction::ReplaceMessage(intercept_message) => {
                    let intercept_frame = Sv2Frame::from_message(
                        intercept_message.replacement_message.clone(),
                        intercept_message.replacement_message.message_type(),
                        0,
                        false,
                    )
                    .expect("Failed to create the frame");
                    downstream_messages.add_message_with_tlvs(
                        intercept_message.replacement_message.message_type(),
                        intercept_message.replacement_message.clone(),
                        None,
                    );
                    send.send(intercept_frame.into())
                        .await
                        .map_err(|_| SnifferError::UpstreamClosed)?;
                    tracing::info!(
                        "🔍 Sv2 Sniffer {} | Replaced: {} with {} | Direction: ⬆",
                        identifier,
                        message_type_to_name(msg_type),
                        message_type_to_name(intercept_message.replacement_message.message_type())
                    );
                }
            }
        } else {
            downstream_messages.add_message_with_tlvs(msg_type, msg.clone(), tlv_fields);
            send.send(frame.into())
                .await
                .map_err(|_| SnifferError::UpstreamClosed)?;
            tracing::info!(
                "🔍 Sv2 Sniffer {} | Forwarded: {} | Direction: ⬆ | Data: {}",
                identifier,
                message_type_to_name(msg_type),
                msg
            );
        }
    }
    Err(SnifferError::DownstreamClosed)
}

pub async fn recv_from_up_send_to_down(
    recv: Receiver<MessageFrame>,
    send: Sender<OutboundFrame>,
    upstream_messages: MessagesAggregator,
    action: Vec<InterceptAction>,
    identifier: &str,
    negotiated_extensions: std::sync::Arc<std::sync::Mutex<Vec<u16>>>,
) -> Result<(), SnifferError> {
    while let Ok(mut frame) = recv.recv().await {
        let extensions = negotiated_extensions.lock().unwrap().clone();
        let (msg_type, msg, tlv_fields) = message_from_frame_with_tlvs(&mut frame, &extensions);

        // Track extension negotiation
        if let AnyMessageOwned::Extensions(ExtensionsOwned::ExtensionsNegotiation(
            ExtensionsNegotiationOwned::RequestExtensionsSuccess(success),
        )) = &msg
        {
            let mut exts = negotiated_extensions.lock().unwrap();
            *exts = success.supported_extensions.clone().into_inner();
            tracing::info!(
                "🔍 Sniffer {} | Tracked negotiated extensions: {:?}",
                identifier,
                *exts
            );
        }

        let action = action.iter().find(|action| {
            action
                .find_matching_action(msg_type, MessageDirection::ToDownstream)
                .is_some()
        });

        if let Some(action) = action {
            match action {
                InterceptAction::IgnoreMessage(_) => {
                    tracing::info!(
                        "🔍 Sv2 Sniffer {} | Ignored: {} | Direction: ⬇",
                        identifier,
                        message_type_to_name(msg_type)
                    );
                    continue;
                }
                InterceptAction::ReplaceMessage(intercept_message) => {
                    let intercept_frame = Sv2Frame::from_message(
                        intercept_message.replacement_message.clone(),
                        intercept_message.replacement_message.message_type(),
                        0,
                        false,
                    )
                    .expect("Failed to create the frame");
                    upstream_messages.add_message_with_tlvs(
                        intercept_message.replacement_message.message_type(),
                        intercept_message.replacement_message.clone(),
                        None,
                    );
                    send.send(intercept_frame.into())
                        .await
                        .map_err(|_| SnifferError::DownstreamClosed)?;
                    tracing::info!(
                        "🔍 Sv2 Sniffer {} | Replaced: {} with {} | Direction: ⬇",
                        identifier,
                        message_type_to_name(msg_type),
                        message_type_to_name(intercept_message.replacement_message.message_type())
                    );
                }
            }
        } else {
            upstream_messages.add_message_with_tlvs(msg_type, msg.clone(), tlv_fields);
            send.send(frame.into())
                .await
                .map_err(|_| SnifferError::DownstreamClosed)?;
            tracing::info!(
                "🔍 Sv2 Sniffer {} | Forwarded: {} | Direction: ⬇ | Data: {}",
                identifier,
                message_type_to_name(msg_type),
                msg
            );
        }
    }
    Err(SnifferError::UpstreamClosed)
}

pub fn message_from_frame(frame: &mut MessageFrame) -> (MsgType, AnyMessageOwned) {
    let (msg_type, msg, _) = message_from_frame_with_tlvs(frame, &[]);
    (msg_type, msg)
}

pub fn message_from_frame_with_tlvs(
    frame: &mut MessageFrame,
    negotiated_extensions: &[u16],
) -> (MsgType, AnyMessageOwned, Option<Vec<Tlv>>) {
    let header = frame.header();
    let payload = frame.payload();

    // Try to parse with TLV support if extensions are negotiated
    if !negotiated_extensions.is_empty() {
        match parse_message_frame_with_tlvs(header, payload, negotiated_extensions) {
            Ok((message, tlv_fields)) => {
                let message = into_owned(message);
                return (header.msg_type(), message, tlv_fields);
            }
            Err(e) => {
                println!(
                    "Failed to parse frame with TLVs: {e:?}, falling back to standard parsing"
                );
            }
        }
    }

    // Fallback to standard parsing without TLV support
    let mut payload = frame.payload().to_vec();
    let message: Result<AnyMessage<'_>, _> = (header, payload.as_mut_slice()).try_into();
    match message {
        Ok(message) => {
            let message = into_owned(message);
            (header.msg_type(), message, None)
        }
        _ => {
            println!("Received frame with invalid payload or message type: {frame:?}");
            panic!();
        }
    }
}

pub fn into_owned(m: AnyMessage<'_>) -> AnyMessageOwned {
    m.into_owned()
}

pub mod http {
    /// Make a GET request that returns both the HTTP status code and the response body.
    /// Unlike `make_get_request`, this does NOT panic on non-2xx status codes (e.g. 404),
    /// making it suitable for testing API error responses.
    /// Only retries on 5xx errors or connection failures.
    ///
    /// `request_timeout` is the per-request timeout applied to each attempt; `None`
    /// leaves the underlying client default (which is effectively unbounded).
    pub fn make_get_request_with_status(
        url: &str,
        retries: usize,
        request_timeout: Option<std::time::Duration>,
    ) -> (i32, Vec<u8>) {
        for attempt in 1..=retries {
            let mut req = minreq::get(url);
            if let Some(t) = request_timeout {
                req = req.with_timeout(t.as_secs().max(1));
            }
            let response = req.send();
            match response {
                Ok(res) => {
                    let status_code = res.status_code;
                    if (500..600).contains(&status_code) {
                        eprintln!(
                            "Attempt {attempt}: URL {url} returned a server error code {status_code}"
                        );
                    } else {
                        return (status_code, res.as_bytes().to_vec());
                    }
                }
                Err(err) => {
                    eprintln!(
                        "Attempt {}: Failed to fetch URL {}: {:?}",
                        attempt + 1,
                        url,
                        err
                    );
                }
            }

            if attempt < retries {
                let delay = 1u64 << (attempt - 1);
                eprintln!("Retrying in {delay} seconds (exponential backoff)...");
                std::thread::sleep(std::time::Duration::from_secs(delay));
            }
        }
        panic!("Cannot reach URL {url} after {retries} attempts");
    }

    pub fn make_get_request(download_url: &str, retries: usize) -> Vec<u8> {
        for attempt in 1..=retries {
            let response = minreq::get(download_url).send();
            match response {
                Ok(res) => {
                    let status_code = res.status_code;
                    if (200..300).contains(&status_code) {
                        return res.as_bytes().to_vec();
                    } else if (500..600).contains(&status_code) {
                        eprintln!(
                            "Attempt {attempt}: URL {download_url} returned a server error code {status_code}"
                        );
                    } else {
                        panic!(
                            "URL {download_url} returned unexpected status code {status_code}. Aborting."
                        );
                    }
                }
                Err(err) => {
                    eprintln!(
                        "Attempt {}: Failed to fetch URL {}: {:?}",
                        attempt + 1,
                        download_url,
                        err
                    );
                }
            }

            if attempt < retries {
                let delay = 1u64 << (attempt - 1);
                eprintln!("Retrying in {delay} seconds (exponential backoff)...");
                std::thread::sleep(std::time::Duration::from_secs(delay));
            }
        }
        // If all retries fail, panic with an error message
        panic!("Cannot reach URL {download_url} after {retries} attempts");
    }
}

pub mod tarball {
    use std::{
        fs::{self, File},
        io::{BufReader, Read},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);

    struct StagingDirectory(PathBuf);

    impl Drop for StagingDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    pub fn read_from_file(path: &str) -> Vec<u8> {
        let file = File::open(path).unwrap_or_else(|_| {
            panic!("Cannot find {path:?} specified with env var BITCOIND_TARBALL_FILE")
        });
        let mut reader = BufReader::new(file);
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer).unwrap();
        buffer
    }

    pub fn unpack(tarball_bytes: &[u8], destination: &Path) {
        use std::{io::Write as IoWrite, process::Command};

        // Use pid-unique temp name so concurrent test processes can't share it.
        let pid = std::process::id();
        let temp_tarball = destination.join(format!("temp-{pid}.tar.gz"));
        let mut temp_file = File::create(&temp_tarball).unwrap();
        temp_file.write_all(tarball_bytes).unwrap();
        drop(temp_file);

        // Use system tar command to extract, which properly handles GNU sparse files
        let output = Command::new("tar")
            .arg("-xzf")
            .arg(&temp_tarball)
            .arg("-C")
            .arg(destination)
            .arg("--strip-components=0")
            .output()
            .expect("Failed to execute tar command");

        if !output.status.success() {
            eprintln!("tar stderr: {}", String::from_utf8_lossy(&output.stderr));
            panic!("tar extraction failed");
        }

        // Clean up temp tarball
        std::fs::remove_file(&temp_tarball).ok();
    }

    /// Extracts `relative_path` into a private staging directory, prepares it, then atomically
    /// publishes it beneath `destination`.
    ///
    /// The staging directory is created on the same filesystem as the final path, so a successful
    /// rename makes the complete file or directory visible in one operation. Callers must serialize
    /// publication and check that the final path is still absent while holding that lock.
    pub fn unpack_path_atomically(
        tarball_bytes: &[u8],
        destination: &Path,
        relative_path: &Path,
        prepare: impl FnOnce(&Path),
    ) {
        assert!(
            !relative_path.is_absolute(),
            "published tarball path must be relative"
        );
        fs::create_dir_all(destination).unwrap_or_else(|e| {
            panic!(
                "failed to create artifact directory {}: {e}",
                destination.display()
            )
        });

        let artifact_name = relative_path
            .file_name()
            .expect("published tarball path must have a file name")
            .to_string_lossy();
        let staging = loop {
            let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
            let path = destination.join(format!(
                ".{artifact_name}.staging-{}-{id}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => break StagingDirectory(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("failed to create staging directory {}: {e}", path.display()),
            }
        };

        unpack(tarball_bytes, &staging.0);
        let staged_path = staging.0.join(relative_path);
        assert!(
            staged_path.exists(),
            "artifact {} not found after unpack in {}",
            relative_path.display(),
            staging.0.display()
        );
        prepare(&staged_path);

        let final_path = destination.join(relative_path);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|e| {
                panic!(
                    "failed to create artifact parent directory {}: {e}",
                    parent.display()
                )
            });
        }
        fs::rename(&staged_path, &final_path).unwrap_or_else(|e| {
            panic!(
                "failed to publish artifact {} to {}: {e}",
                staged_path.display(),
                final_path.display()
            )
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Command;

        #[test]
        fn atomic_unpack_publishes_only_after_preparation() {
            let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
            let root = StagingDirectory(std::env::temp_dir().join(format!(
                "sv2-atomic-unpack-test-{}-{id}",
                std::process::id()
            )));
            let source = root.0.join("source");
            let artifact = source.join("artifact");
            fs::create_dir_all(&artifact).unwrap();
            fs::write(artifact.join("payload"), b"complete").unwrap();

            let archive = root.0.join("artifact.tar.gz");
            let status = Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .arg("artifact")
                .status()
                .expect("failed to create test tarball");
            assert!(status.success(), "failed to create test tarball");

            let destination = root.0.join("destination");
            let final_path = destination.join("artifact");
            unpack_path_atomically(
                &fs::read(&archive).unwrap(),
                &destination,
                Path::new("artifact"),
                |staged_path| {
                    assert!(!final_path.exists());
                    fs::write(staged_path.join("prepared"), b"yes").unwrap();
                },
            );

            assert_eq!(fs::read(final_path.join("payload")).unwrap(), b"complete");
            assert_eq!(fs::read(final_path.join("prepared")).unwrap(), b"yes");
        }
    }
}

pub mod fs_utils {
    use std::{fs, path::Path};

    /// Recursively copy all contents from source directory to destination directory
    pub fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
        if !dst.exists() {
            fs::create_dir_all(dst)?;
        }

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if src_path.is_dir() {
                copy_dir_contents(&src_path, &dst_path)?;
            } else {
                fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }
}
