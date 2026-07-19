//! v32.x-specific JDP handlers.

use super::{BitcoinCoreSv2JDP, error::BitcoinCoreSv2JDPError};
use stratum_core::bitcoin::{Block, consensus::serialize};
use tracing::{debug, error, info, warn};

const MAX_SUBMIT_BLOCK_ATTEMPTS: usize = 3;
const SUBMIT_BLOCK_RETRY_BACKOFF_MS: u64 = 15;

impl BitcoinCoreSv2JDP {
    /// Submits a solved block to Bitcoin Core via `submitBlock`.
    pub(crate) async fn handle_push_solution(&self, block: Block) {
        let block_bytes: Vec<u8> = serialize(&block);
        debug!(
            block_bytes_len = block_bytes.len(),
            tx_count = block.txdata.len(),
            "Submitting solved block via submitBlock"
        );

        // a dedicated thread is used to submit blocks to Bitcoin Core
        // therefore retries should be extremely rare
        for attempt in 1..=MAX_SUBMIT_BLOCK_ATTEMPTS {
            let mut submit_block_request = self.mining_ipc_client.submit_block_request();

            match submit_block_request.get().get_context() {
                Ok(mut context) => context.set_thread(self.submit_block_thread_ipc_client.clone()),
                Err(e) => {
                    error!("Failed to set submitBlock request thread context: {e}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            }

            submit_block_request.get().set_block(&block_bytes);

            let submit_block_response = match submit_block_request.send().promise.await {
                Ok(response) => response,
                Err(e) => {
                    let err: BitcoinCoreSv2JDPError = e.into();
                    if err.is_thread_busy() && attempt < MAX_SUBMIT_BLOCK_ATTEMPTS {
                        warn!(
                            attempt,
                            max_attempts = MAX_SUBMIT_BLOCK_ATTEMPTS,
                            "Transient IPC contention during submitBlock (thread busy); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            SUBMIT_BLOCK_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }

                    error!("Failed to send submitBlock request: {err:?}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let submit_block_result = match submit_block_response.get() {
                Ok(result) => result,
                Err(e) => {
                    let err: BitcoinCoreSv2JDPError = e.into();
                    if err.is_thread_busy() && attempt < MAX_SUBMIT_BLOCK_ATTEMPTS {
                        warn!(
                            attempt,
                            max_attempts = MAX_SUBMIT_BLOCK_ATTEMPTS,
                            "Transient IPC contention while reading submitBlock response (thread busy); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            SUBMIT_BLOCK_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }

                    error!("Failed to get submitBlock result: {err:?}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let accepted = submit_block_result.get_result();
            let reason = submit_block_result.get_reason();
            let debug_msg = submit_block_result.get_debug();

            if accepted {
                info!(
                    reason = ?reason,
                    debug = ?debug_msg,
                    "Bitcoin Core accepted block via submitBlock"
                );
            } else {
                warn!(
                    reason = ?reason,
                    debug = ?debug_msg,
                    "Bitcoin Core rejected block via submitBlock"
                );
            }

            return;
        }
    }
}
