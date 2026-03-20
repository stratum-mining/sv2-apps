use crate::template_distribution_protocol::BitcoinCoreSv2TDP;

use crate::template_distribution_protocol::error::BitcoinCoreSv2TDPError;
use stratum_core::{
    parsers_sv2::TemplateDistribution,
    template_distribution_sv2::{
        CoinbaseOutputConstraints, RequestTransactionData, RequestTransactionDataError,
        SubmitSolution,
    },
};
use tokio_util::sync::CancellationToken;

impl BitcoinCoreSv2TDP {
    pub(crate) async fn handle_coinbase_output_constraints(
        &mut self,
        coinbase_output_constraints: CoinbaseOutputConstraints,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        tracing::debug!("handle_coinbase_output_constraints() called");

        // break the loop in monitor_ipc_templates() and spawn a new one at the end of this function
        // that's because we no longer care about templates created under previous constraints
        tracing::debug!("Cancelling template_ipc_client_cancellation_token");
        self.template_ipc_client_cancellation_token.cancel();

        tracing::debug!("Creating new template IPC client with new constraints");
        let template_ipc_client = self
            .new_template_ipc_client(
                coinbase_output_constraints.coinbase_output_max_additional_size,
                coinbase_output_constraints.coinbase_output_max_additional_sigops,
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create new template IPC client: {:?}", e);
                e
            })?;

        {
            let mut current_template_ipc_client_guard =
                self.current_template_ipc_client.borrow_mut();
            *current_template_ipc_client_guard = Some(template_ipc_client);
        }
        tracing::debug!("Updated current_template_ipc_client");

        self.template_ipc_client_cancellation_token = CancellationToken::new();
        tracing::debug!("Created new template_ipc_client_cancellation_token");

        // Wait for the old monitor_ipc_templates task to finish before spawning a new one
        tracing::debug!("Waiting for current monitor_ipc_templates() task to finish");
        let handle = self.monitor_ipc_templates_handle.borrow_mut().take();
        #[allow(clippy::collapsible_if)]
        if let Some(handle) = handle {
            if let Err(e) = handle.await {
                tracing::error!("monitor_ipc_templates task panicked: {:?}", e);
                return Err(BitcoinCoreSv2TDPError::FailedToWaitForMonitorIpcTemplatesTask);
            }
        }

        tracing::debug!("Spawning new monitor_ipc_templates() task");
        self.monitor_ipc_templates();

        Ok(())
    }

    pub(crate) async fn handle_request_transaction_data(
        &self,
        request_transaction_data: RequestTransactionData,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        tracing::debug!(
            "handle_request_transaction_data() called for template_id: {}",
            request_transaction_data.template_id
        );

        let is_stale = {
            let stale_template_ids_guard = self.stale_template_ids.read().map_err(|e| {
                tracing::error!("Failed to acquire read lock on stale_template_ids: {:?}", e);
                BitcoinCoreSv2TDPError::FailedToSendRequestTransactionDataResponseMessage
            })?;
            stale_template_ids_guard.contains(&request_transaction_data.template_id)
        };
        if is_stale {
            tracing::debug!(
                "Template {} is stale, sending error response",
                request_transaction_data.template_id
            );
            let request_transaction_data_error = RequestTransactionDataError {
                template_id: request_transaction_data.template_id,
                error_code: "stale-template-id"
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
                tracing::error!(
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
                tracing::error!("Failed to acquire read lock on template_data: {:?}", e);
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
                    tracing::debug!(
                        "Template {} found, sending success response",
                        request_transaction_data.template_id
                    );

                    let request_transaction_data_success = match template_data
                        .get_request_transaction_data_success_message(self.thread_map.clone())
                        .await
                    {
                        Ok(request_transaction_data_success) => request_transaction_data_success,
                        Err(e) => {
                            tracing::error!("Failed to fetch template tx data: {:?}", e);
                            return Err(BitcoinCoreSv2TDPError::FailedToFetchTemplateTxData);
                        }
                    };
                    TemplateDistribution::RequestTransactionDataSuccess(
                        request_transaction_data_success,
                    )
                }
                None => {
                    tracing::debug!(
                        "Template {} not found, sending error response",
                        request_transaction_data.template_id
                    );
                    TemplateDistribution::RequestTransactionDataError(RequestTransactionDataError {
                        template_id: request_transaction_data.template_id,
                        error_code: "template-id-not-found"
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    })
                }
            }
        };

        if let Err(e) = self.outgoing_messages.send(response_message.clone()).await {
            tracing::error!("Failed to send message: {:?}", e);
            return Err(BitcoinCoreSv2TDPError::FailedToSendRequestTransactionDataResponseMessage);
        }

        Ok(())
    }

    pub(crate) async fn handle_submit_solution(
        &self,
        submit_solution: SubmitSolution<'static>,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        tracing::debug!(
            "handle_submit_solution() called for template_id: {}",
            submit_solution.template_id
        );
        let template_data = {
            let template_data_guard = self.template_data.read().map_err(|e| {
                tracing::error!("Failed to acquire read lock on template_data: {:?}", e);
                BitcoinCoreSv2TDPError::TemplateNotFound
            })?;

            let Some(template_data) = template_data_guard.get(&submit_solution.template_id) else {
                tracing::error!(
                    "Template data not found for template id: {}",
                    submit_solution.template_id
                );
                tracing::debug!(
                    "Available template IDs: {:?}",
                    template_data_guard.keys().collect::<Vec<_>>()
                );
                return Err(BitcoinCoreSv2TDPError::TemplateNotFound);
            };
            template_data.clone()
        };
        tracing::debug!("Found template data for solution submission");

        let solution_block_dir = self
            .unix_socket_path
            .parent()
            .expect("unix_socket_path must have a parent");

        tracing::debug!("Submitting solution to Bitcoin Core");
        match template_data
            .submit_solution(
                submit_solution,
                self.thread_ipc_client.clone(),
                self.thread_map.clone(),
                solution_block_dir,
            )
            .await
        {
            Ok(_) => {
                tracing::debug!("Solution submitted successfully");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to submit solution: {:?}", e);
                Err(BitcoinCoreSv2TDPError::FailedToSubmitSolution)
            }
        }
    }
}
