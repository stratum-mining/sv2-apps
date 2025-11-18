//! # Persistence Module
//!
//! Provides a generic persistence abstraction that can be used across different
//! Stratum V2 application roles with support for multiple backend implementations.
//!
//! ## Architecture
//!
//! - `PersistenceBackend` trait - Core abstraction for persistence
//! - `NoOpBackend` - Zero-cost no-op implementation (used when feature disabled)
//! - `FileBackend` - File-based persistence (available with `persistence` feature)
//!
//! ## Usage Pattern
//!
//! Applications implement the `IntoPersistence` trait for their config types,
//! allowing flexible configuration while maintaining type safety:
//! - **With feature enabled:** Applications can use any backend (file, sqlite, etc.)
//! - **Without feature:** Always uses `NoOpBackend` (zero-cost, optimized away by compiler)

#[cfg(feature = "persistence")]
pub mod file;
// #[cfg(feature = "persistence")]
// pub mod sqlite;
pub mod noop;

#[cfg(feature = "persistence")]
use std::sync::Arc;
use std::time::SystemTime;

use stratum_core::bitcoin::hashes::sha256d::Hash;

#[cfg(feature = "persistence")]
pub use file::FileBackend;
// #[cfg(feature = "persistence")]
// pub use sqlite::SqliteBackend;
pub use noop::NoOpBackend;

#[cfg(feature = "persistence")]
use crate::task_manager::TaskManager;

/// Entity types that can be persisted
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityType {
    Share,
    // Connection,
}

/// Generic event that can be persisted
#[derive(Debug, Clone)]
pub enum PersistenceEvent {
    Share(ShareEvent),
    // Connection(ConnectionEvent),
}

/// This structure contains all the critical data about a share submission,
/// including validation results and channel metadata. Serialization format
/// is left to the caller - this is just the data structure.
#[derive(Debug, Clone)]
pub struct ShareEvent {
    pub error_code: Option<String>,
    pub extranonce_prefix: Vec<u8>,
    pub is_block_found: bool,
    pub is_valid: bool,
    pub nominal_hash_rate: f32,
    pub nonce: u32,
    pub ntime: u32,
    pub rollable_extranonce_size: Option<u16>,
    pub share_hash: Option<Hash>,
    pub share_work: f64,
    pub target: [u8; 32],
    pub template_id: Option<u64>,
    pub timestamp: SystemTime,
    pub user_identity: String,
    pub version: u32,
}

// /// Connection event data
// #[derive(Debug, Clone)]
// pub struct ConnectionEvent {
//     pub client_id: String,
//     pub connected_at: SystemTime,
//     pub disconnected_at: Option<SystemTime>,
//     pub ip_address: String,
//     pub user_agent: Option<String>,
// }

/// Trait for handling persistence of share events.
///
/// Implementations of this trait handle the actual persistence operations,
/// ensuring that persistence operations are non-blocking and can handle failures internally.
pub trait PersistenceBackend: Send + Sync + std::fmt::Debug + Clone {
    /// Sends a share event for persistence.
    ///
    /// This method MUST be non-blocking and infallible from the caller's perspective.
    ///
    /// # Arguments
    ///
    /// * `event` - The persistence event to persist
    fn persist_event(&self, event: PersistenceEvent);

    /// Optional method to flush any pending events.
    ///
    /// This is a hint that the caller would like any buffered events to be processed
    /// immediately, but implementations are free to ignore this if not applicable.
    fn flush(&self) {}

    /// Optional method called when the persistence handler is being dropped.
    ///
    /// Implementations can use this for cleanup operations, but should not block.
    fn shutdown(&self) {}
}

/// Backend implementation selector
///
/// This enum is used internally by Persistence to dispatch to the correct backend.
/// Applications implementing `IntoPersistence` will construct variants of this enum.
#[cfg(feature = "persistence")]
pub enum Backend {
    File(FileBackend),
    // Sqlite(SqliteBackend),
    NoOp(NoOpBackend),
}

#[cfg(not(feature = "persistence"))]
pub(crate) enum Backend {
    NoOp(NoOpBackend),
}

// Persistence manager struct that dispatches to configured backend
pub struct Persistence {
    backend: Backend,
    enabled_entities: std::collections::HashSet<EntityType>,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    ConfigMismatch,
    #[cfg(feature = "persistence")]
    Custom(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::ConfigMismatch => write!(f, "Configuration mismatch"),
            #[cfg(feature = "persistence")]
            Error::Custom(s) => write!(f, "Configuration error: {}", s),
        }
    }
}

impl std::error::Error for Error {}

/// Trait for types that can configure a persistence backend.
///
/// This allows applications to define their own config structures
/// while still being able to create Persistence instances.
///
/// # Example
///
/// ```ignore
/// use stratum_apps::persistence::{IntoPersistence, Persistence, EntityType, Backend, FileBackend, Error};
///
/// struct MyConfig {
///     file_path: PathBuf,
///     channel_size: usize,
/// }
///
/// impl IntoPersistence for MyConfig {
///     fn into_persistence(self) -> Result<Persistence, Error> {
///         let backend = Backend::File(FileBackend::new(self.file_path, self.channel_size)?);
///         Ok(Persistence::with_backend(backend, vec![EntityType::Share]))
///     }
/// }
/// ```
#[cfg(feature = "persistence")]
pub trait IntoPersistence {
    /// Convert this config into a Persistence instance
    fn into_persistence(self, task_manager: Arc<TaskManager>) -> Result<Persistence, Error>;
}

impl Persistence {
    /// Create persistence from any config that implements IntoPersistence.
    ///
    /// This is the primary way to create a Persistence instance. Applications
    /// implement the `IntoPersistence` trait for their config types, allowing
    /// flexible configuration while maintaining type safety.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let persistence = Persistence::new(pool_config.persistence)?;
    /// ```
    #[cfg(feature = "persistence")]
    pub fn new(
        config: Option<impl IntoPersistence>,
        task_manager: Arc<TaskManager>,
    ) -> Result<Self, Error> {
        match config {
            Some(cfg) => cfg.into_persistence(task_manager),
            None => Ok(Self::noop()),
        }
    }

    /// Create persistence when feature is disabled (always NoOp).
    ///
    /// When the `persistence` feature is disabled, this always returns a no-op
    /// handler that compiles to zero overhead.
    #[cfg(not(feature = "persistence"))]
    pub fn new() -> Result<Self, Error> {
        Ok(Self::noop())
    }

    /// Create a no-op persistence handler (no persistence).
    ///
    /// This is useful for testing or when persistence is explicitly disabled.
    pub fn noop() -> Self {
        Self {
            backend: Backend::NoOp(NoOpBackend::new()),
            enabled_entities: std::collections::HashSet::new(),
        }
    }

    /// Create with a specific backend (for advanced use cases).
    ///
    /// This is typically called from `IntoPersistence` implementations.
    /// Most users should use `new()` instead.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let backend = Backend::File(FileBackend::new(path, size)?);
    /// let persistence = Persistence::with_backend(backend, vec![EntityType::Share]);
    /// ```
    #[cfg(feature = "persistence")]
    pub fn with_backend(
        backend: Backend,
        enabled_entities: impl IntoIterator<Item = EntityType>,
    ) -> Self {
        Self {
            backend,
            enabled_entities: enabled_entities.into_iter().collect(),
        }
    }

    /// Persist an event (checks if entity type is enabled)
    #[inline]
    pub fn persist(&self, event: PersistenceEvent) {
        let entity_type = match &event {
            PersistenceEvent::Share(_) => EntityType::Share,
            // PersistenceEvent::Connection(_) => EntityType::Connection,
        };

        if self.enabled_entities.contains(&entity_type) {
            match &self.backend {
                #[cfg(feature = "persistence")]
                Backend::File(b) => b.persist_event(event),
                // #[cfg(feature = "persistence")]
                // Backend::Sqlite(b) => b.persist_event(event),
                Backend::NoOp(b) => b.persist_event(event),
            }
        }
    }

    pub fn flush(&self) {
        match &self.backend {
            #[cfg(feature = "persistence")]
            Backend::File(b) => b.flush(),
            // #[cfg(feature = "persistence")]
            // Backend::Sqlite(b) => b.flush(),
            Backend::NoOp(b) => b.flush(),
        }
    }

    pub fn shutdown(&self) {
        match &self.backend {
            #[cfg(feature = "persistence")]
            Backend::File(b) => b.shutdown(),
            // #[cfg(feature = "persistence")]
            // Backend::Sqlite(b) => b.shutdown(),
            Backend::NoOp(b) => b.shutdown(),
        }
    }
}

impl Clone for Persistence {
    fn clone(&self) -> Self {
        Self {
            backend: match &self.backend {
                #[cfg(feature = "persistence")]
                Backend::File(b) => Backend::File(b.clone()),
                // #[cfg(feature = "persistence")]
                // Backend::Sqlite(b) => Backend::Sqlite(b.clone()),
                Backend::NoOp(b) => Backend::NoOp(*b),
            },
            enabled_entities: self.enabled_entities.clone(),
        }
    }
}

impl std::fmt::Debug for Persistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.backend {
            #[cfg(feature = "persistence")]
            Backend::File(_) => write!(
                f,
                "Persistence(File, entities: {:?})",
                self.enabled_entities
            ),
            // #[cfg(feature = "persistence")]
            // Backend::Sqlite(_) => write!(f, "Persistence(Sqlite, entities: {:?})",
            // self.enabled_entities),
            Backend::NoOp(_) => write!(f, "Persistence(NoOp)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_core::bitcoin::hashes::Hash as HashTrait;

    fn create_test_event() -> ShareEvent {
        let share_hash = Some(Hash::from_byte_array([0u8; 32]));
        ShareEvent {
            error_code: None,
            extranonce_prefix: vec![],
            is_block_found: false,
            is_valid: true,
            nominal_hash_rate: 1.0,
            nonce: 1,
            ntime: 1,
            rollable_extranonce_size: None,
            share_hash,
            share_work: 1.0,
            target: [0; 32],
            template_id: None,
            timestamp: SystemTime::now(),
            user_identity: "test".to_string(),
            version: 1,
        }
    }

    #[test]
    fn test_noop_handler() {
        let handler = NoOpBackend::new();
        let event = create_test_event();

        // Should not panic - all operations are no-ops
        handler.persist_event(PersistenceEvent::Share(event));
        handler.flush();
        handler.shutdown();
    }

    #[tokio::test]
    #[cfg(feature = "persistence")]
    async fn test_file_handler() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_file_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&test_file);

        let task_manager = Arc::new(crate::task_manager::TaskManager::new());
        let handler = FileBackend::new(test_file.clone(), 100, task_manager).unwrap();

        let event = create_test_event();
        handler.persist_event(PersistenceEvent::Share(event));
        handler.shutdown();

        // Give worker thread time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }

    #[test]
    fn test_noop_persistence() {
        let persistence = Persistence::noop();
        let event = create_test_event();

        // Should not panic - all operations are no-ops
        persistence.persist(PersistenceEvent::Share(event));
        persistence.flush();
        persistence.shutdown();
    }

    #[tokio::test]
    #[cfg(feature = "persistence")]
    async fn test_persistence_with_backend() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_persistence_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&test_file);

        let task_manager = Arc::new(crate::task_manager::TaskManager::new());
        let backend =
            Backend::File(FileBackend::new(test_file.clone(), 100, task_manager).unwrap());
        let persistence = Persistence::with_backend(backend, vec![EntityType::Share]);

        let event = create_test_event();
        persistence.persist(PersistenceEvent::Share(event));
        persistence.shutdown();

        // Give worker thread time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }

    #[tokio::test]
    #[cfg(feature = "persistence")]
    async fn test_into_persistence_trait() {
        use std::path::PathBuf;

        // Example config struct
        struct TestConfig {
            file_path: PathBuf,
            channel_size: usize,
        }

        impl IntoPersistence for TestConfig {
            fn into_persistence(
                self,
                task_manager: Arc<TaskManager>,
            ) -> Result<Persistence, Error> {
                let backend = Backend::File(FileBackend::new(
                    self.file_path,
                    self.channel_size,
                    task_manager,
                )?);
                Ok(Persistence::with_backend(backend, vec![EntityType::Share]))
            }
        }

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_trait_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&test_file);

        let config = TestConfig {
            file_path: test_file.clone(),
            channel_size: 100,
        };

        let task_manager = Arc::new(crate::task_manager::TaskManager::new());
        let persistence = Persistence::new(Some(config), task_manager).unwrap();
        let event = create_test_event();
        persistence.persist(PersistenceEvent::Share(event));
        persistence.shutdown();

        // Give worker thread time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }
}
