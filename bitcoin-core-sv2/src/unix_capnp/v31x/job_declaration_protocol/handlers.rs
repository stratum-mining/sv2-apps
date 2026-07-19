//! v31.x-specific JDP handlers.

use super::BitcoinCoreSv2JDP;
use stratum_core::bitcoin::Block;
use tracing::warn;

impl BitcoinCoreSv2JDP {
    /// Submits a mining solution to Bitcoin Core.
    ///
    /// Not yet implemented for v31.x IPC, which does not expose `submitBlock`.
    pub(crate) async fn handle_push_solution(&self, _block: Block) {
        warn!(
            "Ignoring PushSolution for v31.x backend: Bitcoin Core v31.x IPC does not expose submitBlock"
        );
    }
}
