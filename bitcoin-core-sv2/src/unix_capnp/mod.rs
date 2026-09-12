//! Bitcoin Core IPC-backed protocol runtimes.
//!
//! This backend uses UNIX-socket Cap'n Proto RPC clients to communicate with Bitcoin Core.
//!
//! ## Runtime constraint
//!
//! Due to `capnp-rpc` `!Send` internals, these runtimes must execute inside a
//! [`tokio::task::LocalSet`].

pub mod v30x;
pub mod v31x;

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
/// count is what bounds memory while a chain tip does not move.
///
/// The cap must cover the job history a downstream may still submit a solution against: a
/// `SubmitSolution` naming a template that has already been destroyed is dropped, and that is a
/// lost block which shows up only in the logs. Sixteen matches `MAX_PAST_JOBS` in `channels_sv2`,
/// the number of past jobs a channel keeps at one chain tip when a pool does not override it. A
/// pool that raises its own job history above this cap reopens that gap.
const MAX_SAME_TIP_TEMPLATES: usize = 16;

/// Bitcoin Core's `MAX_MONEY` consensus constant, in satoshis (21,000,000 BTC).
///
/// Used as a `fee_threshold` sentinel in `waitNext` requests: Bitcoin Core skips fee-based
/// template updates when `fee_threshold >= MAX_MONEY`, while still returning a new template
/// immediately on chain tip changes.
const MAX_MONEY: i64 = 21_000_000 * 100_000_000;

/// Max time a `waitNext` request is allowed to block before timing out (in milliseconds).
const WAIT_NEXT_TIMEOUT_MS: f64 = 10_000.0;

/// Max time an interrupt sent during shutdown may wait for Bitcoin Core's reply.
///
/// `send()` only queues the request: it reaches Bitcoin Core when the Cap'n Proto `RpcSystem`
/// task next runs, which needs something on this `LocalSet` to still be awaiting. The reply is
/// therefore awaited rather than left on a task of its own, and the wait is bounded so a node that
/// has stopped answering cannot hold shutdown either.
const INTERRUPT_REPLY_TIMEOUT_MS: u64 = 1_000;
