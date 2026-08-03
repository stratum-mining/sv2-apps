//! Version-agnostic Template Distribution Protocol runtime API.
//!
//! This module exposes a runtime handle that sits between Sv2 TDP message channels and the
//! selected Bitcoin Core IPC backend.
//!
//! From the caller perspective, the runtime:
//! - consumes incoming [`stratum_core::parsers_sv2::TemplateDistribution`] messages (for example
//!   `CoinbaseOutputConstraints`, `RequestTransactionData`, and `SubmitSolution`);
//! - emits outgoing [`stratum_core::parsers_sv2::TemplateDistribution`] messages (`NewTemplate`,
//!   `SetNewPrevHash`, and transaction-data responses).
//!
//! `fee_threshold` controls template refreshes driven by mempool fee deltas, while `min_interval`
//! enforces a minimum spacing between mempool-driven template updates. Chain tip updates are never
//! throttled: while inside the `min_interval` window, `waitNext` requests are issued with
//! `fee_threshold = MAX_MONEY` so Bitcoin Core only wakes the monitor for a chain tip change.

use crate::{
    runtime_api::{BitcoinCoreSv2Error, BitcoinCoreSv2Protocol, BitcoinCoreVersion},
    unix_capnp::{v30x, v31x},
};
use async_channel::{Receiver, Sender};
use std::path::Path;
use stratum_core::parsers_sv2::TemplateDistributionOwned;
pub use tokio_util::sync::CancellationToken;

/// Version-agnostic TDP runtime handle.
///
/// Instances are created with [`new`], which selects the concrete backend for the requested
/// [`BitcoinCoreVersion`].
pub enum BitcoinCoreSv2TDP {
    V30X(v30x::template_distribution_protocol::BitcoinCoreSv2TDP),
    V31X(v31x::template_distribution_protocol::BitcoinCoreSv2TDP),
}

impl BitcoinCoreSv2TDP {
    pub async fn run(&mut self) {
        match self {
            Self::V30X(runtime) => runtime.run().await,
            Self::V31X(runtime) => runtime.run().await,
        }
    }
}

pub type BitcoinCoreSv2TDPError = BitcoinCoreSv2Error;

/// Builds a version-agnostic TDP runtime from the selected Bitcoin Core major version.
#[allow(clippy::too_many_arguments)]
pub async fn new<P>(
    version: BitcoinCoreVersion,
    bitcoin_core_unix_socket_path: P,
    fee_threshold: u64,
    min_interval: u8,
    incoming_messages: Receiver<TemplateDistributionOwned>,
    outgoing_messages: Sender<TemplateDistributionOwned>,
    global_cancellation_token: CancellationToken,
) -> Result<BitcoinCoreSv2TDP, BitcoinCoreSv2TDPError>
where
    P: AsRef<Path>,
{
    match version {
        BitcoinCoreVersion::V30X => v30x::template_distribution_protocol::BitcoinCoreSv2TDP::new(
            bitcoin_core_unix_socket_path,
            fee_threshold,
            min_interval,
            incoming_messages,
            outgoing_messages,
            global_cancellation_token,
        )
        .await
        .map(BitcoinCoreSv2TDP::V30X)
        .map_err(|error| {
            BitcoinCoreSv2TDPError::from_debug(version, BitcoinCoreSv2Protocol::TDP, error)
        }),
        BitcoinCoreVersion::V31X => v31x::template_distribution_protocol::BitcoinCoreSv2TDP::new(
            bitcoin_core_unix_socket_path,
            fee_threshold,
            min_interval,
            incoming_messages,
            outgoing_messages,
            global_cancellation_token,
        )
        .await
        .map(BitcoinCoreSv2TDP::V31X)
        .map_err(|error| {
            BitcoinCoreSv2TDPError::from_debug(version, BitcoinCoreSv2Protocol::TDP, error)
        }),
    }
}
