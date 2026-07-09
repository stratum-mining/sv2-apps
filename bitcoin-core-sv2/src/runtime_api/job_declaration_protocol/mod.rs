//! Version-agnostic Job Declaration Protocol runtime API.
//!
//! This module exposes a runtime handle that receives [`io::JdRequest`] messages and bridges them
//! to the selected Bitcoin Core IPC backend.
//!
//! The request channel covers the two base JDP flows:
//! - `DeclareMiningJob`
//! - `PushSolution`
//!
//! Token lifecycle and higher-level protocol state remain the caller responsibility (for example,
//! associating `AllocateMiningJobToken`/`DeclareMiningJob`/`SetCustomMiningJob` state).

pub mod io;

use crate::{
    runtime_api::{BitcoinCoreSv2Error, BitcoinCoreSv2Protocol, BitcoinCoreVersion},
    unix_capnp::{v30x, v31x, v32x},
};
use async_channel::Receiver;
use io::JdRequest;
use std::path::Path;
pub use tokio_util::sync::CancellationToken;

/// Version-agnostic JDP runtime handle.
///
/// Instances are created with [`new`], which selects the concrete backend for the requested
/// [`BitcoinCoreVersion`].
pub enum BitcoinCoreSv2JDP {
    V30X(v30x::job_declaration_protocol::BitcoinCoreSv2JDP),
    V31X(v31x::job_declaration_protocol::BitcoinCoreSv2JDP),
    V32X(v32x::job_declaration_protocol::BitcoinCoreSv2JDP),
}

impl BitcoinCoreSv2JDP {
    pub async fn run(&self) {
        match self {
            Self::V30X(runtime) => runtime.run().await,
            Self::V31X(runtime) => runtime.run().await,
            Self::V32X(runtime) => runtime.run().await,
        }
    }
}

pub type BitcoinCoreSv2JDPError = BitcoinCoreSv2Error;

/// Builds a version-agnostic JDP runtime from the selected Bitcoin Core major version.
pub async fn new<P>(
    version: BitcoinCoreVersion,
    bitcoin_core_unix_socket_path: P,
    incoming_requests: Receiver<JdRequest>,
    cancellation_token: CancellationToken,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<BitcoinCoreSv2JDP, BitcoinCoreSv2JDPError>
where
    P: AsRef<Path>,
{
    match version {
        BitcoinCoreVersion::V30X => v30x::job_declaration_protocol::BitcoinCoreSv2JDP::new(
            bitcoin_core_unix_socket_path,
            incoming_requests,
            cancellation_token,
            ready_tx,
        )
        .await
        .map(BitcoinCoreSv2JDP::V30X)
        .map_err(|error| {
            BitcoinCoreSv2JDPError::from_debug(version, BitcoinCoreSv2Protocol::JDP, error)
        }),
        BitcoinCoreVersion::V31X => v31x::job_declaration_protocol::BitcoinCoreSv2JDP::new(
            bitcoin_core_unix_socket_path,
            incoming_requests,
            cancellation_token,
            ready_tx,
        )
        .await
        .map(BitcoinCoreSv2JDP::V31X)
        .map_err(|error| {
            BitcoinCoreSv2JDPError::from_debug(version, BitcoinCoreSv2Protocol::JDP, error)
        }),
        BitcoinCoreVersion::V32X => v32x::job_declaration_protocol::BitcoinCoreSv2JDP::new(
            bitcoin_core_unix_socket_path,
            incoming_requests,
            cancellation_token,
            ready_tx,
        )
        .await
        .map(BitcoinCoreSv2JDP::V32X)
        .map_err(|error| {
            BitcoinCoreSv2JDPError::from_debug(version, BitcoinCoreSv2Protocol::JDP, error)
        }),
    }
}
