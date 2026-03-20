//! Local mirror of Bitcoin Core's mempool state for job validation.

use std::collections::HashMap;
use stratum_core::bitcoin::{Block, BlockHash, CompactTarget, Transaction, Wtxid};

/// Local cache of mempool transactions and current template parameters.
///
/// Tracks transactions by wtxid and maintains the current prev_hash, nbits,
/// and min_ntime from the most recent block template.
#[derive(Default)]
pub struct MempoolMirror {
    txdata: HashMap<Wtxid, Transaction>,
    current_prev_hash: Option<BlockHash>,
    current_nbits: Option<CompactTarget>,
    current_min_ntime: Option<u32>,
}

impl MempoolMirror {
    /// Creates a new empty mempool mirror.
    pub fn new() -> Self {
        Default::default()
    }

    /// Updates the mirror with transactions from a block template.
    ///
    /// Clears stale transactions if the prev_hash changes.
    pub fn update(&mut self, block: &Block) {
        let prev_hash = block.header.prev_blockhash;
        if self.current_prev_hash != Some(prev_hash) {
            self.txdata.clear();
        }
        self.current_prev_hash = Some(prev_hash);
        self.current_nbits = Some(block.header.bits);
        self.current_min_ntime = Some(block.header.time);

        // skip the coinbase transaction
        for tx in block.txdata.iter().skip(1) {
            let wtxid = tx.compute_wtxid();
            self.txdata.insert(wtxid, tx.clone());
        }
    }

    /// Adds transactions to the mempool mirror.
    ///
    /// Used to add missing transactions from ProvideMissingTransactionsSuccess messages.
    pub fn add_transactions(&mut self, transactions: Vec<Transaction>) {
        for tx in transactions {
            let wtxid = tx.compute_wtxid();
            self.txdata.insert(wtxid, tx);
        }
    }

    /// Retrieves transactions by wtxid.
    pub fn get_txdata(&self, wtxids: &[Wtxid]) -> Vec<Transaction> {
        wtxids
            .iter()
            .filter_map(|wtxid| self.txdata.get(wtxid).cloned())
            .collect()
    }

    /// Returns wtxids that are not present in the mempool.
    pub fn verify(&self, wtxids: &[Wtxid]) -> Vec<Wtxid> {
        wtxids
            .iter()
            .filter(|&wtxid| !self.txdata.contains_key(wtxid))
            .copied()
            .collect()
    }

    /// Returns the current template's prev_hash.
    pub fn get_current_prev_hash(&self) -> Option<BlockHash> {
        self.current_prev_hash
    }

    /// Returns the current template's difficulty target (nbits).
    pub fn get_current_nbits(&self) -> Option<CompactTarget> {
        self.current_nbits
    }

    /// Returns the current template's minimum timestamp (min_ntime).
    pub fn get_current_min_ntime(&self) -> Option<u32> {
        self.current_min_ntime
    }
}
