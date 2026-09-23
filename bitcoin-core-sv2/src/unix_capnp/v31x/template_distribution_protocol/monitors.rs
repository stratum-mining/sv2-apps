//! Background monitors for Bitcoin Core v31.x Sv2 Template Distribution Protocol via capnp over
//! UNIX socket.

use super::{BitcoinCoreSv2TDP, bitcoin_capnp_types::capnp};
use crate::unix_capnp::{MAX_MONEY, TEMPLATE_RETIREMENT_SWEEP_INTERVAL_SECS, WAIT_NEXT_TIMEOUT_MS};
use std::time::{Duration, Instant};
use stratum_core::parsers_sv2::TemplateDistributionOwned;
use tracing::{debug, error, info, warn};

impl BitcoinCoreSv2TDP {
    /// Spawns a new task to monitor the IPC templates
    ///
    /// This task is responsible for:
    /// - Creating a dedicated blocking_thread_ipc_client for waitNext requests
    /// - Entering a loop to handle waitNext requests
    /// - Handling the response from the waitNext request
    /// - Updating the current template data
    /// - Sending the NewTemplate message
    pub fn monitor_ipc_templates(&self) {
        let mut self_clone = self.clone();

        let handle = tokio::task::spawn_local(async move {
            debug!("monitor_ipc_templates() task started");
            // a dedicated thread_ipc_client is used for waitNext requests
            // this is because waitNext requests are blocking, and we don't want to block the main
            // thread where other requests are handled
            //
            // as soon as this task is cancelled, the blocking_thread_ipc_client is dropped,
            // which cleans up the thread on the Bitcoin Core side
            debug!("Creating dedicated blocking_thread_ipc_client for waitNext requests");
            // Stop waiting once cancelled (`None`).
            let Some(blocking_thread_ipc_client) = self_clone
                .global_cancellation_token
                .run_until_cancelled(self_clone.new_thread_ipc_client())
                .await
            else {
                debug!("monitor_ipc_templates() exiting due to cancellation");
                return;
            };
            let blocking_thread_ipc_client = match blocking_thread_ipc_client {
                Ok(blocking_thread_ipc_client) => blocking_thread_ipc_client,
                Err(e) => {
                    error!("Failed to create blocking thread IPC client: {:?}", e);
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.global_cancellation_token.cancel();
                    return;
                }
            };

            let mut template_ipc_client = match self_clone.current_template_ipc_client() {
                Ok(template_ipc_client) => template_ipc_client,
                Err(e) => {
                    error!("Failed to get current template IPC client: {:?}", e);
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.global_cancellation_token.cancel();
                    return;
                }
            };

            debug!("monitor_ipc_templates() entering main loop");
            loop {
                debug!("monitor_ipc_templates() loop iteration start");

                // Fee-driven template updates are throttled by suppressing fee-based `waitNext`
                // wakeups while the `min_interval` window is active: the `fee_threshold` is raised
                // to MAX_MONEY, so Bitcoin Core only returns early on a chain tip change (or when
                // the timeout expires).
                //
                // A `waitNext` request is always outstanding (the loop never sleeps), so chain tip
                // changes are always detected immediately.
                let (fee_threshold, timeout_ms) = match self_clone.last_sent_template_instant {
                    Some(last_sent_template_instant) => {
                        let elapsed_ms = last_sent_template_instant.elapsed().as_millis();
                        let min_interval_ms = self_clone.min_interval as u128 * 1_000;

                        if elapsed_ms < min_interval_ms {
                            // Safe cast: min_interval is u8 (max 255), so remaining_ms is at most
                            // 255,000 ms, which fits comfortably in f64
                            let remaining_ms = (min_interval_ms - elapsed_ms) as f64;
                            debug!(
                                "Throttling fee-based template updates for {} more milliseconds (waiting only for chain tip changes)",
                                remaining_ms
                            );
                            (MAX_MONEY, remaining_ms.min(WAIT_NEXT_TIMEOUT_MS))
                        } else {
                            (self_clone.fee_threshold as i64, WAIT_NEXT_TIMEOUT_MS)
                        }
                    }
                    None => (self_clone.fee_threshold as i64, WAIT_NEXT_TIMEOUT_MS),
                };

                // Create a new request for each iteration
                let wait_next_request = match self_clone
                    .new_wait_next_request(
                        &template_ipc_client,
                        blocking_thread_ipc_client.clone(),
                        fee_threshold,
                        timeout_ms,
                    )
                    .await
                {
                    Ok(wait_next_request) => wait_next_request,
                    Err(e) => {
                        error!("Failed to create waitNext request: {:?}", e);
                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                };

                tokio::select! {
                    _ = self_clone.global_cancellation_token.cancelled() => {
                        debug!("Interrupting waitNext request");
                        self_clone.interrupt_wait_request(&template_ipc_client).await;
                        warn!("Exiting mempool change monitoring loop");
                        break;
                    }
                    _ = self_clone.template_ipc_client_cancellation_token.cancelled() => {
                        debug!("Interrupting waitNext request");
                        self_clone.interrupt_wait_request(&template_ipc_client).await;
                        warn!("Exiting mempool change monitoring loop");
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
                                        self_clone.global_cancellation_token.cancel();
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
                                                continue; // Go back to the start of the loop
                                            }
                                            _ => {
                                                error!("Failed to get new template IPC client: {}", e);
                                                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                                self_clone.global_cancellation_token.cancel();
                                                break;
                                            }
                                        }
                                    }
                                };

                                debug!("Fetching new template data...");
                                // Stop waiting once cancelled (`None`).
                                let Some(new_template_data) = self_clone
                                    .global_cancellation_token
                                    .run_until_cancelled(self_clone.fetch_template_data(
                                        new_template_ipc_client.clone(),
                                        blocking_thread_ipc_client.clone(),
                                    ))
                                    .await
                                else {
                                    debug!("monitor_ipc_templates() exiting due to cancellation");
                                    break;
                                };
                                let new_template_data = match new_template_data {
                                    Ok(new_template_data) => new_template_data,
                                    Err(e) => {
                                        error!("Failed to fetch template data: {:?}", e);
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let new_prev_hash = new_template_data.get_prev_hash();
                                let current_prev_hash = match self_clone.current_prev_hash.borrow().clone() {
                                    Some(prev_hash) => prev_hash,
                                    None => {
                                        error!("current_prev_hash is not set");
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }
                                };

                                if new_prev_hash != current_prev_hash {
                                    info!("⛓️ Chain Tip changed! New prev_hash: {}", new_prev_hash);
                                    debug!("CHAIN TIP CHANGE DETECTED - old: {}, new: {}", current_prev_hash, new_prev_hash);

                                    if let Err(e) = self_clone.retire_all_templates() {
                                        error!("Failed to retire the previous chain tip's templates: {:?}", e);
                                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }

                                    match self_clone.publish_template(new_template_data, true, true, false).await {
                                        Ok(()) => {
                                            self_clone.set_current_template_ipc_client(new_template_ipc_client.clone());
                                            template_ipc_client = new_template_ipc_client;
                                        }
                                        Err(e) => {
                                            error!("Failed to publish chain-tip template: {:?}", e);
                                            warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                            self_clone.global_cancellation_token.cancel();
                                            break;
                                        }
                                    }
                                } else {
                                    // Fee-driven template updates are throttled upstream via the
                                    // waitNext fee_threshold (see the top of the loop), so by the
                                    // time a fee-update template is returned here, the min_interval
                                    // window has already expired.
                                    info!("💹 Mempool fees increased! Sending NewTemplate message.");
                                    debug!("MEMPOOL FEE CHANGE DETECTED - sending non-future template");

                                    match self_clone.publish_template(new_template_data, false, false, true).await {
                                        Ok(()) => {
                                            self_clone.set_current_template_ipc_client(new_template_ipc_client.clone());
                                            template_ipc_client = new_template_ipc_client;
                                        }
                                        Err(e) => {
                                            error!("Failed to publish fee-update template: {:?}", e);
                                            warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                            self_clone.global_cancellation_token.cancel();
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("waitNext request failed with error: {}", e);
                                error!("Failed to get response: {}", e);
                                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                self_clone.global_cancellation_token.cancel();
                                break;
                            }
                        }
                    }
                }
            }

            debug!("monitor_ipc_templates() task exiting");
        });

        // Store the handle so we can wait for this task to finish before spawning a new one
        // when handle_coinbase_output_constraints is called
        *self.monitor_ipc_templates_handle.borrow_mut() = Some(handle);
    }

    /// Spawns a new task to monitor the incoming messages
    ///
    /// This task is responsible for:
    /// - Entering a loop to listen for incoming messages
    /// - Routing incoming messages to the appropriate handler
    pub fn monitor_incoming_messages(&self) {
        let mut self_clone = self.clone();

        tokio::task::spawn_local(async move {
            debug!("monitor_incoming_messages() task started");
            loop {
                tokio::select! {
                    _ = self_clone.global_cancellation_token.cancelled() => {
                        warn!("Exiting incoming messages loop");
                        debug!("monitor_incoming_messages() exiting due to cancellation");
                        break;
                    }
                    Ok(incoming_message) = self_clone.incoming_messages.recv() => {
                        info!("Received: {}", incoming_message);
                        debug!("monitor_incoming_messages() processing message");

                        match incoming_message {
                            TemplateDistributionOwned::CoinbaseOutputConstraints(coinbase_output_constraints) => {
                                debug!("Received CoinbaseOutputConstraints - max_additional_size: {}, max_additional_sigops: {}",
                                    coinbase_output_constraints.coinbase_output_max_additional_size,
                                    coinbase_output_constraints.coinbase_output_max_additional_sigops);
                                if let Err(e) = self_clone.handle_coinbase_output_constraints(coinbase_output_constraints).await {
                                    error!("Failed to handle coinbase output constraints: {:?}", e);
                                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                    self_clone.global_cancellation_token.cancel();
                                    break;
                                }
                            }
                            TemplateDistributionOwned::RequestTransactionData(request_transaction_data) => {
                                debug!("Received RequestTransactionData for template_id: {}", request_transaction_data.template_id);
                                let handling = self_clone.handle_request_transaction_data(request_transaction_data);
                                // Stop waiting once cancelled (`None`).
                                let Some(handled) = self_clone
                                    .global_cancellation_token
                                    .run_until_cancelled(handling)
                                    .await
                                else {
                                    debug!("monitor_incoming_messages() exiting due to cancellation");
                                    break;
                                };
                                if let Err(e) = handled {
                                    error!("Failed to handle request transaction data: {:?}", e);
                                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                    self_clone.global_cancellation_token.cancel();
                                    break;
                                }
                            }
                            TemplateDistributionOwned::SubmitSolution(submit_solution) => {
                                debug!("Received SubmitSolution for template_id: {}", submit_solution.template_id);
                                let handling = self_clone.handle_submit_solution(submit_solution);
                                // Stop waiting once cancelled (`None`).
                                let Some(handled) = self_clone
                                    .global_cancellation_token
                                    .run_until_cancelled(handling)
                                    .await
                                else {
                                    debug!("monitor_incoming_messages() exiting due to cancellation");
                                    break;
                                };
                                if let Err(e) = handled {
                                    error!("Failed to handle submit solution: {:?}", e);
                                    // no need to activate the global cancellation token here
                                }
                            }
                            _ => {
                                error!("Received unexpected message: {}", incoming_message);
                                warn!("Ignoring message");
                                continue;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Spawns a new task to destroy retired templates
    ///
    /// This task is responsible for:
    /// - Creating a dedicated thread_ipc_client for destroy requests
    /// - Sweeping for the retired templates whose grace period has passed
    /// - Removing them from the template data and dropping their authorization
    /// - Destroying the Bitcoin Core capability each one holds
    pub(crate) fn monitor_template_retirement(&self) {
        let self_clone = self.clone();

        tokio::task::spawn_local(async move {
            debug!("monitor_template_retirement() task started");
            // one thread_ipc_client serves every destroy request, rather than one per retirement
            debug!("Creating dedicated thread_ipc_client for destroy requests");
            // Stop waiting once cancelled (`None`).
            let Some(thread_ipc_client) = self_clone
                .global_cancellation_token
                .run_until_cancelled(self_clone.new_thread_ipc_client())
                .await
            else {
                debug!("monitor_template_retirement() exiting due to cancellation");
                return;
            };
            let thread_ipc_client = match thread_ipc_client {
                Ok(thread_ipc_client) => thread_ipc_client,
                Err(e) => {
                    error!("Failed to create thread IPC client: {:?}", e);
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.global_cancellation_token.cancel();
                    return;
                }
            };

            'sweep: loop {
                tokio::select! {
                    _ = self_clone.global_cancellation_token.cancelled() => {
                        debug!("monitor_template_retirement() exiting due to cancellation");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(
                        TEMPLATE_RETIREMENT_SWEEP_INTERVAL_SECS,
                    )) => {}
                }

                // Taking a template out of the map is what ends its life: from here on a request
                // naming it is answered as an unknown template id. They are taken while the lock
                // is held and destroyed once it has been dropped, because destroying awaits.
                let due_templates = {
                    let mut template_data_guard = match self_clone.template_data.write() {
                        Ok(template_data_guard) => template_data_guard,
                        Err(e) => {
                            error!("Failed to acquire write lock on template_data: {:?}", e);
                            warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                            self_clone.global_cancellation_token.cancel();
                            break;
                        }
                    };

                    let now = Instant::now();
                    template_data_guard
                        .extract_if(|_, template_data| {
                            template_data
                                .get_retire_at()
                                .is_some_and(|retire_at| retire_at <= now)
                        })
                        .collect::<Vec<_>>()
                };

                for (template_id, template_data) in due_templates {
                    // Stop waiting once cancelled (`None`).
                    let Some(destroyed) = self_clone
                        .global_cancellation_token
                        .run_until_cancelled(
                            template_data.destroy_ipc_client(thread_ipc_client.clone()),
                        )
                        .await
                    else {
                        debug!("monitor_template_retirement() exiting due to cancellation");
                        break 'sweep;
                    };
                    if let Err(e) = destroyed {
                        error!("Failed to destroy template IPC client: {:?}", e);
                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        break 'sweep;
                    }

                    debug!("Retired template {}", template_id);
                }
            }

            debug!("monitor_template_retirement() task exiting");
        });
    }
}
