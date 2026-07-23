//! # Bitcoin Core Sv2 Library
//!
//! `bitcoin_core_sv2` bridges Bitcoin Core IPC with Stratum V2 protocols:
//! - [Template Distribution Protocol](https://github.com/stratum-mining/sv2-spec/blob/main/07-Template-Distribution-Protocol.md)
//! - [Job Declaration Protocol](https://github.com/stratum-mining/sv2-spec/blob/main/08-Job-Declaration-Protocol.md)
//!
//! ## Overview
//!
//! `bitcoin_core_sv2` can be used to:
//! - Build Sv2 applications acting as TDP clients (for example Pool or JDC) connected directly to a
//!   Bitcoin Core node.
//! - Build Sv2 template-provider applications acting as TDP servers backed by Bitcoin Core.
//! - Build Sv2 applications acting as JDP servers (for example Pool or JDS) connected directly to a
//!   Bitcoin Core node.
//!
//! ## Module layout
//!
//! - [`runtime_api`] exposes version-agnostic runtime handles and protocol-specific `new(version,
//!   ...)` factories with enum dispatch across backend versions.
//! - [`CancellationToken`] is re-exported at the crate root for runtime shutdown signaling across
//!   protocols.
//! - [`unix_capnp::v30x`] contains the Bitcoin Core v30.x IPC implementation.
//! - [`unix_capnp::v31x`] contains the Bitcoin Core v31.x IPC implementation.
//!
//! ## Flavor direction
//!
//! `unix_capnp` is the currently implemented backend flavor. The crate keeps this flavor name
//! explicit to leave room for additional backend families in the future (for example, a
//! `tcp_capnp` flavor or an `http_json_rpc` flavor).
//!
//! Downstream applications should integrate through [`runtime_api`] and choose a
//! [`runtime_api::BitcoinCoreVersion`] at runtime.
//!
//! Backend-specific IPC/runtime constraints are documented under [`unix_capnp`].

pub mod runtime_api;
pub mod unix_capnp;

/// Shared runtime cancellation primitive used by both TDP and JDP APIs.
pub use tokio_util::sync::CancellationToken;
