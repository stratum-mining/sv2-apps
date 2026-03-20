use crate::{
    error,
    error::JDSError,
    job_declarator::{job_validation::DeclareMiningJobResult, JobDeclarator},
};
use std::time::Instant;
use stratum_apps::{
    stratum_core::{
        bitcoin::{consensus, hashes::Hash, Amount, TxOut, Wtxid},
        handlers_sv2::HandleJobDeclarationMessagesFromClientAsync,
        job_declaration_sv2::{
            AllocateMiningJobToken, AllocateMiningJobTokenSuccess, DeclareMiningJob,
            DeclareMiningJobError, DeclareMiningJobSuccess, ProvideMissingTransactions,
            ProvideMissingTransactionsSuccess, PushSolution,
        },
        parsers_sv2::{JobDeclaration, Tlv},
    },
    utils::types::JdToken,
};
use tracing::info;

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleJobDeclarationMessagesFromClientAsync for JobDeclarator {
    type Error = JDSError<error::JobDeclarator>;

    fn get_negotiated_extensions_with_client(
        &self,
        client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        // Shutdown: client_id is always Some (set by handle_jdp_message); None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;
        // Disconnect: downstream may have been cleaned up between message dispatch and handling.
        let negotiated_extensions = self.downstream_clients.get(&client_id).ok_or_else(|| {
            JDSError::disconnect(error::JDSErrorKind::ClientNotFound(client_id), client_id)
        })?;
        Ok(negotiated_extensions
            .negotiated_extensions
            .super_safe_lock(|extensions| extensions.clone()))
    }

    async fn handle_allocate_mining_job_token(
        &mut self,
        client_id: Option<usize>,
        msg: AllocateMiningJobToken<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        let allocated_token = self.token_manager.allocate(client_id);

        let coinbase_tx_output = TxOut {
            value: Amount::from_sat(0), // spec says we must set the value to 0
            script_pubkey: self.coinbase_reward_script.script_pubkey(),
        };

        // spec says we must use a CompactSize encoded array, even if there's only one output
        let coinbase_tx_outputs: Vec<TxOut> = vec![coinbase_tx_output];
        let serialized_coinbase_tx_outputs = consensus::serialize(&coinbase_tx_outputs);

        // response message
        let allocate_mining_job_token_success = AllocateMiningJobTokenSuccess {
            request_id: msg.request_id,
            mining_job_token: allocated_token
                .to_le_bytes()
                .to_vec()
                .try_into()
                .expect("must always be valid B0_255"),
            coinbase_outputs: serialized_coinbase_tx_outputs
                .try_into()
                .expect("must always be valid B0_64K"),
        };

        let client_sender = self
            .job_declarator_io
            .downstream_client_senders
            .get(&client_id)
            .ok_or_else(|| {
                error::JDSError::disconnect(
                    error::JDSErrorKind::ClientSenderNotFound(client_id),
                    client_id,
                )
            })?
            .clone();
        client_sender
            .send((
                JobDeclaration::AllocateMiningJobTokenSuccess(allocate_mining_job_token_success),
                None,
            ))
            .await
            .map_err(|e| error::JDSError::disconnect(e, client_id))?;

        Ok(())
    }

    async fn handle_declare_mining_job(
        &mut self,
        client_id: Option<usize>,
        msg: DeclareMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        let client_sender = self
            .job_declarator_io
            .downstream_client_senders
            .get(&client_id)
            .ok_or_else(|| {
                error::JDSError::disconnect(
                    error::JDSErrorKind::ClientSenderNotFound(client_id),
                    client_id,
                )
            })?
            .clone();

        // can we parse `DeclareMiningJob.mining_job_token` into a `JdToken`?
        let token: JdToken = match msg.mining_job_token.inner_as_ref().try_into() {
            Ok(token_bytes) => u64::from_le_bytes(token_bytes),
            Err(_) => {
                // Send DeclareMiningJobError back to client
                let error_message = DeclareMiningJobError {
                    request_id: msg.request_id,
                    error_code: "invalid-mining-job-token"
                        .as_bytes()
                        .to_vec()
                        .try_into()
                        .expect("error code must be valid B0_255"),
                    error_details: Vec::new().try_into().unwrap(),
                };

                client_sender
                    .send((JobDeclaration::DeclareMiningJobError(error_message), None))
                    .await
                    .map_err(|e| error::JDSError::disconnect(e, client_id))?;

                return Ok(());
            }
        };

        // was `DeclareMiningJob.mining_job_token` previously allocated for this client?
        if !self.token_manager.is_allocated(token, client_id) {
            // Send DeclareMiningJobError back to client
            let error_message = DeclareMiningJobError {
                request_id: msg.request_id,
                error_code: "invalid-mining-job-token"
                    .as_bytes()
                    .to_vec()
                    .try_into()
                    .expect("error code must be valid B0_255"),
                error_details: Vec::new().try_into().unwrap(),
            };
            client_sender
                .send((JobDeclaration::DeclareMiningJobError(error_message), None))
                .await
                .map_err(|e| error::JDSError::disconnect(e, client_id))?;

            return Ok(());
        }

        // validate job
        let response = match self
            .job_validator
            .handle_declare_mining_job(msg.clone(), None)
            .await
        {
            // if job is valid, activate token and return DeclareMiningJobSuccess
            DeclareMiningJobResult::Success => {
                let activated_token = self.token_manager.activate(token, client_id);

                let declare_mining_job_success = DeclareMiningJobSuccess {
                    request_id: msg.request_id,
                    new_mining_job_token: activated_token
                        .to_le_bytes()
                        .to_vec()
                        .try_into()
                        .expect("must always be valid B0_255"),
                };
                JobDeclaration::DeclareMiningJobSuccess(declare_mining_job_success)
            }
            // if job is invalid, return DeclareMiningJobError
            DeclareMiningJobResult::Error(error) => {
                // make sure we clean up the allocated token
                self.token_manager.deallocate(token);

                let declare_mining_job_error = DeclareMiningJobError {
                    request_id: msg.request_id,
                    error_code: error
                        .as_bytes()
                        .to_vec()
                        .try_into()
                        .expect("error code string must be valid B0_255"),
                    error_details: Vec::new()
                        .try_into()
                        .expect("empty array must be valid B0_64K"),
                };
                JobDeclaration::DeclareMiningJobError(declare_mining_job_error)
            }
            // if missing transactions, add to pending declare mining jobs and return
            // ProvideMissingTransactions
            DeclareMiningJobResult::MissingTransactions(missing_wtxids) => {
                // Disconnect: downstream may have disconnected between dispatch and handling.
                let downstream = self.downstream_clients.get(&client_id).ok_or_else(|| {
                    JDSError::disconnect(error::JDSErrorKind::ClientNotFound(client_id), client_id)
                })?;
                downstream
                    .pending_declare_mining_jobs
                    .insert(msg.request_id, (Instant::now(), msg.as_static()));

                // Convert missing Wtxids to u16 indices by finding their positions in
                // DeclareMiningJob.wtxid_list
                let unknown_tx_position_list: Vec<u16> = missing_wtxids
                    .iter()
                    .filter_map(|missing_wtxid| {
                        msg.wtxid_list
                            .inner_as_ref()
                            .iter()
                            .position(|u256_bytes| {
                                let bytes: [u8; 32] =
                                    (*u256_bytes).try_into().expect("U256 is 32 bytes");
                                let wtxid = Wtxid::from_byte_array(bytes);
                                wtxid == *missing_wtxid
                            })
                            .map(|pos| pos as u16)
                    })
                    .collect();

                let provide_missing_transactions = ProvideMissingTransactions {
                    request_id: msg.request_id,
                    unknown_tx_position_list: unknown_tx_position_list.into(),
                };
                JobDeclaration::ProvideMissingTransactions(provide_missing_transactions)
            }
        };

        client_sender
            .send((response, None))
            .await
            .map_err(|e| error::JDSError::disconnect(e, client_id))?;

        Ok(())
    }

    async fn handle_provide_missing_transactions_success(
        &mut self,
        client_id: Option<usize>,
        msg: ProvideMissingTransactionsSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        let client_sender = self
            .job_declarator_io
            .downstream_client_senders
            .get(&client_id)
            .ok_or_else(|| {
                error::JDSError::disconnect(
                    error::JDSErrorKind::ClientSenderNotFound(client_id),
                    client_id,
                )
            })?
            .clone();

        // Scope downstream guard so it's dropped before later awaits.
        let maybe_pending_declare_mining_job = {
            // Disconnect: downstream may have disconnected between dispatch and handling.
            let downstream = self.downstream_clients.get(&client_id).ok_or_else(|| {
                JDSError::disconnect(error::JDSErrorKind::ClientNotFound(client_id), client_id)
            })?;
            downstream
                .pending_declare_mining_jobs
                .remove(&msg.request_id)
        };

        let pending_declare_mining_job = match maybe_pending_declare_mining_job {
            Some((_, (_, declare_mining_job))) => declare_mining_job,
            None => {
                return Err(error::JDSError::log(
                    error::JDSErrorKind::PendingDeclareMiningJobNotFound(msg.request_id),
                ));
            }
        };

        let pending_declare_mining_job_token: JdToken = u64::from_le_bytes(
            pending_declare_mining_job
                .mining_job_token
                .inner_as_ref()
                .try_into()
                .expect("already validated"),
        );

        let response = match self
            .job_validator
            .handle_declare_mining_job(pending_declare_mining_job.clone(), Some(msg.clone()))
            .await
        {
            // if job is valid, activate token and return DeclareMiningJobSuccess
            DeclareMiningJobResult::Success => {
                let activated_token = self
                    .token_manager
                    .activate(pending_declare_mining_job_token, client_id);

                let declare_mining_job_success = DeclareMiningJobSuccess {
                    request_id: msg.request_id,
                    new_mining_job_token: activated_token
                        .to_le_bytes()
                        .to_vec()
                        .try_into()
                        .expect("must always be valid B0_255"),
                };
                JobDeclaration::DeclareMiningJobSuccess(declare_mining_job_success)
            }
            // if job is invalid, return DeclareMiningJobError
            DeclareMiningJobResult::Error(error_code) => {
                // make sure we clean up the allocated token
                self.token_manager
                    .deallocate(pending_declare_mining_job_token);

                let declare_mining_job_error = DeclareMiningJobError {
                    request_id: msg.request_id,
                    error_code: error_code
                        .as_bytes()
                        .to_vec()
                        .try_into()
                        .expect("error code string must be valid B0_255"),
                    error_details: Vec::new()
                        .try_into()
                        .expect("empty array must be valid B0_64K"),
                };
                JobDeclaration::DeclareMiningJobError(declare_mining_job_error)
            }
            // if we're still missing transactions, just reject this DeclareMiningJob
            DeclareMiningJobResult::MissingTransactions(_) => {
                // make sure we clean up the allocated token
                self.token_manager
                    .deallocate(pending_declare_mining_job_token);

                let declare_mining_job_error = DeclareMiningJobError {
                    request_id: msg.request_id,
                    error_code: "missing-txs"
                        .as_bytes()
                        .to_vec()
                        .try_into()
                        .expect("error code string must be valid B0_255"),
                    error_details: Vec::new()
                        .try_into()
                        .expect("empty array must be valid B0_64K"),
                };
                JobDeclaration::DeclareMiningJobError(declare_mining_job_error)
            }
        };

        client_sender
            .send((response, None))
            .await
            .map_err(|e| error::JDSError::disconnect(e, client_id))?;

        Ok(())
    }

    async fn handle_push_solution(
        &mut self,
        _client_id: Option<usize>,
        msg: PushSolution<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        self.job_validator.handle_push_solution(msg).await;

        Ok(())
    }
}
