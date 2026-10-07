//! Error types for Bitcoin Core v32.x Sv2 Job Declaration Protocol via capnp over UNIX socket.

use std::path::PathBuf;
use stratum_core::bitcoin::consensus;

use bitcoin_capnp_types_v32::capnp;

/// Errors from the [`crate::unix_capnp::v32x::job_declaration_protocol::BitcoinCoreSv2JDP`] layer.
#[derive(Debug)]
pub enum BitcoinCoreSv2JDPError {
    /// Cap'n Proto RPC error.
    CapnpError(capnp::Error),
    /// Failed to create a dedicated thread IPC client, capturing the underlying context.
    FailedToCreateThreadIpcClient(String),
    /// Failed to connect to the Bitcoin Core Unix socket.
    CannotConnectToUnixSocket(PathBuf, String),
    /// Failed to deserialize a block header from the IPC response.
    FailedToDeserializeBlockHeader(consensus::encode::Error),
    /// Failed to deserialize a transaction from the IPC response.
    FailedToDeserializeTransaction(consensus::encode::Error),
    /// `getTransactionsByWitnessID` answered a list of a different length than it was asked for,
    /// leaving no way to match its elements back to the declared wtxids.
    UnexpectedTransactionLookupLength { declared: usize, answered: usize },
    /// Readiness signal receiver was dropped before bootstrap completed.
    ReadinessSignalFailed,
    /// The cancellation token fired before bootstrap completed.
    BootstrapCancelled,
}

impl From<capnp::Error> for BitcoinCoreSv2JDPError {
    fn from(error: capnp::Error) -> Self {
        BitcoinCoreSv2JDPError::CapnpError(error)
    }
}
