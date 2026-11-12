//! ## Configuration Module
//!
//! Defines [`JDSConfig`], the configuration structure for the Job Declaration Server (JDS), along
//! with its supporting types.
//!
//! This module handles:
//! - Initializing [`JDSConfig`]
//! - Managing JDS-specific listener/extension settings
//! - Validating and converting coinbase outputs

use std::net::SocketAddr;
use stratum_apps::{config_helpers::CoinbaseRewardScript, network_helpers::ResponderCredentials};

/// Partial configuration deserialized from the `[jds]` TOML section, when embedded in Pool.
///
/// Contains only JDS-specific fields. Shared fields (Noise credentials, coinbase script) are
/// inherited from the parent Pool config.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct JDSPartialConfig {
    listen_address: SocketAddr,
    #[serde(default)]
    supported_extensions: Vec<u16>,
    #[serde(default)]
    required_extensions: Vec<u16>,
}

/// Complete JDS configuration with all required fields populated.
///
/// Built from [`JDSPartialConfig`] combined with Pool config values.
/// All fields are non-optional and ready to use.
#[derive(Clone, Debug)]
pub struct JDSConfig {
    listen_address: SocketAddr,
    credentials: ResponderCredentials,
    coinbase_reward_script: CoinbaseRewardScript,
    supported_extensions: Vec<u16>,
    required_extensions: Vec<u16>,
}

impl JDSPartialConfig {
    /// Constructs a minimal [`JDSPartialConfig`] with just the listen address.
    ///
    /// All other fields use defaults. This is typically used for programmatic construction
    /// (e.g., in tests) rather than TOML deserialization.
    pub fn new(listen_address: SocketAddr) -> Self {
        Self {
            listen_address,
            supported_extensions: Vec::new(),
            required_extensions: Vec::new(),
        }
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl JDSConfig {
    /// Constructs a complete [`JDSConfig`] directly (typically for tests).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        listen_address: SocketAddr,
        credentials: ResponderCredentials,
        coinbase_reward_script: CoinbaseRewardScript,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> Self {
        Self {
            listen_address,
            credentials,
            coinbase_reward_script,
            supported_extensions,
            required_extensions,
        }
    }

    /// Constructs a complete [`JDSConfig`] from a partial config and Pool settings.
    ///
    /// This is used when JDS is embedded in Pool. The partial config contains JDS-specific
    /// fields from the `[jds]` TOML section, while shared fields are inherited from Pool.
    #[allow(clippy::too_many_arguments)]
    pub fn from_partial(
        partial: JDSPartialConfig,
        credentials: ResponderCredentials,
        coinbase_reward_script: CoinbaseRewardScript,
    ) -> Self {
        Self {
            listen_address: partial.listen_address,
            credentials,
            coinbase_reward_script,
            supported_extensions: partial.supported_extensions,
            required_extensions: partial.required_extensions,
        }
    }

    /// Address the JDS downstream server listens on (Noise-encrypted JDP).
    pub fn listen_address(&self) -> &SocketAddr {
        &self.listen_address
    }

    /// Static key and certificate presented to downstreams in the Noise handshake.
    pub fn responder_credentials(&self) -> &ResponderCredentials {
        &self.credentials
    }

    /// Script included in `AllocateMiningJobTokenSuccess.coinbase_outputs`.
    pub fn coinbase_reward_script(&self) -> &CoinbaseRewardScript {
        &self.coinbase_reward_script
    }

    /// SV2 extension types that JDS supports.
    pub fn supported_extensions(&self) -> &[u16] {
        &self.supported_extensions
    }

    /// SV2 extension types that JDS requires from downstreams.
    pub fn required_extensions(&self) -> &[u16] {
        &self.required_extensions
    }
}
