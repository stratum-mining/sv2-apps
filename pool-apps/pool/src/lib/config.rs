//! ## Configuration Module
//!
//! Defines [`PoolConfig`], the configuration structure for the Pool, along with its supporting
//! types.
//!
//! This module handles:
//! - Initializing [`PoolConfig`]
//! - Managing [`TemplateProviderType`], [`AuthorityConfig`], [`CoinbaseRewardScript`], and
//!   [`ConnectionConfig`]
//! - Validating and converting coinbase outputs
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

pub use jd_server_sv2::config::{JDSConfig, JDSPartialConfig};
use stratum_apps::{
    config_helpers::{CoinbaseRewardScript, opt_path_from_toml},
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    stratum_core::bitcoin::{Amount, TxOut},
    tp_type::TemplateProviderType,
    utils::types::{SharesBatchSize, SharesPerMinute},
};

use crate::error::PoolErrorKind;

/// Configuration for the Pool, including connection, authority, and coinbase settings.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PoolConfig {
    listen_address: SocketAddr,
    template_provider_type: TemplateProviderType,
    authority_public_key: Secp256k1PublicKey,
    authority_secret_key: Secp256k1SecretKey,
    cert_validity_sec: u64,
    coinbase_reward_script: CoinbaseRewardScript,
    pool_signature: String,
    shares_per_minute: SharesPerMinute,
    share_batch_size: SharesBatchSize,
    #[serde(default, deserialize_with = "opt_path_from_toml")]
    log_file: Option<PathBuf>,
    #[serde(default)]
    server_id: u8,
    #[serde(default)]
    supported_extensions: Vec<u16>,
    #[serde(default)]
    required_extensions: Vec<u16>,
    #[serde(default)]
    monitoring_address: Option<SocketAddr>,
    #[serde(default)]
    jds: Option<JDSPartialConfig>,
    #[serde(default)]
    monitoring_cache_refresh_secs: Option<u64>,
    /// Past jobs retained per channel for late-share validation.
    ///
    /// `None` (the default) and `Some(0)` both select the `channels_sv2` default. It is a
    /// retention window — `cap / job rate` — and the rate is this deployment's, which is why it
    /// is settable here; see `channels_sv2` for sizing.
    ///
    /// Do not lower it on a pool serving job-declaration clients: each accepted
    /// `SetCustomMiningJob` retires the active job, so the rate is the client's, not this pool's.
    #[serde(default)]
    max_past_jobs: Option<usize>,
}

impl PoolConfig {
    /// Creates a new instance of the [`PoolConfig`].
    ///
    /// # Panics
    ///
    /// Panics if `coinbase_reward_script` is empty.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool_connection: ConnectionConfig,
        template_provider_type: TemplateProviderType,
        authority_config: AuthorityConfig,
        coinbase_reward_script: CoinbaseRewardScript,
        shares_per_minute: SharesPerMinute,
        share_batch_size: SharesBatchSize,
        server_id: u8,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        monitoring_address: Option<SocketAddr>,
        monitoring_cache_refresh_secs: Option<u64>,
        jds: Option<JDSPartialConfig>,
    ) -> Self {
        Self {
            listen_address: pool_connection.listen_address,
            template_provider_type,
            authority_public_key: authority_config.public_key,
            authority_secret_key: authority_config.secret_key,
            cert_validity_sec: pool_connection.cert_validity_sec,
            coinbase_reward_script,
            pool_signature: pool_connection.signature,
            shares_per_minute,
            share_batch_size,
            max_past_jobs: None,
            log_file: None,
            server_id,
            supported_extensions,
            required_extensions,
            monitoring_address,
            monitoring_cache_refresh_secs,
            jds,
        }
    }

    /// Returns the coinbase output.
    pub fn coinbase_reward_script(&self) -> &CoinbaseRewardScript {
        &self.coinbase_reward_script
    }

    /// Returns Pool listenining address.
    pub fn listen_address(&self) -> &SocketAddr {
        &self.listen_address
    }

    /// Returns the authority public key.
    pub fn authority_public_key(&self) -> &Secp256k1PublicKey {
        &self.authority_public_key
    }

    /// Returns the authority secret key.
    pub fn authority_secret_key(&self) -> &Secp256k1SecretKey {
        &self.authority_secret_key
    }

    /// Returns the certificate validity in seconds.
    pub fn cert_validity_sec(&self) -> u64 {
        self.cert_validity_sec
    }

    /// Returns the Pool signature.
    pub fn pool_signature(&self) -> &String {
        &self.pool_signature
    }

    /// Returns the Template Provider type.
    pub fn template_provider_type(&self) -> &TemplateProviderType {
        &self.template_provider_type
    }

    /// Returns the share batch size.
    pub fn share_batch_size(&self) -> usize {
        self.share_batch_size
    }

    /// Sets the coinbase output.
    pub fn set_coinbase_reward_script(&mut self, coinbase_output: CoinbaseRewardScript) {
        self.coinbase_reward_script = coinbase_output;
    }

    /// Returns the shares per minute.
    /// Past jobs retained per channel, or `None` to use the `channels_sv2` default.
    pub fn max_past_jobs(&self) -> Option<usize> {
        self.max_past_jobs
    }

    /// Overrides the retained past-jobs cap. Mainly for tests and A/B deployments that vary
    /// it between otherwise-identical instances.
    pub fn set_max_past_jobs(&mut self, max_past_jobs: Option<usize>) {
        self.max_past_jobs = max_past_jobs;
    }

    pub fn shares_per_minute(&self) -> f32 {
        self.shares_per_minute
    }

    /// Returns the supported extensions.
    pub fn supported_extensions(&self) -> &[u16] {
        &self.supported_extensions
    }

    /// Returns the required extensions.
    pub fn required_extensions(&self) -> &[u16] {
        &self.required_extensions
    }

    /// Sets the log directory.
    pub fn set_log_dir(&mut self, log_dir: Option<PathBuf>) {
        if let Some(dir) = log_dir {
            self.log_file = Some(dir);
        }
    }
    /// Returns the log directory.
    pub fn log_dir(&self) -> Option<&Path> {
        self.log_file.as_deref()
    }

    /// Returns the server id.
    pub fn server_id(&self) -> u8 {
        self.server_id
    }

    pub fn get_txout(&self) -> TxOut {
        TxOut {
            value: Amount::from_sat(0),
            script_pubkey: self.coinbase_reward_script.script_pubkey(),
        }
    }

    /// Returns the monitoring address (optional).
    pub fn monitoring_address(&self) -> Option<SocketAddr> {
        self.monitoring_address
    }

    /// Returns the monitoring cache refresh interval in seconds.
    pub fn monitoring_cache_refresh_secs(&self) -> Option<u64> {
        self.monitoring_cache_refresh_secs
    }

    /// Builds a complete [`JDSConfig`] from the partial `[jds]` TOML section
    /// plus shared fields inherited from Pool config.
    ///
    /// Returns `Ok(None)` when the `[jds]` TOML section is absent.
    #[allow(clippy::result_large_err)]
    pub fn build_jds_config(&self) -> Result<Option<JDSConfig>, PoolErrorKind> {
        let Some(jds_partial) = self.jds.clone() else {
            return Ok(None);
        };

        let jds_config = JDSConfig::from_partial(
            jds_partial,
            self.authority_public_key,
            self.authority_secret_key,
            self.cert_validity_sec,
            self.coinbase_reward_script.clone(),
        );

        Ok(Some(jds_config))
    }
}

/// Pool's authority public and secret keys.
pub struct AuthorityConfig {
    pub public_key: Secp256k1PublicKey,
    pub secret_key: Secp256k1SecretKey,
}

impl AuthorityConfig {
    pub fn new(public_key: Secp256k1PublicKey, secret_key: Secp256k1SecretKey) -> Self {
        Self {
            public_key,
            secret_key,
        }
    }
}

/// Connection settings for the Pool listener.
pub struct ConnectionConfig {
    listen_address: SocketAddr,
    cert_validity_sec: u64,
    signature: String,
}

impl ConnectionConfig {
    pub fn new(listen_address: SocketAddr, cert_validity_sec: u64, signature: String) -> Self {
        Self {
            listen_address,
            cert_validity_sec,
            signature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::config_helpers::load_config;

    /// Writes a minimal pool config with `extra` spliced in, so each test varies exactly
    /// one thing, and loads it through the SAME loader the binary uses — not a bare
    /// `toml::from_str`. That way the test covers serde attributes, the env-override layer
    /// and the enum handling, rather than just the struct definition.
    fn load_with(extra: &str, name: &str) -> PoolConfig {
        let path = std::env::temp_dir().join(format!("pool-config-{name}.toml"));
        std::fs::write(
            &path,
            format!(
                r#"
listen_address = "0.0.0.0:34254"
authority_public_key = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"
authority_secret_key = "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n"
cert_validity_sec = 3600
coinbase_reward_script = "addr(tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8)"
pool_signature = "test"
shares_per_minute = 6.0
share_batch_size = 10
{extra}

[template_provider_type.Sv2Tp]
address = "127.0.0.1:8442"
"#
            ),
        )
        .expect("write temp config");
        let cfg = load_config(&path, "POOL_TEST_UNUSED", &[], &["template_provider_type"])
            .expect("config loads");
        let _ = std::fs::remove_file(&path);
        cfg
    }

    #[test]
    fn max_past_jobs_defaults_to_none_when_absent() {
        // Absent must mean "use the library default", not 0: a zero cap would evict the job
        // that just retired and reject the most common late share as invalid-job-id.
        assert_eq!(load_with("", "absent").max_past_jobs(), None);
    }

    #[test]
    fn max_past_jobs_is_read_from_config() {
        // The A/B deployment varies this single value between otherwise-identical pools, so
        // it has to survive loading verbatim.
        assert_eq!(
            load_with("max_past_jobs = 2", "set").max_past_jobs(),
            Some(2)
        );
    }

    #[test]
    fn max_past_jobs_setter_overrides() {
        let mut cfg = load_with("", "setter");
        cfg.set_max_past_jobs(Some(50));
        assert_eq!(cfg.max_past_jobs(), Some(50));
    }
}
