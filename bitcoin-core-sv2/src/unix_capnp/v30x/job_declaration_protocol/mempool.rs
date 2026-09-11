//! Local mempool mirror for Bitcoin Core v30.x Sv2 Job Declaration Protocol via capnp over UNIX
//! socket.

use std::collections::HashMap;
use stratum_core::bitcoin::{Block, BlockHash, CompactTarget, Transaction, Weight, Wtxid};

/// Weight of transactions the JDP mempool mirror carries forward beyond Bitcoin Core's current
/// template: entries from earlier templates at the same chain tip, and transactions clients
/// supplied for declarations `checkBlock` accepted.
///
/// This is a memory bound, not a consensus one, and is sized in blocks only because that is the
/// unit the data comes in. Without it every accepted declaration could leave another block's worth
/// of transactions in the mirror until the chain tip moves. One block is the most a single
/// declaration can draw from the carry-over, so it is the smallest budget under which a client can
/// re-declare what it just had validated without supplying it again; the multiplier leaves room
/// for a few downstreams with distinct transaction sets and for the template's own churn between
/// refreshes, at a worst-case footprint of a few tens of megabytes. Raise it if a deployment with
/// many such downstreams sees needless resupply rounds.
const MAX_MIRROR_CARRY_OVER_WEIGHT: u64 = 4 * Weight::MAX_BLOCK.to_wu();

/// Why [`MempoolMirror::resolve_txdata`] could not hand back a transaction list.
#[derive(Debug)]
pub enum ResolveError {
    /// Declared wtxids that resolved to nothing, in declaration order.
    Missing(Vec<Wtxid>),
    /// The transactions resolved so far already outweigh the caller's budget.
    TooHeavy,
}

/// Local cache of mempool transactions and current template parameters.
///
/// Tracks transactions by wtxid and maintains the current prev_hash, nbits,
/// and min_ntime from the most recent block template.
///
/// The transactions are Bitcoin Core's current template plus a bounded carry-over of what Core
/// vouched for earlier at the same chain tip, whether in a previous template or by accepting a
/// declared block through `checkBlock`; see [`MempoolMirror::update`].
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
    /// Clears stale transactions if the prev_hash changes. Otherwise the template is
    /// authoritative and whatever else the mirror holds is carried forward only up to
    /// `MAX_MIRROR_CARRY_OVER_WEIGHT`, a memory bound explained on that constant, so neither
    /// mempool churn nor client-supplied transactions can grow the mirror without bound between
    /// chain tips.
    pub fn update(&mut self, block: &Block) {
        let prev_hash = block.header.prev_blockhash;
        if self.current_prev_hash != Some(prev_hash) {
            self.txdata.clear();
        }

        // Which entries survive past the budget is arbitrary: this is a bound on the mirror, not a
        // retention policy, and anything dropped that a declaration still needs is simply
        // requested from the client again.
        let template: HashMap<Wtxid, &Transaction> = block
            .txdata
            .iter()
            .skip(1) // the coinbase is never part of the mirror
            .map(|tx| (tx.compute_wtxid(), tx))
            .collect();
        let mut carried = 0;
        self.txdata.retain(|wtxid, tx| {
            template.contains_key(wtxid) || {
                carried += tx.weight().to_wu();
                carried <= MAX_MIRROR_CARRY_OVER_WEIGHT
            }
        });
        for (wtxid, tx) in template {
            self.txdata.entry(wtxid).or_insert_with(|| tx.clone());
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
    }

    /// Commits transactions into the mempool mirror.
    ///
    /// Only call this for transactions Bitcoin Core has already accepted as part of a valid
    /// block: unvalidated client-supplied transactions must stay staged outside the mirror. They
    /// are kept so a client need not supply them again for every declaration at the same chain
    /// tip, and count towards the carry-over budget the next [`MempoolMirror::update`] enforces.
    pub fn add_transactions(&mut self, transactions: Vec<Transaction>) {
        for tx in transactions {
            let wtxid = tx.compute_wtxid();
            self.txdata.insert(wtxid, tx);
        }
    }

    /// Resolves `wtxids` into transactions, preferring `staged` over the mirror's own txdata.
    ///
    /// `staged` holds client-supplied transactions that have not been validated yet, so they are
    /// deliberately kept out of the mirror until the caller commits them via
    /// [`MempoolMirror::add_transactions`]. A wtxid is the transaction's own hash, so both sources
    /// resolve it to the same bytes.
    ///
    /// Resolution stops as soon as what it has resolved weighs more than `weight_budget`, which is
    /// also what keeps a declaration from cloning an unbounded slice of the mirror: the caller
    /// only ever pays for a budget's worth of transactions before it can reject the declaration.
    ///
    /// Returns [`ResolveError::Missing`] with every wtxid that could not be resolved, preserving
    /// `wtxids` order.
    pub fn resolve_txdata(
        &self,
        wtxids: &[Wtxid],
        staged: &HashMap<Wtxid, Transaction>,
        weight_budget: u64,
    ) -> Result<Vec<Transaction>, ResolveError> {
        let mut txdata = Vec::with_capacity(wtxids.len());
        let mut missing = Vec::new();
        let mut weight = 0;

        for wtxid in wtxids {
            match staged.get(wtxid).or_else(|| self.txdata.get(wtxid)) {
                Some(tx) => {
                    weight += tx.weight().to_wu();
                    if weight > weight_budget {
                        return Err(ResolveError::TooHeavy);
                    }
                    txdata.push(tx.clone());
                }
                None => missing.push(*wtxid),
            }
        }

        if missing.is_empty() {
            Ok(txdata)
        } else {
            Err(ResolveError::Missing(missing))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_core::bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxMerkleNode, TxOut, Txid, Witness,
        absolute::LockTime,
        block::{Header, Version as BlockVersion},
        hashes::Hash,
        transaction::Version,
    };

    /// A transaction weighing just under a quarter of the carry-over budget: four fit, five do not.
    fn heavy_tx(seed: u8) -> Transaction {
        // Weight is four times the size for a transaction carrying no witness; the fields around
        // the script add a few dozen bytes, so leave them room under the quarter.
        let quarter_budget_bytes = (MAX_MIRROR_CARRY_OVER_WEIGHT / 4) as usize / 4;
        let script_sig_len = quarter_budget_bytes - 100;

        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([seed; 32]),
                    vout: 0,
                },
                script_sig: ScriptBuf::from_bytes(vec![seed; script_sig_len]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(0),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    /// A template at a fixed chain tip carrying `txdata` behind a bare coinbase.
    fn template(txdata: Vec<Transaction>) -> Block {
        let coinbase = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![],
        };

        Block {
            header: Header {
                version: BlockVersion::TWO,
                prev_blockhash: BlockHash::all_zeros(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: 0,
                bits: CompactTarget::from_consensus(0),
                nonce: 0,
            },
            txdata: std::iter::once(coinbase).chain(txdata).collect(),
        }
    }

    #[test]
    fn update_carries_at_most_the_budget_beyond_the_template() {
        let mut mirror = MempoolMirror::new();
        let kept = heavy_tx(0x01);
        let supplied = [
            heavy_tx(0x02),
            heavy_tx(0x03),
            heavy_tx(0x04),
            heavy_tx(0x05),
            heavy_tx(0x06),
        ];
        let supplied_wtxids: Vec<Wtxid> = supplied.iter().map(|tx| tx.compute_wtxid()).collect();

        mirror.update(&template(vec![kept.clone()]));
        mirror.add_transactions(supplied.to_vec());

        // Same tip, and the template no longer carries any of the supplied transactions.
        mirror.update(&template(vec![kept.clone()]));

        // This exercises the carry-over bound, not the resolve budget, so leave the latter open.
        let staged = HashMap::new();
        assert!(
            mirror
                .resolve_txdata(&[kept.compute_wtxid()], &staged, u64::MAX)
                .is_ok(),
            "the template's own transaction is never dropped"
        );
        match mirror.resolve_txdata(&supplied_wtxids, &staged, u64::MAX) {
            Err(ResolveError::Missing(missing)) => assert_eq!(
                missing.len(),
                1,
                "a budget's worth of the supplied transactions survives the refresh"
            ),
            other => panic!("expected one of five supplied transactions dropped, got {other:?}"),
        }
    }
}
