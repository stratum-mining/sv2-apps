//! Local mempool mirror for Bitcoin Core v31.x Sv2 Job Declaration Protocol via capnp over UNIX
//! socket.

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
    current_bip34_height: Option<u32>,
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
        self.current_bip34_height = block.txdata.first().map(|coinbase| {
            coinbase
                .input
                .first()
                .and_then(|input| {
                    decode_bip34_height_from_coinbase_script_sig(input.script_sig.as_bytes())
                })
                // Fallback for non-canonical/missing BIP34 encoding in some templates.
                .unwrap_or_else(|| coinbase.lock_time.to_consensus_u32())
        });

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

    /// Returns the current template's BIP34 height decoded from coinbase scriptSig.
    pub fn get_current_bip34_height(&self) -> Option<u32> {
        self.current_bip34_height
    }
}

/// Decodes BIP34 height from the first push in coinbase scriptSig.
/// Returns None if scriptSig does not start with a canonical small push.
/// Shared by JDP components that need to compare declared vs current chain context.
pub(crate) fn decode_bip34_height_from_coinbase_script_sig(script_sig: &[u8]) -> Option<u32> {
    let first = *script_sig.first()?;

    // Support small-integer opcodes (OP_0, OP_1..OP_16) used by some templates.
    if first == 0x00 {
        return Some(0);
    }
    if (0x51..=0x60).contains(&first) {
        return Some((first - 0x50) as u32);
    }

    // Canonical small push form: first byte is push length (1..=4).
    let push_len = first as usize;
    if push_len == 0 || push_len > 4 || script_sig.len() < 1 + push_len {
        return None;
    }

    let mut height_bytes = [0u8; 4];
    height_bytes[..push_len].copy_from_slice(&script_sig[1..1 + push_len]);
    Some(u32::from_le_bytes(height_bytes))
}
