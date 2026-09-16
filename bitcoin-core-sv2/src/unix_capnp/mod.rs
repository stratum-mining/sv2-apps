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

/// Bitcoin Core's `MAX_MONEY` consensus constant, in satoshis (21,000,000 BTC).
///
/// Used as a `fee_threshold` sentinel in `waitNext` requests: Bitcoin Core skips fee-based
/// template updates when `fee_threshold >= MAX_MONEY`, while still returning a new template
/// immediately on chain tip changes.
const MAX_MONEY: i64 = 21_000_000 * 100_000_000;

/// Max time a `waitNext` request is allowed to block before timing out (in milliseconds).
const WAIT_NEXT_TIMEOUT_MS: f64 = 10_000.0;

/// Max attempts for `force_update_mempool_mirror` retries on transient "thread busy" IPC
/// contention.
const FORCE_UPDATE_MAX_ATTEMPTS: usize = 3;

/// Backoff between `force_update_mempool_mirror` retry attempts (in milliseconds).
const FORCE_UPDATE_RETRY_BACKOFF_MS: u64 = 25;

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
