//! Errors from decoding or checking keys.

use bs58::decode::Error as Bs58DecodeError;
use core::fmt::Display;

#[derive(Debug)]
pub enum Error {
    Bs58Decode(Bs58DecodeError),
    Secp256k1(secp256k1::Error),
    KeyVersion(u16),
    KeyLength,
    Custom(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bs58Decode(error) => write!(f, "Base58 code error: {error}"),
            Self::Secp256k1(error) => write!(f, "Secp256k1 error: {error}"),
            Self::KeyVersion(obtained) => {
                write!(f, "Unknown public key version. version found: {obtained}")
            }
            Self::KeyLength => write!(f, "Bad key length"),
            Self::Custom(error) => write!(f, "Custom error: {error}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
#[cfg(not(feature = "std"))]
#[rustversion::since(1.81)]
impl core::error::Error for Error {}

impl From<Bs58DecodeError> for Error {
    fn from(e: Bs58DecodeError) -> Self {
        Error::Bs58Decode(e)
    }
}

impl From<secp256k1::Error> for Error {
    fn from(e: secp256k1::Error) -> Self {
        Error::Secp256k1(e)
    }
}
