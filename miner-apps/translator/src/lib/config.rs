//! ## Translator Configuration Module
//!
//! Defines [`TranslatorConfig`], the primary configuration structure for the Translator.
//!
//! This module provides the necessary structures to configure the Translator,
//! managing connections and settings for both upstream and downstream interfaces.
//!
//! This module handles:
//! - Upstream server address, port, and authentication key ([`Upstream`])
//! - Downstream interface address and port ([`TranslatorConfig::downstream_address`],
//!   [`TranslatorConfig::downstream_port`])
//! - Supported protocol versions
//! - Downstream difficulty adjustment parameters ([`DownstreamDifficultyConfig`])
use std::path::{Path, PathBuf};

use serde::Deserialize;
use std::net::SocketAddr;
use stratum_apps::{
    config_helpers::opt_path_from_toml,
    key_utils::Secp256k1PublicKey,
    payout::{MissingMinerPayoutMode, PayoutMode, PayoutModeError},
    utils::types::{Hashrate, SharesPerMinute},
};

/// Configuration for the Translator.
#[derive(Debug, Deserialize, Clone)]
pub struct TranslatorConfig {
    pub upstreams: Vec<Upstream>,
    /// The address for the downstream interface.
    pub downstream_address: String,
    /// The port for the downstream interface.
    pub downstream_port: u16,
    /// The maximum supported protocol version for communication.
    pub max_supported_version: u16,
    /// The minimum supported protocol version for communication.
    pub min_supported_version: u16,
    /// The size of the extranonce2 field for downstream mining connections.
    pub downstream_extranonce2_size: u16,
    /// Whether to verify upstream coinbase outputs against a payout address encoded in each
    /// upstream `user_identity`.
    #[serde(default)]
    pub verify_payout: bool,
    /// Configuration settings for managing difficulty on the downstream connection.
    pub downstream_difficulty_config: DownstreamDifficultyConfig,
    /// Whether to aggregate all downstream connections into a single upstream channel.
    /// If true, all miners share one channel. If false, each miner gets its own channel.
    pub aggregate_channels: bool,
    /// Protocol extensions that the translator supports (will request if supported by server).
    #[serde(default)]
    pub supported_extensions: Vec<u16>,
    /// Protocol extensions that the translator requires (server must support these).
    /// If the upstream server doesn't support these, the translator will fail over to another
    /// upstream.
    #[serde(default)]
    pub required_extensions: Vec<u16>,
    /// The path to the log file for the Translator.
    #[serde(default, deserialize_with = "opt_path_from_toml")]
    log_file: Option<PathBuf>,
    /// Optional monitoring server bind address
    #[serde(default)]
    monitoring_address: Option<SocketAddr>,
    #[serde(default)]
    monitoring_cache_refresh_secs: Option<u64>,
    /// Emit one Prometheus series per connected SV1 miner (`sv1_client_hashrate`).
    ///
    /// Off by default: the series count scales with miner count. The JSON API at
    /// `/api/v1/sv1/clients` serves per-client state either way; this only adds
    /// per-miner history to Prometheus.
    #[serde(default)]
    monitoring_sv1_per_client_metrics: Option<bool>,
    #[serde(default)]
    miner_telemetry: MinerTelemetryConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct MinerTelemetryConfig {
    /// Private IPv4 CIDRs to scan for miner management interfaces.
    #[serde(default)]
    pub cidrs: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Upstream {
    /// The address of the upstream server.
    pub address: String,
    /// The port of the upstream server.
    pub port: u16,
    /// The Secp256k1 public key used to authenticate the upstream authority.
    pub authority_pubkey: Secp256k1PublicKey,
    /// The user identity/username to use when connecting to the pool.
    /// This will be appended with a counter for each mining channel (e.g., username.miner1,
    /// username.miner2).
    pub user_identity: String,
}

impl Upstream {
    /// Creates a new `UpstreamConfig` instance.
    pub fn new(
        address: String,
        port: u16,
        authority_pubkey: Secp256k1PublicKey,
        user_identity: String,
    ) -> Self {
        Self {
            address,
            port,
            authority_pubkey,
            user_identity,
        }
    }
}

impl TranslatorConfig {
    /// Creates a new `TranslatorConfig` instance with the specified upstream and downstream
    /// configurations and version constraints.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        upstreams: Vec<Upstream>,
        downstream_address: String,
        downstream_port: u16,
        downstream_difficulty_config: DownstreamDifficultyConfig,
        max_supported_version: u16,
        min_supported_version: u16,
        downstream_extranonce2_size: u16,
        verify_payout: bool,
        aggregate_channels: bool,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        monitoring_address: Option<SocketAddr>,
        monitoring_cache_refresh_secs: Option<u64>,
    ) -> Self {
        Self {
            upstreams,
            downstream_address,
            downstream_port,
            max_supported_version,
            min_supported_version,
            downstream_extranonce2_size,
            verify_payout,
            downstream_difficulty_config,
            aggregate_channels,
            supported_extensions,
            required_extensions,
            log_file: None,
            monitoring_address,
            monitoring_cache_refresh_secs,
            monitoring_sv1_per_client_metrics: None,
            miner_telemetry: MinerTelemetryConfig::default(),
        }
    }

    /// Whether to emit one Prometheus series per connected SV1 miner. Off unless
    /// explicitly enabled.
    pub fn monitoring_sv1_per_client_metrics(&self) -> bool {
        self.monitoring_sv1_per_client_metrics.unwrap_or(false)
    }

    /// Returns the monitoring server bind address (if enabled)
    pub fn monitoring_address(&self) -> Option<SocketAddr> {
        self.monitoring_address
    }

    /// Returns the monitoring cache refresh interval in seconds.
    pub fn monitoring_cache_refresh_secs(&self) -> Option<u64> {
        self.monitoring_cache_refresh_secs
    }

    /// Returns the miner management CIDRs used for telemetry discovery.
    pub fn miner_telemetry_cidrs(&self) -> &[String] {
        &self.miner_telemetry.cidrs
    }

    pub(crate) fn expected_payout_distribution(
        &self,
        user_identity: &str,
    ) -> Result<Option<PayoutMode>, PayoutModeError> {
        if !self.verify_payout {
            return Ok(None);
        }

        match PayoutMode::try_from(user_identity) {
            Ok(
                payout_mode @ (PayoutMode::Solo { .. }
                | PayoutMode::LegacySolo { .. }
                | PayoutMode::Donate { .. }),
            ) => Ok(Some(payout_mode)),
            Ok(PayoutMode::FullDonation) => Err(PayoutModeError::MissingMinerPayout {
                user_identity: user_identity.to_string(),
                mode: MissingMinerPayoutMode::FullDonation,
            }),
            Err(PayoutModeError::NoPayoutMode(_)) => Err(PayoutModeError::MissingMinerPayout {
                user_identity: user_identity.to_string(),
                mode: MissingMinerPayoutMode::NoPayoutMode,
            }),
            Err(e) => Err(e),
        }
    }

    pub fn set_log_dir(&mut self, log_dir: Option<PathBuf>) {
        if let Some(dir) = log_dir {
            self.log_file = Some(dir);
        }
    }
    pub fn log_dir(&self) -> Option<&Path> {
        self.log_file.as_deref()
    }
}

/// Configuration settings for managing difficulty adjustments on the downstream connection.
#[derive(Debug, Deserialize, Clone)]
pub struct DownstreamDifficultyConfig {
    /// The minimum hashrate expected from an individual miner on the downstream connection.
    pub min_individual_miner_hashrate: Hashrate,
    /// The target number of shares per minute for difficulty adjustment.
    pub shares_per_minute: SharesPerMinute,
    /// Whether to enable variable difficulty adjustment mechanism.
    /// If false, difficulty will be managed by upstream (useful with JDC).
    pub enable_vardiff: bool,
    /// Interval in seconds for sending keepalive jobs to downstream miners.
    /// The translator will send periodic mining.notify messages with updated time
    /// to prevent SV1 miners from timing out when the upstream doesn't send new jobs
    /// frequently enough (e.g., due to low Bitcoin mempool activity).
    /// Set to 0 to disable keepalive jobs.
    pub job_keepalive_interval_secs: u16,
}

impl DownstreamDifficultyConfig {
    /// Creates a new `DownstreamDifficultyConfig` instance.
    pub fn new(
        min_individual_miner_hashrate: Hashrate,
        shares_per_minute: SharesPerMinute,
        enable_vardiff: bool,
        job_keepalive_interval_secs: u16,
    ) -> Self {
        Self {
            min_individual_miner_hashrate,
            shares_per_minute,
            enable_vardiff,
            job_keepalive_interval_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn create_test_upstream() -> Upstream {
        // Use a valid base58-encoded public key from the key-utils test cases
        let pubkey_str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
        let pubkey = Secp256k1PublicKey::from_str(pubkey_str).unwrap();
        Upstream::new("127.0.0.1".to_string(), 4444, pubkey, "IT_TEST".to_string())
    }

    fn create_test_difficulty_config() -> DownstreamDifficultyConfig {
        DownstreamDifficultyConfig::new(100.0, 5.0, true, 60)
    }

    #[test]
    fn test_upstream_creation() {
        let upstream = create_test_upstream();
        assert_eq!(upstream.address, "127.0.0.1");
        assert_eq!(upstream.port, 4444);
    }

    #[test]
    fn test_downstream_difficulty_config_creation() {
        let config = create_test_difficulty_config();
        assert_eq!(config.min_individual_miner_hashrate, 100.0);
        assert_eq!(config.shares_per_minute, 5.0);
        assert!(config.enable_vardiff);
    }

    #[test]
    fn test_translator_config_creation() {
        let upstreams = vec![create_test_upstream()];
        let difficulty_config = create_test_difficulty_config();

        let config = TranslatorConfig::new(
            upstreams,
            "0.0.0.0".to_string(),
            3333,
            difficulty_config,
            2,
            1,
            4,
            false,
            true,
            vec![],
            vec![],
            None,
            None,
        );

        assert_eq!(config.upstreams.len(), 1);
        assert_eq!(config.downstream_address, "0.0.0.0");
        assert_eq!(config.downstream_port, 3333);
        assert_eq!(config.max_supported_version, 2);
        assert_eq!(config.min_supported_version, 1);
        assert_eq!(config.downstream_extranonce2_size, 4);
        assert!(!config.verify_payout);
        assert!(config.aggregate_channels);
        assert!(config.supported_extensions.is_empty());
        assert!(config.required_extensions.is_empty());
        assert!(config.log_file.is_none());
    }

    #[test]
    fn payout_verification_requires_explicit_opt_in() {
        let upstreams = vec![create_test_upstream()];
        let difficulty_config = create_test_difficulty_config();
        let payout_address = "bc1qtzqxqaxyy6lda2fhdtp5dp0v56vlf6g0tljy2x";

        let disabled_config = TranslatorConfig::new(
            upstreams.clone(),
            "0.0.0.0".to_string(),
            3333,
            difficulty_config.clone(),
            2,
            1,
            4,
            false,
            true,
            vec![],
            vec![],
            None,
            None,
        );

        assert!(
            disabled_config
                .expected_payout_distribution(payout_address)
                .unwrap()
                .is_none()
        );

        let enabled_config = TranslatorConfig::new(
            upstreams,
            "0.0.0.0".to_string(),
            3333,
            difficulty_config,
            2,
            1,
            4,
            true,
            true,
            vec![],
            vec![],
            None,
            None,
        );

        assert!(matches!(
            enabled_config
                .expected_payout_distribution(payout_address)
                .unwrap(),
            Some(PayoutMode::LegacySolo { .. })
        ));
    }

    #[test]
    fn payout_verification_requires_miner_payout_identity() {
        let config = TranslatorConfig::new(
            vec![create_test_upstream()],
            "0.0.0.0".to_string(),
            3333,
            create_test_difficulty_config(),
            2,
            1,
            4,
            true,
            true,
            vec![],
            vec![],
            None,
            None,
        );

        assert!(matches!(
            config
                .expected_payout_distribution("sri/donate/worker")
                .unwrap_err(),
            PayoutModeError::MissingMinerPayout {
                mode: MissingMinerPayoutMode::FullDonation,
                ..
            }
        ));

        assert!(matches!(
            config
                .expected_payout_distribution("invalid_address.worker")
                .unwrap_err(),
            PayoutModeError::MissingMinerPayout {
                mode: MissingMinerPayoutMode::NoPayoutMode,
                ..
            }
        ));
    }

    #[test]
    fn payout_verification_rejects_address_like_typos() {
        let config = TranslatorConfig::new(
            vec![create_test_upstream()],
            "0.0.0.0".to_string(),
            3333,
            create_test_difficulty_config(),
            2,
            1,
            4,
            true,
            true,
            vec![],
            vec![],
            None,
            None,
        );

        assert!(matches!(
            config
                .expected_payout_distribution("bc1q_typo.worker")
                .unwrap_err(),
            PayoutModeError::MissingMinerPayout {
                mode: MissingMinerPayoutMode::NoPayoutMode,
                ..
            }
        ));
    }

    #[test]
    fn test_translator_config_log_dir() {
        let upstreams = vec![create_test_upstream()];
        let difficulty_config = create_test_difficulty_config();

        let mut config = TranslatorConfig::new(
            upstreams,
            "0.0.0.0".to_string(),
            3333,
            difficulty_config,
            2,
            1,
            4,
            false,
            false,
            vec![],
            vec![],
            None,
            None,
        );

        assert!(config.log_dir().is_none());

        let log_path = PathBuf::from("/tmp/logs");
        config.set_log_dir(Some(log_path.clone()));
        assert_eq!(config.log_dir(), Some(log_path.as_path()));

        config.set_log_dir(None);
        assert_eq!(config.log_dir(), Some(log_path.as_path())); // Should remain unchanged
    }

    #[test]
    fn test_multiple_upstreams() {
        let upstream1 = create_test_upstream();
        let mut upstream2 = create_test_upstream();
        upstream2.address = "192.168.1.1".to_string();
        upstream2.port = 5555;

        let upstreams = vec![upstream1, upstream2];
        let difficulty_config = create_test_difficulty_config();

        let config = TranslatorConfig::new(
            upstreams,
            "0.0.0.0".to_string(),
            3333,
            difficulty_config,
            2,
            1,
            4,
            false,
            true,
            vec![],
            vec![],
            None,
            None,
        );

        assert_eq!(config.upstreams.len(), 2);
        assert_eq!(config.upstreams[0].address, "127.0.0.1");
        assert_eq!(config.upstreams[0].port, 4444);
        assert_eq!(config.upstreams[1].address, "192.168.1.1");
        assert_eq!(config.upstreams[1].port, 5555);
    }

    #[test]
    fn test_vardiff_disabled_config() {
        let mut difficulty_config = create_test_difficulty_config();
        difficulty_config.enable_vardiff = false;

        let upstreams = vec![create_test_upstream()];
        let config = TranslatorConfig::new(
            upstreams,
            "0.0.0.0".to_string(),
            3333,
            difficulty_config,
            2,
            1,
            4,
            false,
            false,
            vec![],
            vec![],
            None,
            None,
        );

        assert!(!config.downstream_difficulty_config.enable_vardiff);
        assert!(!config.aggregate_channels);
    }

    #[test]
    fn test_upstream_user_identity_toml_present() {
        let toml = r#"
            address = "127.0.0.1"
            port = 4444
            authority_pubkey = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan"
            user_identity = "sri/solo/bc1qtest"
        "#;
        let upstream: Upstream = toml::from_str(toml).unwrap();
        assert_eq!(upstream.user_identity, "sri/solo/bc1qtest");
    }

    #[test]
    fn test_upstream_user_identity_toml_absent_is_rejected() {
        let toml = r#"
            address = "127.0.0.1"
            port = 4444
            authority_pubkey = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan"
        "#;
        assert!(toml::from_str::<Upstream>(toml).is_err());
    }

    #[test]
    fn test_upstream_user_identity_toml_empty_string() {
        let toml = r#"
            address = "127.0.0.1"
            port = 4444
            authority_pubkey = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan"
            user_identity = ""
        "#;
        let upstream: Upstream = toml::from_str(toml).unwrap();
        assert_eq!(upstream.user_identity, "");
    }

    #[test]
    fn test_multi_upstream_identity_toml() {
        let pubkey = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
        let toml = format!(
            r#"
            downstream_address = "0.0.0.0"
            downstream_port = 34255
            max_supported_version = 2
            min_supported_version = 2
            downstream_extranonce2_size = 8
            aggregate_channels = false

            [downstream_difficulty_config]
            min_individual_miner_hashrate = 10000000.0
            shares_per_minute = 6.0
            enable_vardiff = false
            job_keepalive_interval_secs = 60

            [[upstreams]]
            address = "127.0.0.1"
            port = 3333
            authority_pubkey = "{pubkey}"
            user_identity = "sri/solo/bc1qprimary"

            [[upstreams]]
            address = "192.168.1.1"
            port = 3333
            authority_pubkey = "{pubkey}"
            user_identity = "bc1qbackup.worker"
            "#
        );
        let config: TranslatorConfig = toml::from_str(&toml).unwrap();
        assert_eq!(config.upstreams.len(), 2);
        assert_eq!(config.upstreams[0].user_identity, "sri/solo/bc1qprimary");
        assert_eq!(config.upstreams[1].user_identity, "bc1qbackup.worker");
    }

    #[test]
    fn sv1_per_client_metrics_defaults_off_and_parses_when_set() {
        let pubkey = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
        let config_toml = |extra: &str| {
            format!(
                r#"
                downstream_address = "0.0.0.0"
                downstream_port = 34255
                max_supported_version = 2
                min_supported_version = 2
                downstream_extranonce2_size = 8
                aggregate_channels = false
                {extra}

                [downstream_difficulty_config]
                min_individual_miner_hashrate = 10000000.0
                shares_per_minute = 6.0
                enable_vardiff = false
                job_keepalive_interval_secs = 60

                [[upstreams]]
                address = "127.0.0.1"
                port = 3333
                authority_pubkey = "{pubkey}"
                user_identity = "sri/solo/bc1qprimary"
                "#
            )
        };

        let off: TranslatorConfig = toml::from_str(&config_toml("")).unwrap();
        assert!(
            !off.monitoring_sv1_per_client_metrics(),
            "per-client SV1 metrics must be off unless explicitly enabled"
        );

        let on: TranslatorConfig =
            toml::from_str(&config_toml("monitoring_sv1_per_client_metrics = true")).unwrap();
        assert!(on.monitoring_sv1_per_client_metrics());
    }
}
