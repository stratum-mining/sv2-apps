//! # Stratum Apps - SV2 Application Utilities
//!
//! This crate consolidates the essential utilities needed for building Stratum V2 applications.
//! It combines the functionality from the original separate utility crates into a single,
//! well-organized library with feature-based compilation.
//!
//! ## Features
//!
//! ### Core Features
//! - `network` - High-level networking utilities (enabled by default)
//! - `fallback-coordinator` - Runtime fallback coordination helpers (enabled by default)
//! - `config` - Configuration management helpers (enabled by default)
//! - `payout` - Shared payout-mode parsing and coinbase-output distribution helpers
//! - `monitoring` - HTTP and Prometheus monitoring helpers (optional)
//! - `asic-rs-telemetry` - Optional miner telemetry helpers powered by `asic-rs`
//! - `std` - Standard-library support for key and random utilities (enabled by default)
//! - `core` - Re-export and enable `stratum-core`
//! - `bitcoin-core-sv2` - Re-export and enable `bitcoin_core_sv2`
//!
//! ### Role-Specific Feature Bundles
//! - `pool` - Everything needed for pool applications
//! - `jd_client` - Everything needed for JD client applications
//! - `jd_server` - Configuration helpers and Bitcoin Core IPC runtime APIs for JD server
//!   applications
//! - `translator` - Everything needed for translator applications (includes SV1 and payout helpers)
//! - `mining_device` - Configuration helpers for mining device applications
//!
//! ## Modules
//!
//! - [`network_helpers`] - High-level networking utilities for SV2 connections
//! - [`config_helpers`] - Configuration management and parsing utilities
//! - \[`payout`\] - Payout-mode parsing and coinbase-output construction/verification helpers

/// Re-export all the modules from `stratum_core`
#[cfg(feature = "core")]
pub use stratum_core;

/// Re-export all the modules from `bitcoin_core_sv2`
#[cfg(feature = "bitcoin-core-sv2")]
pub use bitcoin_core_sv2;

/// Re-export `tokio_util`, for the [`tokio_util::sync::CancellationToken`] required by the
/// networking helpers
#[cfg(feature = "tokio-util")]
pub use tokio_util;

/// High-level networking utilities for SV2 connections
///
/// Provides connection management, encrypted streams, and protocol handling.
/// Originally from the `network_helpers_sv2` crate.
#[cfg(feature = "network")]
pub mod network_helpers;

/// Configuration management helpers
///
/// Utilities for parsing configuration files, handling coinbase outputs,
/// and setting up logging. Originally from the `config_helpers_sv2` crate.
#[cfg(feature = "config")]
pub mod config_helpers;

/// Key utilities for cryptographic operations
///
/// Provides Secp256k1 key management, serialization/deserialization, and signature services.
/// Supports both standard and no_std environments.
pub mod key_utils;

/// Utility methods used in apps.
pub mod utils;

/// Channel monitoring - expose channel data via HTTP JSON APIs
#[cfg(feature = "monitoring")]
pub mod monitoring;

// Task orchestrator used in SRI apps.
pub mod task_manager;
/// Template provider type
///
/// Provides the type of template provider that will be used.
pub mod tp_type;

/// Creates a CoinbaseOutputConstraints message from a list of coinbase outputs
pub mod coinbase_output_constraints;

/// Shared payout-mode parsing and coinbase-output distribution helpers.
#[cfg(feature = "payout")]
pub mod payout;

/// Fallback coordinator
#[cfg(feature = "fallback-coordinator")]
pub mod fallback_coordinator;

/// Share synchronous primitives
pub mod sync;

/// Shared async channel cleanup helpers.
pub mod channel_utils;
