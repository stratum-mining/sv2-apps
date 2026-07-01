//! Background monitors for Bitcoin Core v32.x Sv2 Job Declaration Protocol via capnp over UNIX
//! socket.

use crate::unix_capnp::v32x::job_declaration_protocol::BitcoinCoreSv2JDP;
use bitcoin_capnp_types_v32::capnp;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

impl BitcoinCoreSv2JDP {
    /// Spawns a `spawn_local` task that issues `waitNext` requests to Bitcoin Core and
    /// refreshes the [`MempoolMirror`](super::mempool::MempoolMirror) whenever the template
    /// changes. Returns the [`JoinHandle`] so the caller can await clean shutdown.
    pub fn monitor_and_update_mempool_mirror(&self) -> JoinHandle<()> {
        let self_clone = self.clone();

        tokio::task::spawn_local(async move {
            debug!("monitor_mempool_mirror() task started");
            debug!("Creating dedicated blocking_thread_ipc_client for waitNext requests");
            let blocking_thread_ipc_client = match self_clone.new_thread_ipc_client().await {
                Ok(blocking_thread_ipc_client) => blocking_thread_ipc_client,
                Err(e) => {
                    error!("Failed to create blocking thread IPC client: {:?}", e);
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.cancellation_token.cancel();
                    return;
                }
            };
            debug!("monitor_mempool_mirror() entering main loop");

            loop {
                // Create a new waitNext request for each iteration
                let mut wait_next_request = self_clone
                    .current_template_ipc_client
                    .borrow_mut()
                    .wait_next_request();

                match wait_next_request.get().get_context() {
                    Ok(mut context) => context.set_thread(blocking_thread_ipc_client.clone()),
                    Err(e) => {
                        error!("Failed to set thread: {}", e);
                        self_clone.cancellation_token.cancel();
                        break;
                    }
                }

                let mut wait_next_request_options = match wait_next_request.get().get_options() {
                    Ok(options) => options,
                    Err(e) => {
                        error!("Failed to get waitNext request options: {}", e);
                        self_clone.cancellation_token.cancel();
                        break;
                    }
                };

                // Rebuild aggressively instead of waiting only for tip changes.
                // Bitcoin Core reevaluates fee growth on a 1s tick, and with
                // fee_threshold = 0 it returns any candidate whose total fees
                // are not lower than the current template. In steady state this
                // usually produces a new BlockTemplate about once per second.
                wait_next_request_options.set_fee_threshold(0);

                // Bound how long a single waitNext call can stay attached to
                // one BlockTemplate before the loop recreates it from the
                // latest current_template_ipc_client when Bitcoin Core does not
                // produce a returnable candidate. This is a fallback, not the
                // expected cadence of template updates.
                wait_next_request_options.set_timeout(3_000.0);

                tokio::select! {
                    _ = self_clone.cancellation_token.cancelled() => {
                        debug!("Interrupting waitNext request");
                        if let Err(e) = self_clone.interrupt_wait_request().await {
                            error!("Failed to interrupt waitNext request: {:?}", e);
                        }
                        warn!("Exiting mempool mirror loop");
                        debug!("monitor_mempool_mirror() exiting due to cancellation");
                        break;
                    }
                    wait_next_request_response = wait_next_request.send().promise => {
                        match wait_next_request_response {
                            Ok(response) => {
                                let result = match response.get() {
                                    Ok(result) => result,
                                    Err(e) => {
                                        error!("Failed to get response: {}", e);
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let new_template_ipc_client = match result.get_result() {
                                    Ok(new_template_ipc_client) => {
                                        debug!("waitNext returned new template IPC client");
                                        new_template_ipc_client
                                    },
                                    Err(e) => {
                                        match e.kind {
                                            capnp::ErrorKind::MessageContainsNullCapabilityPointer => {
                                                debug!("waitNext timed out (no mempool changes)");
                                                debug!("Continuing to next waitNext iteration");
                                                continue;
                                            }
                                            _ => {
                                                error!("Failed to get new template IPC client: {}", e);
                                                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
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
                                    debug!("Updated current_template_ipc_client with new template");
                                }

                                // update the mempool mirror
                                if let Err(e) = self_clone.update_mempool_mirror().await {
                                    if e.is_thread_busy() {
                                        warn!(
                                            error = ?e,
                                            "Transient IPC contention while updating mempool mirror (thread busy); retrying"
                                        );
                                        continue;
                                    }

                                    error!("Failed to update mempool mirror: {:?}", e);
                                    self_clone.cancellation_token.cancel();
                                    break;
                                }
                            }
                            Err(e) => {
                                let err: super::error::BitcoinCoreSv2JDPError = e.into();
                                if err.is_thread_busy() {
                                    warn!(
                                        error = ?err,
                                        "Transient IPC contention during waitNext (thread busy); retrying"
                                    );
                                    continue;
                                }
                                debug!("waitNext request failed with error: {:?}", err);
                                error!("Failed to get response: {:?}", err);
                                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                self_clone.cancellation_token.cancel();
                                break;
                            }
                        }
                    }
                }
            }
            debug!("monitor_mempool_mirror() task exiting");
        })
    }
}
