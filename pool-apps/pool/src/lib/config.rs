//! ## Configuration Module
//!
//! Defines [`PoolConfig`], the configuration structure for the Pool, along with its supporting
//! types.
//!
//! This module handles:
//! - Initializing [`PoolConfig`]
//! - Managing [`TemplateProviderConfig`], [`AuthorityConfig`], [`CoinbaseOutput`], and
//!   [`ConnectionConfig`]
//! - Validating and converting coinbase outputs
#[cfg(feature = "persistence")]
use std::sync::Arc;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[cfg(feature = "persistence")]
use stratum_apps::task_manager::TaskManager;
use stratum_apps::{
    config_helpers::CoinbaseRewardScript,
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    stratum_core::bitcoin::{Amount, TxOut},
    utils::types::{SharesBatchSize, SharesPerMinute},
};

/// Configuration for the Pool, including connection, authority, and coinbase settings.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PoolConfig {
    listen_address: SocketAddr,
    tp_address: String,
    tp_authority_public_key: Option<Secp256k1PublicKey>,
    authority_public_key: Secp256k1PublicKey,
    authority_secret_key: Secp256k1SecretKey,
    cert_validity_sec: u64,
    coinbase_reward_script: CoinbaseRewardScript,
    pool_signature: String,
    shares_per_minute: SharesPerMinute,
    share_batch_size: SharesBatchSize,
    log_file: Option<PathBuf>,
    server_id: u16,
    #[cfg(feature = "persistence")]
    persistence: Option<PersistenceConfig>,
}

/// File backend configuration
#[cfg(feature = "persistence")]
#[derive(Clone, Debug, serde::Deserialize)]
pub struct FileBackendConfig {
    /// Path to the persistence file
    pub file_path: PathBuf,
    /// Channel buffer size for async persistence
    #[serde(default = "default_channel_size")]
    pub channel_size: usize,
}

/// Persistence configuration for share event logging.
///
/// This is only available when the `persistence` feature is enabled.
#[cfg(feature = "persistence")]
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PersistenceConfig {
    /// Backend type: "file", "sqlite", etc.
    pub backend: String,
    /// Which entities to persist (e.g., ["shares"])
    #[serde(default = "default_entities")]
    pub entities: Vec<String>,
    /// File backend configuration (only used when backend = "file")
    #[serde(default)]
    pub file: Option<FileBackendConfig>,
    // Future: Add more backend configs
    // pub sqlite: Option<SqliteBackendConfig>,
}

#[cfg(feature = "persistence")]
fn default_channel_size() -> usize {
    10000
}

#[cfg(feature = "persistence")]
fn default_entities() -> Vec<String> {
    vec!["shares".to_string()]
}

/// Implement IntoPersistence trait for pool's config type
#[cfg(feature = "persistence")]
impl stratum_apps::persistence::IntoPersistence for PersistenceConfig {
    fn into_persistence(self, task_manager: Arc<TaskManager>) -> Result<stratum_apps::persistence::Persistence, stratum_apps::persistence::Error> {
        use stratum_apps::persistence::{Backend, EntityType, FileBackend, Persistence};
        
        // Parse entity types
        let enabled_entities: Vec<EntityType> = self
            .entities
            .iter()
            .filter_map(|s| match s.as_str() {
                "shares" => Some(EntityType::Share),
                // Future: "connections" => Some(EntityType::Connection),
                _ => {
                    tracing::warn!("Unknown entity type: {}", s);
                    None
                }
            })
            .collect();
        
        // Create backend based on config
        let backend = match self.backend.as_str() {
            "file" => {
                let file_config = self.file
                    .ok_or_else(|| stratum_apps::persistence::Error::Custom(
                        "[persistence.file] section required for file backend".to_string()
                    ))?;
                
                Backend::File(FileBackend::new(file_config.file_path, file_config.channel_size, task_manager)?)
            }
            // Future: Add more backends here
            // "sqlite" => {
            //     let sqlite_config = self.sqlite
            //         .ok_or_else(|| Error::Custom("[persistence.sqlite] section required".to_string()))?;
            //     Backend::Sqlite(SqliteBackend::new(sqlite_config.database_path, sqlite_config.pool_size)?)
            // }
            other => {
                return Err(stratum_apps::persistence::Error::Custom(
                    format!("Unknown backend type: {}", other)
                ));
            }
        };
        
        Ok(Persistence::with_backend(backend, enabled_entities))
    }
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
        template_provider: TemplateProviderConfig,
        authority_config: AuthorityConfig,
        coinbase_reward_script: CoinbaseRewardScript,
        shares_per_minute: SharesPerMinute,
        share_batch_size: SharesBatchSize,
        server_id: u16,
        #[cfg(feature = "persistence")] persistence: Option<PersistenceConfig>,
    ) -> Self {
        Self {
            listen_address: pool_connection.listen_address,
            tp_address: template_provider.address,
            tp_authority_public_key: template_provider.authority_public_key,
            authority_public_key: authority_config.public_key,
            authority_secret_key: authority_config.secret_key,
            cert_validity_sec: pool_connection.cert_validity_sec,
            coinbase_reward_script,
            pool_signature: pool_connection.signature,
            shares_per_minute,
            share_batch_size,
            log_file: None,
            server_id,
            #[cfg(feature = "persistence")]
            persistence,
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

    /// Return the Template Provider authority public key.
    pub fn tp_authority_public_key(&self) -> Option<&Secp256k1PublicKey> {
        self.tp_authority_public_key.as_ref()
    }

    /// Returns the Template Provider address.
    pub fn tp_address(&self) -> &String {
        &self.tp_address
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
    pub fn shares_per_minute(&self) -> f32 {
        self.shares_per_minute
    }

    /// Change TP address.
    pub fn set_tp_address(&mut self, tp_address: String) {
        self.tp_address = tp_address;
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
    pub fn server_id(&self) -> u16 {
        self.server_id
    }

    /// Returns the persistence configuration.
    ///
    /// Only available when the `persistence` feature is enabled.
    #[cfg(feature = "persistence")]
    pub fn persistence(&self) -> Option<&PersistenceConfig> {
        self.persistence.as_ref()
    }

    pub fn get_txout(&self) -> TxOut {
        TxOut {
            value: Amount::from_sat(0),
            script_pubkey: self.coinbase_reward_script.script_pubkey().to_owned(),
        }
    }
}

/// Configuration for connecting to a Template Provider.
pub struct TemplateProviderConfig {
    address: String,
    authority_public_key: Option<Secp256k1PublicKey>,
}

impl TemplateProviderConfig {
    pub fn new(address: String, authority_public_key: Option<Secp256k1PublicKey>) -> Self {
        Self {
            address,
            authority_public_key,
        }
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

    #[cfg(feature = "persistence")]
    #[test]
    fn test_persistence_config_file_backend() {
        use stratum_apps::persistence::IntoPersistence;
        use std::path::PathBuf;

        let config = PersistenceConfig {
            backend: "file".to_string(),
            entities: vec!["shares".to_string()],
            file: Some(FileBackendConfig {
                file_path: PathBuf::from("/tmp/test_pool_persistence.log"),
                channel_size: 5000,
            }),
        };

        // Create a TaskManager for the test
        let task_manager = Arc::new(TaskManager::new());

        // Test that config can be converted to Persistence
        let result = config.into_persistence(task_manager);
        assert!(result.is_ok());
        
        // Clean up test file if created
        let _ = std::fs::remove_file("/tmp/test_pool_persistence.log");
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_persistence_config_missing_file_section() {
        use stratum_apps::persistence::IntoPersistence;

        let config = PersistenceConfig {
            backend: "file".to_string(),
            entities: vec!["shares".to_string()],
            file: None, // Missing file config
        };

        // Create a TaskManager for the test
        let task_manager = Arc::new(TaskManager::new());

        // Should fail because file backend requires [persistence.file] section
        let result = config.into_persistence(task_manager);
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("[persistence.file] section required"));
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_persistence_config_unknown_backend() {
        use stratum_apps::persistence::IntoPersistence;
        use std::path::PathBuf;

        let config = PersistenceConfig {
            backend: "unknown_backend".to_string(),
            entities: vec!["shares".to_string()],
            file: Some(FileBackendConfig {
                file_path: PathBuf::from("/tmp/test.log"),
                channel_size: 5000,
            }),
        };

        // Create a TaskManager for the test
        let task_manager = Arc::new(TaskManager::new());

        // Should fail with unknown backend error
        let result = config.into_persistence(task_manager);
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("Unknown backend type"));
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_persistence_config_entity_filtering() {
        use stratum_apps::persistence::IntoPersistence;
        use std::path::PathBuf;

        let config = PersistenceConfig {
            backend: "file".to_string(),
            entities: vec![
                "shares".to_string(),
                "unknown_entity".to_string(), // Should be filtered out
            ],
            file: Some(FileBackendConfig {
                file_path: PathBuf::from("/tmp/test.log"),
                channel_size: 5000,
            }),
        };

        // Create a TaskManager for the test
        let task_manager = Arc::new(TaskManager::new());

        // Should succeed and filter out unknown entities
        let result = config.into_persistence(task_manager);
        assert!(result.is_ok());
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_file_backend_config_channel_size() {
        use std::path::PathBuf;
        
        // Test that FileBackendConfig can be created with custom channel_size
        let config = FileBackendConfig {
            file_path: PathBuf::from("/tmp/test.log"),
            channel_size: 5000,
        };
        assert_eq!(config.channel_size, 5000);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_persistence_config_multiple_entities() {
        use stratum_apps::persistence::IntoPersistence;
        use std::path::PathBuf;

        // Test with multiple entities (even though only "shares" is currently supported)
        let config = PersistenceConfig {
            backend: "file".to_string(),
            entities: vec!["shares".to_string()],
            file: Some(FileBackendConfig {
                file_path: PathBuf::from("/tmp/test_multi.log"),
                channel_size: 10000,
            }),
        };

        // Create a TaskManager for the test
        let task_manager = Arc::new(TaskManager::new());

        let result = config.into_persistence(task_manager);
        assert!(result.is_ok());
        
        // Clean up
        let _ = std::fs::remove_file("/tmp/test_multi.log");
    }
}
