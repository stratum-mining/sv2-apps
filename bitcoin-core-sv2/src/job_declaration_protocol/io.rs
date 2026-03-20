//! Request / response types exchanged between `jd-server` and the Bitcoin Core IPC thread.

use stratum_core::{
    bitcoin::{BlockHash, CompactTarget, Transaction, Txid, Wtxid, block::Version},
    job_declaration_sv2::PushSolution,
};
use tokio::sync::oneshot;

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
    /// Submit a mining solution to Bitcoin Core (fire-and-forget).
    PushSolution {
        push_solution: PushSolution<'static>,
    },
}

/// The result of trying to handle a DeclareMiningJob request.
#[derive(Debug, Clone)]
pub enum JdResponse {
    Success {
        prev_hash: BlockHash,
        nbits: CompactTarget,
        /// Txids for all transactions (excluding coinbase), in the same order as the declared
        /// wtxid_list. Enables the caller to build the txid merkle tree for validating
        /// SetCustomMiningJob.merkle_path.
        txid_list: Vec<Txid>,
    },
    Error(String), // error_code string
    MissingTransactions(Vec<Wtxid>),
}
