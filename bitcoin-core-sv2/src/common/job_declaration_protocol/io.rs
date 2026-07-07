//! Request / response types exchanged between `jd-server` and the Bitcoin Core IPC thread.

use stratum_core::bitcoin::{
    BlockHash, CompactTarget, Transaction, TxMerkleNode, Wtxid, block::Version,
};
use tokio::sync::oneshot;

/// Identifies a downstream client of `jd-server`.
pub type DownstreamId = usize;

/// Identifies a `DeclareMiningJob` request within a downstream connection.
pub type RequestId = u32;

/// Snapshot of the template parameters used by the mirror-based validators (v30.x/v31.x)
/// at decision time.
///
/// This lets those backends distinguish stale-tip races from other validation failures.
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
/// `DeclareMiningJob` is built from a `DeclareMiningJob` message (plus an optional
/// `ProvideMissingTransactionsSuccess`), `PushSolution` from a `PushSolution` message. The
/// remaining variants let `jd-server` mirror its job lifecycle so backends that retain
/// per-job state (the v32.x `TxCollection` backend keeps a validated `BlockTemplate` per
/// declared job) can release it.
pub enum JdRequest {
    /// Validate a declared mining job.
    ///
    /// Invariants (enforced by `jd-server`): `wtxid_list` contains no duplicates and every
    /// transaction in `missing_txs` is part of `wtxid_list`. The v32.x `TxCollection`
    /// backend relies on these; Bitcoin Core rejects violations with RPC-level errors.
    DeclareMiningJob {
        downstream_id: DownstreamId,
        request_id: RequestId,
        version: Version,
        coinbase_tx: Transaction,
        wtxid_list: Vec<Wtxid>,
        missing_txs: Vec<Transaction>,
        response_tx: oneshot::Sender<JdResponse>,
    },
    /// Submit a solution for a previously declared job to Bitcoin Core (fire-and-forget).
    ///
    /// `downstream_id`/`request_id` identify the `DeclareMiningJob` the solution belongs
    /// to. `coinbase_tx` is the declared coinbase with the solution's extranonce applied.
    PushSolution {
        downstream_id: DownstreamId,
        request_id: RequestId,
        version: u32,
        ntime: u32,
        nonce: u32,
        coinbase_tx: Transaction,
    },
    /// Release any backend state retained for a declared job that will never be used
    /// (consumed with an error, expired, or superseded). Fire-and-forget; harmless when the
    /// backend retained nothing.
    ReleaseDeclaredJob {
        downstream_id: DownstreamId,
        request_id: RequestId,
    },
    /// Release all backend state retained for a disconnected downstream (fire-and-forget).
    CleanupDownstream { downstream_id: DownstreamId },
}

/// The result of trying to handle a DeclareMiningJob request.
///
/// `Error` and `MissingTransactions` carry only the chain tip (`prev_hash`) the validator
/// operated against: stale-tip classification is restricted to `prev_hash` comparison
/// (see https://github.com/stratum-mining/sv2-apps/issues/597).
#[derive(Debug, Clone)]
pub enum JdResponse {
    Success {
        prev_hash: BlockHash,
        nbits: CompactTarget,
        min_ntime: u32,
        /// Coinbase merkle branch (sibling hashes from leaf to root at position 0) in the
        /// txid merkle tree, used by `jd-server` to validate a `SetCustomMiningJob`'s
        /// `merkle_path`. It does not depend on the coinbase transaction itself.
        merkle_path: Vec<TxMerkleNode>,
    },
    Error {
        error_code: &'static str,
        /// Chain tip at decision time; `None` when the failure happened before the validator
        /// could establish one (e.g. internal IPC errors).
        prev_hash: Option<BlockHash>,
    },
    MissingTransactions {
        missing_wtxids: Vec<Wtxid>,
        prev_hash: BlockHash,
    },
}
