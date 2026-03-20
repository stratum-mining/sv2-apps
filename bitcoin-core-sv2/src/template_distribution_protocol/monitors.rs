use crate::template_distribution_protocol::BitcoinCoreSv2TDP;

use std::collections::HashSet;
use stratum_core::parsers_sv2::TemplateDistribution;
use tracing::info;

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
            tracing::debug!("monitor_ipc_templates() task started");
            // a dedicated thread_ipc_client is used for waitNext requests
            // this is because waitNext requests are blocking, and we don't want to block the main
            // thread where other requests are handled
            //
            // as soon as this task is cancelled, the blocking_thread_ipc_client is dropped,
            // which cleans up the thread on the Bitcoin Core side
            tracing::debug!("Creating dedicated blocking_thread_ipc_client for waitNext requests");
            let blocking_thread_ipc_client = match self_clone.new_thread_ipc_client().await {
                Ok(blocking_thread_ipc_client) => blocking_thread_ipc_client,
                Err(e) => {
                    tracing::error!("Failed to create blocking thread IPC client: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.global_cancellation_token.cancel();
                    return;
                }
            };

            tracing::debug!("monitor_ipc_templates() entering main loop");
            loop {
                tracing::debug!("monitor_ipc_templates() loop iteration start");

                // Create a new request for each iteration
                let wait_next_request = match self_clone
                    .new_wait_next_request(blocking_thread_ipc_client.clone())
                    .await
                {
                    Ok(wait_next_request) => wait_next_request,
                    Err(e) => {
                        tracing::error!("Failed to create waitNext request: {:?}", e);
                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                };

                tokio::select! {
                    _ = self_clone.global_cancellation_token.cancelled() => {
                        tracing::debug!("Interrupting waitNext request");
                        self_clone.interrupt_wait_request().await.expect("Failed to interrupt waitNext request");
                        tracing::warn!("Exiting mempool change monitoring loop");
                        break;
                    }
                    _ = self_clone.template_ipc_client_cancellation_token.cancelled() => {
                        tracing::debug!("Interrupting waitNext request");
                        if let Err(e) = self_clone.interrupt_wait_request().await {
                            tracing::error!("Failed to interrupt waitNext request: {:?}", e);
                            tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                            self_clone.global_cancellation_token.cancel();
                            break;
                        }
                        tracing::warn!("Exiting mempool change monitoring loop");
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
                                        self_clone.global_cancellation_token.cancel();
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
                                                continue; // Go back to the start of the loop
                                            }
                                            _ => {
                                                tracing::error!("Failed to get new template IPC client: {}", e);
                                                tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                                self_clone.global_cancellation_token.cancel();
                                                break;
                                            }
                                        }
                                    }
                                };

                                {
                                    let mut current_template_ipc_client_guard = self_clone.current_template_ipc_client.borrow_mut();
                                    *current_template_ipc_client_guard = Some(new_template_ipc_client);
                                    tracing::debug!("Updated current_template_ipc_client with new template");
                                }

                                tracing::debug!("Fetching new template data...");
                                let new_template_data = match self_clone.fetch_template_data(blocking_thread_ipc_client.clone()).await {
                                    Ok(new_template_data) => new_template_data,
                                    Err(e) => {
                                        tracing::error!("Failed to fetch template data: {:?}", e);
                                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }
                                };

                                let new_prev_hash = new_template_data.get_prev_hash();
                                let current_prev_hash = match self_clone.current_prev_hash.borrow().clone() {
                                    Some(prev_hash) => prev_hash,
                                    None => {
                                        tracing::error!("current_prev_hash is not set");
                                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }
                                };

                                if new_prev_hash != current_prev_hash {
                                    info!("⛓️ Chain Tip changed! New prev_hash: {}", new_prev_hash);
                                    tracing::debug!("CHAIN TIP CHANGE DETECTED - old: {}, new: {}", current_prev_hash, new_prev_hash);
                                    self_clone.current_prev_hash.replace(Some(new_prev_hash));

                                    // save stale template ids, cleanup and save the new template data
                                    let stale_template_ids = {
                                        let mut template_data_guard = match self_clone.template_data.write() {
                                            Ok(guard) => guard,
                                            Err(e) => {
                                                tracing::error!("Failed to acquire write lock on template_data: {:?}", e);
                                                tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                                self_clone.global_cancellation_token.cancel();
                                                break;
                                            }
                                        };

                                        let stale_template_ids: HashSet<_> = template_data_guard.clone().into_keys().collect();

                                        // save the new template data while we still have the lock
                                        tracing::debug!("Saving new template data with template_id: {}", new_template_data.get_template_id());
                                        template_data_guard.insert(new_template_data.get_template_id(), new_template_data.clone());

                                        stale_template_ids
                                    };

                                    // process the stale template data after 10s
                                    self_clone.process_stale_template_data(stale_template_ids).await;

                                    // send the future NewTemplate message
                                    let future_template = match new_template_data.get_new_template_message(true) {
                                        Ok(future_template) => future_template,
                                        Err(e) => {
                                            tracing::error!("Failed to get future template message: {:?}", e);
                                            tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                            self_clone.global_cancellation_token.cancel();
                                            break;
                                        }
                                    };
                                    tracing::debug!("Sending NewTemplate (future=true) after chain tip change");

                                    if let Err(e) = self_clone.outgoing_messages.send(TemplateDistribution::NewTemplate(future_template.clone())).await {
                                        tracing::error!("Failed to send future NewTemplate message: {:?}", e);
                                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }

                                    // send the SetNewPrevHash message
                                    let set_new_prev_hash = new_template_data.get_set_new_prev_hash_message();
                                    tracing::debug!("Sending SetNewPrevHash after chain tip change");

                                    match self_clone.outgoing_messages.send(TemplateDistribution::SetNewPrevHash(set_new_prev_hash.clone())).await {
                                        Ok(_) => {
                                            tracing::debug!("Successfully sent SetNewPrevHash");
                                        },
                                        Err(e) => {
                                            tracing::error!("Failed to send SetNewPrevHash message: {:?}", e);
                                            tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                            self_clone.global_cancellation_token.cancel();
                                            break;
                                        }
                                    }
                                } else {
                                    // check if the minimum interval has been reached
                                    if let Some(last_sent_template_instant) = self_clone.last_sent_template_instant {
                                        let elapsed = last_sent_template_instant.elapsed().as_millis();
                                        let min_interval_millis = self_clone.min_interval as u128 * 1_000;

                                        // if the minimum interval has not been reached, sleep for the remaining time
                                        if elapsed < min_interval_millis {
                                            let sleep_duration = min_interval_millis - elapsed;
                                            // Safe cast: min_interval is u8 (max 255), so sleep_duration is at most 255,000 ms,
                                            // which fits comfortably in u64 (max: 18,446,744,073,709,551,615)
                                            tracing::debug!("Sleeping for {} milliseconds to reach the minimum interval", sleep_duration);
                                            tokio::time::sleep(std::time::Duration::from_millis(sleep_duration as u64)).await;
                                        }
                                    }

                                    self_clone.last_sent_template_instant = Some(std::time::Instant::now());

                                    info!("💹 Mempool fees increased! Sending NewTemplate message.");
                                    tracing::debug!("MEMPOOL FEE CHANGE DETECTED - sending non-future template");

                                    // send the non-future NewTemplate message
                                    let non_future_template = match new_template_data.get_new_template_message(false) {
                                        Ok(non_future_template) => non_future_template,
                                        Err(e) => {
                                            tracing::error!("Failed to get non-future template message: {:?}", e);
                                            tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                            self_clone.global_cancellation_token.cancel();
                                            break;
                                        }
                                    };
                                    tracing::debug!("Sending NewTemplate (future=false) after fee change");

                                    if let Err(e) = self_clone.outgoing_messages.send(TemplateDistribution::NewTemplate(non_future_template.clone())).await {
                                        tracing::error!("Failed to send future NewTemplate message: {:?}", e);
                                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                        self_clone.global_cancellation_token.cancel();
                                        break;
                                    }

                                    // save the new template data
                                    tracing::debug!("Saving template data for template_id: {}", new_template_data.get_template_id());
                                    let mut template_data_guard = match self_clone.template_data.write() {
                                        Ok(guard) => guard,
                                        Err(e) => {
                                            tracing::error!("Failed to acquire write lock on template_data: {:?}", e);
                                            tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                            self_clone.global_cancellation_token.cancel();
                                            break;
                                        }
                                    };
                                    template_data_guard.insert(new_template_data.get_template_id(), new_template_data.clone());

                                }
                            }
                            Err(e) => {
                                tracing::debug!("waitNext request failed with error: {}", e);
                                tracing::error!("Failed to get response: {}", e);
                                tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                self_clone.global_cancellation_token.cancel();
                                break;
                            }
                        }
                    }
                }
            }

            tracing::debug!("monitor_ipc_templates() task exiting");
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
            tracing::debug!("monitor_incoming_messages() task started");
            loop {
                tokio::select! {
                    _ = self_clone.global_cancellation_token.cancelled() => {
                        tracing::warn!("Exiting incoming messages loop");
                        tracing::debug!("monitor_incoming_messages() exiting due to cancellation");
                        break;
                    }
                    Ok(incoming_message) = self_clone.incoming_messages.recv() => {
                        tracing::info!("Received: {}", incoming_message);
                        tracing::debug!("monitor_incoming_messages() processing message");

                        match incoming_message {
                            TemplateDistribution::CoinbaseOutputConstraints(coinbase_output_constraints) => {
                                tracing::debug!("Received CoinbaseOutputConstraints - max_additional_size: {}, max_additional_sigops: {}",
                                    coinbase_output_constraints.coinbase_output_max_additional_size,
                                    coinbase_output_constraints.coinbase_output_max_additional_sigops);
                                if let Err(e) = self_clone.handle_coinbase_output_constraints(coinbase_output_constraints).await {
                                    tracing::error!("Failed to handle coinbase output constraints: {:?}", e);
                                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                    self_clone.global_cancellation_token.cancel();
                                    break;
                                }
                            }
                            TemplateDistribution::RequestTransactionData(request_transaction_data) => {
                                tracing::debug!("Received RequestTransactionData for template_id: {}", request_transaction_data.template_id);
                                if let Err(e) = self_clone.handle_request_transaction_data(request_transaction_data).await {
                                    tracing::error!("Failed to handle request transaction data: {:?}", e);
                                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                    self_clone.global_cancellation_token.cancel();
                                    break;
                                }
                            }
                            TemplateDistribution::SubmitSolution(submit_solution) => {
                                tracing::debug!("Received SubmitSolution for template_id: {}", submit_solution.template_id);
                                if let Err(e) = self_clone.handle_submit_solution(submit_solution).await {
                                    tracing::error!("Failed to handle submit solution: {:?}", e);
                                    // no need to activate the global cancellation token here
                                }
                            }
                            _ => {
                                tracing::error!("Received unexpected message: {}", incoming_message);
                                tracing::warn!("Ignoring message");
                                continue;
                            }
                        }
                    }
                }
            }
        });
    }
}
