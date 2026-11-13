//! The certificate a Noise server presents, issued in advance where the authority key is kept.

use crate::key_utils::{Secp256k1PublicKey, Secp256k1SecretKey};
use bs58::decode;
use core::{convert::TryFrom, fmt::Display, str::FromStr};
use secp256k1::{Keypair, Secp256k1};
use serde::{Deserialize, Serialize};
use stratum_core::noise_sv2::{
    CERTIFICATE_TIME_LEEWAY, CERTIFICATE_VERSION, SIGNATURE_NOISE_MESSAGE_SIZE,
    SignatureNoiseMessage,
};

/// A Noise certificate issued in advance for a server's static key: the 74-byte
/// `SIGNATURE_NOISE_MESSAGE` of Sv2 spec 4.5.2, which the server sends verbatim during the
/// handshake.
///
/// The format, the signature and the checks are `noise_sv2`'s [`SignatureNoiseMessage`], the type
/// Sv2 Noise initiators verify; the signature covers the version, the validity window and the
/// x-only static public key (spec 4.5.3), so a certificate is bound to one static key and one
/// authority. This type adds the base58check encoding used in configuration files and the
/// environment, like the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Certificate(pub [u8; SIGNATURE_NOISE_MESSAGE_SIZE]);

/// Why a certificate string could not be decoded, or which check it failed.
#[derive(Debug)]
pub enum Error {
    /// The base58check string does not decode.
    Decode(decode::Error),
    /// The string decoded to something other than the 74 bytes of a `SIGNATURE_NOISE_MESSAGE`.
    Length(usize),
    /// The certificate carries a version other than [`CERTIFICATE_VERSION`].
    Version(u16),
    /// The signature does not verify for the given static and authority keys.
    Signature,
    /// The clock reading `now` falls outside the validity window.
    Window {
        valid_from: u32,
        not_valid_after: u32,
        now: u32,
    },
}

impl Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "Base58 code error: {error}"),
            Self::Length(len) => write!(
                f,
                "certificate decodes to {len} bytes, expected {SIGNATURE_NOISE_MESSAGE_SIZE}"
            ),
            Self::Version(version) => write!(
                f,
                "certificate version {version} is not supported, expected {CERTIFICATE_VERSION}"
            ),
            Self::Signature => write!(
                f,
                "certificate signature does not verify for this static key and authority key"
            ),
            Self::Window {
                valid_from,
                not_valid_after,
                now,
            } => write!(
                f,
                "certificate is valid from {valid_from} until {not_valid_after} (unix time), \
                 but the clock reads {now}"
            ),
        }
    }
}

impl core::error::Error for Error {}

impl From<decode::Error> for Error {
    fn from(error: decode::Error) -> Self {
        Error::Decode(error)
    }
}

impl TryFrom<String> for Certificate {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl FromStr for Certificate {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let decoded = decode(value).with_check(None).into_vec()?;
        let bytes: [u8; SIGNATURE_NOISE_MESSAGE_SIZE] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| Error::Length(decoded.len()))?;
        Ok(Certificate(bytes))
    }
}

impl From<Certificate> for String {
    fn from(certificate: Certificate) -> Self {
        certificate.to_string()
    }
}

impl Display for Certificate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&bs58::encode(self.0).with_check().into_string())
    }
}

impl Certificate {
    /// Issues a certificate for `static_public_key`, signed by `authority_secret_key`, valid
    /// between the two unix timestamps.
    #[cfg(feature = "std")]
    pub fn sign(
        static_public_key: &Secp256k1PublicKey,
        authority_secret_key: &Secp256k1SecretKey,
        valid_from: u32,
        not_valid_after: u32,
    ) -> Self {
        Self::sign_with_rng(
            static_public_key,
            authority_secret_key,
            valid_from,
            not_valid_after,
            &mut rand::thread_rng(),
        )
    }

    /// Like [`Self::sign`], with the random number generator the signature uses supplied by the
    /// caller.
    pub fn sign_with_rng<R: rand::Rng + rand::CryptoRng>(
        static_public_key: &Secp256k1PublicKey,
        authority_secret_key: &Secp256k1SecretKey,
        valid_from: u32,
        not_valid_after: u32,
        rng: &mut R,
    ) -> Self {
        let mut bytes: [u8; SIGNATURE_NOISE_MESSAGE_SIZE] = SignatureNoiseMessage {
            version: CERTIFICATE_VERSION,
            valid_from,
            not_valid_after,
            signature: [0; 64],
        }
        .into();
        let authority =
            Keypair::from_secret_key(&Secp256k1::signing_only(), &authority_secret_key.0);
        SignatureNoiseMessage::sign_with_rng(&mut bytes, &static_public_key.0, &authority, rng);
        Certificate(bytes)
    }

    /// Checks that this certificate was issued for `static_public_key` by the holder of
    /// `authority_public_key`, and that `now` (unix time) falls inside its validity window, with
    /// the same clock tolerance Sv2 Noise initiators apply.
    pub fn verify(
        &self,
        static_public_key: &Secp256k1PublicKey,
        authority_public_key: &Secp256k1PublicKey,
        now: u32,
    ) -> Result<(), Error> {
        // The window is reported first, so a failure below can only be the signature.
        self.check_validity(now)?;
        if self
            .message()
            .verify_with_now(&static_public_key.0, &authority_public_key.0, now)
        {
            Ok(())
        } else {
            Err(Error::Signature)
        }
    }

    /// The checks that need no authority key: the version is the supported one and `now` (unix
    /// time) falls inside the validity window, with the same clock tolerance as [`Self::verify`].
    pub fn check_validity(&self, now: u32) -> Result<(), Error> {
        let message = self.message();
        if message.version != CERTIFICATE_VERSION {
            return Err(Error::Version(message.version));
        }
        let (valid_from, not_valid_after) = (message.valid_from, message.not_valid_after);
        if valid_from.saturating_sub(CERTIFICATE_TIME_LEEWAY) > now
            || not_valid_after.saturating_add(CERTIFICATE_TIME_LEEWAY) < now
        {
            return Err(Error::Window {
                valid_from,
                not_valid_after,
                now,
            });
        }
        Ok(())
    }

    /// Certificate format version.
    pub fn version(&self) -> u16 {
        self.message().version
    }

    /// Start of the validity window (unix time).
    pub fn valid_from(&self) -> u32 {
        self.message().valid_from
    }

    /// End of the validity window (unix time).
    pub fn not_valid_after(&self) -> u32 {
        self.message().not_valid_after
    }

    fn message(&self) -> SignatureNoiseMessage {
        SignatureNoiseMessage::from(self.0)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use secp256k1::SecretKey;

    #[test]
    #[cfg(feature = "std")]
    fn certificate_round_trips_and_verifies() {
        let authority_secret: Secp256k1SecretKey =
            "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n"
                .parse()
                .unwrap();
        let authority_public = Secp256k1PublicKey::from(authority_secret);
        let static_secret: Secp256k1SecretKey =
            "24Zfqe5i5LQVc8yV6okoMpiQn7DohfxTBRJSgBKeH65oS6Gtkd"
                .parse()
                .unwrap();
        let static_public = Secp256k1PublicKey::from(static_secret);

        let certificate = Certificate::sign(&static_public, &authority_secret, 1_000, 2_000);
        assert_eq!(certificate.version(), CERTIFICATE_VERSION);
        assert_eq!(certificate.valid_from(), 1_000);
        assert_eq!(certificate.not_valid_after(), 2_000);
        let encoded = certificate.to_string();
        assert_eq!(encoded.parse::<Certificate>().unwrap(), certificate);

        assert!(
            certificate
                .verify(&static_public, &authority_public, 1_500)
                .is_ok()
        );
        // The clock tolerance of Sv2 Noise initiators is honoured at both edges.
        assert!(
            certificate
                .verify(&static_public, &authority_public, 995)
                .is_ok()
        );
        assert!(
            certificate
                .verify(&static_public, &authority_public, 2_005)
                .is_ok()
        );
        assert!(matches!(
            certificate.verify(&static_public, &authority_public, 2_011),
            Err(Error::Window { now: 2_011, .. })
        ));
        assert!(certificate.check_validity(1_500).is_ok());
        assert!(matches!(
            certificate.check_validity(989),
            Err(Error::Window { now: 989, .. })
        ));
        // Issued for another static key, or by another authority: the signature fails.
        let other = Secp256k1PublicKey::from(Secp256k1SecretKey(
            SecretKey::from_slice(&[7u8; 32]).unwrap(),
        ));
        assert!(matches!(
            certificate.verify(&other, &authority_public, 1_500),
            Err(Error::Signature)
        ));
        assert!(matches!(
            certificate.verify(&static_public, &static_public, 1_500),
            Err(Error::Signature)
        ));
        // A truncated string is reported as a length problem, not a checksum one.
        let mut short = certificate.0.to_vec();
        short.pop();
        let short = bs58::encode(short).with_check().into_string();
        assert!(matches!(
            short.parse::<Certificate>(),
            Err(Error::Length(73))
        ));
        let mut wrong_version = certificate;
        wrong_version.0[0] = 1;
        assert!(matches!(
            wrong_version.verify(&static_public, &authority_public, 1_500),
            Err(Error::Version(1))
        ));
    }

    // Vector from stratum-mining/stratum#2437: a certificate issued outside this code base for
    // the static key below, signed by the authority behind the shared test secret, valid from
    // 2026-10-08T23:00:00Z to 2026-10-10T01:00:00Z.
    #[test]
    fn certificate_verification_matches_an_independent_issuer() {
        let authority: Secp256k1PublicKey = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"
            .parse()
            .unwrap();
        let static_secret: Secp256k1SecretKey =
            "24Zfqe5i5LQVc8yV6okoMpiQn7DohfxTBRJSgBKeH65oS6Gtkd"
                .parse()
                .unwrap();
        let certificate: Certificate = "11Bq97W2YjA6HzQsudhPAFE93sqF6b23Uuo7gWvZY5HQH4U6f5iU7cuv5YPM2HKJgDZXZRoMR6t43aJWBRLjibkFqvHuWj8LTArMrmLSVq"
            .parse()
            .unwrap();
        assert_eq!(certificate.version(), 0);
        assert_eq!(certificate.valid_from(), 1_791_500_400);
        assert_eq!(certificate.not_valid_after(), 1_791_594_000);
        assert!(
            certificate
                .verify(
                    &Secp256k1PublicKey::from(static_secret),
                    &authority,
                    1_791_504_000
                )
                .is_ok()
        );
    }
}
