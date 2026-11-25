//! Keys and signatures for Sv2 Noise authentication.
//!
//! - [`crate::key_utils::keys`]: the authority and static keys, as the base58check strings
//!   configuration uses.
//! - [`crate::key_utils::certificate`]: the certificate a server presents for its static key,
//!   issued in advance (feature `core`, as its format is `noise_sv2`'s).
//! - [`crate::key_utils::signature`]: Schnorr signing and verification over a digest.
//! - [`crate::key_utils::error`]: what can go wrong while decoding or checking them.

#[cfg(feature = "core")]
pub mod certificate;
pub mod error;
pub mod keys;
pub mod signature;

#[cfg(feature = "core")]
pub use certificate::Certificate;
pub use error::Error;
pub use keys::{Secp256k1PublicKey, Secp256k1SecretKey};
pub use signature::SignatureService;
