//! Error types for Bitcoin Core v30.x Sv2 Job Declaration Protocol via capnp over UNIX socket.

use std::path::PathBuf;
use stratum_core::bitcoin::consensus;

use bitcoin_capnp_types_v30::capnp;

/// Errors from the [`crate::unix_capnp::v30x::job_declaration_protocol::BitcoinCoreSv2JDP`] layer.
#[derive(Debug)]
pub enum BitcoinCoreSv2JDPError {
    /// Cap'n Proto RPC error.
    CapnpError(capnp::Error),
    /// Failed to connect to the Bitcoin Core Unix socket.
    CannotConnectToUnixSocket(PathBuf, String),
    /// Failed to deserialize a block from the IPC response.
    FailedToDeserializeBlock(consensus::encode::Error),
    /// Readiness signal receiver was dropped before bootstrap completed.
    ReadinessSignalFailed,
    /// The cancellation token fired before bootstrap completed.
    BootstrapCancelled,
}

impl BitcoinCoreSv2JDPError {
    /// Returns true when Bitcoin Core rejected the request because its worker thread was mid-call.
    ///
    /// Bitcoin Core 30.1 through 30.3 bundle libmultiprocess v7.0-pre1, which throws `thread busy`
    /// when a request arrives for a worker thread that is already executing one. Core is in that
    /// state while it processes a newly arrived block, so the condition clears on its own and the
    /// request succeeds when retried; treating it as fatal instead tears down the IPC connection,
    /// and with it the application, every time a block lands mid-request.
    ///
    /// The v31x tree deliberately does not carry this. Bitcoin Core 31.x bundles libmultiprocess
    /// v8.0, where simultaneous requests to a worker thread are queued rather than rejected, so a
    /// v31.x node never answers with it.
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
