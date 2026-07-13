//! Cache of client-provided transactions for the Bitcoin Core v32.x Sv2 Job Declaration
//! Protocol.
//!
//! Transactions supplied through `ProvideMissingTransactions.Success` are typically ones the
//! JDS node will never learn over P2P (prioritized or out-of-band transactions that don't
//! meet its mempool policy). Without a cache, every downstream declaring a template with such
//! a transaction — and every retry — pays its own `ProvideMissingTransactions` round trip.
//!
//! Unlike the mempool mirror of the v30.x/v31.x backends, this cache involves no background
//! synchronization and is never the source of truth: Bitcoin Core's `unknownTxPos` decides
//! what is missing, and the cache is only consulted to supply those transactions without
//! asking the client again. Entries are keyed by wtxid, which commits to the full serialized
//! transaction, so an entry inserted by one downstream cannot misrepresent a transaction
//! requested by another.
//!
//! The cache is bounded by serialized size with least-recently-used eviction. Confirmed
//! transactions stop appearing in declared templates, stop being looked up, and age out on
//! their own. A cache miss is never an error; it merely costs the round trip that would have
//! happened anyway.
//!
//! This cache becomes unnecessary if Bitcoin Core grows a node-side store for
//! `TxCollection`-provided transactions (see the discussion in
//! https://github.com/bitcoin/bitcoin/pull/35671).

use std::collections::HashMap;
use stratum_core::bitcoin::{Transaction, Wtxid, consensus::serialize};
use tracing::{debug, warn};

/// Default cache budget. Generous compared to the worst case of one full block (~4 MB) of
/// exotic transactions, while remaining far below what the retired mempool mirror held.
pub const DEFAULT_PROVIDED_TX_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

struct CacheEntry {
    tx: Transaction,
    size: usize,
    last_used: u64,
}

/// Bounded, least-recently-used cache of client-provided transactions keyed by wtxid.
pub struct ProvidedTxCache {
    max_bytes: usize,
    total_bytes: usize,
    tick: u64,
    entries: HashMap<Wtxid, CacheEntry>,
}

impl ProvidedTxCache {
    /// Creates an empty cache holding at most `max_bytes` of serialized transactions.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Inserts a transaction, evicting least-recently-used entries if the budget is
    /// exceeded. A transaction larger than the whole budget is ignored.
    pub fn insert(&mut self, tx: Transaction) {
        let wtxid = tx.compute_wtxid();
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(&wtxid) {
            entry.last_used = tick;
            return;
        }

        let size = serialize(&tx).len();
        if size > self.max_bytes {
            warn!(
                %wtxid,
                size,
                max_bytes = self.max_bytes,
                "Ignoring provided transaction larger than the whole cache budget"
            );
            return;
        }

        self.total_bytes += size;
        self.entries.insert(
            wtxid,
            CacheEntry {
                tx,
                size,
                last_used: tick,
            },
        );
        self.evict_to_fit();
    }

    /// Returns a copy of the transaction with the given wtxid, marking it recently used.
    pub fn get(&mut self, wtxid: &Wtxid) -> Option<Transaction> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(wtxid)?;
        entry.last_used = tick;
        Some(entry.tx.clone())
    }

    fn evict_to_fit(&mut self) {
        while self.total_bytes > self.max_bytes {
            let Some(oldest_wtxid) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(wtxid, _)| *wtxid)
            else {
                return;
            };
            if let Some(entry) = self.entries.remove(&oldest_wtxid) {
                self.total_bytes -= entry.size;
                debug!(wtxid = %oldest_wtxid, size = entry.size, "Evicted provided transaction from cache");
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn contains(&self, wtxid: &Wtxid) -> bool {
        self.entries.contains_key(wtxid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_core::bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness, absolute::LockTime,
        transaction::Version,
    };

    /// Builds a distinct dummy transaction; `script_len` pads the output script to control
    /// the serialized size.
    fn dummy_tx(marker: u32, script_len: usize) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::from_consensus(marker),
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(0),
                script_pubkey: ScriptBuf::from_bytes(vec![0x6a; script_len]),
            }],
        }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut cache = ProvidedTxCache::new(1024);
        let tx = dummy_tx(1, 10);
        let wtxid = tx.compute_wtxid();

        assert!(cache.get(&wtxid).is_none());
        cache.insert(tx.clone());
        assert_eq!(cache.get(&wtxid), Some(tx));
    }

    #[test]
    fn duplicate_insert_does_not_grow_cache() {
        let mut cache = ProvidedTxCache::new(1024);
        let tx = dummy_tx(1, 10);
        cache.insert(tx.clone());
        let bytes_after_first = cache.total_bytes;
        cache.insert(tx);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_bytes, bytes_after_first);
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let tx_size = serialize(&dummy_tx(0, 100)).len();
        // Room for exactly two transactions.
        let mut cache = ProvidedTxCache::new(2 * tx_size);

        let tx_a = dummy_tx(1, 100);
        let tx_b = dummy_tx(2, 100);
        let tx_c = dummy_tx(3, 100);
        let wtxid_a = tx_a.compute_wtxid();
        let wtxid_b = tx_b.compute_wtxid();
        let wtxid_c = tx_c.compute_wtxid();

        cache.insert(tx_a);
        cache.insert(tx_b);
        // Touch A so B becomes the least recently used entry.
        assert!(cache.get(&wtxid_a).is_some());
        cache.insert(tx_c);

        assert!(cache.contains(&wtxid_a));
        assert!(!cache.contains(&wtxid_b));
        assert!(cache.contains(&wtxid_c));
        assert!(cache.total_bytes <= 2 * tx_size);
    }

    #[test]
    fn ignores_transaction_larger_than_budget() {
        let mut cache = ProvidedTxCache::new(64);
        let tx = dummy_tx(1, 1000);
        let wtxid = tx.compute_wtxid();
        cache.insert(tx);
        assert!(!cache.contains(&wtxid));
        assert_eq!(cache.total_bytes, 0);
    }
}
