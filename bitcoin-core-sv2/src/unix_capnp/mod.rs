//! Bitcoin Core IPC-backed protocol runtimes.
//!
//! This backend uses UNIX-socket Cap'n Proto RPC clients to communicate with Bitcoin Core.
//!
//! ## Runtime constraint
//!
//! Due to `capnp-rpc` `!Send` internals, these runtimes must execute inside a
//! [`tokio::task::LocalSet`].
//!
//! ## Socket trust
//!
//! The IPC transport carries no authentication. Each runtime connects to whatever process listens
//! at the configured socket path and logs the uid serving it. Keeping that path replaceable only
//! by the user running Bitcoin Core is a deployment requirement; see the crate README.

pub mod v30x;
pub mod v31x;

use tokio::net::UnixStream;
use tracing::{info, warn};

/// The minimum block reserved weight established by Bitcoin Core.
const MIN_BLOCK_RESERVED_WEIGHT: u64 = 2000;

/// BIP141 weight factor (witness scale factor), used to convert vsize to weight units.
const WEIGHT_FACTOR: u64 = 4;

/// Grace period before stale template data is retired after a chain tip change, in seconds.
///
/// Allows in-flight `RequestTransactionData` and `SubmitSolution` requests to complete before
/// the template data is retired.
const STALE_TEMPLATE_GRACE_PERIOD_SECS: u64 = 10;

/// Templates kept usable at one chain tip, beyond which the oldest are retired.
///
/// A fee refresh does not invalidate the template it supersedes, so these are retired by count
/// rather than by timer: each one holds a Bitcoin Core `BlockTemplate` capability alive, and the
/// count is what bounds memory while a chain tip does not move. Eight covers a job history of
/// `min_interval` times eight seconds, far longer than a solution takes to come back.
const MAX_SAME_TIP_TEMPLATES: usize = 8;

/// Bitcoin Core's `MAX_MONEY` consensus constant, in satoshis (21,000,000 BTC).
///
/// Used as a `fee_threshold` sentinel in `waitNext` requests: Bitcoin Core skips fee-based
/// template updates when `fee_threshold >= MAX_MONEY`, while still returning a new template
/// immediately on chain tip changes.
const MAX_MONEY: i64 = 21_000_000 * 100_000_000;

/// Max time a `waitNext` request is allowed to block before timing out (in milliseconds).
const WAIT_NEXT_TIMEOUT_MS: f64 = 10_000.0;

/// Logs the effective uid of the process serving a connected Bitcoin Core IPC socket.
///
/// The IPC transport carries no authentication of its own: whatever process owns the socket at
/// the configured path answers as Bitcoin Core. Logging the uid lets an operator confirm from the
/// logs that the socket is served by the user running their node.
pub(crate) fn log_peer_uid(stream: &UnixStream) {
    match stream.peer_cred() {
        Ok(cred) => info!("Bitcoin Core IPC socket is served by uid {}", cred.uid()),
        Err(e) => warn!("Cannot read Bitcoin Core IPC peer credentials: {e}"),
    }
}
