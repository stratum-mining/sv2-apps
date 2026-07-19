//! Chain-tip state tracking for Bitcoin Core v32.x Sv2 Job Declaration Protocol.

use stratum_core::bitcoin::{BlockHash, CompactTarget};

/// Tracks the current chain-tip parameters for v32.x JDP.
///
/// `next_nbits` stores the **next-block** expected `nBits` as returned by
/// `getmininginfo.next.bits`, not the current tip header's bits.  This is
/// important because at difficulty-adjustment boundaries the tip header's
/// `nBits` differs from the next block's required `nBits`.  `checkBlock`
/// enforces the next-block `nBits` even when `check_pow` is disabled, so
/// validation must source the value from the next-block context rather than
/// from the tip header in order to avoid spurious `bad-diffbits` rejections.
#[derive(Default)]
pub struct ChainTipState {
    current_prev_hash: Option<BlockHash>,
    next_nbits: Option<CompactTarget>,
    current_ntime: Option<u32>,
}

impl ChainTipState {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn set(&mut self, prev_hash: BlockHash, next_nbits: CompactTarget, ntime: u32) {
        self.current_prev_hash = Some(prev_hash);
        self.next_nbits = Some(next_nbits);
        self.current_ntime = Some(ntime);
    }

    pub fn get_current_prev_hash(&self) -> Option<BlockHash> {
        self.current_prev_hash
    }

    pub fn get_next_nbits(&self) -> Option<CompactTarget> {
        self.next_nbits
    }

    pub fn get_current_ntime(&self) -> Option<u32> {
        self.current_ntime
    }
}