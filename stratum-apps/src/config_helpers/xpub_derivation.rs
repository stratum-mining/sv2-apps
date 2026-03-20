//! xpub-based coinbase address derivation with persistence.
//!
//! This module provides utilities for deriving sequential Bitcoin addresses from an
//! extended public key (xpub/tpub) descriptor. It's designed for coinbase rotation
//! in mining pools, where each block found uses a new address derived from the xpub.
//!
//! # Features
//!
//! - Parses wildcard descriptors like `wpkh(xpub.../0/*)`
//! - Derives addresses at sequential indices
//! - Persists the current index to disk (survives restarts)
//! - Thread-safe (uses atomic operations for index)
//!
//! # Example
//!
//! ```ignore
//! use stratum_apps::config_helpers::xpub_derivation::XpubDerivator;
//! use std::path::PathBuf;
//!
//! let descriptor_str = "wpkh(tpub.../0/*)";
//! let derivator = XpubDerivator::new(descriptor_str, 0, PathBuf::from("/tmp/index.dat")).unwrap();
//!
//! // Get current address (peek)
//! let current = derivator.current_script_pubkey().unwrap();
//!
//! // Get next address and increment (rotate)
//! let next = derivator.next_script_pubkey().unwrap();
//! ```

use miniscript::{bitcoin::ScriptBuf, descriptor::DescriptorPublicKey, Descriptor};
use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

/// Errors that can occur during xpub derivation.
#[derive(Debug)]
pub enum XpubDerivationError {
    /// The descriptor does not have a wildcard.
    NoWildcard,
    /// Failed to parse the descriptor string.
    ParseError(String),
    /// Failed to derive at the specified index.
    DerivationFailed(String),
    /// Failed to persist the index to disk.
    PersistenceError(io::Error),
    /// Failed to create parent directories for index file.
    CreateDirectoryError(io::Error),
}

impl fmt::Display for XpubDerivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XpubDerivationError::NoWildcard => {
                write!(f, "descriptor does not have a wildcard (e.g., /0/*)")
            }
            XpubDerivationError::ParseError(msg) => {
                write!(f, "failed to parse descriptor: {}", msg)
            }
            XpubDerivationError::DerivationFailed(msg) => {
                write!(f, "failed to derive at index: {}", msg)
            }
            XpubDerivationError::PersistenceError(e) => {
                write!(f, "failed to persist index: {}", e)
            }
            XpubDerivationError::CreateDirectoryError(e) => {
                write!(f, "failed to create index file directory: {}", e)
            }
        }
    }
}

impl std::error::Error for XpubDerivationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            XpubDerivationError::PersistenceError(e) => Some(e),
            XpubDerivationError::CreateDirectoryError(e) => Some(e),
            _ => None,
        }
    }
}

/// Manages xpub-based address derivation with persistence.
///
/// This struct holds a wildcard descriptor and maintains the current derivation
/// index. The index is persisted to disk so that address derivation can resume
/// after restarts without reusing addresses.
///
/// # Thread Safety
///
/// The derivation index uses `AtomicU32` for thread-safe access. Multiple threads
/// can safely call `next_script_pubkey()` concurrently, though the order of
/// indices assigned is not guaranteed.
///
/// Note: The descriptor is stored as a `String` and re-parsed on each derivation
/// to ensure `Send + Sync` compatibility (miniscript's `Descriptor<DescriptorPublicKey>`
/// uses internal `RefCell` for taproot caching which is not thread-safe).
pub struct XpubDerivator {
    /// The wildcard descriptor string (e.g., "wpkh(xpub.../0/*)")
    descriptor_str: String,

    /// Current derivation index (atomic for thread safety)
    current_index: AtomicU32,

    /// Path to persist the current index
    index_file: PathBuf,
}

impl XpubDerivator {
    /// Creates a new `XpubDerivator` from a wildcard descriptor.
    ///
    /// If the index file exists, loads the persisted index. Otherwise, uses
    /// `start_index` as the initial index.
    ///
    /// Creates parent directories for the index file if they don't exist.
    ///
    /// # Arguments
    ///
    /// * `descriptor_str` - A wildcard descriptor string (must have `*` in derivation path)
    /// * `start_index` - Initial derivation index if no persisted index exists
    /// * `index_file` - Path to store the current index
    ///
    /// # Errors
    ///
    /// Returns `XpubDerivationError::ParseError` if the descriptor string is invalid.
    /// Returns `XpubDerivationError::NoWildcard` if the descriptor doesn't have a wildcard.
    /// Returns `XpubDerivationError::CreateDirectoryError` if parent directories can't be created.
    pub fn new(
        descriptor_str: &str,
        start_index: u32,
        index_file: PathBuf,
    ) -> Result<Self, XpubDerivationError> {
        // Parse the descriptor to validate it
        let descriptor: Descriptor<DescriptorPublicKey> = descriptor_str
            .parse()
            .map_err(|e: miniscript::Error| XpubDerivationError::ParseError(e.to_string()))?;

        // Verify the descriptor has a wildcard
        if !descriptor.has_wildcard() {
            return Err(XpubDerivationError::NoWildcard);
        }

        // Create parent directories if they don't exist
        if let Some(parent) = index_file.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(XpubDerivationError::CreateDirectoryError)?;
            }
        }

        // Load persisted index or use start_index
        let current_index = Self::load_index(&index_file, start_index);

        Ok(Self {
            descriptor_str: descriptor_str.to_string(),
            current_index: AtomicU32::new(current_index),
            index_file,
        })
    }

    /// Returns the current derivation index without incrementing.
    pub fn current_index(&self) -> u32 {
        self.current_index.load(Ordering::SeqCst)
    }

    /// Derives the script pubkey at the current index without incrementing.
    ///
    /// This is useful for getting the current coinbase address without rotating.
    ///
    /// # Errors
    ///
    /// Returns an error if derivation fails (e.g., index out of range).
    pub fn current_script_pubkey(&self) -> Result<ScriptBuf, XpubDerivationError> {
        let index = self.current_index.load(Ordering::SeqCst);
        self.derive_at_index(index)
    }

    /// Increments the index and derives the script pubkey at the new index.
    ///
    /// This is the main method for coinbase rotation. Call this AFTER a block is
    /// found to rotate to the next address.
    ///
    /// The new index is persisted to disk. If persistence fails, a warning is
    /// logged but the operation still succeeds (the index is still incremented
    /// in memory).
    ///
    /// # Errors
    ///
    /// Returns an error if derivation fails.
    pub fn next_script_pubkey(&self) -> Result<ScriptBuf, XpubDerivationError> {
        // Atomically increment and get the NEW index
        // fetch_add returns the old value, so add 1 to get the new value
        let new_index = self.current_index.fetch_add(1, Ordering::SeqCst) + 1;

        // Derive at the NEW index
        let script = self.derive_at_index(new_index)?;

        // Persist the new index
        // Don't fail if persistence fails - just log a warning
        if let Err(e) = self.persist_index() {
            tracing::warn!(
                "Failed to persist coinbase rotation index to {:?}: {}",
                self.index_file,
                e
            );
        }

        Ok(script)
    }

    /// Derives the script pubkey at a specific index.
    fn derive_at_index(&self, index: u32) -> Result<ScriptBuf, XpubDerivationError> {
        // Re-parse descriptor each time for thread safety
        // (miniscript's Descriptor<DescriptorPublicKey> is not Send + Sync due to RefCell)
        let descriptor: Descriptor<DescriptorPublicKey> = self
            .descriptor_str
            .parse()
            .map_err(|e: miniscript::Error| XpubDerivationError::ParseError(e.to_string()))?;

        let definite = descriptor
            .at_derivation_index(index)
            .map_err(|e| XpubDerivationError::DerivationFailed(e.to_string()))?;

        Ok(definite.script_pubkey())
    }

    /// Persists the current index to disk.
    fn persist_index(&self) -> Result<(), XpubDerivationError> {
        let index = self.current_index.load(Ordering::SeqCst);
        fs::write(&self.index_file, index.to_string())
            .map_err(XpubDerivationError::PersistenceError)
    }

    /// Loads the index from disk, or returns the default if the file doesn't exist
    /// or can't be parsed.
    fn load_index(path: &Path, default: u32) -> u32 {
        match fs::read_to_string(path) {
            Ok(contents) => contents.trim().parse().unwrap_or(default),
            Err(_) => default,
        }
    }
}

impl fmt::Debug for XpubDerivator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XpubDerivator")
            .field("descriptor", &self.descriptor_str)
            .field("current_index", &self.current_index.load(Ordering::SeqCst))
            .field("index_file", &self.index_file)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Test tpub from BIP84 test vectors
    const TEST_TPUB: &str = "wpkh(tpubD6NzVbkrYhZ4XgiXtGrdW5XDAPFCL9h7we1vwNCpn8tGbBcgfVYjXyhWo4E1xkh56hjod1RhGjxbaTLV3X4FyWuejifB9jusQ46QzG87VKp/0/*)";

    #[test]
    fn test_new_with_wildcard() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        let derivator = XpubDerivator::new(TEST_TPUB, 0, index_file).unwrap();

        assert_eq!(derivator.current_index(), 0);
    }

    #[test]
    fn test_new_without_wildcard_fails() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        // Descriptor without wildcard
        let desc_str = "wpkh(tpubD6NzVbkrYhZ4XgiXtGrdW5XDAPFCL9h7we1vwNCpn8tGbBcgfVYjXyhWo4E1xkh56hjod1RhGjxbaTLV3X4FyWuejifB9jusQ46QzG87VKp/0/0)";

        let result = XpubDerivator::new(desc_str, 0, index_file);
        assert!(matches!(result, Err(XpubDerivationError::NoWildcard)));
    }

    #[test]
    fn test_new_with_invalid_descriptor_fails() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        let result = XpubDerivator::new("invalid_descriptor", 0, index_file);
        assert!(matches!(result, Err(XpubDerivationError::ParseError(_))));
    }

    #[test]
    fn test_current_script_pubkey_does_not_increment() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        let derivator = XpubDerivator::new(TEST_TPUB, 0, index_file).unwrap();

        // Call current_script_pubkey multiple times
        let script1 = derivator.current_script_pubkey().unwrap();
        let script2 = derivator.current_script_pubkey().unwrap();
        let script3 = derivator.current_script_pubkey().unwrap();

        // All should be the same
        assert_eq!(script1, script2);
        assert_eq!(script2, script3);

        // Index should still be 0
        assert_eq!(derivator.current_index(), 0);
    }

    #[test]
    fn test_next_script_pubkey_increments() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        let derivator = XpubDerivator::new(TEST_TPUB, 0, index_file).unwrap();

        // Get first address
        let script0 = derivator.next_script_pubkey().unwrap();
        assert_eq!(derivator.current_index(), 1);

        // Get second address
        let script1 = derivator.next_script_pubkey().unwrap();
        assert_eq!(derivator.current_index(), 2);

        // Get third address
        let script2 = derivator.next_script_pubkey().unwrap();
        assert_eq!(derivator.current_index(), 3);

        // All should be different
        assert_ne!(script0, script1);
        assert_ne!(script1, script2);
        assert_ne!(script0, script2);
    }

    #[test]
    fn test_index_persistence() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("subdir/index.dat");

        // Create derivator and advance index
        {
            let derivator = XpubDerivator::new(TEST_TPUB, 0, index_file.clone()).unwrap();

            // Advance to index 5
            for _ in 0..5 {
                derivator.next_script_pubkey().unwrap();
            }
            assert_eq!(derivator.current_index(), 5);
        }

        // Create new derivator with same index file - should resume at 5
        {
            let derivator = XpubDerivator::new(TEST_TPUB, 0, index_file).unwrap();

            assert_eq!(derivator.current_index(), 5);
        }
    }

    #[test]
    fn test_start_index() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        let derivator = XpubDerivator::new(TEST_TPUB, 100, index_file).unwrap();

        assert_eq!(derivator.current_index(), 100);

        let script = derivator.next_script_pubkey().unwrap();
        assert_eq!(derivator.current_index(), 101);

        // Should be different from index 0
        let dir2 = tempdir().unwrap();
        let derivator2 = XpubDerivator::new(TEST_TPUB, 0, dir2.path().join("index.dat")).unwrap();
        let script0 = derivator2.next_script_pubkey().unwrap();

        assert_ne!(script, script0);
    }

    #[test]
    fn test_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("a/b/c/index.dat");

        let derivator = XpubDerivator::new(TEST_TPUB, 0, index_file.clone()).unwrap();

        // Should be able to persist
        derivator.next_script_pubkey().unwrap();

        // File should exist
        assert!(index_file.exists());
    }

    #[test]
    fn test_mainnet_xpub() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        // Mainnet xpub
        let desc_str = "wpkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/0/*)";

        let derivator = XpubDerivator::new(desc_str, 0, index_file).unwrap();

        // Should work fine
        let script = derivator.next_script_pubkey().unwrap();
        assert!(!script.is_empty());
    }

    #[test]
    fn test_taproot_descriptor() {
        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        // Taproot descriptor with wildcard
        let desc_str = "tr(tpubD6NzVbkrYhZ4XgiXtGrdW5XDAPFCL9h7we1vwNCpn8tGbBcgfVYjXyhWo4E1xkh56hjod1RhGjxbaTLV3X4FyWuejifB9jusQ46QzG87VKp/0/*)";

        let derivator = XpubDerivator::new(desc_str, 0, index_file).unwrap();

        let script0 = derivator.next_script_pubkey().unwrap();
        let script1 = derivator.next_script_pubkey().unwrap();

        assert_ne!(script0, script1);
        // Taproot scripts start with 0x5120
        assert!(script0.to_hex_string().starts_with("5120"));
    }

    /// Validates derivation against known test vectors.
    ///
    /// This test uses a specific tpub and validates that the derived scripts
    /// match the expected values computed from the known public keys.
    ///
    /// Descriptor: wpkh(tpubDDHYkDsJ8XB1LLjMNrk5gXsmze87LRkWoNqprdXPud9Yx3ZfsjZZJEqscUgSRLJ1EG77KSKygC9uNAeDtgHsLtvH93MnPF2M9Vq5WvGvcLw/0/*)
    ///
    /// Expected derived scripts at each index (P2WPKH format: 0014<20-byte-hash>):
    /// Index 0: 0014798fb52bc77ba8e028dfad1b522505223c7e7ca0
    /// Index 1: 00143acc8d6d349a24a198fb9eec0e27b822c589d407
    /// Index 2: 0014dd4da77967b0a8c59ee3026af582de496abad124
    /// Index 3: 001401b85a64c3c8d8dcf46f49230d938ec1245fcd8e
    /// Index 4: 0014a72ae2dddcc84c99a0abe43f4fbef1a46d153b8e
    #[test]
    fn test_known_derivation_vectors() {
        const KNOWN_TPUB: &str = "wpkh(tpubDDHYkDsJ8XB1LLjMNrk5gXsmze87LRkWoNqprdXPud9Yx3ZfsjZZJEqscUgSRLJ1EG77KSKygC9uNAeDtgHsLtvH93MnPF2M9Vq5WvGvcLw/0/*)";

        // Expected scripts at each index (P2WPKH format: 0014<20-byte-hash>)
        let expected_scripts = [
            "0014798fb52bc77ba8e028dfad1b522505223c7e7ca0", // Index 0
            "00143acc8d6d349a24a198fb9eec0e27b822c589d407", // Index 1
            "0014dd4da77967b0a8c59ee3026af582de496abad124", // Index 2
            "001401b85a64c3c8d8dcf46f49230d938ec1245fcd8e", // Index 3
            "0014a72ae2dddcc84c99a0abe43f4fbef1a46d153b8e", // Index 4
        ];

        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        let derivator = XpubDerivator::new(KNOWN_TPUB, 0, index_file).unwrap();

        // Verify index 0 via current_script_pubkey
        let script0 = derivator.current_script_pubkey().unwrap();
        assert_eq!(
            script0.to_hex_string(),
            expected_scripts[0],
            "Script mismatch at index 0"
        );

        // Verify indices 1-4 via next_script_pubkey (which increments then derives)
        for (i, expected_script) in expected_scripts.iter().enumerate().skip(1) {
            let script = derivator.next_script_pubkey().unwrap();
            assert_eq!(
                script.to_hex_string(),
                *expected_script,
                "Script mismatch at index {}: expected {}, got {}",
                i,
                expected_script,
                script.to_hex_string()
            );
        }
    }

    /// Test the complete rotation flow simulating pool behavior.
    ///
    /// This simulates:
    /// 1. Start at index 2 (from persisted file)
    /// 2. Verify current_script_pubkey() returns index 2's script
    /// 3. After block found, next_script_pubkey() returns index 3's script
    /// 4. After another block, next_script_pubkey() returns index 4's script
    /// 5. Restart and verify resumption at index 4
    #[test]
    fn test_rotation_flow_with_known_vectors() {
        const KNOWN_TPUB: &str = "wpkh(tpubDDHYkDsJ8XB1LLjMNrk5gXsmze87LRkWoNqprdXPud9Yx3ZfsjZZJEqscUgSRLJ1EG77KSKygC9uNAeDtgHsLtvH93MnPF2M9Vq5WvGvcLw/0/*)";

        let expected_scripts = [
            "0014798fb52bc77ba8e028dfad1b522505223c7e7ca0", // Index 0
            "00143acc8d6d349a24a198fb9eec0e27b822c589d407", // Index 1
            "0014dd4da77967b0a8c59ee3026af582de496abad124", // Index 2
            "001401b85a64c3c8d8dcf46f49230d938ec1245fcd8e", // Index 3
            "0014a72ae2dddcc84c99a0abe43f4fbef1a46d153b8e", // Index 4
        ];

        let dir = tempdir().unwrap();
        let index_file = dir.path().join("index.dat");

        // Simulate: index file contains "2" (pool was at index 2)
        fs::write(&index_file, "2").unwrap();

        let derivator = XpubDerivator::new(KNOWN_TPUB, 0, index_file.clone()).unwrap();

        // Step 1: Should load index 2 from file
        assert_eq!(derivator.current_index(), 2);

        // Step 2: current_script_pubkey() should return index 2's script
        let initial_script = derivator.current_script_pubkey().unwrap();
        assert_eq!(
            initial_script.to_hex_string(),
            expected_scripts[2],
            "Initial script should be at index 2"
        );

        // Step 3: First block found - rotate to index 3
        let script_after_first_block = derivator.next_script_pubkey().unwrap();
        assert_eq!(derivator.current_index(), 3);
        assert_eq!(
            script_after_first_block.to_hex_string(),
            expected_scripts[3],
            "After first rotation, script should be at index 3"
        );

        // Step 4: Second block found - rotate to index 4
        let script_after_second_block = derivator.next_script_pubkey().unwrap();
        assert_eq!(derivator.current_index(), 4);
        assert_eq!(
            script_after_second_block.to_hex_string(),
            expected_scripts[4],
            "After second rotation, script should be at index 4"
        );

        // Verify file was updated
        let contents = fs::read_to_string(&index_file).unwrap();
        assert_eq!(contents, "4");

        // Step 5: Simulate restart - create new derivator from same file
        let derivator2 = XpubDerivator::new(KNOWN_TPUB, 0, index_file).unwrap();
        assert_eq!(derivator2.current_index(), 4);
        assert_eq!(
            derivator2.current_script_pubkey().unwrap().to_hex_string(),
            expected_scripts[4],
            "After restart, should resume at index 4"
        );
    }
}
