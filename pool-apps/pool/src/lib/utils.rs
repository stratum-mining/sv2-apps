use std::{convert::TryFrom, net::SocketAddr};
use stratum_apps::{
    config_helpers::CoinbaseRewardScript,
    stratum_core::{
        binary_sv2::Str0255,
        bitcoin::{Amount, TxOut},
        common_messages_sv2::{Protocol, SetupConnection},
        mining_sv2::CloseChannel,
    },
    utils::types::ChannelId,
};

use crate::error::PoolErrorKind;

/// Constructs a `SetupConnection` message for the mining protocol.
#[allow(clippy::result_large_err)]
pub fn get_setup_connection_message(
    min_version: u16,
    max_version: u16,
    address: &SocketAddr,
) -> Result<SetupConnection<'static>, PoolErrorKind> {
    let endpoint_host = address.ip().to_string().into_bytes().try_into()?;
    let vendor = String::new().try_into()?;
    let hardware_version = String::new().try_into()?;
    let firmware = String::new().try_into()?;
    let device_id = String::new().try_into()?;
    let flags = 0b0000_0000_0000_0000_0000_0000_0000_0110;
    Ok(SetupConnection {
        protocol: Protocol::MiningProtocol,
        min_version,
        max_version,
        flags,
        endpoint_host,
        endpoint_port: address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    })
}

/// Constructs a `SetupConnection` message for the Template Provider (TP).
#[allow(clippy::result_large_err)]
pub fn get_setup_connection_message_tp(
    address: SocketAddr,
) -> Result<SetupConnection<'static>, PoolErrorKind> {
    let endpoint_host = address.ip().to_string().into_bytes().try_into()?;
    let vendor = String::new().try_into()?;
    let hardware_version = String::new().try_into()?;
    let firmware = String::new().try_into()?;
    let device_id = String::new().try_into()?;
    Ok(SetupConnection {
        protocol: Protocol::TemplateDistributionProtocol,
        min_version: 2,
        max_version: 2,
        flags: 0b0000_0000_0000_0000_0000_0000_0000_0000,
        endpoint_host,
        endpoint_port: address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    })
}

/// Creates a [`CloseChannel`] message for the given channel ID and reason.
///
/// The `msg` is converted into a [`Str0255`] reason code.  
/// If conversion fails, this function will panic.
pub(crate) fn create_close_channel_msg(channel_id: ChannelId, msg: &str) -> CloseChannel<'_> {
    CloseChannel {
        channel_id,
        reason_code: Str0255::try_from(msg.to_string()).expect("Could not convert message."),
    }
}

/// Represents the payout mode for a mining connection.
///
/// This determines how the coinbase reward is distributed:
/// - `Solo`: Full reward goes to the miner's specified payout address. Pattern:
///   `sri/solo/<payout_address>/<worker_name>` or plain Bitcoin address
/// - `Donate`: Partial donation to pool, remainder to miner. Pattern:
///   `sri/donate/<percentage>/<payout_address>/<worker_name>` (percentage is 0-100, representing
///   pool's portion)
/// - `FullDonation`: Full reward goes to the pool.
#[derive(Debug, Clone)]
pub enum PayoutMode {
    /// Solo mode: miner receives full block reward.
    Solo { script: CoinbaseRewardScript },
    /// Donate mode: pool receives specified percentage, miner gets remainder.
    Donate {
        percentage: u8,
        script: CoinbaseRewardScript,
    },
    /// Full donation mode: full reward goes to the pool.
    FullDonation,
}

impl PayoutMode {
    pub fn coinbase_outputs(
        &self,
        total_value: u64,
        pool_script: &CoinbaseRewardScript,
    ) -> Vec<TxOut> {
        match self {
            PayoutMode::Solo {
                script: coinbase_script,
            } => {
                vec![TxOut {
                    value: Amount::from_sat(total_value),
                    script_pubkey: coinbase_script.script_pubkey(),
                }]
            }

            PayoutMode::Donate {
                percentage,
                script: miner_script,
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

            PayoutMode::FullDonation => {
                vec![TxOut {
                    value: Amount::from_sat(total_value),
                    script_pubkey: pool_script.script_pubkey(),
                }]
            }
        }
    }
}

#[allow(clippy::result_large_err)]
impl TryFrom<&str> for PayoutMode {
    type Error = PoolErrorKind;

    fn try_from(user_identity: &str) -> Result<Self, Self::Error> {
        if user_identity.is_empty() {
            return Ok(PayoutMode::FullDonation);
        }

        let addr = user_identity
            .split_once('.')
            .map(|(addr, _)| addr)
            .unwrap_or(user_identity);

        let descriptor = format!("addr({addr})");
        if let Ok(script) = CoinbaseRewardScript::from_descriptor(&descriptor) {
            return Ok(PayoutMode::Solo { script });
        }

        let mut parts = user_identity.split('/');

        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("sri"), Some("solo"), Some(payout_address), _) => {
                let descriptor = format!("addr({payout_address})");
                if let Ok(script) = CoinbaseRewardScript::from_descriptor(&descriptor) {
                    Ok(PayoutMode::Solo { script })
                } else {
                    Err(PoolErrorKind::PayoutModeError(
                        "Invalid user_identity pattern for solo mining mode.".to_string(),
                    ))
                }
            }

            (Some("sri"), Some("donate"), None, _)
            | (Some("sri"), Some("donate"), Some(_), None) => Ok(PayoutMode::FullDonation),

            (Some("sri"), Some("donate"), Some(percentage), Some(payout_address)) => {
                let descriptor = format!("addr({payout_address})");

                match (
                    percentage.parse::<u8>(),
                    CoinbaseRewardScript::from_descriptor(&descriptor),
                ) {
                    (Ok(p), Ok(script)) if (1..100).contains(&p) => Ok(PayoutMode::Donate {
                        percentage: p,
                        script,
                    }),
                    _ => Err(PoolErrorKind::PayoutModeError(
                        "Invalid user_identity pattern for solo mining mode.".to_string(),
                    )),
                }
            }

            (Some("sri"), Some(_), _, _) => Err(PoolErrorKind::PayoutModeError(
                "Invalid user_identity pattern for solo mining mode.".to_string(),
            )),

            _ => Ok(PayoutMode::FullDonation),
        }
    }
}

#[cfg(test)]
mod tests {
    use stratum_apps::stratum_core::bitcoin::{
        params::{MAINNET, TESTNET4},
        Address,
    };

    use super::*;

    #[test]
    fn test_valid_pool_donate() {
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
    fn test_valid_solo() {
        let valid_testnet_addr = "tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8";
        let valid_mainnet_addr = "bc1qtzqxqaxyy6lda2fhdtp5dp0v56vlf6g0tljy2x";

        assert!(matches!(
            PayoutMode::try_from(format!("sri/solo/{}/worker", valid_testnet_addr).as_str()),
                Ok(PayoutMode::Solo { script }) if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == valid_testnet_addr
        ));
        assert!(matches!(
            PayoutMode::try_from(format!("sri/solo/{}/worker/subworker", valid_mainnet_addr).as_str()),
            Ok(PayoutMode::Solo { script }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string()== valid_mainnet_addr
        ));
        assert!(matches!(
            PayoutMode::try_from(valid_mainnet_addr),
            Ok(PayoutMode::Solo { script }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string() == valid_mainnet_addr
        ));

        assert!(matches!(
                PayoutMode::try_from(valid_testnet_addr), 
                Ok(PayoutMode::Solo { script }) if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == valid_testnet_addr))
    }

    #[test]
    fn test_valid_solo_with_worker_suffix() {
        let valid_mainnet_addr = "bc1qtzqxqaxyy6lda2fhdtp5dp0v56vlf6g0tljy2x";

        assert!(matches!(
            PayoutMode::try_from(format!("{}.worker1", valid_mainnet_addr).as_str()),
            Ok(PayoutMode::Solo { script }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string()== valid_mainnet_addr
        ));
        assert!(matches!(
            PayoutMode::try_from(format!("{}.worker1.subworker", valid_mainnet_addr).as_str()),
            Ok(PayoutMode::Solo { script }) if Address::from_script(script.script_pubkey().as_script(), MAINNET.clone()).unwrap().to_string() == valid_mainnet_addr
        ));
    }

    #[test]
    fn test_invalid_address_with_suffix() {
        assert!(matches!(
            PayoutMode::try_from("invalid_address.worker"),
            Ok(PayoutMode::FullDonation)
        ));
    }

    #[test]
    fn test_valid_donate_with_percentage() {
        let valid_testnet_addr = "tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8";

        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/50/{}/worker", valid_testnet_addr).as_str()).unwrap(),
            PayoutMode::Donate { percentage: 50, script } if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == valid_testnet_addr
        ));

        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/50/{}/worker/subworker", valid_testnet_addr).as_str()).unwrap(),
            PayoutMode::Donate { percentage: 50, script } if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == valid_testnet_addr
        ));

        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/0/{}/worker", valid_testnet_addr).as_str()),
            Err(PoolErrorKind::PayoutModeError(_))
        ));

        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/100/{}/worker", valid_testnet_addr).as_str()),
            Err(PoolErrorKind::PayoutModeError(_))
        ));
    }

    #[test]
    fn test_invalid_patterns() {
        let valid_testnet_addr = "tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8";

        assert!(PayoutMode::try_from("sri/invalid/worker").is_err());
        assert!(PayoutMode::try_from("sri/solo").is_err());
        assert!(PayoutMode::try_from("sri/solo/random_thing_here/worker").is_err());
        assert!(PayoutMode::try_from("sri/solo/").is_err());
        assert!(matches!(
            PayoutMode::try_from("sri/donate/abc/addr/worker"),
            Err(PoolErrorKind::PayoutModeError(_))
        ));
        assert!(matches!(
            PayoutMode::try_from("sri/donate/101/addr/worker"),
            Err(PoolErrorKind::PayoutModeError(_))
        ));

        assert!(matches!(
            PayoutMode::try_from(format!("sri/donate/50/{}", valid_testnet_addr).as_str()).unwrap(),
            PayoutMode::Donate { percentage: 50, script } if Address::from_script(script.script_pubkey().as_script(), TESTNET4.clone()).unwrap().to_string() == valid_testnet_addr
        ));
        assert!(matches!(
            PayoutMode::try_from("other/donate/worker"),
            Ok(PayoutMode::FullDonation)
        ));
        assert!(matches!(
            PayoutMode::try_from("sri/"),
            Err(PoolErrorKind::PayoutModeError(_))
        ));
        assert!(matches!(
            PayoutMode::try_from(""),
            Ok(PayoutMode::FullDonation)
        ));
    }
}
