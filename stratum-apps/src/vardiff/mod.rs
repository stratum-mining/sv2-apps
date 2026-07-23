//! Variable difficulty (vardiff) for SV2 mining applications.
//!
//! This module ports the ckpool `add_submit()` / rolling-average algorithm from
//! [`stratifier.c`](https://github.com/ckolivas/ckpool) and generalizes the
//! hard-coded 18 shares/minute endpoint so callers can pass the configurable
//! `shares_per_minute` already used throughout sv2-apps.

#[cfg(feature = "core")]
mod ckpool;

#[cfg(feature = "core")]
pub use ckpool::VardiffState;
