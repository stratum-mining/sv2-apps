//! Handlers for Bitcoin Core v31.x Sv2 Template Distribution Protocol via capnp over UNIX socket.

use super::{BitcoinCoreSv2TDP, error::BitcoinCoreSv2TDPError};
use stratum_core::{
    parsers_sv2::TemplateDistribution,
    template_distribution_sv2::{
        CoinbaseOutputConstraints, ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID,
        ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND, RequestTransactionData,
        RequestTransactionDataError, SubmitSolution,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

impl BitcoinCoreSv2TDP {
    pub(crate) async fn handle_coinbase_output_constraints(
        &mut self,
        coinbase_output_constraints: CoinbaseOutputConstraints,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        debug!("handle_coinbase_output_constraints() called");

        // Break the loop in monitor_ipc_templates() and spawn a new one after bootstrapping the
        // new template IPC client. We no longer care about templates created under previous
        // constraints for future template monitoring.
        debug!("Cancelling template_ipc_client_cancellation_token");
        self.template_ipc_client_cancellation_token.cancel();

        // Wait for the old monitor_ipc_templates task to finish before bootstrapping a new
        // template IPC client.
        //
        // This keeps template monitoring scoped to one coinbase-output constraint set at a time:
        // the old monitor interrupts its own in-flight waitNext request and exits before the
        // replacement client is published and monitored.
        debug!("Waiting for current monitor_ipc_templates() task to finish");
        let handle = self.monitor_ipc_templates_handle.borrow_mut().take();
        #[allow(clippy::collapsible_if)]
        if let Some(handle) = handle {
            if let Err(e) = handle.await {
                error!("monitor_ipc_templates task panicked: {:?}", e);
                return Err(BitcoinCoreSv2TDPError::FailedToWaitForMonitorIpcTemplatesTask);
            }
        }

        self.template_ipc_client_cancellation_token = CancellationToken::new();
        debug!("Created new template_ipc_client_cancellation_token");

        debug!("Bootstrapping new template IPC client with new constraints");
        self.bootstrap_template_ipc_client_from_coinbase_output_constraints(
            coinbase_output_constraints,
        )
        .await
        .map_err(|e| {
            error!("Failed to bootstrap new template IPC client: {:?}", e);
            e
        })?;

        debug!("Spawning new monitor_ipc_templates() task");
        self.monitor_ipc_templates();

        Ok(())
    }

    pub(crate) async fn handle_request_transaction_data(
        &self,
        request_transaction_data: RequestTransactionData,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        debug!(
            "handle_request_transaction_data() called for template_id: {}",
            request_transaction_data.template_id
        );

        let is_stale = {
            let stale_template_ids_guard = self.stale_template_ids.read().map_err(|e| {
                error!("Failed to acquire read lock on stale_template_ids: {:?}", e);
                BitcoinCoreSv2TDPError::FailedToSendRequestTransactionDataResponseMessage
            })?;
            stale_template_ids_guard.contains(&request_transaction_data.template_id)
        };
        if is_stale {
            debug!(
                "Template {} is stale, sending error response",
                request_transaction_data.template_id
            );
            let request_transaction_data_error = RequestTransactionDataError {
                template_id: request_transaction_data.template_id,
                error_code: ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };

            if let Err(e) = self
                .outgoing_messages
                .send(TemplateDistribution::RequestTransactionDataError(
                    request_transaction_data_error.clone(),
                ))
                .await
            {
                error!(
                    "Failed to send RequestTransactionDataError message: {:?}",
                    e
                );
                return Err(
                    BitcoinCoreSv2TDPError::FailedToSendRequestTransactionDataResponseMessage,
                );
            }

            return Ok(());
        }

        let template_data = {
            let template_data_guard = self.template_data.read().map_err(|e| {
                error!("Failed to acquire read lock on template_data: {:?}", e);
                BitcoinCoreSv2TDPError::FailedToSendRequestTransactionDataResponseMessage
            })?;

            // clone so we can drop the read lock and avoid holding it across the await
            template_data_guard
                .get(&request_transaction_data.template_id)
                .cloned()
        };

        let response_message = {
            match template_data {
                Some(template_data) => {
                    debug!(
                        "Template {} found, sending success response",
                        request_transaction_data.template_id
                    );

                    let request_transaction_data_success = match template_data
                        .get_request_transaction_data_success_message(self.thread_map.clone())
                        .await
                    {
                        Ok(request_transaction_data_success) => request_transaction_data_success,
                        Err(e) => {
                            error!("Failed to fetch template tx data: {:?}", e);
                            return Err(BitcoinCoreSv2TDPError::FailedToFetchTemplateTxData);
                        }
                    };
                    TemplateDistribution::RequestTransactionDataSuccess(
                        request_transaction_data_success,
                    )
                }
                None => {
                    debug!(
                        "Template {} not found, sending error response",
                        request_transaction_data.template_id
                    );
                    TemplateDistribution::RequestTransactionDataError(RequestTransactionDataError {
                        template_id: request_transaction_data.template_id,
                        error_code: ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    })
                }
            }
        };

        if let Err(e) = self.outgoing_messages.send(response_message.clone()).await {
            error!("Failed to send message: {:?}", e);
            return Err(BitcoinCoreSv2TDPError::FailedToSendRequestTransactionDataResponseMessage);
        }

        Ok(())
    }

    pub(crate) async fn handle_submit_solution(
        &self,
        submit_solution: SubmitSolution<'static>,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        debug!(
            "handle_submit_solution() called for template_id: {}",
            submit_solution.template_id
        );
        let template_data = {
            let template_data_guard = self.template_data.read().map_err(|e| {
                error!("Failed to acquire read lock on template_data: {:?}", e);
                BitcoinCoreSv2TDPError::TemplateNotFound
            })?;

            let Some(template_data) = template_data_guard.get(&submit_solution.template_id) else {
                error!(
                    "Template data not found for template id: {}",
                    submit_solution.template_id
                );
                debug!(
                    "Available template IDs: {:?}",
                    template_data_guard.keys().collect::<Vec<_>>()
                );
                return Err(BitcoinCoreSv2TDPError::TemplateNotFound);
            };
            template_data.clone()
        };
        debug!("Found template data for solution submission");

        let solution_block_dir = self
            .unix_socket_path
            .parent()
            .expect("unix_socket_path must have a parent");

        let solutions_dir = solution_block_dir.join("solutions");

        if !solutions_dir.exists() {
            std::fs::create_dir_all(&solutions_dir).map_err(|e| {
                error!("Failed to create solutions directory: {:?}", e);
                BitcoinCoreSv2TDPError::FailedToCreateSolutionDir
            })?;
        }

        debug!("Submitting solution to Bitcoin Core");
        match template_data
            .submit_solution(
                submit_solution,
                self.thread_ipc_client.clone(),
                self.thread_map.clone(),
                &solutions_dir,
            )
            .await
        {
            Ok(_) => {
                debug!("Solution submitted successfully");
                Ok(())
            }
            Err(e) => {
                error!("Failed to submit solution: {:?}", e);
                Err(BitcoinCoreSv2TDPError::FailedToSubmitSolution)
            }
        }
    }
}
