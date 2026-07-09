//! v31.x-specific JDP handlers.

use super::BitcoinCoreSv2JDP;
use stratum_core::bitcoin::Block;

impl BitcoinCoreSv2JDP {
    /// Submits a mining solution to Bitcoin Core.
    ///
    /// Not yet implemented - deliberately left as a stub for future work.
    pub(crate) async fn handle_push_solution(&self, _block: Block) {
        // todo
    }
}
