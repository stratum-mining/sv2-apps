//! Error types for Bitcoin Core v32.x Sv2 Job Declaration Protocol via capnp over UNIX socket.

use std::path::PathBuf;
use stratum_core::bitcoin::consensus;

use super::bitcoin_capnp_types::capnp;

/// Errors from the [`crate::unix_capnp::v32x::job_declaration_protocol::BitcoinCoreSv2JDP`] layer.
#[derive(Debug)]
pub enum BitcoinCoreSv2JDPError {
    /// Cap'n Proto RPC error.
    CapnpError(capnp::Error),
    /// Failed to create a dedicated thread IPC client, capturing the underlying context.
    FailedToCreateThreadIpcClient(String),
    /// Failed to connect to the Bitcoin Core Unix socket.
    CannotConnectToUnixSocket(PathBuf, String),
    /// `getTip` completed without returning a result payload.
    GetTipReturnedNoResult,
    /// Failed to parse tip hash bytes returned by `getTip`.
    FailedToParseTipHashFromGetTip(String),
    /// Failed to decode `executeRpc` text payload as UTF-8.
    FailedToDecodeExecuteRpcResultText(String),
    /// Failed to parse JSON from `executeRpc` response.
    FailedToParseExecuteRpcJsonResponse(serde_json::Error),
    /// `getblockheader` RPC returned a JSON-RPC error object.
    GetBlockHeaderRpcReturnedError(String),
    /// `getblockheader` RPC response `result` is not a header hex string.
    GetBlockHeaderRpcResultIsNotHexString,
    /// Failed to deserialize a hex-encoded block header from RPC `getblockheader`.
    FailedToDeserializeHeaderHex(consensus::encode::FromHexError),
    /// `getmininginfo` RPC returned a JSON-RPC error object.
    GetMiningInfoRpcReturnedError(String),
    /// `getmininginfo` RPC response `result.next.bits` is missing or not a hex string.
    GetMiningInfoMissingNextBits,
    /// Failed to parse `getmininginfo` `result.next.bits` as a hex-encoded `nBits` value.
    FailedToParseGetMiningInfoNextBits(String),
    /// Forced chain-tip refresh exhausted retries without a terminal result.
    ForceUpdateChainTipStateExhaustedRetries,
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
