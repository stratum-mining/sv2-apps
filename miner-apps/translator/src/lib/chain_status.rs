//! Background bitcoind checks for network-difficulty share hashes.
//!
//! Ordinary shares are not queried. A check starts only after a `block_candidate` is recorded,
//! waits before the first RPC, and retries with a longer delay until the block has enough
//! confirmations or the time limit passes. A node that cannot be reached is not recorded as
//! `not_found`.

use std::{
    collections::HashSet,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::share_audit::{ChainStatusUpdate, ShareAudit};

pub use crate::share_audit::ChainCheckRequest;

pub const STATUS_NOT_FOUND: &str = "not_found";
pub const STATUS_IN_BEST_CHAIN: &str = "in_best_chain";
pub const STATUS_ORPHANED_OR_STALE: &str = "orphaned_or_stale";

/// How often a candidate is asked of bitcoind, and when to stop.
#[derive(Clone, Debug)]
pub struct PollPolicy {
    pub initial: Duration,
    pub backoff: Vec<Duration>,
    pub max_age: Duration,
    pub stop_confirmations: u32,
}

impl PollPolicy {
    pub fn production(initial_secs: u64, max_secs: u64, stop_confirmations: u32) -> Self {
        Self {
            initial: Duration::from_secs(initial_secs),
            backoff: vec![
                Duration::from_secs(600),
                Duration::from_secs(1800),
                Duration::from_secs(3600),
            ],
            max_age: Duration::from_secs(max_secs),
            stop_confirmations,
        }
    }

    fn delay_after(&self, checks_done: u32) -> Duration {
        if checks_done == 0 {
            return self.initial;
        }
        let idx = (checks_done - 1) as usize;
        self.backoff
            .get(idx)
            .copied()
            .or_else(|| self.backoff.last().copied())
            .unwrap_or(self.initial)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderLookup {
    NotFound,
    Found { confirmations: i64 },
}

#[derive(Debug)]
pub enum RpcError {
    Transport(String),
    Rpc { code: i64, message: String },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "{msg}"),
            Self::Rpc { code, message } => write!(f, "bitcoind RPC {code}: {message}"),
        }
    }
}

impl std::error::Error for RpcError {}

pub trait HeaderSource: Clone + Send + Sync + 'static {
    fn chain_name(&self) -> Result<String, RpcError>;
    fn header(&self, hash: &str) -> Result<HeaderLookup, RpcError>;
}

/// `confirmations >= 0` is Bitcoin Core's "this header is in the active chain"
/// (the tip itself is 1). A negative count means the node knows the header and it
/// is not in the best chain.
pub fn classify(lookup: &HeaderLookup) -> (&'static str, Option<i64>) {
    match lookup {
        HeaderLookup::NotFound => (STATUS_NOT_FOUND, None),
        HeaderLookup::Found { confirmations } if *confirmations < 0 => {
            (STATUS_ORPHANED_OR_STALE, Some(*confirmations))
        }
        HeaderLookup::Found { confirmations } => (STATUS_IN_BEST_CHAIN, Some(*confirmations)),
    }
}

pub fn should_stop(
    status: &str,
    confirmations: Option<i64>,
    age: Duration,
    policy: &PollPolicy,
) -> bool {
    if age >= policy.max_age {
        return true;
    }
    status == STATUS_IN_BEST_CHAIN
        && confirmations.unwrap_or(0) >= i64::from(policy.stop_confirmations)
}

#[derive(Clone)]
pub struct BitcoinRpcClient {
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
    cookie_path: Option<PathBuf>,
}

impl BitcoinRpcClient {
    /// Accepts `http://host:port` and optional `http://user:pass@host:port`.
    /// HTTPS is rejected: local bitcoind RPC is plain HTTP.
    pub fn new(
        url: &str,
        cookie_path: Option<PathBuf>,
        user: Option<String>,
        password: Option<String>,
    ) -> Result<Self, RpcError> {
        let parsed = parse_http_url(url)?;
        let (user, password) = match (user, password) {
            (Some(u), Some(p)) => (Some(u), Some(p)),
            (None, None) => (parsed.user, parsed.password),
            _ => {
                return Err(RpcError::Transport(
                    "bitcoin RPC user and password must be set together".into(),
                ));
            }
        };
        Ok(Self {
            host: parsed.host,
            port: parsed.port,
            user,
            password,
            cookie_path,
        })
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    fn basic_auth(&self) -> Result<Option<String>, RpcError> {
        if let (Some(user), Some(password)) = (&self.user, &self.password) {
            return Ok(Some(basic_auth_header(user, password)));
        }
        let Some(path) = &self.cookie_path else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path).map_err(|e| {
            RpcError::Transport(format!("failed to read bitcoin cookie {}: {e}", path.display()))
        })?;
        let line = text.trim();
        let Some((user, password)) = line.split_once(':') else {
            return Err(RpcError::Transport(format!(
                "bitcoin cookie {} is missing user:password",
                path.display()
            )));
        };
        Ok(Some(basic_auth_header(user, password)))
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({
            "jsonrpc": "1.0",
            "id": "tproxy-chain-status",
            "method": method,
            "params": params,
        });
        let payload = body.to_string();
        let auth = self.basic_auth()?;
        let mut request = format!(
            "POST / HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.host,
            self.port,
            payload.len(),
        );
        if let Some(auth) = auth {
            request.push_str(&auth);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        request.push_str(&payload);

        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).map_err(|e| {
            RpcError::Transport(format!("connect {}:{}: {e}", self.host, self.port))
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        stream
            .write_all(request.as_bytes())
            .map_err(|e| RpcError::Transport(e.to_string()))?;

        let mut buf = Vec::new();
        stream
            .take(1_048_576)
            .read_to_end(&mut buf)
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let text = String::from_utf8_lossy(&buf);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or(text.as_ref());
        serde_json::from_str(body.trim()).map_err(|e| {
            RpcError::Transport(format!("bitcoind response was not JSON: {e}"))
        })
    }
}

impl HeaderSource for BitcoinRpcClient {
    fn chain_name(&self) -> Result<String, RpcError> {
        let value = self.call("getblockchaininfo", json!([]))?;
        interpret_chain_name(&value)
    }

    fn header(&self, hash: &str) -> Result<HeaderLookup, RpcError> {
        let value = self.call("getblockheader", json!([hash, true]))?;
        interpret_header(&value)
    }
}

struct ParsedUrl {
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
}

fn parse_http_url(url: &str) -> Result<ParsedUrl, RpcError> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(RpcError::Transport(
            "bitcoin RPC URL must start with http:// (local bitcoind does not use HTTPS here)"
                .into(),
        ));
    };
    let rest = rest.trim_end_matches('/');
    let (userinfo, hostport) = match rest.split_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, rest),
    };
    let (host, port) = split_host_port(hostport)?;
    let (user, password) = match userinfo {
        Some(userinfo) => {
            let Some((user, password)) = userinfo.split_once(':') else {
                return Err(RpcError::Transport(
                    "bitcoin RPC URL userinfo must be user:password".into(),
                ));
            };
            (Some(user.to_string()), Some(password.to_string()))
        }
        None => (None, None),
    };
    if host.is_empty() {
        return Err(RpcError::Transport("bitcoin RPC URL is missing a host".into()));
    }
    Ok(ParsedUrl {
        host,
        port,
        user,
        password,
    })
}

fn split_host_port(hostport: &str) -> Result<(String, u16), RpcError> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(RpcError::Transport(
                "bitcoin RPC IPv6 URL is missing ']'".into(),
            ));
        };
        let port = match after.strip_prefix(':') {
            Some(port) if !port.is_empty() => parse_port(port)?,
            _ => 8332,
        };
        return Ok((host.to_string(), port));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && !host.is_empty() => {
            Ok((host.to_string(), parse_port(port)?))
        }
        _ => Ok((hostport.to_string(), 8332)),
    }
}

fn parse_port(port: &str) -> Result<u16, RpcError> {
    port.parse::<u16>()
        .map_err(|_| RpcError::Transport(format!("invalid bitcoin RPC port {port}")))
}

fn basic_auth_header(user: &str, password: &str) -> String {
    let token = base64_encode(format!("{user}:{password}").as_bytes());
    format!("Authorization: Basic {token}")
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = (data[i] as u32) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

pub fn interpret_header(value: &Value) -> Result<HeaderLookup, RpcError> {
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("bitcoind error")
            .to_string();
        if code == -5 {
            return Ok(HeaderLookup::NotFound);
        }
        return Err(RpcError::Rpc { code, message });
    }
    let confirmations = value
        .get("result")
        .and_then(|r| r.get("confirmations"))
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            RpcError::Transport("getblockheader result has no confirmations".into())
        })?;
    Ok(HeaderLookup::Found { confirmations })
}

pub fn interpret_chain_name(value: &Value) -> Result<String, RpcError> {
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("bitcoind error")
            .to_string();
        return Err(RpcError::Rpc { code, message });
    }
    let chain = value
        .get("result")
        .and_then(|r| r.get("chain"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Ok(match chain {
        "main" => "mainnet".to_string(),
        "test" => "testnet".to_string(),
        other => other.to_string(),
    })
}

pub async fn run<S: HeaderSource>(
    mut incoming: UnboundedReceiver<ChainCheckRequest>,
    source: S,
    audit: ShareAudit,
    policy: PollPolicy,
    cancel: CancellationToken,
    fallback: CancellationToken,
) {
    let mut active = HashSet::new();
    let mut joins = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = fallback.cancelled() => break,
            req = incoming.recv() => {
                let Some(req) = req else { break };
                if !active.insert(req.share_hash.clone()) {
                    debug!(
                        share_hash = %req.share_hash,
                        "chain status: already polling this hash"
                    );
                    continue;
                }
                let source = source.clone();
                let audit = audit.clone();
                let policy = policy.clone();
                let cancel = cancel.clone();
                let fallback = fallback.clone();
                joins.spawn(async move {
                    watch_one(req, source, audit, policy, cancel, fallback).await;
                });
            }
        }
    }
    while joins.join_next().await.is_some() {}
}

async fn watch_one<S: HeaderSource>(
    req: ChainCheckRequest,
    source: S,
    audit: ShareAudit,
    policy: PollPolicy,
    cancel: CancellationToken,
    fallback: CancellationToken,
) {
    let started = std::time::Instant::now();
    let mut checks_done = 0u32;
    let mut last: Option<(&'static str, Option<i64>)> = None;
    let mut network: Option<String> = None;
    loop {
        if !wait(policy.delay_after(checks_done), &cancel, &fallback).await {
            return;
        }
        if network.is_none() {
            match lookup_chain(source.clone()).await {
                Ok(name) => network = Some(name),
                Err(e) => {
                    warn!(error = %e, "chain status: getblockchaininfo failed");
                }
            }
        }
        let hash = req.share_hash.clone();
        match lookup_header(source.clone(), hash).await {
            Ok(lookup) => {
                checks_done = checks_done.saturating_add(1);
                let (status, confirmations) = classify(&lookup);
                let expired = started.elapsed() >= policy.max_age;
                let changed = last != Some((status, confirmations));
                if changed || expired {
                    audit.record_chain_status(&ChainStatusUpdate {
                        share_hash: req.share_hash.clone(),
                        worker: req.worker.clone(),
                        downstream_channel_id: req.downstream_channel_id,
                        upstream_channel_id: req.upstream_channel_id,
                        job_id: req.job_id,
                        sequence_number: req.sequence_number,
                        chain_status: status,
                        confirmations,
                        network: network.clone(),
                        poll_expired: expired,
                    });
                }
                last = Some((status, confirmations));
                if should_stop(status, confirmations, started.elapsed(), &policy) {
                    info!(
                        share_hash = %req.share_hash,
                        chain_status = status,
                        confirmations,
                        "chain status: stopped polling"
                    );
                    return;
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    share_hash = %req.share_hash,
                    "chain status: bitcoind RPC failed; hash left unchanged"
                );
                if started.elapsed() >= policy.max_age {
                    warn!(
                        share_hash = %req.share_hash,
                        "chain status: stopped polling after the time limit without an answer"
                    );
                    return;
                }
            }
        }
    }
}

async fn lookup_chain<S: HeaderSource>(source: S) -> Result<String, RpcError> {
    tokio::task::spawn_blocking(move || source.chain_name())
        .await
        .map_err(|e| RpcError::Transport(e.to_string()))?
}

async fn lookup_header<S: HeaderSource>(source: S, hash: String) -> Result<HeaderLookup, RpcError> {
    tokio::task::spawn_blocking(move || source.header(&hash))
        .await
        .map_err(|e| RpcError::Transport(e.to_string()))?
}

async fn wait(delay: Duration, cancel: &CancellationToken, fallback: &CancellationToken) -> bool {
    if delay.is_zero() {
        return !cancel.is_cancelled() && !fallback.is_cancelled();
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = fallback.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn policy_immediate(max_age: Duration, stop_confirmations: u32) -> PollPolicy {
        PollPolicy {
            initial: Duration::ZERO,
            backoff: vec![Duration::ZERO],
            max_age,
            stop_confirmations,
        }
    }

    fn sample_request() -> ChainCheckRequest {
        ChainCheckRequest {
            share_hash: "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f".into(),
            worker: Some("rig1".into()),
            downstream_channel_id: 1,
            upstream_channel_id: 1,
            job_id: 4,
            sequence_number: 9,
        }
    }

    #[test]
    fn classifies_core_header_results() {
        assert_eq!(
            classify(&HeaderLookup::NotFound),
            (STATUS_NOT_FOUND, None)
        );
        assert_eq!(
            classify(&HeaderLookup::Found { confirmations: 6 }),
            (STATUS_IN_BEST_CHAIN, Some(6))
        );
        assert_eq!(
            classify(&HeaderLookup::Found { confirmations: -1 }),
            (STATUS_ORPHANED_OR_STALE, Some(-1))
        );
    }

    #[test]
    fn stops_at_confirmations_or_age() {
        let policy = PollPolicy::production(120, 86_400, 6);
        assert!(!should_stop(
            STATUS_IN_BEST_CHAIN,
            Some(1),
            Duration::from_secs(60),
            &policy
        ));
        assert!(should_stop(
            STATUS_IN_BEST_CHAIN,
            Some(6),
            Duration::from_secs(60),
            &policy
        ));
        assert!(!should_stop(
            STATUS_ORPHANED_OR_STALE,
            Some(-1),
            Duration::from_secs(60),
            &policy
        ));
        assert!(should_stop(
            STATUS_NOT_FOUND,
            None,
            Duration::from_secs(86_400),
            &policy
        ));
    }

    #[test]
    fn parses_header_and_chain_payloads() {
        let best = json!({
            "result": {
                "hash": "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
                "confirmations": 800000
            },
            "error": null
        });
        assert_eq!(
            interpret_header(&best).unwrap(),
            HeaderLookup::Found { confirmations: 800000 }
        );
        let missing = json!({"result": null, "error": {"code": -5, "message": "Block not found"}});
        assert_eq!(interpret_header(&missing).unwrap(), HeaderLookup::NotFound);
        let warming = json!({"result": null, "error": {"code": -28, "message": "Loading block index..."}});
        assert!(interpret_header(&warming).is_err());
        let chain = json!({"result": {"chain": "main"}, "error": null});
        assert_eq!(interpret_chain_name(&chain).unwrap(), "mainnet");
    }

    #[test]
    fn parses_http_url_without_keeping_userinfo_in_the_endpoint() {
        let client = BitcoinRpcClient::new(
            "http://rpcuser:secret@127.0.0.1:18332",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(client.endpoint(), "http://127.0.0.1:18332");
        assert!(client.basic_auth().unwrap().unwrap().starts_with("Authorization: Basic "));
        assert!(BitcoinRpcClient::new("https://127.0.0.1:8332", None, None, None).is_err());
    }

    #[derive(Clone)]
    struct Scripted {
        chain: String,
        headers: Arc<Mutex<Vec<Result<HeaderLookup, RpcError>>>>,
    }

    impl HeaderSource for Scripted {
        fn chain_name(&self) -> Result<String, RpcError> {
            Ok(self.chain.clone())
        }
        fn header(&self, _hash: &str) -> Result<HeaderLookup, RpcError> {
            let mut queued = self.headers.lock().expect("script lock");
            if queued.is_empty() {
                return Err(RpcError::Transport("no more scripted responses".into()));
            }
            queued.remove(0)
        }
    }

    #[tokio::test]
    async fn known_header_is_recorded_in_best_chain() {
        let dir = std::env::temp_dir().join(format!("tproxy-chain-best-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = ShareAudit::open(&path, false).unwrap();
        let source = Scripted {
            chain: "mainnet".into(),
            headers: Arc::new(Mutex::new(vec![Ok(HeaderLookup::Found { confirmations: 6 })])),
        };
        watch_one(
            sample_request(),
            source,
            audit,
            policy_immediate(Duration::from_secs(3600), 6),
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"event\":\"chain_status\""));
        assert!(text.contains("\"chain_status\":\"in_best_chain\""));
        assert!(text.contains("\"confirmations\":6"));
        assert!(text.contains("\"network\":\"mainnet\""));
        assert!(text.contains("\"poll_expired\":false"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_hash_ages_out_as_not_found() {
        let dir = std::env::temp_dir().join(format!("tproxy-chain-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = ShareAudit::open(&path, false).unwrap();
        let source = Scripted {
            chain: "mainnet".into(),
            headers: Arc::new(Mutex::new(vec![Ok(HeaderLookup::NotFound)])),
        };
        watch_one(
            sample_request(),
            source,
            audit,
            policy_immediate(Duration::ZERO, 6),
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"chain_status\":\"not_found\""));
        assert!(text.contains("\"poll_expired\":true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_failure_does_not_record_not_found() {
        let dir = std::env::temp_dir().join(format!("tproxy-chain-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = ShareAudit::open(&path, false).unwrap();
        let source = Scripted {
            chain: "mainnet".into(),
            headers: Arc::new(Mutex::new(vec![Err(RpcError::Rpc {
                code: -28,
                message: "Loading block index...".into(),
            })])),
        };
        watch_one(
            sample_request(),
            source,
            audit,
            policy_immediate(Duration::ZERO, 6),
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!text.contains("not_found"));
        assert!(text.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn http_getblockheader_fixture_roundtrip() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).unwrap();
            let text = String::from_utf8_lossy(&req);
            let body = if text.contains("getblockchaininfo") {
                r#"{"result":{"chain":"regtest"},"error":null,"id":1}"#
            } else if text.contains("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f")
            {
                r#"{"result":{"confirmations":6},"error":null,"id":1}"#
            } else {
                r#"{"result":null,"error":{"code":-5,"message":"Block not found"},"id":1}"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });

        let client = BitcoinRpcClient::new(
            &format!("http://{}:{}", addr.ip(), addr.port()),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(client.chain_name().unwrap(), "regtest");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 4096];
            let _ = sock.read(&mut req).unwrap();
            let body = r#"{"result":{"confirmations":6},"error":null,"id":1}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let client = BitcoinRpcClient::new(
            &format!("http://{}:{}", addr.ip(), addr.port()),
            None,
            Some("u".into()),
            Some("p".into()),
        )
        .unwrap();
        assert_eq!(
            client
                .header("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f")
                .unwrap(),
            HeaderLookup::Found { confirmations: 6 }
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 4096];
            let _ = sock.read(&mut req).unwrap();
            let body = r#"{"result":null,"error":{"code":-5,"message":"Block not found"},"id":1}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let client =
            BitcoinRpcClient::new(&format!("http://{}:{}", addr.ip(), addr.port()), None, None, None)
                .unwrap();
        assert_eq!(
            client.header("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
            HeaderLookup::NotFound
        );
    }
}
