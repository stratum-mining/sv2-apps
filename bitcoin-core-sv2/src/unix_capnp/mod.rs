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
pub mod v32x;

// Shared cross-version implementation namespace is internal plumbing,
// not stable public API.
pub(crate) mod v31x_v30x;
