use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};
use stratum_apps::{
    config_helpers::{CoinbaseRewardScript, opt_path_from_toml},
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    stratum_core::bitcoin::{Amount, TxOut},
    tp_type::TemplateProviderType,
    utils::types::{SharesBatchSize, SharesPerMinute},
};

#[derive(Debug, Deserialize, Clone)]
pub struct JobDeclaratorClientConfig {
    // The address on which the JDC will listen for incoming connections when acting as an
    // upstream.
    listening_address: SocketAddr,
    // The maximum supported SV2 protocol version.
    max_supported_version: u16,
    // The minimum supported SV2 protocol version.
    min_supported_version: u16,
    // The public key used by this JDC for noise encryption.
    authority_public_key: Secp256k1PublicKey,
    /// The secret key used by this JDC for noise encryption.
    authority_secret_key: Secp256k1SecretKey,
    /// The validity period (in seconds) for the certificate used in noise.
    cert_validity_sec: u64,
    /// The template provider type that this JDC will use.
    template_provider_type: TemplateProviderType,
    /// A list of upstream Job Declarator Servers (JDS) that this JDC can connect to.
    /// JDC can fallover between these upstreams.
    upstreams: Vec<Upstream>,
    /// This is only used during solo-mining.
    pub coinbase_reward_script: CoinbaseRewardScript,
    /// A signature string identifying this JDC instance.
    jdc_signature: String,
    /// The path to the log file where JDC will write logs.
    #[serde(default, deserialize_with = "opt_path_from_toml")]
    log_file: Option<PathBuf>,
    /// Shares per minute
    shares_per_minute: SharesPerMinute,
    /// share batch size
    share_batch_size: SharesBatchSize,
    /// JDC mode: FullTemplate, CoinbaseOnly, or SoloMining
    #[serde(deserialize_with = "deserialize_jdc_mode", default)]
    pub mode: ConfigJDCMode,
    /// Protocol extensions that the JDC supports (will accept if requested by downstream clients).
    #[serde(default)]
    supported_extensions: Vec<u16>,
    /// Protocol extensions that the JDC requires (downstream clients must support these).
    #[serde(default)]
    required_extensions: Vec<u16>,
    /// Optional monitoring server bind address
    #[serde(default)]
    monitoring_address: Option<SocketAddr>,
    #[serde(default)]
    monitoring_cache_refresh_secs: Option<u64>,
    #[serde(default)]
    miner_telemetry: MinerTelemetryConfig,
    /// Minimum rollable extranonce bytes JDC reserves for future extended downstreams on its
    /// single upstream channel (fixed at first open). Defaults to
    /// [`DEFAULT_RESERVED_DOWNSTREAM_ROLLABLE_EXTRANONCE_SIZE`] (8) when omitted; set higher if
    /// downstreams may request more.
    #[serde(default = "default_reserved_downstream_rollable_extranonce_size")]
    reserved_downstream_rollable_extranonce_size: u8,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct MinerTelemetryConfig {
    /// Private IPv4 CIDRs to scan for miner management interfaces.
    #[serde(default)]
    pub cidrs: Vec<String>,
}

/// Default value used by
/// [`JobDeclaratorClientConfig::reserved_downstream_rollable_extranonce_size`]
/// when the config file does not specify one.
pub const DEFAULT_RESERVED_DOWNSTREAM_ROLLABLE_EXTRANONCE_SIZE: u8 = 8;

fn default_reserved_downstream_rollable_extranonce_size() -> u8 {
    DEFAULT_RESERVED_DOWNSTREAM_ROLLABLE_EXTRANONCE_SIZE
}

impl JobDeclaratorClientConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        listening_address: SocketAddr,
        protocol_config: ProtocolConfig,
        shares_per_minute: SharesPerMinute,
        shares_batch_size: SharesBatchSize,
        pool_config: PoolConfig,
        cert_validity_sec: u64,
        template_provider_type: TemplateProviderType,
        upstreams: Vec<Upstream>,
        jdc_signature: String,
        jdc_mode: Option<ConfigJDCMode>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        monitoring_address: Option<SocketAddr>,
        monitoring_cache_refresh_secs: Option<u64>,
        reserved_downstream_rollable_extranonce_size: Option<u8>,
    ) -> Self {
        Self {
            listening_address,
            max_supported_version: protocol_config.max_supported_version,
            min_supported_version: protocol_config.min_supported_version,
            authority_public_key: pool_config.authority_public_key,
            authority_secret_key: pool_config.authority_secret_key,
            cert_validity_sec,
            template_provider_type,
            upstreams,
            coinbase_reward_script: protocol_config.coinbase_reward_script,
            jdc_signature,
            log_file: None,
            shares_per_minute,
            share_batch_size: shares_batch_size,
            mode: jdc_mode.unwrap_or_default(),
            supported_extensions,
            required_extensions,
            monitoring_address,
            monitoring_cache_refresh_secs,
            miner_telemetry: MinerTelemetryConfig::default(),
            reserved_downstream_rollable_extranonce_size:
                reserved_downstream_rollable_extranonce_size
                    .unwrap_or(DEFAULT_RESERVED_DOWNSTREAM_ROLLABLE_EXTRANONCE_SIZE),
        }
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

    /// Returns the listening address of the Job Declarator Client.
    pub fn listening_address(&self) -> &SocketAddr {
        &self.listening_address
    }

    /// Returns the list of upstreams.
    ///
    /// JDC will try to fallback to the next upstream in case of failure of the current one.
    pub fn upstreams(&self) -> &Vec<Upstream> {
        &self.upstreams
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

    /// Returns the template provider type.
    pub fn template_provider_type(&self) -> &TemplateProviderType {
        &self.template_provider_type
    }

    /// Returns the minimum supported version.
    pub fn min_supported_version(&self) -> u16 {
        self.min_supported_version
    }

    /// Returns the maximum supported version.
    pub fn max_supported_version(&self) -> u16 {
        self.max_supported_version
    }

    /// Returns the JDC signature.
    pub fn jdc_signature(&self) -> &str {
        &self.jdc_signature
    }

    pub fn get_txout(&self) -> TxOut {
        TxOut {
            value: Amount::from_sat(0),
            script_pubkey: self.coinbase_reward_script.script_pubkey(),
        }
    }

    pub fn log_file(&self) -> Option<&Path> {
        self.log_file.as_deref()
    }
    pub fn set_log_file(&mut self, log_file: Option<PathBuf>) {
        if let Some(log_file) = log_file {
            self.log_file = Some(log_file);
        }
    }

    pub fn shares_per_minute(&self) -> SharesPerMinute {
        self.shares_per_minute
    }

    pub fn share_batch_size(&self) -> SharesBatchSize {
        self.share_batch_size
    }

    /// Returns the supported extensions.
    pub fn supported_extensions(&self) -> &[u16] {
        &self.supported_extensions
    }

    /// Returns the required extensions.
    pub fn required_extensions(&self) -> &[u16] {
        &self.required_extensions
    }

    /// Rollable extranonce bytes reserved for future extended downstreams (same as the config
    /// field).
    pub fn reserved_downstream_rollable_extranonce_size(&self) -> u8 {
        self.reserved_downstream_rollable_extranonce_size
    }
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum ConfigJDCMode {
    #[default]
    FullTemplate,
    CoinbaseOnly,
    SoloMining,
}

impl std::str::FromStr for ConfigJDCMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "FULLTEMPLATE" => Ok(ConfigJDCMode::FullTemplate),
            "COINBASEONLY" => Ok(ConfigJDCMode::CoinbaseOnly),
            "SOLOMINING" => Ok(ConfigJDCMode::SoloMining),
            _ => Err(format!(
                "unknown JDC mode `{s}`, expected FullTemplate, CoinbaseOnly, SoloMining"
            )),
        }
    }
}

fn deserialize_jdc_mode<'de, D>(deserializer: D) -> Result<ConfigJDCMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    ConfigJDCMode::from_str(&s).map_err(serde::de::Error::custom)
}

/// Represents pool specific encryption keys.
pub struct PoolConfig {
    authority_public_key: Secp256k1PublicKey,
    authority_secret_key: Secp256k1SecretKey,
}

impl PoolConfig {
    /// Creates a new instance of [`PoolConfig`].
    pub fn new(
        authority_public_key: Secp256k1PublicKey,
        authority_secret_key: Secp256k1SecretKey,
    ) -> Self {
        Self {
            authority_public_key,
            authority_secret_key,
        }
    }
}

/// Represent protocol versioning the JDC supports.
pub struct ProtocolConfig {
    // The maximum supported SV2 protocol version.
    max_supported_version: u16,
    // The minimum supported SV2 protocol version.
    min_supported_version: u16,
    // A coinbase output to be included in block templates.
    coinbase_reward_script: CoinbaseRewardScript,
}

impl ProtocolConfig {
    // Creates a new instance of [`ProtocolConfig`].
    pub fn new(
        max_supported_version: u16,
        min_supported_version: u16,
        coinbase_reward_script: CoinbaseRewardScript,
    ) -> Self {
        Self {
            max_supported_version,
            min_supported_version,
            coinbase_reward_script,
        }
    }
}

/// Represents necessary fields required to connect to JDS
#[derive(Debug, Deserialize, Clone)]
pub struct Upstream {
    // The public key of the upstream pool's authority for authentication.
    pub authority_pubkey: Secp256k1PublicKey,
    // The address of the upstream pool's main server.
    pub pool_address: String,
    pub pool_port: u16,
    // The network address of the JDS.
    pub jds_address: String,
    pub jds_port: u16,
    pub user_identity: String,
}

impl Upstream {
    /// Creates a new instance of [`Upstream`].
    pub fn new(
        authority_pubkey: Secp256k1PublicKey,
        pool_address: String,
        pool_port: u16,
        jds_address: String,
        jds_port: u16,
        user_identity: String,
    ) -> Self {
        Self {
            authority_pubkey,
            pool_address,
            pool_port,
            jds_address,
            jds_port,
            user_identity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PUBKEY: &str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";

    #[test]
    fn test_upstream_user_identity_toml_present() {
        let toml = format!(
            r#"
            authority_pubkey = "{TEST_PUBKEY}"
            pool_address = "127.0.0.1"
            pool_port = 3333
            jds_address = "127.0.0.1"
            jds_port = 3334
            user_identity = "bc1qfallback.worker"
            "#
        );
        let upstream: Upstream = toml::from_str(&toml).unwrap();
        assert_eq!(upstream.user_identity, "bc1qfallback.worker");
    }

    #[test]
    fn test_upstream_user_identity_toml_absent_is_rejected() {
        let toml = format!(
            r#"
            authority_pubkey = "{TEST_PUBKEY}"
            pool_address = "127.0.0.1"
            pool_port = 3333
            jds_address = "127.0.0.1"
            jds_port = 3334
            "#
        );
        assert!(toml::from_str::<Upstream>(&toml).is_err());
    }

    #[test]
    fn test_upstream_user_identity_toml_empty_string() {
        let toml = format!(
            r#"
            authority_pubkey = "{TEST_PUBKEY}"
            pool_address = "127.0.0.1"
            pool_port = 3333
            jds_address = "127.0.0.1"
            jds_port = 3334
            user_identity = ""
            "#
        );
        let upstream: Upstream = toml::from_str(&toml).unwrap();
        assert_eq!(upstream.user_identity, "");
    }
}
