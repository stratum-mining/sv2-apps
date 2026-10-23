//! Shared payout-mode parsing and coinbase-output distribution helpers.
//!
//! This module is meant for applications that accept SRI-style mining identities and need a
//! single source of truth for reward distribution. Pool-like applications can use
//! [`crate::payout::PayoutMode::coinbase_outputs`] to build outputs, while proxy/client
//! applications can use [`crate::payout::PayoutMode::validate_coinbase_outputs`] or
//! [`crate::payout::PayoutMode::validate_coinbase_tx_parts`] to verify upstream jobs.

use std::fmt;

use crate::{
    config_helpers::CoinbaseRewardScript,
    stratum_core::bitcoin::{
        Amount, ScriptBuf, Transaction, TxOut, VarInt,
        consensus::{Decodable, deserialize},
    },
};

// Legacy solo identities do not encode a fee policy, so allow at most a 10% service fee.
const MIN_LEGACY_SOLO_PAYOUT_PERCENTAGE: u8 = 90;

/// Number of bytes in a coinbase transaction before the scriptSig-length VarInt.
///
/// Layout: `version` (4) + segwit `marker`/`flag` (2) + `input_count` (1) + previous-output
/// `txid` (32) + previous-output `vout` index (4). The scriptSig-length VarInt and the scriptSig
/// bytes themselves come immediately after these 43 bytes.
const COINBASE_PREFIX_LEN: usize = 43;

/// Maximum length of a coinbase scriptSig, per the BIP34 consensus rule.
///
/// A coinbase scriptSig must be between 2 and 100 bytes; we only enforce the upper bound here
/// because the lower bound is checked when the reconstructed transaction is decoded and
/// structurally validated.
const MAX_COINBASE_SCRIPT_SIG_SIZE: usize = 100;

/// Represents the payout mode encoded by a mining `user_identity`.
///
/// Supported patterns:
/// - `sri/solo/<payout_address>/<worker_name>`: full reward goes to the miner.
/// - `<payout_address>` or `<payout_address>.<worker_name>`: legacy solo mode; payout verification
///   checks that the miner address receives at least 90% of spendable coinbase outputs.
/// - `sri/donate/<percentage>/<payout_address>/<worker_name>`: pool receives `percentage`, miner
///   receives the remainder.
/// - `sri/donate/<worker_name>`: full reward goes to the pool.
#[derive(Debug, Clone)]
pub enum PayoutMode {
    /// Solo mode: miner receives full block reward.
    Solo {
        /// Miner payout address as supplied in `user_identity`.
        address: String,
        /// Miner payout script.
        script: CoinbaseRewardScript,
    },
    /// Legacy solo mode: miner payout address must receive at least 90% of spendable coinbase
    /// outputs.
    LegacySolo {
        /// Miner payout address as supplied in `user_identity`.
        address: String,
        /// Miner payout script.
        script: CoinbaseRewardScript,
    },
    /// Donate mode: pool receives specified percentage, miner gets remainder.
    Donate {
        /// Pool's portion, from 1 to 99.
        percentage: u8,
        /// Miner payout address as supplied in `user_identity`.
        address: String,
        /// Miner payout script.
        script: CoinbaseRewardScript,
    },
    /// Full donation mode: full reward goes to the pool.
    FullDonation,
}

impl PayoutMode {
    /// Creates coinbase outputs for this payout mode.
    pub fn coinbase_outputs(
        &self,
        total_value: u64,
        pool_script: &CoinbaseRewardScript,
    ) -> Vec<TxOut> {
        match self {
            Self::Solo {
                script: coinbase_script,
                ..
            }
            | Self::LegacySolo {
                script: coinbase_script,
                ..
            } => {
                vec![TxOut {
                    value: Amount::from_sat(total_value),
                    script_pubkey: coinbase_script.script_pubkey(),
                }]
            }

            Self::Donate {
                percentage,
                script: miner_script,
                ..
            } => {
                let pool_value = (total_value * *percentage as u64) / 100;
                let miner_value = total_value.saturating_sub(pool_value);

                vec![
                    TxOut {
                        value: Amount::from_sat(pool_value),
                        script_pubkey: pool_script.script_pubkey(),
                    },
                    TxOut {
                        value: Amount::from_sat(miner_value),
                        script_pubkey: miner_script.script_pubkey(),
                    },
                ]
            }

            Self::FullDonation => {
                vec![TxOut {
                    value: Amount::from_sat(total_value),
                    script_pubkey: pool_script.script_pubkey(),
                }]
            }
        }
    }

    /// Returns whether two payout modes produce the same coinbase outputs.
    ///
    /// The 100-satoshi reference value distinguishes all supported donation percentages while
    /// keeping the comparison independent of a particular template's reward value.
    pub fn has_same_coinbase_outputs_as(
        &self,
        other: &Self,
        pool_script: &CoinbaseRewardScript,
    ) -> bool {
        self.coinbase_outputs(100, pool_script) == other.coinbase_outputs(100, pool_script)
    }

    /// Verifies that spendable outputs match the miner-side payout encoded by this mode.
    ///
    /// OP_RETURN outputs are ignored. [`PayoutMode::FullDonation`] has no miner payout address, so
    /// it returns success without checking a miner output.
    pub fn validate_coinbase_outputs(
        &self,
        outputs: &[TxOut],
    ) -> Result<(), PayoutValidationError> {
        let Some(script_pubkey) = self.miner_script_pubkey() else {
            return Ok(());
        };

        let total_spendable_sats = outputs
            .iter()
            .filter(|output| !output.script_pubkey.is_op_return())
            .map(|output| output.value.to_sat())
            .sum();
        if total_spendable_sats == 0 {
            return Err(PayoutValidationError::NoSpendableOutputs);
        }

        let actual_miner_sats = outputs
            .iter()
            .filter(|output| !output.script_pubkey.is_op_return())
            .filter(|output| output.script_pubkey.as_bytes() == script_pubkey.as_bytes())
            .map(|output| output.value.to_sat())
            .sum();
        if matches!(self, Self::LegacySolo { .. }) {
            let expected_miner_sats = self.expected_legacy_solo_miner_sats(total_spendable_sats);
            if actual_miner_sats < expected_miner_sats {
                return Err(PayoutValidationError::PayoutMismatch {
                    address: self
                        .miner_address()
                        .expect("miner script exists only when miner address exists")
                        .to_string(),
                    expected_sats: expected_miner_sats,
                    expected_percentage: MIN_LEGACY_SOLO_PAYOUT_PERCENTAGE,
                    total_spendable_sats,
                    actual_sats: actual_miner_sats,
                });
            }

            return Ok(());
        }

        let expected_miner_sats = self.expected_miner_sats(total_spendable_sats);
        if actual_miner_sats != expected_miner_sats {
            return Err(PayoutValidationError::PayoutMismatch {
                address: self
                    .miner_address()
                    .expect("miner script exists only when miner address exists")
                    .to_string(),
                expected_sats: expected_miner_sats,
                expected_percentage: self.expected_miner_percentage(),
                total_spendable_sats,
                actual_sats: actual_miner_sats,
            });
        }

        Ok(())
    }

    /// Verifies `NewExtendedMiningJob` coinbase transaction parts against this payout mode.
    ///
    /// The SV2 split only guarantees that `coinbase_tx_suffix` is the part after the full
    /// extranonce. The suffix can still contain remaining coinbase scriptSig bytes before the input
    /// sequence, so this reconstructs and deserializes the full transaction before checking
    /// outputs.
    ///
    /// The extranonce bytes are zero-filled because payout verification only needs the transaction
    /// to decode and expose its outputs; the actual extranonce value does not affect the output
    /// set.
    pub fn validate_coinbase_tx_parts(
        &self,
        coinbase_tx_prefix: &[u8],
        coinbase_tx_suffix: &[u8],
        full_extranonce_size: usize,
    ) -> Result<(), PayoutValidationError> {
        let mut coinbase = Vec::with_capacity(
            coinbase_tx_prefix.len() + full_extranonce_size + coinbase_tx_suffix.len(),
        );
        coinbase.extend_from_slice(coinbase_tx_prefix);
        coinbase.resize(coinbase.len() + full_extranonce_size, 0);
        coinbase.extend_from_slice(coinbase_tx_suffix);

        let coinbase: Transaction = deserialize(&coinbase)
            .map_err(|e| PayoutValidationError::DecodeCoinbaseTransaction(e.to_string()))?;

        self.validate_coinbase_outputs(&coinbase.output)
    }

    fn miner_address(&self) -> Option<&str> {
        match self {
            Self::Solo { address, .. }
            | Self::LegacySolo { address, .. }
            | Self::Donate { address, .. } => Some(address.as_str()),
            Self::FullDonation => None,
        }
    }

    fn miner_script_pubkey(&self) -> Option<ScriptBuf> {
        match self {
            Self::Solo { script, .. }
            | Self::LegacySolo { script, .. }
            | Self::Donate { script, .. } => Some(script.script_pubkey()),
            Self::FullDonation => None,
        }
    }

    fn expected_miner_percentage(&self) -> u8 {
        match self {
            Self::Solo { .. } | Self::LegacySolo { .. } => 100,
            Self::Donate { percentage, .. } => 100 - percentage,
            Self::FullDonation => 0,
        }
    }

    fn expected_miner_sats(&self, total_spendable_sats: u64) -> u64 {
        match self {
            Self::Solo { .. } | Self::LegacySolo { .. } => total_spendable_sats,
            Self::Donate { percentage, .. } => {
                let pool_sats = (total_spendable_sats * *percentage as u64) / 100;
                total_spendable_sats.saturating_sub(pool_sats)
            }
            Self::FullDonation => 0,
        }
    }

    fn expected_legacy_solo_miner_sats(&self, total_spendable_sats: u64) -> u64 {
        (total_spendable_sats * MIN_LEGACY_SOLO_PAYOUT_PERCENTAGE as u64).div_ceil(100)
    }
}

impl TryFrom<&str> for PayoutMode {
    type Error = PayoutModeError;

    fn try_from(user_identity: &str) -> Result<Self, Self::Error> {
        if user_identity.is_empty() {
            return Err(PayoutModeError::NoPayoutMode(user_identity.to_string()));
        }

        let addr = address_part_from_user_identity(user_identity);

        if let Ok(script) = script_from_address(addr) {
            return Ok(Self::LegacySolo {
                address: addr.to_string(),
                script,
            });
        }

        let mut parts = user_identity.split('/');

        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("sri"), Some("solo"), Some(payout_address), _) => {
                let script = script_from_address(payout_address)?;
                Ok(Self::Solo {
                    address: payout_address.to_string(),
                    script,
                })
            }

            (Some("sri"), Some("donate"), None, _)
            | (Some("sri"), Some("donate"), Some(_), None) => Ok(Self::FullDonation),

            (Some("sri"), Some("donate"), Some(percentage), Some(payout_address)) => {
                let percentage = percentage.parse::<u8>().map_err(|_| {
                    PayoutModeError::InvalidDonationPercentage(percentage.to_string())
                })?;
                if !(1..100).contains(&percentage) {
                    return Err(PayoutModeError::InvalidDonationPercentage(
                        percentage.to_string(),
                    ));
                }

                let script = script_from_address(payout_address)?;
                Ok(Self::Donate {
                    percentage,
                    address: payout_address.to_string(),
                    script,
                })
            }

            (Some("sri"), Some(_), _, _) => Err(PayoutModeError::InvalidUserIdentity(
                user_identity.to_string(),
            )),

            _ => Err(PayoutModeError::NoPayoutMode(user_identity.to_string())),
        }
    }
}

impl fmt::Display for PayoutMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Solo { address, .. } => {
                write!(f, "100% miner payout to {address}")
            }
            Self::LegacySolo { address, .. } => write!(
                f,
                "at least {MIN_LEGACY_SOLO_PAYOUT_PERCENTAGE}% miner payout to {address}"
            ),
            Self::Donate {
                percentage,
                address,
                ..
            } => write!(
                f,
                "{}% miner payout to {} ({}% pool donation)",
                100 - percentage,
                address,
                percentage
            ),
            Self::FullDonation => write!(f, "100% pool payout"),
        }
    }
}

/// Errors produced while parsing a payout mode from a `user_identity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutModeError {
    /// No payout mode was encoded in `user_identity`.
    NoPayoutMode(String),
    /// `sri/...` was used with an unsupported payout pattern.
    InvalidUserIdentity(String),
    /// A payout address was present but could not be converted into a script.
    InvalidPayoutAddress { address: String, error: String },
    /// Donation percentage was not an integer in the supported 1..100 range.
    InvalidDonationPercentage(String),
    /// Payout verification was requested but no miner payout address is present.
    MissingMinerPayout {
        user_identity: String,
        mode: MissingMinerPayoutMode,
    },
}

/// Payout modes that cannot be verified because they do not include a miner payout address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissingMinerPayoutMode {
    /// `sri/donate/<worker>` full donation mode: all reward goes to the pool.
    FullDonation,
    /// No SRI payout mode or legacy address payout was encoded.
    NoPayoutMode,
}

impl fmt::Display for PayoutModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPayoutMode(user_identity) => {
                write!(
                    f,
                    "no payout mode encoded in user_identity: {user_identity}"
                )
            }
            Self::InvalidUserIdentity(user_identity) => {
                write!(
                    f,
                    "invalid user_identity pattern for payout mode: {user_identity}"
                )
            }
            Self::InvalidPayoutAddress { address, error } => {
                write!(f, "invalid payout address `{address}`: {error}")
            }
            Self::InvalidDonationPercentage(percentage) => {
                write!(f, "invalid donation percentage: {percentage}")
            }
            Self::MissingMinerPayout {
                user_identity,
                mode: MissingMinerPayoutMode::FullDonation,
            } => write!(
                f,
                "verify_payout is enabled, but user_identity `{user_identity}` opts into full donation mode (`sri/donate/<worker>`), which has no miner payout to verify; disable verify_payout or use sri/solo/<address>/<worker>, sri/donate/<percentage>/<address>/<worker>, <address>, or <address>.<worker>"
            ),
            Self::MissingMinerPayout {
                user_identity,
                mode: MissingMinerPayoutMode::NoPayoutMode,
            } => write!(
                f,
                "verify_payout is enabled, but user_identity `{user_identity}` does not opt into a payout mode, so there is no miner payout to verify; disable verify_payout for pool usernames or use sri/solo/<address>/<worker>, sri/donate/<percentage>/<address>/<worker>, <address>, or <address>.<worker>"
            ),
        }
    }
}

impl std::error::Error for PayoutModeError {}

/// Errors produced while verifying coinbase outputs against a payout mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutValidationError {
    /// The coinbase output set has no spendable outputs.
    NoSpendableOutputs,
    /// The miner payout did not match the expected distribution.
    PayoutMismatch {
        /// Address encoded by the payout mode.
        address: String,
        /// Expected miner payout in satoshis.
        expected_sats: u64,
        /// Expected miner payout percentage.
        expected_percentage: u8,
        /// Total spendable coinbase output value in satoshis.
        total_spendable_sats: u64,
        /// Actual amount paid to the miner script in satoshis.
        actual_sats: u64,
    },
    /// Failed to decode the reconstructed coinbase transaction.
    DecodeCoinbaseTransaction(String),
}

impl fmt::Display for PayoutValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSpendableOutputs => write!(f, "coinbase has no spendable outputs"),
            Self::PayoutMismatch {
                address,
                expected_sats,
                expected_percentage,
                total_spendable_sats,
                actual_sats,
            } => write!(
                f,
                "coinbase payout mismatch for {address}: expected {expected_sats} sats ({expected_percentage}% of {total_spendable_sats} spendable sats), found {actual_sats} sats"
            ),
            Self::DecodeCoinbaseTransaction(e) => {
                write!(f, "failed to decode coinbase transaction: {e}")
            }
        }
    }
}

impl std::error::Error for PayoutValidationError {}

/// Errors produced while reconstructing a declared coinbase transaction from its untrusted parts.
///
/// Every variant maps to the single `invalid-coinbase-tx` error code on the wire; the granularity
/// exists only for logging and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredCoinbaseError {
    /// The prefix is shorter than the fixed [`COINBASE_PREFIX_LEN`] header preceding the
    /// scriptSig-length VarInt.
    PrefixTooShort,
    /// Failed to decode the scriptSig-length VarInt from the prefix.
    InvalidScriptSigVarInt,
    /// The declared scriptSig length exceeds the consensus bound
    /// [`MAX_COINBASE_SCRIPT_SIG_SIZE`].
    ScriptSigTooLarge,
    /// The declared scriptSig length is inconsistent with the bytes present in the prefix
    /// (the declared length is smaller than the scriptSig bytes already supplied).
    ScriptSigLengthMismatch,
    /// The reconstructed transaction failed to deserialize.
    Decode(String),
}

impl fmt::Display for DeclaredCoinbaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixTooShort => write!(
                f,
                "coinbase prefix shorter than {COINBASE_PREFIX_LEN} bytes"
            ),
            Self::InvalidScriptSigVarInt => {
                write!(f, "failed to decode scriptSig length from coinbase prefix")
            }
            Self::ScriptSigTooLarge => write!(
                f,
                "declared coinbase scriptSig length exceeds {MAX_COINBASE_SCRIPT_SIG_SIZE} bytes"
            ),
            Self::ScriptSigLengthMismatch => write!(
                f,
                "declared coinbase scriptSig length is smaller than the scriptSig bytes already in the prefix"
            ),
            Self::Decode(e) => write!(f, "failed to decode declared coinbase transaction: {e}"),
        }
    }
}

impl std::error::Error for DeclaredCoinbaseError {}

/// Reconstructs and decodes a declared coinbase transaction from its untrusted parts.
///
/// The coinbase is split into a `coinbase_tx_prefix`, a zero-filled extranonce gap, and a
/// `coinbase_tx_suffix`. The extranonce size is derived from the scriptSig-length VarInt inside
/// the prefix. Both parts are attacker-controlled, so this function validates every length and
/// offset with checked arithmetic *before* slicing or allocating:
///
/// - the prefix must be at least [`COINBASE_PREFIX_LEN`] bytes (no out-of-bounds slice),
/// - the declared scriptSig length is capped at [`MAX_COINBASE_SCRIPT_SIG_SIZE`], bounding the
///   zero-filled extranonce allocation,
/// - the declared scriptSig length must be consistent with the scriptSig bytes already present in
///   the prefix (no `usize` underflow).
///
/// On any failure it returns a [`DeclaredCoinbaseError`] instead of panicking or allocating an
/// attacker-selected amount of memory.
pub fn decode_declared_coinbase_tx(
    coinbase_tx_prefix: &[u8],
    coinbase_tx_suffix: &[u8],
) -> Result<Transaction, DeclaredCoinbaseError> {
    // The prefix must at least cover the fixed header up to the scriptSig-length VarInt.
    if coinbase_tx_prefix.len() < COINBASE_PREFIX_LEN {
        return Err(DeclaredCoinbaseError::PrefixTooShort);
    }

    // Decode the scriptSig-length VarInt starting right after the fixed header.
    let mut cursor = &coinbase_tx_prefix[COINBASE_PREFIX_LEN..];
    let script_sig_size_u64 = VarInt::consensus_decode(&mut cursor)
        .map_err(|_| DeclaredCoinbaseError::InvalidScriptSigVarInt)?
        .0;

    // Bound the declared scriptSig length before it can drive any allocation. Compare in `u64`
    // space so the bound cannot be bypassed by a truncating `u64 -> usize` cast, then convert.
    if script_sig_size_u64 > MAX_COINBASE_SCRIPT_SIG_SIZE as u64 {
        return Err(DeclaredCoinbaseError::ScriptSigTooLarge);
    }
    let script_sig_size = script_sig_size_u64 as usize;

    // Offset of the first scriptSig byte within the prefix. Both terms are bounded, so the
    // addition cannot overflow.
    let varint_size = VarInt(script_sig_size as u64).size();
    let script_sig_offset = COINBASE_PREFIX_LEN + varint_size;

    // scriptSig bytes already present in the prefix. The VarInt was decoded from
    // `coinbase_tx_prefix[COINBASE_PREFIX_LEN..]`, so `script_sig_offset <= prefix.len()`; use
    // saturating_sub defensively.
    let script_sig_bytes_in_prefix = coinbase_tx_prefix.len().saturating_sub(script_sig_offset);

    // The remaining scriptSig bytes form the zero-filled extranonce. If the prefix already
    // contains more scriptSig bytes than the declared length, the declaration is inconsistent.
    let full_extranonce_size = script_sig_size
        .checked_sub(script_sig_bytes_in_prefix)
        .ok_or(DeclaredCoinbaseError::ScriptSigLengthMismatch)?;

    // Reconstruct prefix + zero-filled extranonce + suffix.
    let mut coinbase = Vec::with_capacity(
        coinbase_tx_prefix.len() + full_extranonce_size + coinbase_tx_suffix.len(),
    );
    coinbase.extend_from_slice(coinbase_tx_prefix);
    coinbase.resize(coinbase.len() + full_extranonce_size, 0);
    coinbase.extend_from_slice(coinbase_tx_suffix);

    Transaction::consensus_decode(&mut &coinbase[..])
        .map_err(|e| DeclaredCoinbaseError::Decode(e.to_string()))
}

fn script_from_address(address: &str) -> Result<CoinbaseRewardScript, PayoutModeError> {
    CoinbaseRewardScript::from_descriptor(&format!("addr({address})")).map_err(|e| {
        PayoutModeError::InvalidPayoutAddress {
            address: address.to_string(),
            error: e.to_string(),
        }
    })
}

fn address_part_from_user_identity(user_identity: &str) -> &str {
    user_identity
        .split_once('.')
        .map(|(address, _)| address)
        .unwrap_or(user_identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stratum_core::bitcoin::{
        Address,
        consensus::serialize,
        params::{MAINNET, TESTNET4},
    };

    const MINER_ADDRESS: &str = "bc1qtzqxqaxyy6lda2fhdtp5dp0v56vlf6g0tljy2x";
    const OTHER_ADDRESS: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    const TESTNET_ADDRESS: &str = "tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8";
    const FULL_EXTRANONCE_SIZE: usize = 8;

    fn tx_out(value: u64, address: &str) -> TxOut {
        TxOut {
            value: Amount::from_sat(value),
            script_pubkey: script_from_address(address).unwrap().script_pubkey(),
        }
    }

    fn coinbase_tx_parts(outputs: Vec<TxOut>, script_sig_suffix: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let script_sig_prefix = [0x03, 0x01, 0x02, 0x03];
        let script_sig_len =
            script_sig_prefix.len() + FULL_EXTRANONCE_SIZE + script_sig_suffix.len();
        assert!(script_sig_len < 0xfd);

        let mut prefix = Vec::new();
        prefix.extend([0x02, 0x00, 0x00, 0x00]);
        prefix.push(0x01);
        prefix.extend([0; 32]);
        prefix.extend([0xff, 0xff, 0xff, 0xff]);
        prefix.push(script_sig_len as u8);
        prefix.extend(script_sig_prefix);

        let mut suffix = Vec::new();
        suffix.extend(script_sig_suffix);
        suffix.extend([0xff, 0xff, 0xff, 0xff]);
        suffix.extend(serialize(&outputs));
        suffix.extend([0, 0, 0, 0]);

        (prefix, suffix)
    }

    fn validate_tx_outputs(
        expected: &PayoutMode,
        outputs: Vec<TxOut>,
    ) -> Result<(), PayoutValidationError> {
        let (prefix, suffix) = coinbase_tx_parts(outputs, &[]);
        expected.validate_coinbase_tx_parts(&prefix, &suffix, FULL_EXTRANONCE_SIZE)
    }

    #[test]
    fn parses_full_donation_identities() {
        assert!(matches!(
            PayoutMode::try_from("sri/donate/worker"),
            Ok(PayoutMode::FullDonation)
        ));
        assert!(matches!(
            PayoutMode::try_from("sri/donate"),
            Ok(PayoutMode::FullDonation)
        ));
    }

    #[test]
    fn parses_solo_identities() {
        assert!(matches!(
            PayoutMode::try_from(format!("sri/solo/{TESTNET_ADDRESS}/worker").as_str()),
            Ok(PayoutMode::Solo { script, .. }) if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == TESTNET_ADDRESS
        ));
        assert!(matches!(
            PayoutMode::try_from(format!("sri/solo/{MINER_ADDRESS}/worker/subworker").as_str()),
            Ok(PayoutMode::Solo { script, .. }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string() == MINER_ADDRESS
        ));
        assert!(matches!(
            PayoutMode::try_from(MINER_ADDRESS),
            Ok(PayoutMode::LegacySolo { script, .. }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string() == MINER_ADDRESS
        ));
    }

    #[test]
    fn parses_legacy_address_identity_with_worker_suffix() {
        assert!(matches!(
            PayoutMode::try_from(format!("{MINER_ADDRESS}.worker1").as_str()),
            Ok(PayoutMode::LegacySolo { script, .. }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string() == MINER_ADDRESS
        ));
        assert!(matches!(
            PayoutMode::try_from(format!("{MINER_ADDRESS}.worker1.subworker").as_str()),
            Ok(PayoutMode::LegacySolo { script, .. }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string() == MINER_ADDRESS
        ));
    }

    #[test]
    fn arbitrary_pool_usernames_have_no_payout_mode() {
        assert!(matches!(
            PayoutMode::try_from("invalid_address.worker"),
            Err(PayoutModeError::NoPayoutMode(_))
        ));
        assert!(matches!(
            PayoutMode::try_from(""),
            Err(PayoutModeError::NoPayoutMode(_))
        ));
        assert!(matches!(
            PayoutMode::try_from("other/donate/worker"),
            Err(PayoutModeError::NoPayoutMode(_))
        ));
    }

    #[test]
    fn permissive_parser_treats_address_like_typos_as_no_payout_mode() {
        assert!(matches!(
            PayoutMode::try_from("bc1q_typo.worker"),
            Err(PayoutModeError::NoPayoutMode(_))
        ));
    }

    #[test]
    fn parses_partial_donation_identities() {
        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/50/{TESTNET_ADDRESS}/worker").as_str()).unwrap(),
            PayoutMode::Donate { percentage: 50, script, .. } if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == TESTNET_ADDRESS
        ));

        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/50/{TESTNET_ADDRESS}").as_str()).unwrap(),
            PayoutMode::Donate { percentage: 50, script, .. } if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == TESTNET_ADDRESS
        ));
    }

    #[test]
    fn rejects_invalid_sri_patterns() {
        assert!(PayoutMode::try_from("sri/invalid/worker").is_err());
        assert!(PayoutMode::try_from("sri/solo").is_err());
        assert!(PayoutMode::try_from("sri/solo/random_thing_here/worker").is_err());
        assert!(PayoutMode::try_from("sri/solo/").is_err());
        assert!(matches!(
            PayoutMode::try_from("sri/donate/abc/addr/worker"),
            Err(PayoutModeError::InvalidDonationPercentage(_))
        ));
        assert!(matches!(
            PayoutMode::try_from("sri/donate/101/addr/worker"),
            Err(PayoutModeError::InvalidDonationPercentage(_))
        ));
        assert!(matches!(
            PayoutMode::try_from("sri/"),
            Err(PayoutModeError::InvalidUserIdentity(_))
        ));
    }

    #[test]
    fn builds_pool_coinbase_outputs_for_all_modes() {
        let pool_script = script_from_address(OTHER_ADDRESS).unwrap();

        let solo = PayoutMode::try_from(MINER_ADDRESS).unwrap();
        let solo_outputs = solo.coinbase_outputs(1_000, &pool_script);
        assert_eq!(solo_outputs.len(), 1);
        assert_eq!(solo_outputs[0].value.to_sat(), 1_000);

        let donate =
            PayoutMode::try_from(format!("sri/donate/10/{MINER_ADDRESS}/w").as_str()).unwrap();
        let donate_outputs = donate.coinbase_outputs(1_000, &pool_script);
        assert_eq!(donate_outputs.len(), 2);
        assert_eq!(donate_outputs[0].value.to_sat(), 100);
        assert_eq!(donate_outputs[1].value.to_sat(), 900);

        let full_donation = PayoutMode::FullDonation;
        let full_donation_outputs = full_donation.coinbase_outputs(1_000, &pool_script);
        assert_eq!(full_donation_outputs.len(), 1);
        assert_eq!(full_donation_outputs[0].value.to_sat(), 1_000);
    }

    #[test]
    fn compares_payout_modes_by_coinbase_outputs() {
        let pool_script = script_from_address(OTHER_ADDRESS).unwrap();
        let legacy_solo = PayoutMode::try_from(MINER_ADDRESS).unwrap();
        let solo =
            PayoutMode::try_from(format!("sri/solo/{MINER_ADDRESS}/worker").as_str()).unwrap();
        let other_solo =
            PayoutMode::try_from(format!("sri/solo/{TESTNET_ADDRESS}/worker").as_str()).unwrap();
        let full_donation = PayoutMode::FullDonation;

        assert!(legacy_solo.has_same_coinbase_outputs_as(&solo, &pool_script));
        assert!(!legacy_solo.has_same_coinbase_outputs_as(&other_solo, &pool_script));
        assert!(!legacy_solo.has_same_coinbase_outputs_as(&full_donation, &pool_script));
        assert!(
            full_donation.has_same_coinbase_outputs_as(&PayoutMode::FullDonation, &pool_script)
        );
    }

    #[test]
    fn validates_full_solo_distribution() {
        let expected =
            PayoutMode::try_from(format!("sri/solo/{MINER_ADDRESS}/w1").as_str()).unwrap();

        validate_tx_outputs(&expected, vec![tx_out(1_000, MINER_ADDRESS)]).unwrap();
    }

    #[test]
    fn rejects_full_solo_distribution_with_other_spendable_output() {
        let expected =
            PayoutMode::try_from(format!("sri/solo/{MINER_ADDRESS}/w1").as_str()).unwrap();

        let err = validate_tx_outputs(
            &expected,
            vec![tx_out(900, MINER_ADDRESS), tx_out(100, OTHER_ADDRESS)],
        )
        .unwrap_err();

        assert!(matches!(
            err,
            PayoutValidationError::PayoutMismatch {
                expected_sats: 1000,
                actual_sats: 900,
                ..
            }
        ));
    }

    #[test]
    fn validates_legacy_solo_distribution_with_service_fee_output() {
        let expected = PayoutMode::try_from(format!("{MINER_ADDRESS}.w1").as_str()).unwrap();

        validate_tx_outputs(
            &expected,
            vec![tx_out(991, MINER_ADDRESS), tx_out(9, OTHER_ADDRESS)],
        )
        .unwrap();
    }

    #[test]
    fn rejects_legacy_solo_distribution_below_minimum() {
        let expected = PayoutMode::try_from(format!("{MINER_ADDRESS}.w1").as_str()).unwrap();

        let err = validate_tx_outputs(
            &expected,
            vec![tx_out(899, MINER_ADDRESS), tx_out(101, OTHER_ADDRESS)],
        )
        .unwrap_err();

        assert!(matches!(
            err,
            PayoutValidationError::PayoutMismatch {
                expected_sats: 900,
                expected_percentage: 90,
                actual_sats: 899,
                ..
            }
        ));
    }

    #[test]
    fn rejects_legacy_solo_distribution_without_miner_address() {
        let expected = PayoutMode::try_from(format!("{MINER_ADDRESS}.w1").as_str()).unwrap();

        let err = validate_tx_outputs(&expected, vec![tx_out(1_000, OTHER_ADDRESS)]).unwrap_err();

        assert!(matches!(
            err,
            PayoutValidationError::PayoutMismatch {
                expected_sats: 900,
                expected_percentage: 90,
                total_spendable_sats: 1000,
                actual_sats: 0,
                ..
            }
        ));
    }

    #[test]
    fn validates_partial_donation_distribution() {
        let expected =
            PayoutMode::try_from(format!("sri/donate/10/{MINER_ADDRESS}/w1").as_str()).unwrap();

        validate_tx_outputs(
            &expected,
            vec![tx_out(100, OTHER_ADDRESS), tx_out(900, MINER_ADDRESS)],
        )
        .unwrap();
    }

    #[test]
    fn rejects_wrong_partial_donation_distribution() {
        let expected =
            PayoutMode::try_from(format!("sri/donate/10/{MINER_ADDRESS}/w1").as_str()).unwrap();

        let err = validate_tx_outputs(
            &expected,
            vec![tx_out(200, OTHER_ADDRESS), tx_out(800, MINER_ADDRESS)],
        )
        .unwrap_err();

        assert!(matches!(
            err,
            PayoutValidationError::PayoutMismatch {
                expected_sats: 900,
                actual_sats: 800,
                ..
            }
        ));
    }

    #[test]
    fn full_donation_has_no_miner_payout_to_verify() {
        let expected = PayoutMode::FullDonation;

        validate_tx_outputs(&expected, vec![tx_out(1_000, OTHER_ADDRESS)]).unwrap();
    }

    #[test]
    fn validates_coinbase_with_remaining_scriptsig_bytes_after_extranonce() {
        let expected = PayoutMode::try_from(format!("{MINER_ADDRESS}.w1").as_str()).unwrap();
        let (prefix, suffix) = coinbase_tx_parts(
            vec![tx_out(1_000, MINER_ADDRESS), tx_out(1, OTHER_ADDRESS)],
            b"/NexusPool/",
        );

        expected
            .validate_coinbase_tx_parts(&prefix, &suffix, FULL_EXTRANONCE_SIZE)
            .unwrap();
    }

    // ---------------------------------------------------------------------
    // `decode_declared_coinbase_tx` regressions
    //
    // These cover the unsafe reconstruction paths described in issue #645:
    // short prefixes, oversized/mismatched scriptSig lengths, offset
    // underflow, and oversized missing-extranonce allocation. None of them may
    // panic or allocate an attacker-selected amount of memory.
    // ---------------------------------------------------------------------

    /// Builds a segwit-layout coinbase prefix whose declared scriptSig length VarInt is set to
    /// `declared_script_sig_len`, followed by `script_sig_bytes_in_prefix` scriptSig bytes, and a
    /// matching suffix containing the input sequence, a single output, and the locktime.
    ///
    /// This lets a test decouple the *declared* scriptSig length from the bytes actually present,
    /// which is exactly what an attacker controls.
    fn declared_coinbase_parts(
        declared_script_sig_len: &[u8],
        script_sig_bytes_in_prefix: &[u8],
    ) -> (Vec<u8>, Vec<u8>) {
        // Segwit layout assumed by the decoder: version(4) + marker(0x00)/flag(0x01)(2) +
        // input_count(1) + prev txid(32) + prev vout(4) = 43 bytes before the scriptSig-length
        // VarInt.
        let mut prefix = Vec::new();
        prefix.extend([0x02, 0x00, 0x00, 0x00]); // version
        prefix.push(0x00); // segwit marker
        prefix.push(0x01); // segwit flag
        prefix.push(0x01); // input count
        prefix.extend([0; 32]); // prev txid
        prefix.extend([0xff, 0xff, 0xff, 0xff]); // prev vout index
        prefix.extend(declared_script_sig_len); // scriptSig length VarInt
        prefix.extend(script_sig_bytes_in_prefix); // scriptSig bytes already in prefix

        let mut suffix = Vec::new();
        suffix.extend([0xff, 0xff, 0xff, 0xff]); // input sequence
        suffix.extend(serialize(&vec![tx_out(0, MINER_ADDRESS)])); // outputs
        // witness for the single input: 1 stack item of length 1 (value 0x00). A non-empty
        // witness is required for the decoder to accept the segwit marker/flag.
        suffix.push(0x01); // one witness stack item
        suffix.push(0x01); // item length 1
        suffix.push(0x00); // item byte
        suffix.extend([0, 0, 0, 0]); // locktime

        (prefix, suffix)
    }

    #[test]
    fn decodes_valid_declared_coinbase() {
        // scriptSig = 4 prefix bytes + 8 zero-filled extranonce bytes = 12, encoded as a single
        // byte (< 0xfd).
        let (prefix, suffix) = declared_coinbase_parts(&[12], &[0x03, 0x01, 0x02, 0x03]);
        decode_declared_coinbase_tx(&prefix, &suffix).expect("valid declaration must decode");
    }

    #[test]
    fn rejects_short_prefix_without_panicking() {
        let short = vec![0u8; COINBASE_PREFIX_LEN - 1];
        let err = decode_declared_coinbase_tx(&short, &[]).unwrap_err();
        assert!(matches!(err, DeclaredCoinbaseError::PrefixTooShort));
    }

    #[test]
    fn rejects_oversized_script_sig_varint_before_allocating() {
        // A single-byte scriptSig length of 200 exceeds the 100-byte consensus bound.
        let (prefix, suffix) = declared_coinbase_parts(&[200], &[0x03, 0x01, 0x02, 0x03]);
        let err = decode_declared_coinbase_tx(&prefix, &suffix).unwrap_err();
        assert!(matches!(err, DeclaredCoinbaseError::ScriptSigTooLarge));
    }

    #[test]
    fn rejects_huge_script_sig_varint_before_allocating() {
        // 0xfd followed by a u16 of 0xffff => a declared scriptSig length of 65535, which would
        // previously have driven a large missing-extranonce allocation.
        let (prefix, suffix) = declared_coinbase_parts(&[0xfd, 0xff, 0xff], &[]);
        let err = decode_declared_coinbase_tx(&prefix, &suffix).unwrap_err();
        assert!(matches!(err, DeclaredCoinbaseError::ScriptSigTooLarge));
    }

    #[test]
    fn rejects_truncated_script_sig_varint() {
        // 0xfd announces a 2-byte VarInt but provides only one byte, so decoding must fail rather
        // than slice out of bounds.
        let (prefix, suffix) = declared_coinbase_parts(&[0xfd, 0x10], &[]);
        let err = decode_declared_coinbase_tx(&prefix, &suffix).unwrap_err();
        assert!(matches!(err, DeclaredCoinbaseError::InvalidScriptSigVarInt));
    }

    #[test]
    fn rejects_script_sig_length_underflow() {
        // Declared scriptSig length (4) is smaller than the scriptSig bytes already present in the
        // prefix (6). Previously this underflowed `usize` and panicked.
        let (prefix, suffix) = declared_coinbase_parts(&[4], &[0x03, 0x01, 0x02, 0x03, 0x04, 0x05]);
        let err = decode_declared_coinbase_tx(&prefix, &suffix).unwrap_err();
        assert!(matches!(
            err,
            DeclaredCoinbaseError::ScriptSigLengthMismatch
        ));
    }

    #[test]
    fn rejects_structurally_invalid_reconstruction() {
        // A valid scriptSig length but a suffix that does not deserialize into a transaction.
        let (prefix, _) = declared_coinbase_parts(&[12], &[0x03, 0x01, 0x02, 0x03]);
        let err = decode_declared_coinbase_tx(&prefix, &[0xde, 0xad]).unwrap_err();
        assert!(matches!(err, DeclaredCoinbaseError::Decode(_)));
    }

    #[test]
    fn accepts_boundary_script_sig_of_max_size() {
        // Declared scriptSig length exactly at the 100-byte bound must be accepted (then fail only
        // if the surrounding bytes do not form a valid transaction). Build a real one.
        let script_sig_bytes_in_prefix = [0x03, 0x01, 0x02, 0x03];
        let remaining = MAX_COINBASE_SCRIPT_SIG_SIZE - script_sig_bytes_in_prefix.len();
        let (prefix, suffix) = declared_coinbase_parts(&[100], &script_sig_bytes_in_prefix);
        // We must supply `remaining` more scriptSig bytes via the extranonce gap for the tx to be
        // structurally consistent; the decoder fills them with zeros.
        assert_eq!(
            prefix.len() - COINBASE_PREFIX_LEN - VarInt(100).size(),
            script_sig_bytes_in_prefix.len()
        );
        let tx = decode_declared_coinbase_tx(&prefix, &suffix)
            .expect("boundary-size scriptSig must decode");
        // The reconstructed scriptSig must be exactly the bounded size.
        assert_eq!(
            tx.input[0].script_sig.len(),
            remaining + script_sig_bytes_in_prefix.len()
        );
    }
}
