use std::{convert::TryFrom, net::SocketAddr};
use stratum_apps::{
    stratum_core::{
        binary_sv2::Str0255Owned,
        common_messages_sv2::{Protocol, SetupConnectionOwned},
        mining_sv2::CloseChannelOwned,
        parsers_sv2::{MiningOwned, Tlv},
    },
    utils::types::ChannelId,
};

use crate::error::PoolErrorKind;

pub use stratum_apps::payout::{PayoutMode, PayoutModeError};

pub(crate) type DownstreamMessage = (MiningOwned, Option<Vec<Tlv>>);

/// Constructs a `SetupConnection` message for the mining protocol.
#[allow(clippy::result_large_err)]
pub fn get_setup_connection_message(
    min_version: u16,
    max_version: u16,
    address: &SocketAddr,
) -> Result<SetupConnectionOwned, PoolErrorKind> {
    let endpoint_host = address.ip().to_string().try_into()?;
    let vendor = "".try_into()?;
    let hardware_version = "".try_into()?;
    let firmware = "".try_into()?;
    let device_id = "".try_into()?;
    let flags = 0b0000_0000_0000_0000_0000_0000_0000_0110;
    Ok(SetupConnectionOwned {
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
) -> Result<SetupConnectionOwned, PoolErrorKind> {
    let endpoint_host = address.ip().to_string().try_into()?;
    let vendor = "".try_into()?;
    let hardware_version = "".try_into()?;
    let firmware = "".try_into()?;
    let device_id = "".try_into()?;
    Ok(SetupConnectionOwned {
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
pub(crate) fn create_close_channel_msg(channel_id: ChannelId, msg: &str) -> CloseChannelOwned {
    CloseChannelOwned {
        channel_id,
        reason_code: Str0255Owned::try_from(msg).expect("Could not convert message."),
    }
}
