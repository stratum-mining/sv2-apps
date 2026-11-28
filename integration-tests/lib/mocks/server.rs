use super::Connection;
use crate::{
    message_aggregator::DEFAULT_TIMEOUT,
    types::MsgType,
    utils::{POLL_INTERVAL, handshake_as_server},
};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use stratum_apps::{
    stratum_core::{
        codec_sv2::SerializedFrame,
        common_messages_sv2::{
            ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, Protocol, SetupConnectionErrorOwned,
            SetupConnectionOwned, SetupConnectionSuccessOwned,
        },
        parsers_sv2::{AnyMessageOwned, CommonMessagesOwned},
    },
    sync::{SharedLock, SharedMap},
};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::{CancellationToken, DropGuard};

/// How long [`MockServer::wait_for_held_connection`] watches a connection before taking it for
/// one the peer keeps.
pub const HELD_AFTER: Duration = Duration::from_secs(2);

/// Identifies one connection of a [`MockServer`]: `0` for the first connection to complete the
/// Noise handshake with the server, `1` for the second, and so on.
pub type ConnectionId = usize;

/// What a [`MockServer`] supports, deciding how it answers the `SetupConnection` that opens each
/// connection. The server closes a connection after answering it with `SetupConnection.Error`.
#[derive(Clone)]
pub struct ServerSetupConnection {
    /// The sub-protocols the server supports, each paired with the flags of the
    /// `SetupConnection.Success` the server answers a `SetupConnection` for that sub-protocol
    /// with. Flags are defined per sub-protocol, so each one carries its own.
    ///
    /// A `SetupConnection` for a sub-protocol that isn't listed is answered with
    /// `SetupConnection.Error`.
    pub protocols: Vec<(Protocol, u32)>,
    /// The lowest protocol version the server supports.
    pub min_version: u16,
    /// The highest protocol version the server supports. A `SetupConnection` whose version range
    /// shares no version with the server's is answered with `SetupConnection.Error`.
    pub max_version: u16,
}

impl ServerSetupConnection {
    /// A server supporting, at version 2, each sub-protocol in `protocols`, given as
    /// `(sub-protocol, flags of the server's SetupConnection.Success for it)`.
    pub fn new(protocols: impl IntoIterator<Item = (Protocol, u32)>) -> Self {
        Self {
            protocols: protocols.into_iter().collect(),
            min_version: 2,
            max_version: 2,
        }
    }

    // The server's answer to `request`.
    fn answer(&self, request: &SetupConnectionOwned) -> AnyMessageOwned {
        let error = |error_code: &str| {
            CommonMessagesOwned::SetupConnectionError(SetupConnectionErrorOwned {
                flags: 0,
                error_code: error_code
                    .to_string()
                    .try_into()
                    .expect("an error code fits into STR0_255"),
            })
        };
        let supported = self
            .protocols
            .iter()
            .find(|(protocol, _)| *protocol == request.protocol);
        let used_version = request.max_version.min(self.max_version);
        AnyMessageOwned::Common(match supported {
            None => error(ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL),
            Some(_) if used_version < request.min_version.max(self.min_version) => {
                error(ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH)
            }
            Some((_, flags)) => {
                CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
                    used_version,
                    flags: *flags,
                })
            }
        })
    }
}

/// A mock Sv2 server. It accepts every connection opened to its address, and a test addresses
/// each of them by its [`ConnectionId`].
///
/// A connection becomes available to the test once its Noise handshake is done and, on a server
/// with a [`ServerSetupConnection`], its `SetupConnection` exchange is over, whether the server
/// answered it or the exchange failed. A method that is given the
/// [`ConnectionId`] of a connection that isn't available yet waits for it, unless its
/// documentation says otherwise.
///
/// Dropping the server closes its connections at once. Its address is released as soon as the
/// server's accepting task runs again, so a test that starts another server on the same address
/// awaits something in between.
pub struct MockServer {
    address: SocketAddr,
    connections: SharedMap<ConnectionId, Connection>,
    // Why the server stopped accepting connections, if it did.
    accept_failure: SharedLock<Option<String>>,
    // How many connections failed the Noise handshake, and so never became available.
    failed_handshakes: Arc<AtomicUsize>,
    // Closes every connection and ends every task of the server when the server is dropped.
    _tasks: DropGuard,
}

impl MockServer {
    /// Starts a server listening on `address`.
    ///
    /// With a `setup_connection`, the server answers the `SetupConnection` that opens each
    /// connection, and the connection becomes available to the test once that answer is sent.
    /// Without one, the server sends nothing on its own.
    pub async fn new(address: SocketAddr, setup_connection: Option<ServerSetupConnection>) -> Self {
        let listener = TcpListener::bind(address)
            .await
            .unwrap_or_else(|error| panic!("MockServer {address}: can't listen: {error}"));
        let connections = SharedMap::new();
        let accept_failure = SharedLock::new(None);
        let failed_handshakes = Arc::new(AtomicUsize::new(0));
        let tasks = CancellationToken::new();
        tokio::spawn(accept_connections(
            listener,
            address,
            setup_connection,
            connections.clone(),
            accept_failure.clone(),
            failed_handshakes.clone(),
            tasks.clone(),
        ));
        Self {
            address,
            connections,
            accept_failure,
            failed_handshakes,
            _tasks: tasks.drop_guard(),
        }
    }

    /// Waits for the server to accept `connection`.
    pub async fn wait_for_connection(&self, connection: ConnectionId) {
        self.connection(connection).await;
    }

    /// Sends `message` on `connection`.
    pub async fn send(&self, connection: ConnectionId, message: AnyMessageOwned) {
        self.connection(connection).await.send(message).await;
    }

    /// Sends `frame` on `connection` as it is, so a test can send bytes that aren't a valid
    /// message.
    pub async fn send_frame(&self, connection: ConnectionId, frame: SerializedFrame) {
        self.connection(connection).await.send_frame(frame).await;
    }

    /// Returns the oldest message received on `connection` and removes it from the connection's
    /// queue. Returns `None` if the queue is empty or the connection hasn't been accepted.
    pub fn next_message(&self, connection: ConnectionId) -> Option<(MsgType, AnyMessageOwned)> {
        self.connections
            .with(&connection, Connection::next_message)
            .flatten()
    }

    /// Waits until a message of `message_type` has been received on `connection`.
    pub async fn wait_for_message_type(&self, connection: ConnectionId, message_type: u8) {
        self.connection(connection)
            .await
            .wait_for_message_type(message_type)
            .await;
    }

    /// Like [`MockServer::wait_for_message_type`], but also removes the messages from the queue of
    /// `connection` up to and including the first one of `message_type`.
    pub async fn wait_for_message_type_and_clean_queue(
        &self,
        connection: ConnectionId,
        message_type: u8,
    ) {
        self.connection(connection)
            .await
            .wait_for_message_type_and_clean_queue(message_type)
            .await;
    }

    /// Removes every message from the queue of `connection`.
    pub async fn clean_queue(&self, connection: ConnectionId) {
        self.connection(connection).await.clean_queue();
    }

    /// Watches `connection` for `deadline`, returning `false` as soon as its queue holds a message
    /// of `message_type`, and `true` if the whole duration passes without one.
    ///
    /// This doesn't wait for `connection` to become available: a connection that never does
    /// received no message, so the answer is `true`. It panics instead if the server stopped
    /// accepting connections, which proves nothing about what the peer would have sent.
    pub async fn assert_message_not_present(
        &self,
        connection: ConnectionId,
        message_type: u8,
        deadline: Duration,
    ) -> bool {
        let start = Instant::now();
        loop {
            if let Some(connection) = self.connections.get_cloned(&connection) {
                return connection
                    .assert_message_not_present(
                        message_type,
                        deadline.saturating_sub(start.elapsed()),
                    )
                    .await;
            }
            if start.elapsed() >= deadline {
                if let Some(failure) = self.accept_failure.get().ok().flatten() {
                    panic!(
                        "MockServer {}: connection {connection} can't be accepted: {failure}",
                        self.address
                    );
                }
                return true;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Closes `connection`, once the messages already sent on it have been written out.
    pub async fn close(&self, connection: ConnectionId) {
        self.connection(connection).await.close();
    }

    /// Waits until `connection` is closed: by the peer, by [`MockServer::close`], or by the server
    /// after it answered the connection's `SetupConnection` with `SetupConnection.Error`.
    pub async fn wait_for_close(&self, connection: ConnectionId) {
        self.connection(connection).await.wait_for_close().await;
    }

    /// Waits for a connection the peer keeps: the first one, from `first` on, that stays open for
    /// [`HELD_AFTER`] once this call has reached it. Returns its ID.
    ///
    /// A peer that only probes the server closes its connection once `SetupConnection` is
    /// answered, so the connections of its probes are skipped. Like
    /// [`MockServer::wait_for_close`], it panics on a connection that failed.
    pub async fn wait_for_held_connection(&self, first: ConnectionId) -> ConnectionId {
        let mut id = first;
        loop {
            let connection = self.connection(id).await;
            if tokio::time::timeout(HELD_AFTER, connection.wait_for_close())
                .await
                .is_err()
            {
                return id;
            }
            id += 1;
        }
    }

    // Waits for the server to accept `connection`.
    async fn connection(&self, connection: ConnectionId) -> Connection {
        let start = Instant::now();
        loop {
            if let Some(connection) = self.connections.get_cloned(&connection) {
                return connection;
            }
            if let Some(failure) = self.accept_failure.get().ok().flatten() {
                panic!(
                    "MockServer {}: connection {connection} can't be accepted: {failure}",
                    self.address
                );
            }
            if start.elapsed() > DEFAULT_TIMEOUT {
                panic!(
                    "MockServer {}: timeout while waiting for connection {connection} to complete \
                     the Noise handshake and, if the server answers it, to send SetupConnection \
                     ({} connections failed the Noise handshake)",
                    self.address,
                    self.failed_handshakes.load(Ordering::Relaxed)
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

// Accepts connections on `listener` until `tasks` is cancelled or accepting fails, serving each
// on its own task.
async fn accept_connections(
    listener: TcpListener,
    address: SocketAddr,
    setup_connection: Option<ServerSetupConnection>,
    connections: SharedMap<ConnectionId, Connection>,
    accept_failure: SharedLock<Option<String>>,
    failed_handshakes: Arc<AtomicUsize>,
    tasks: CancellationToken,
) {
    let next_id = Arc::new(AtomicUsize::new(0));
    loop {
        let stream = tokio::select! {
            _ = tasks.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::error!("MockServer {address}: can't accept connections: {error}");
                    _ = accept_failure.set(Some(error.to_string()));
                    return;
                }
            },
        };
        tokio::spawn(serve_connection(
            stream,
            address,
            setup_connection.clone(),
            connections.clone(),
            next_id.clone(),
            failed_handshakes.clone(),
            tasks.clone(),
        ));
    }
}

// Performs the Noise handshake on `stream`, answers the peer's `SetupConnection` if the server has
// a `setup_connection`, makes the connection available to the test, and then queues what the peer
// sends until the connection closes or `tasks` is cancelled.
async fn serve_connection(
    stream: TcpStream,
    address: SocketAddr,
    setup_connection: Option<ServerSetupConnection>,
    connections: SharedMap<ConnectionId, Connection>,
    next_id: Arc<AtomicUsize>,
    failed_handshakes: Arc<AtomicUsize>,
    tasks: CancellationToken,
) {
    let handshake = tokio::select! {
        _ = tasks.cancelled() => return,
        handshake = handshake_as_server(stream, tasks.clone()) => handshake,
    };
    let Some((frames, sender)) = handshake else {
        tracing::warn!("MockServer {address}: a Noise handshake failed");
        failed_handshakes.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // Only a connection that completed the handshake takes an ID, so a bare TCP probe of the
    // address doesn't shift the IDs of the connections after it.
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let connection = Connection::new(format!("MockServer {address} connection {id}"), sender);

    if let Some(setup_connection) = setup_connection {
        match frames.recv().await {
            Ok(mut frame) => match connection.record(&mut frame) {
                Some(AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(request))) => {
                    let answer = setup_connection.answer(&request);
                    let refused = matches!(
                        answer,
                        AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionError(_))
                    );
                    // A peer that is already gone closed the connection, which isn't a failure.
                    if let Err(reason) = connection.try_send(answer).await
                        && !connection.sender.is_closed()
                    {
                        connection.fail(reason);
                    }
                    // A server closes the connection after `SetupConnection.Error`.
                    if refused {
                        connection.close();
                    }
                }
                Some(message) => connection.fail(format!(
                    "the first message was {message}, not SetupConnection"
                )),
                // The frame couldn't be parsed, which already failed the connection.
                None => {}
            },
            Err(_) => connection
                .fail("the peer closed the connection before sending SetupConnection".to_string()),
        }
    }

    connections.insert(id, connection.clone());
    // A connection that failed stops queuing messages.
    if connection.failure().is_none() {
        connection.capture(frames).await;
    }
}
