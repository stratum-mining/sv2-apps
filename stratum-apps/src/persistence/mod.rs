//! # Persistence Module
//!
//! Provides a generic persistence abstraction that can be used across different
//! Stratum V2 application roles with support for multiple backend implementations.

pub mod file;

use std::time::SystemTime;

use stratum_core::bitcoin::hashes::sha256d::Hash;

pub use file::FileHandler;

/// Trait for handling persistence of share events.
///
/// Implementations of this trait handle the actual persistence operations,
/// ensuring that persistence operations are non-blocking and can handle failures internally.
pub trait SharePersistenceHandler: Send + Sync {
    /// Sends a share event for persistence.
    ///
    /// This method MUST be non-blocking and infallible from the caller's perspective.
    ///
    /// # Arguments
    ///
    /// * `event` - The share event to persist
    fn persist_event(&self, event: ShareEvent);

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

/// Share persistence abstraction
///
/// This enum wraps a persistence handler implementation and provides a unified
/// interface that works whether persistence is enabled or disabled.
///
/// # Type Parameters
///
/// * `T` - The handler implementation type (e.g., `FileHandler`)
///
/// # Example
///
/// ```rust,no_run
/// use stratum_apps::persistence::{SharePersistence, FileHandler};
/// use std::path::PathBuf;
///
/// // Create with enabled persistence
/// let handler = FileHandler::new(PathBuf::from("shares.log"), 1000).unwrap();
/// let persistence = SharePersistence::new(Some(handler));
///
/// // Create with disabled persistence
/// let no_persistence: SharePersistence<FileHandler> = SharePersistence::new(None);
/// ```
#[derive(Debug, Clone)]
pub enum SharePersistence<T> {
    /// Persistence is enabled with the given handler
    Enabled(T),
    /// Persistence is disabled - all operations are no-ops
    Disabled,
}

impl<T> SharePersistence<T> {
    /// Create a new SharePersistence from an optional handler.
    ///
    /// If `Some(handler)` is provided, returns `SharePersistence::Enabled(handler)`.
    /// If `None` is provided, returns `SharePersistence::Disabled`.
    pub fn new(handler: Option<T>) -> Self {
        match handler {
            Some(h) => SharePersistence::Enabled(h),
            None => SharePersistence::Disabled,
        }
    }
}

impl<T> Default for SharePersistence<T> {
    /// Default persistence is Disabled (no persistence).
    fn default() -> Self {
        SharePersistence::Disabled
    }
}

impl<T: SharePersistenceHandler> SharePersistenceHandler for SharePersistence<T> {
    fn persist_event(&self, event: ShareEvent) {
        match self {
            SharePersistence::Enabled(handler) => handler.persist_event(event),
            SharePersistence::Disabled => {
                // No-op - persistence is disabled
            }
        }
    }

    fn flush(&self) {
        match self {
            SharePersistence::Enabled(handler) => handler.flush(),
            SharePersistence::Disabled => {
                // No-op - persistence is disabled
            }
        }
    }

    fn shutdown(&self) {
        match self {
            SharePersistence::Enabled(handler) => handler.shutdown(),
            SharePersistence::Disabled => {
                // No-op - persistence is disabled
            }
        }
    }
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
    pub share_hash: Hash,
    pub share_work: f64,
    pub target: [u8; 32],
    pub template_id: Option<u64>,
    pub timestamp: SystemTime,
    pub user_identity: String,
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persistence_disabled() {
        use stratum_core::bitcoin::hashes::Hash as HashTrait;
        let persistence: SharePersistence<FileHandler> = SharePersistence::default();

        // Create a test event
        let share_hash = Hash::from_byte_array([0u8; 32]);

        let event = ShareEvent {
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
        };

        // Should not panic - all operations are no-ops
        persistence.persist_event(event);
        persistence.flush();
        persistence.shutdown();
    }

    #[test]
    fn test_persistence_default_is_disabled() {
        let persistence: SharePersistence<FileHandler> = SharePersistence::default();
        matches!(persistence, SharePersistence::Disabled);
    }

    #[test]
    fn test_persistence_enabled() {
        use stratum_core::bitcoin::hashes::Hash as HashTrait;

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_enabled_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&test_file);

        let handler = FileHandler::new(test_file.clone(), 100).unwrap();
        let persistence = SharePersistence::new(Some(handler));

        let share_hash = Hash::from_byte_array([0u8; 32]);
        let event = ShareEvent {
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
        };

        persistence.persist_event(event);
        persistence.shutdown();

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }
}
