//! Chain-tip state tracking for Bitcoin Core v32.x Sv2 Job Declaration Protocol via capnp over
//! UNIX socket.

use stratum_core::bitcoin::{BlockHash, CompactTarget, block::Header};

/// The template parameters a declaration is validated against.
///
/// Taken from Bitcoin Core's current block template rather than from the chain tip's own header,
/// which matters for `nbits`: a template carries the difficulty the *next* block must meet, and at
/// a retarget boundary that differs from the tip's. `checkBlock` enforces the next-block value even
/// with proof of work checking disabled, so sourcing it from the tip would make every declaration
/// at such a boundary fail with `bad-diffbits`.
#[derive(Default)]
pub struct ChainTipState {
    current_prev_hash: Option<BlockHash>,
    current_nbits: Option<CompactTarget>,
    current_ntime: Option<u32>,
}

impl ChainTipState {
    /// Creates an empty state, before the first template has been fetched.
    pub fn new() -> Self {
        Default::default()
    }

    /// Adopts the parameters of `header`, the header of Bitcoin Core's current template.
    pub fn update(&mut self, header: &Header) {
        self.current_prev_hash = Some(header.prev_blockhash);
        self.current_nbits = Some(header.bits);
        self.current_ntime = Some(header.time);
    }

    pub fn get_current_prev_hash(&self) -> Option<BlockHash> {
        self.current_prev_hash
    }

    pub fn get_current_nbits(&self) -> Option<CompactTarget> {
        self.current_nbits
    }

    pub fn get_current_ntime(&self) -> Option<u32> {
        self.current_ntime
    }
}
