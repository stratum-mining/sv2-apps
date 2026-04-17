use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};
use stratum_apps::{
    config_helpers::{opt_path_from_toml, CoinbaseRewardScript},
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    stratum_core::bitcoin::{Amount, TxOut},
    tp_type::{TemplateProviderType, VALID_NETWORKS},
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
    /// User Identity
    user_identity: String,
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
    /// Optional override for the Bitcoin network name exposed via `GET /api/v1/global`.
    /// When absent the network is inferred from the template provider config:
    /// - `Sv2Tp`: mapped from the sv2-tp port using well-known defaults (see
    ///   `stratum_apps::tp_type::network_from_tp_port`). Returns `None` for non-standard ports.
    /// - `BitcoinCoreIpc`: taken directly from the `BitcoinNetwork` enum value.
    /// Values follow bitcoin-cli convention: `"main"`, `"test"`, `"testnet4"`,
    /// `"signet"`, `"regtest"`.
    #[serde(default)]
    network: Option<String>,
}

impl JobDeclaratorClientConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        listening_address: SocketAddr,
        protocol_config: ProtocolConfig,
        user_identity: String,
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
            user_identity,
            shares_per_minute,
            share_batch_size: shares_batch_size,
            mode: jdc_mode.unwrap_or_default(),
            supported_extensions,
            required_extensions,
            monitoring_address,
            monitoring_cache_refresh_secs,
            network: None,
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
            script_pubkey: self.coinbase_reward_script.script_pubkey().to_owned(),
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
    pub fn user_identity(&self) -> &str {
        &self.user_identity
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

    /// Set the Bitcoin network override (builder style).
    /// Only needed for non-standard sv2-tp port setups; `BitcoinCoreIpc` configs derive the
    /// network automatically from the `BitcoinNetwork` enum value.
    pub fn with_network(mut self, network: Option<String>) -> Self {
        self.network = network;
        self
    }

    /// Returns the effective Bitcoin network name: the explicit `network` override if set,
    /// otherwise inferred from `template_provider_type`.
    ///
    /// Returns `None` if the explicit override is not one of the known values, or if using
    /// `Sv2Tp` with a non-standard port (set `network` explicitly for non-standard port setups).
    pub fn effective_network(&self) -> Option<String> {
        if let Some(ref n) = self.network {
            if !VALID_NETWORKS.contains(&n.as_str()) {
                tracing::warn!(
                    "jdc config: network {:?} is not a recognised value \
                     (expected one of {:?}); network will not be reported.",
                    n,
                    VALID_NETWORKS
                );
                return None;
            }
            return Some(n.clone());
        }
        self.template_provider_type.infer_network().map(|s| s.to_string())
    }
}

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum ConfigJDCMode {
    #[default]
    FullTemplate,
    CoinbaseOnly,
    SoloMining,
}

impl std::str::FromStr for ConfigJDCMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "COINBASEONLY" => Ok(ConfigJDCMode::CoinbaseOnly),
            "SOLOMINING" => Ok(ConfigJDCMode::SoloMining),
            _ => Ok(ConfigJDCMode::FullTemplate),
        }
    }
}

fn deserialize_jdc_mode<'de, D>(deserializer: D) -> Result<ConfigJDCMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    Ok(ConfigJDCMode::from_str(&s).unwrap_or_default())
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
}

impl Upstream {
    /// Creates a new instance of [`Upstream`].
    pub fn new(
        authority_pubkey: Secp256k1PublicKey,
        pool_address: String,
        pool_port: u16,
        jds_address: String,
        jds_port: u16,
    ) -> Self {
        Self {
            authority_pubkey,
            pool_address,
            pool_port,
            jds_address,
            jds_port,
        }
    }
}

#[cfg(test)]
mod tests {
    use stratum_apps::tp_type::{BitcoinNetwork, TemplateProviderType};

    fn sv2_tp(address: &str) -> TemplateProviderType {
        TemplateProviderType::Sv2Tp {
            address: address.to_string(),
            public_key: None,
        }
    }

    fn ipc_tp(network: BitcoinNetwork) -> TemplateProviderType {
        TemplateProviderType::BitcoinCoreIpc {
            network,
            data_dir: None,
            fee_threshold: 0,
            min_interval: 5,
        }
    }

    #[test]
    fn infer_network_sv2tp_standard_ports() {
        assert_eq!(sv2_tp("127.0.0.1:18447").infer_network(), Some("regtest"));
        assert_eq!(sv2_tp("127.0.0.1:8442").infer_network(), Some("main"));
        assert_eq!(sv2_tp("127.0.0.1:18442").infer_network(), Some("test"));
        assert_eq!(sv2_tp("127.0.0.1:48442").infer_network(), Some("testnet4"));
        assert_eq!(sv2_tp("127.0.0.1:38442").infer_network(), Some("signet"));
    }

    #[test]
    fn infer_network_sv2tp_nonstandard_port_returns_none() {
        assert_eq!(sv2_tp("127.0.0.1:4444").infer_network(), None);
        assert_eq!(sv2_tp("127.0.0.1:3333").infer_network(), None);
    }

    #[test]
    fn infer_network_bitcoin_core_ipc() {
        assert_eq!(ipc_tp(BitcoinNetwork::Regtest).infer_network(), Some("regtest"));
        assert_eq!(ipc_tp(BitcoinNetwork::Mainnet).infer_network(), Some("main"));
        assert_eq!(ipc_tp(BitcoinNetwork::Testnet4).infer_network(), Some("testnet4"));
        assert_eq!(ipc_tp(BitcoinNetwork::Signet).infer_network(), Some("signet"));
    }
}
