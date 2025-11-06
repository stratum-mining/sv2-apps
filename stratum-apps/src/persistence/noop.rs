//! No-op persistence handler that compiles to zero overhead.
//!
//! This handler implements the SharePersistenceHandler trait but does nothing.
//! The compiler will optimize away all calls to this handler with `#[inline(always)]`,
//! resulting in true zero-cost abstraction when persistence is not needed.

use super::{ShareEvent, SharePersistenceHandler};

/// A persistence handler that does nothing.
///
/// This is used when persistence feature is disabled. The compiler will optimize
/// away all calls to this handler, resulting in zero runtime cost.
///
/// # Example
///
/// ```rust,no_run
/// use stratum_apps::persistence::{NoOpHandler, SharePersistenceHandler};
///
/// let handler = NoOpHandler;
/// // All operations compile to nothing
/// // handler.persist_event(event);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpHandler;

impl NoOpHandler {
    /// Create a new NoOpHandler.
    pub const fn new() -> Self {
        NoOpHandler
    }
}

impl SharePersistenceHandler for NoOpHandler {
    #[inline(always)]
    fn persist_event(&self, _event: ShareEvent) {
        // Intentionally empty - compiles to nothing
    }

    #[inline(always)]
    fn flush(&self) {
        // Intentionally empty - compiles to nothing
    }

    #[inline(always)]
    fn shutdown(&self) {
        // Intentionally empty - compiles to nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use stratum_core::bitcoin::hashes::{sha256d::Hash, Hash as HashTrait};

    #[test]
    fn test_noop_handler() {
        let handler = NoOpHandler::new();

        let share_hash = Some(Hash::from_byte_array([0u8; 32]));
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
        handler.persist_event(event);
        handler.flush();
        handler.shutdown();
    }

    #[test]
    fn test_noop_is_default() {
        let _handler: NoOpHandler = Default::default();
    }

    #[test]
    fn test_noop_is_const() {
        const _HANDLER: NoOpHandler = NoOpHandler::new();
    }
}
