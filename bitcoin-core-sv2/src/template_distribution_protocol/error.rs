use std::path::Path;
use stratum_core::bitcoin::{
    block::ValidationError, consensus, consensus::encode::Error as ConsensusEncodeError,
};

/// Error type for [`crate::BitcoinCoreSv2TDP`]
#[derive(Debug)]
pub enum BitcoinCoreSv2TDPError {
    CapnpError(capnp::Error),
    CannotConnectToUnixSocket(Box<Path>, String),
    InvalidTemplateHeader(consensus::encode::Error),
    InvalidTemplateHeaderLength,
    FailedToSerializeCoinbasePrefix,
    FailedToSerializeCoinbaseOutputs,
    TemplateNotFound,
    TemplateIpcClientNotFound,
    FailedToSendNewTemplateMessage,
    FailedToSendSetNewPrevHashMessage,
    FailedToFetchTemplateTxData,
    FailedToSendRequestTransactionDataResponseMessage,
    FailedToRecvTemplateDistributionMessage,
    FailedToSendTemplateDistributionMessage,
    FailedToSubmitSolution,
    FailedToSetThread,
    FailedToGetWaitNextRequestOptions,
    FailedToSendInterruptWaitRequest,
    FailedToWaitForMonitorIpcTemplatesTask,
}

impl From<capnp::Error> for BitcoinCoreSv2TDPError {
    fn from(error: capnp::Error) -> Self {
        BitcoinCoreSv2TDPError::CapnpError(error)
    }
}

impl From<consensus::encode::Error> for BitcoinCoreSv2TDPError {
    fn from(error: consensus::encode::Error) -> Self {
        BitcoinCoreSv2TDPError::InvalidTemplateHeader(error)
    }
}

#[derive(Debug)]
pub enum TemplateDataError {
    InvalidCoinbaseTx(ConsensusEncodeError),
    InvalidSolution,
    InvalidSolutionPoW(ValidationError),
    InvalidMerkleRoot,
    InvalidBlockVersion,
    InvalidCoinbaseTxVersion,
    InvalidCoinbaseScriptSig,
    FailedToSumCoinbaseOutputs,
    CapnpError(capnp::Error),
    FailedIpcSubmitSolution,
    FailedToSerializeEmptyCoinbaseOutputs,
    FailedToConvertMerklePathHashToU256,
    FailedToCreateMerklePathSeq,
    BitcoinCoreSv2TDPError(BitcoinCoreSv2TDPError),
}

impl From<BitcoinCoreSv2TDPError> for TemplateDataError {
    fn from(error: BitcoinCoreSv2TDPError) -> Self {
        TemplateDataError::BitcoinCoreSv2TDPError(error)
    }
}

impl From<ConsensusEncodeError> for TemplateDataError {
    fn from(error: ConsensusEncodeError) -> Self {
        TemplateDataError::InvalidCoinbaseTx(error)
    }
}

impl From<capnp::Error> for TemplateDataError {
    fn from(error: capnp::Error) -> Self {
        TemplateDataError::CapnpError(error)
    }
}

impl std::fmt::Display for TemplateDataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateDataError::InvalidCoinbaseTx(e) => {
                write!(f, "Invalid coinbase transaction: {}", e)
            }
            TemplateDataError::InvalidSolution => write!(f, "Invalid solution"),
            TemplateDataError::InvalidSolutionPoW(e) => write!(f, "Invalid solution: {}", e),
            TemplateDataError::InvalidMerkleRoot => write!(f, "Invalid merkle root"),
            TemplateDataError::InvalidBlockVersion => write!(f, "Invalid block version"),
            TemplateDataError::InvalidCoinbaseTxVersion => {
                write!(f, "Invalid coinbase transaction version")
            }
            TemplateDataError::InvalidCoinbaseScriptSig => {
                write!(f, "Invalid coinbase script signature")
            }
            TemplateDataError::FailedToSerializeEmptyCoinbaseOutputs => {
                write!(f, "Failed to serialize empty coinbase outputs")
            }
            TemplateDataError::FailedToSumCoinbaseOutputs => {
                write!(f, "Failed to sum coinbase outputs")
            }
            TemplateDataError::CapnpError(e) => write!(f, "Cap'n Proto error: {}", e),
            TemplateDataError::FailedIpcSubmitSolution => {
                write!(f, "Failed to submit solution via IPC")
            }
            TemplateDataError::FailedToConvertMerklePathHashToU256 => {
                write!(f, "Failed to convert merkle path hash to U256")
            }
            TemplateDataError::FailedToCreateMerklePathSeq => {
                write!(f, "Failed to create merkle path sequence")
            }
            TemplateDataError::BitcoinCoreSv2TDPError(error) => {
                write!(f, "Bitcoin Core Sv2 error: {:?}", error)
            }
        }
    }
}

impl std::error::Error for TemplateDataError {}
