//! Schnorr signing and verification over a 32-byte digest.

use secp256k1::{
    Keypair, Message as SecpMessage, Secp256k1, SecretKey, SignOnly, VerifyOnly, XOnlyPublicKey,
    schnorr::Signature,
};

pub struct SignatureService {
    secp_sign: Secp256k1<SignOnly>,
    secp_verify: Secp256k1<VerifyOnly>,
}

impl SignatureService {
    pub fn new() -> Self {
        SignatureService {
            secp_sign: Secp256k1::signing_only(),
            secp_verify: Secp256k1::verification_only(),
        }
    }

    #[cfg(feature = "std")]
    pub fn sign(&self, message: Vec<u8>, private_key: SecretKey) -> Signature {
        self.sign_with_rng(message, private_key, &mut rand::thread_rng())
    }

    #[inline]
    pub fn sign_with_rng<R: rand::Rng + rand::CryptoRng>(
        &self,
        message: Vec<u8>,
        private_key: SecretKey,
        rng: &mut R,
    ) -> Signature {
        let secret_key = private_key;
        let kp = Keypair::from_secret_key(&self.secp_sign, &secret_key);

        self.secp_sign.sign_schnorr_with_rng(
            &SecpMessage::from_digest_slice(&message).unwrap(),
            &kp,
            rng,
        )
    }

    pub fn verify(
        &self,
        message: Vec<u8>,
        signature: secp256k1::schnorr::Signature,
        public_key: XOnlyPublicKey,
    ) -> Result<(), secp256k1::Error> {
        let x_only_public_key = public_key;

        // Verify signature
        self.secp_verify.verify_schnorr(
            &signature,
            &secp256k1::Message::from_digest_slice(&message)?,
            &x_only_public_key,
        )
    }
}

impl Default for SignatureService {
    fn default() -> Self {
        Self::new()
    }
}
