//! Background monitor tasks for the Bitcoin Core IPC connection.

use crate::job_declaration_protocol::BitcoinCoreSv2JDP;
use tokio::task::JoinHandle;

impl BitcoinCoreSv2JDP {
    /// Spawns a `spawn_local` task that issues `waitNext` requests to Bitcoin Core and
    /// refreshes the [`MempoolMirror`](super::mempool::MempoolMirror) whenever the template
    /// changes. Returns the [`JoinHandle`] so the caller can await clean shutdown.
    pub fn monitor_and_update_mempool_mirror(&self) -> JoinHandle<()> {
        let self_clone = self.clone();

        tokio::task::spawn_local(async move {
            tracing::debug!("monitor_mempool_mirror() task started");
            tracing::debug!("monitor_mempool_mirror() entering main loop");

            loop {
                // Create a new waitNext request for each iteration
                let mut wait_next_request = self_clone
                    .current_template_ipc_client
                    .borrow_mut()
                    .wait_next_request();

                match wait_next_request.get().get_context() {
                    Ok(mut context) => context.set_thread(self_clone.thread_ipc_client.clone()),
                    Err(e) => {
                        tracing::error!("Failed to set thread: {}", e);
                        self_clone.cancellation_token.cancel();
                        break;
                    }
                }

                let mut wait_next_request_options = match wait_next_request.get().get_options() {
                    Ok(options) => options,
                    Err(e) => {
                        tracing::error!("Failed to get waitNext request options: {}", e);
                        self_clone.cancellation_token.cancel();
                        break;
                    }
                };

                // 0 sat fee threshold (accept all mempool transactions)
                wait_next_request_options.set_fee_threshold(0);

                // 10 seconds timeout for waitNext requests
                // please note that this is NOT how often we expect to get new templates
                // it's just the max time we'll wait for the current waitNext request to complete
                wait_next_request_options.set_timeout(10_000.0);

                tokio::select! {
                    _ = self_clone.cancellation_token.cancelled() => {
                        tracing::debug!("Interrupting waitNext request");
                        if let Err(e) = self_clone.interrupt_wait_request().await {
                            tracing::error!("Failed to interrupt waitNext request: {:?}", e);
                        }
                        tracing::warn!("Exiting mempool mirror loop");
                        tracing::debug!("monitor_mempool_mirror() exiting due to cancellation");
                        break;
                    }
                    wait_next_request_response = wait_next_request.send().promise => {
                        match wait_next_request_response {
                            Ok(response) => {
                                let result = match response.get() {
                                    Ok(result) => result,
                                    Err(e) => {
                                        tracing::error!("Failed to get response: {}", e);
                                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let new_template_ipc_client = match result.get_result() {
                                    Ok(new_template_ipc_client) => {
                                        tracing::debug!("waitNext returned new template IPC client");
                                        new_template_ipc_client
                                    },
                                    Err(e) => {
                                        match e.kind {
                                            capnp::ErrorKind::MessageContainsNullCapabilityPointer => {
                                                tracing::debug!("waitNext timed out (no mempool changes)");
                                                tracing::debug!("Continuing to next waitNext iteration");
                                                continue;
                                            }
                                            _ => {
                                                tracing::error!("Failed to get new template IPC client: {}", e);
                                                tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                                self_clone.cancellation_token.cancel();
                                                break;
                                            }
                                        }
                                    }
                                };

                                // update the current template IPC client
                                {
                                    let mut current_template_ipc_client_guard = self_clone.current_template_ipc_client.borrow_mut();
                                    *current_template_ipc_client_guard = new_template_ipc_client;
                                    tracing::debug!("Updated current_template_ipc_client with new template");
                                }

                                // update the mempool mirror
                                if let Err(e) = self_clone.update_mempool_mirror().await {
                                    tracing::error!("Failed to update mempool mirror: {:?}", e);
                                    self_clone.cancellation_token.cancel();
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("waitNext request failed with error: {}", e);
                                tracing::error!("Failed to get response: {}", e);
                                tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                self_clone.cancellation_token.cancel();
                                break;
                            }
                        }
                    }
                }
            }
            tracing::debug!("monitor_mempool_mirror() task exiting");
        })
    }
}
