//! # Persistence Module
//!
//! Provides a generic persistence abstraction that can be used across different
//! Stratum V2 application roles with support for multiple backend implementations.
//!
//! ## Architecture
//!
//! - `SharePersistenceHandler` trait - Core abstraction for persistence
//! - `NoOpHandler` - Zero-cost no-op implementation (used when feature disabled)
//! - `FileHandler` - File-based persistence (available with `persistence` feature)
//!
//! ## Compile-Time Configuration
//!
//! The `DefaultHandler` type alias switches based on the `persistence` feature:
//! - **With feature:** `FileHandler` (file-based persistence)
//! - **Without feature:** `NoOpHandler` (zero-cost, optimized away by compiler)

#[cfg(feature = "persistence")]
pub mod file;
pub mod noop;

use std::time::SystemTime;

use stratum_core::bitcoin::hashes::sha256d::Hash;

#[cfg(feature = "persistence")]
pub use file::FileHandler;
pub use noop::NoOpHandler;

/// Trait for handling persistence of share events.
///
/// Implementations of this trait handle the actual persistence operations,
/// ensuring that persistence operations are non-blocking and can handle failures internally.
pub trait SharePersistenceHandler: Send + Sync + std::fmt::Debug + Clone {
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

/// Default persistence handler type.
///
/// This switches based on the `persistence` feature flag:
/// - **With `persistence` feature:** `FileHandler` (file-based persistence)
/// - **Without feature:** `NoOpHandler` (zero-cost, compiled away)
#[cfg(feature = "persistence")]
pub type DefaultHandler = FileHandler;

#[cfg(not(feature = "persistence"))]
pub type DefaultHandler = NoOpHandler;

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
        let handler = NoOpHandler::new();
        let event = create_test_event();

        // Should not panic - all operations are no-ops
        handler.persist_event(event);
        handler.flush();
        handler.shutdown();
    }

    #[test]
    #[cfg(feature = "persistence")]
    fn test_file_handler() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_file_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&test_file);

        let handler = FileHandler::new(test_file.clone(), 100).unwrap();

        let event = create_test_event();
        handler.persist_event(event);
        handler.shutdown();

        // Give worker thread time to process
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }
}
