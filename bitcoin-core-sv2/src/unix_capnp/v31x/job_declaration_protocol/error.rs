//! Error types for Bitcoin Core v31.x Sv2 Job Declaration Protocol via capnp over UNIX socket.

use std::path::PathBuf;
use stratum_core::bitcoin::consensus;

use super::super::capnp_types::capnp;

/// Errors from the [`crate::unix_capnp::v31x::job_declaration_protocol::BitcoinCoreSv2JDP`] layer.
#[derive(Debug)]
pub enum BitcoinCoreSv2JDPError {
    /// Cap'n Proto RPC error.
    CapnpError(capnp::Error),
    /// Failed to create a dedicated thread IPC client, capturing the underlying context.
    FailedToCreateThreadIpcClient(String),
    /// Failed to connect to the Bitcoin Core Unix socket.
    CannotConnectToUnixSocket(PathBuf, String),
    /// Failed to deserialize a block from the IPC response.
    FailedToDeserializeBlock(consensus::encode::Error),
    /// Readiness signal receiver was dropped before bootstrap completed.
    ReadinessSignalFailed,
}

impl BitcoinCoreSv2JDPError {
    /// Returns true when the error indicates transient IPC contention in Bitcoin Core.
    pub fn is_thread_busy(&self) -> bool {
        matches!(
            self,
            BitcoinCoreSv2JDPError::CapnpError(capnp_error)
                if capnp_error.to_string().contains("thread busy")
        )
    }
}

impl From<capnp::Error> for BitcoinCoreSv2JDPError {
    fn from(error: capnp::Error) -> Self {
        BitcoinCoreSv2JDPError::CapnpError(error)
    }
}
