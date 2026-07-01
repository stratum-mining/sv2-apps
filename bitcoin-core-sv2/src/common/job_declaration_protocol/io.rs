//! Request / response types exchanged between `jd-server` and the Bitcoin Core IPC thread.

use stratum_core::bitcoin::{Block, BlockHash, CompactTarget, Transaction, Wtxid, block::Version};
use tokio::sync::oneshot;

/// Snapshot of the template parameters used by the validator at decision time.
///
/// This lets callers distinguish stale-tip races from other validation failures.
///
/// Please check https://github.com/stratum-mining/sv2-apps/issues/364
/// for more details on the regression that motivated this field.
#[derive(Debug, Clone, Copy)]
pub struct ValidationContext {
    pub prev_hash: BlockHash,
    pub nbits: CompactTarget,
    pub min_ntime: u32,
}

/// A request sent from `jd-server` to the [`BitcoinCoreSv2JDP`](super::BitcoinCoreSv2JDP) IPC
/// thread.
///
/// Built from a `DeclareMiningJob` (plus an optional `ProvideMissingTransactionsSuccess`)
/// or a `PushSolution`.
pub enum JdRequest {
    /// Validate a declared mining job via Bitcoin Core's `checkBlock`.
    DeclareMiningJob {
        version: Version,
        coinbase_tx: Transaction,
        wtxid_list: Vec<Wtxid>,
        missing_txs: Vec<Transaction>,
        response_tx: oneshot::Sender<JdResponse>,
    },
    /// Submit a fully assembled block to Bitcoin Core (fire-and-forget).
    PushSolution { block: Block },
}

/// The result of trying to handle a DeclareMiningJob request.
#[derive(Debug, Clone)]
pub enum JdResponse {
    Success {
        prev_hash: BlockHash,
        nbits: CompactTarget,
        min_ntime: u32,
        /// Full non-coinbase transaction list in declaration order.
        ///
        /// This is used by `jd-server` both to reconstruct solved blocks when handling
        /// `PushSolution` and to derive txids for merkle-root/merkle-path validation.
        txdata: Vec<Transaction>,
    },
    Error {
        error_code: &'static str,
        validation_context: ValidationContext,
    },
    MissingTransactions {
        missing_wtxids: Vec<Wtxid>,
        validation_context: ValidationContext,
    },
}
