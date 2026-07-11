//! v32.x-specific background chain-tip monitor.

use super::BitcoinCoreSv2JDP;
use stratum_core::bitcoin::{BlockHash, hashes::Hash};
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

impl BitcoinCoreSv2JDP {
    /// Spawns a `spawn_local` task that issues `waitTipChanged` requests to Bitcoin Core and
    /// refreshes local chain-tip state on each detected tip transition. Returns the
    /// [`JoinHandle`] so the caller can await clean shutdown.
    pub fn monitor_and_update_chain_tip_state(&self) -> JoinHandle<()> {
        let self_clone = self.clone();

        tokio::task::spawn_local(async move {
            debug!("monitor_chain_tip_state() task started");
            debug!(
                "Creating dedicated blocking_thread_ipc_client for waitTipChanged requests"
            );
            let blocking_thread_ipc_client = match self_clone.new_thread_ipc_client().await {
                Ok(blocking_thread_ipc_client) => blocking_thread_ipc_client,
                Err(e) => {
                    error!(
                        "Failed to create blocking thread IPC client: {:?}",
                        e
                    );
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.cancellation_token.cancel();
                    return;
                }
            };

            let mut current_tip_hash = match self_clone.chain_tip_state.borrow().get_current_prev_hash() {
                Some(hash) => hash,
                None => {
                    if let Err(e) = self_clone.update_chain_tip_state(None).await {
                        error!("Failed to bootstrap chain-tip state for monitor: {:?}", e);
                        self_clone.cancellation_token.cancel();
                        return;
                    }

                    match self_clone.chain_tip_state.borrow().get_current_prev_hash() {
                        Some(hash) => hash,
                        None => {
                            error!("chain_tip_state prev_hash missing after bootstrap");
                            self_clone.cancellation_token.cancel();
                            return;
                        }
                    }
                }
            };

            debug!("monitor_chain_tip_state() entering main loop");

            loop {
                let mut wait_tip_changed_request =
                    self_clone.mining_ipc_client.wait_tip_changed_request();

                match wait_tip_changed_request.get().get_context() {
                    Ok(mut context) => {
                        context.set_thread(blocking_thread_ipc_client.clone())
                    }
                    Err(e) => {
                        error!("Failed to set thread: {}", e);
                        self_clone.cancellation_token.cancel();
                        break;
                    }
                }

                wait_tip_changed_request
                    .get()
                    .set_current_tip(current_tip_hash.as_byte_array());
                wait_tip_changed_request.get().set_timeout(3_000.0);

                tokio::select! {
                    _ = self_clone.cancellation_token.cancelled() => {
                        debug!("Interrupting waitTipChanged request");
                        if let Err(e) = self_clone.interrupt_wait_tip_changed_request().await {
                            error!(
                                "Failed to interrupt waitTipChanged request: {:?}",
                                e
                            );
                        }
                        warn!("Exiting chain-tip state loop");
                        debug!(
                            "monitor_chain_tip_state() exiting due to cancellation"
                        );
                        break;
                    }
                    wait_tip_changed_response = wait_tip_changed_request
                        .send()
                        .promise =>
                    {
                        match wait_tip_changed_response {
                            Ok(response) => {
                                let result = match response.get() {
                                    Ok(result) => result,
                                    Err(e) => {
                                        error!("Failed to get response: {}", e);
                                        warn!(
                                            "Terminating Sv2 Bitcoin Core IPC Connection"
                                        );
                                        self_clone.cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let changed_tip_ref = match result.get_result() {
                                    Ok(changed_tip_ref) => changed_tip_ref,
                                    Err(e) => {
                                        error!("Failed to get waitTipChanged result: {}", e);
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let new_tip_hash_bytes = match changed_tip_ref.get_hash() {
                                    Ok(hash) => hash,
                                    Err(e) => {
                                        error!("Failed to read changed tip hash: {}", e);
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let new_tip_hash = match BlockHash::from_slice(new_tip_hash_bytes) {
                                    Ok(hash) => hash,
                                    Err(e) => {
                                        error!("Failed to parse changed tip hash: {e}");
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.cancellation_token.cancel();
                                        break;
                                    }
                                };

                                if new_tip_hash == current_tip_hash {
                                    debug!("waitTipChanged returned unchanged tip (timeout/no-op)");
                                    continue;
                                }

                                if let Err(e) =
                                    self_clone.update_chain_tip_state(Some(new_tip_hash)).await
                                {
                                    if e.is_thread_busy() {
                                        warn!(
                                            error = ?e,
                                            "Transient IPC contention while refreshing chain-tip state (thread busy); retrying"
                                        );
                                        continue;
                                    }

                                    error!(
                                        "Failed to update chain-tip state: {:?}",
                                        e
                                    );
                                    self_clone.cancellation_token.cancel();
                                    break;
                                }

                                current_tip_hash = new_tip_hash;
                            }
                            Err(e) => {
                                let err: super::error::BitcoinCoreSv2JDPError =
                                    e.into();
                                if err.is_thread_busy() {
                                    warn!(
                                        error = ?err,
                                        "Transient IPC contention during waitTipChanged (thread busy); retrying"
                                    );
                                    continue;
                                }
                                debug!(
                                    "waitTipChanged request failed with error: {:?}",
                                    err
                                );
                                error!("Failed to get response: {:?}", err);
                                warn!(
                                    "Terminating Sv2 Bitcoin Core IPC Connection"
                                );
                                self_clone.cancellation_token.cancel();
                                break;
                            }
                        }
                    }
                }
            }
            debug!("monitor_chain_tip_state() task exiting");
        })
    }
}
