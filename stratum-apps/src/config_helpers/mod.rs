//! Configuration management helpers for SV2 applications
//!
//! This module provides utilities for:
//! - Parsing configuration files (TOML, etc.)
//! - Handling coinbase output specifications
//! - xpub-based coinbase rotation
//! - Setting up logging and tracing
//!
//! Originally from the `config_helpers_sv2` crate.

mod coinbase_output;
pub use coinbase_output::{CoinbaseRewardScript, Error as CoinbaseOutputError};

mod xpub_derivation;
pub use xpub_derivation::{XpubDerivationError, XpubDerivator};

pub mod logging;

mod toml;
pub use toml::{duration_from_toml, opt_path_from_toml};
