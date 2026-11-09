//! Keys and signatures for Sv2 Noise authentication.
//!
//! - [`crate::key_utils::keys`]: the authority and static keys, as the base58check strings
//!   configuration uses.
//! - [`crate::key_utils::signature`]: Schnorr signing and verification over a digest.
//! - [`crate::key_utils::error`]: what can go wrong while decoding or checking them.

pub mod error;
pub mod keys;
pub mod signature;

pub use error::Error;
pub use keys::{Secp256k1PublicKey, Secp256k1SecretKey};
pub use signature::SignatureService;
