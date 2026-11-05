use crate::{
    error,
    error::JDSError,
    job_declarator::{
        JobDeclarator, downstream::PendingDeclareMiningJob, job_validation::DeclareMiningJobResult,
    },
};
use stratum_apps::{
    stratum_core::{
        bitcoin::{Amount, TxOut, consensus},
        handlers_sv2::HandleJobDeclarationMessagesFromClientOwnedAsync,
        job_declaration_sv2::{
            AllocateMiningJobTokenOwned, AllocateMiningJobTokenSuccessOwned,
            DeclareMiningJobErrorOwned, DeclareMiningJobOwned, DeclareMiningJobSuccessOwned,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
            ERROR_CODE_DECLARE_MINING_JOB_MISSING_TXS, ProvideMissingTransactionsOwned,
            ProvideMissingTransactionsSuccessOwned, PushSolutionOwned,
        },
        parsers_sv2::{JobDeclarationOwned, Tlv},
    },
    utils::types::JdToken,
};
use tracing::{info, warn};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleJobDeclarationMessagesFromClientOwnedAsync for JobDeclarator {
    type Error = JDSError<error::JobDeclarator>;

    fn get_negotiated_extensions_with_client(
        &self,
        client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        // Shutdown: client_id is always Some (set by handle_jdp_message); None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;
        // Disconnect: downstream may have been cleaned up between message dispatch and handling.
        let Some(negotiated_extensions) = self.downstream_clients.with(&client_id, |downstream| {
            downstream
                .negotiated_extensions
                .get()
                .map_err(JDSError::shutdown)
        }) else {
            return Err(JDSError::disconnect(
                error::JDSErrorKind::ClientNotFound(client_id),
                client_id,
            ));
        };
        negotiated_extensions
    }

    async fn handle_allocate_mining_job_token(
        &mut self,
        client_id: Option<usize>,
        msg: AllocateMiningJobTokenOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        let user_identity = msg.user_identifier.as_utf8_or_hex();

        let allocated_token = self.token_manager.allocate(client_id, user_identity);

        let coinbase_tx_output = TxOut {
            value: Amount::from_sat(0), // spec says we must set the value to 0
            script_pubkey: self.coinbase_reward_script.script_pubkey(),
        };

        // spec says we must use a CompactSize encoded array, even if there's only one output
        let coinbase_tx_outputs: Vec<TxOut> = vec![coinbase_tx_output];
        let serialized_coinbase_tx_outputs = consensus::serialize(&coinbase_tx_outputs);

        // response message
        let allocate_mining_job_token_success = AllocateMiningJobTokenSuccessOwned {
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
            .with(&client_id, |sender| sender.clone())
            .ok_or_else(|| {
                error::JDSError::disconnect(
                    error::JDSErrorKind::ClientSenderNotFound(client_id),
                    client_id,
                )
            })?;
        client_sender
            .send((
                JobDeclarationOwned::AllocateMiningJobTokenSuccess(
                    allocate_mining_job_token_success,
                ),
                None,
            ))
            .await
            .map_err(|e| error::JDSError::disconnect(e, client_id))?;

        Ok(())
    }

    async fn handle_declare_mining_job(
        &mut self,
        client_id: Option<usize>,
        msg: DeclareMiningJobOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        let client_sender = self
            .job_declarator_io
            .downstream_client_senders
            .with(&client_id, |sender| sender.clone())
            .ok_or_else(|| {
                error::JDSError::disconnect(
                    error::JDSErrorKind::ClientSenderNotFound(client_id),
                    client_id,
                )
            })?;

        // can we parse `DeclareMiningJob.mining_job_token` into a `JdToken`?
        let token: JdToken = match msg.mining_job_token.try_as_array::<8>() {
            Ok(token_bytes) => u64::from_le_bytes(token_bytes),
            Err(_) => {
                // Send DeclareMiningJobError back to client
                let error_message = DeclareMiningJobErrorOwned {
                    request_id: msg.request_id,
                    error_code: ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN
                        .as_bytes()
                        .to_vec()
                        .try_into()
                        .expect("error code must be valid B0_255"),
                    error_details: Vec::new().try_into().map_err(error::JDSError::shutdown)?,
                };

                client_sender
                    .send((
                        JobDeclarationOwned::DeclareMiningJobError(error_message),
                        None,
                    ))
                    .await
                    .map_err(|e| error::JDSError::disconnect(e, client_id))?;

                return Ok(());
            }
        };

        // was `DeclareMiningJob.mining_job_token` previously allocated for this client?
        if !self.token_manager.is_allocated(token, client_id) {
            // Send DeclareMiningJobError back to client
            let error_message = DeclareMiningJobErrorOwned {
                request_id: msg.request_id,
                error_code: ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN
                    .as_bytes()
                    .to_vec()
                    .try_into()
                    .expect("error code must be valid B0_255"),
                error_details: Vec::new().try_into().map_err(error::JDSError::shutdown)?,
            };
            client_sender
                .send((
                    JobDeclarationOwned::DeclareMiningJobError(error_message),
                    None,
                ))
                .await
                .map_err(|e| error::JDSError::disconnect(e, client_id))?;

            return Ok(());
        }

        // validate job
        let response = match self
            .job_validator
            .handle_declare_mining_job(client_id, msg.clone(), None)
            .await
        {
            // if job is valid, activate token and return DeclareMiningJobSuccess
            DeclareMiningJobResult::Success => {
                match self.token_manager.activate(token, client_id) {
                    Some(activated_token) => {
                        let declare_mining_job_success = DeclareMiningJobSuccessOwned {
                            request_id: msg.request_id,
                            new_mining_job_token: activated_token
                                .to_le_bytes()
                                .to_vec()
                                .try_into()
                                .expect("must always be valid B0_255"),
                        };
                        JobDeclarationOwned::DeclareMiningJobSuccess(declare_mining_job_success)
                    }
                    None => {
                        let declare_mining_job_error = DeclareMiningJobErrorOwned {
                            request_id: msg.request_id,
                            error_code: ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN
                                .as_bytes()
                                .to_vec()
                                .try_into()
                                .expect("error code string must be valid B0_255"),
                            error_details: Vec::new()
                                .try_into()
                                .expect("empty array must be valid B0_64K"),
                        };
                        JobDeclarationOwned::DeclareMiningJobError(declare_mining_job_error)
                    }
                }
            }
            // if job is invalid, return DeclareMiningJobError
            DeclareMiningJobResult::Error(error) => {
                // make sure we clean up the allocated token
                self.token_manager.deallocate(token);

                let declare_mining_job_error = DeclareMiningJobErrorOwned {
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
                JobDeclarationOwned::DeclareMiningJobError(declare_mining_job_error)
            }
            // if missing transactions, add to pending declare mining jobs and return
            // ProvideMissingTransactions
            DeclareMiningJobResult::MissingTransactions(missing_wtxids) => {
                let (pending, unknown_tx_position_list) =
                    PendingDeclareMiningJob::new(msg.clone(), &missing_wtxids);

                // Disconnect: downstream may have disconnected between dispatch and handling.
                self.downstream_clients
                    .with(&client_id, |downstream| {
                        downstream
                            .pending_declare_mining_jobs
                            .insert(msg.request_id, pending);
                    })
                    .ok_or_else(|| {
                        JDSError::disconnect(
                            error::JDSErrorKind::ClientNotFound(client_id),
                            client_id,
                        )
                    })?;

                let provide_missing_transactions = ProvideMissingTransactionsOwned {
                    request_id: msg.request_id,
                    unknown_tx_position_list: unknown_tx_position_list
                        .try_into()
                        .map_err(error::JDSError::shutdown)?,
                };
                JobDeclarationOwned::ProvideMissingTransactions(provide_missing_transactions)
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
        msg: ProvideMissingTransactionsSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        let client_sender = self
            .job_declarator_io
            .downstream_client_senders
            .with(&client_id, |sender| sender.clone())
            .ok_or_else(|| {
                error::JDSError::disconnect(
                    error::JDSErrorKind::ClientSenderNotFound(client_id),
                    client_id,
                )
            })?;

        // Disconnect: downstream may have disconnected between dispatch and handling.
        let maybe_pending_declare_mining_job = self
            .downstream_clients
            .with(&client_id, |downstream| {
                downstream
                    .pending_declare_mining_jobs
                    .remove(&msg.request_id)
            })
            .ok_or_else(|| {
                JDSError::disconnect(error::JDSErrorKind::ClientNotFound(client_id), client_id)
            })?;

        let mut pending = match maybe_pending_declare_mining_job {
            Some((_, pending)) => pending,
            None => {
                return Err(error::JDSError::log(
                    error::JDSErrorKind::PendingDeclareMiningJobNotFound(msg.request_id),
                ));
            }
        };

        let pending_declare_mining_job_token: JdToken = u64::from_le_bytes(
            pending
                .declare_mining_job
                .mining_job_token
                .try_as_array::<8>()
                .expect("already validated"),
        );

        // The validator gets everything the client has supplied for this declaration, this
        // answer included, since a round only asks for what the node did not hold on the round
        // before. An answer with more transactions than the declaration lists, or more bytes
        // than a block holds, is rejected here, before it is kept.
        let transaction_count = msg.transaction_list.len();
        if !pending.supply(msg.transaction_list.into_inner()) {
            warn!(
                client_id,
                request_id = msg.request_id,
                transaction_count,
                declared = pending.declare_mining_job.wtxid_list.len(),
                "Rejecting DeclareMiningJob: more transactions supplied than declared, or more bytes than a block holds"
            );
            let refusal = self.refuse_declaration(
                client_id,
                msg.request_id,
                pending_declare_mining_job_token,
                ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
            );
            client_sender
                .send((refusal, None))
                .await
                .map_err(|e| error::JDSError::disconnect(e, client_id))?;

            return Ok(());
        }
        let supplied = pending.supplied();

        let response = match self
            .job_validator
            .handle_declare_mining_job(
                client_id,
                pending.declare_mining_job.clone(),
                Some(supplied),
            )
            .await
        {
            // if job is valid, activate token and return DeclareMiningJobSuccess
            DeclareMiningJobResult::Success => {
                match self
                    .token_manager
                    .activate(pending_declare_mining_job_token, client_id)
                {
                    Some(activated_token) => {
                        let declare_mining_job_success = DeclareMiningJobSuccessOwned {
                            request_id: msg.request_id,
                            new_mining_job_token: activated_token
                                .to_le_bytes()
                                .to_vec()
                                .try_into()
                                .expect("must always be valid B0_255"),
                        };
                        JobDeclarationOwned::DeclareMiningJobSuccess(declare_mining_job_success)
                    }
                    None => {
                        let declare_mining_job_error = DeclareMiningJobErrorOwned {
                            request_id: msg.request_id,
                            error_code: ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN
                                .as_bytes()
                                .to_vec()
                                .try_into()
                                .expect("error code string must be valid B0_255"),
                            error_details: Vec::new()
                                .try_into()
                                .expect("empty array must be valid B0_64K"),
                        };
                        JobDeclarationOwned::DeclareMiningJobError(declare_mining_job_error)
                    }
                }
            }
            // if job is invalid, return DeclareMiningJobError
            DeclareMiningJobResult::Error(error_code) => {
                // make sure we clean up the allocated token
                self.token_manager
                    .deallocate(pending_declare_mining_job_token);

                let declare_mining_job_error = DeclareMiningJobErrorOwned {
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
                JobDeclarationOwned::DeclareMiningJobError(declare_mining_job_error)
            }
            // Transactions the node held on the round before can be gone from its mempool by
            // now. Those are asked for in one more round; a transaction the client was already
            // asked for and still did not supply rejects this DeclareMiningJob instead.
            DeclareMiningJobResult::MissingTransactions(missing_wtxids) => {
                match pending.next_round(client_id, &missing_wtxids) {
                    Some(unknown_tx_position_list) => {
                        info!(
                            client_id,
                            request_id = msg.request_id,
                            transactions = unknown_tx_position_list.len(),
                            "Asking again for declared transactions the node no longer holds"
                        );
                        // Disconnect: downstream may have disconnected between dispatch and
                        // handling.
                        self.downstream_clients
                            .with(&client_id, |downstream| {
                                downstream
                                    .pending_declare_mining_jobs
                                    .insert(msg.request_id, pending);
                            })
                            .ok_or_else(|| {
                                JDSError::disconnect(
                                    error::JDSErrorKind::ClientNotFound(client_id),
                                    client_id,
                                )
                            })?;

                        let provide_missing_transactions = ProvideMissingTransactionsOwned {
                            request_id: msg.request_id,
                            unknown_tx_position_list: unknown_tx_position_list
                                .try_into()
                                .map_err(error::JDSError::shutdown)?,
                        };
                        JobDeclarationOwned::ProvideMissingTransactions(
                            provide_missing_transactions,
                        )
                    }
                    None => self.refuse_declaration(
                        client_id,
                        msg.request_id,
                        pending_declare_mining_job_token,
                        ERROR_CODE_DECLARE_MINING_JOB_MISSING_TXS,
                    ),
                }
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
        client_id: Option<usize>,
        msg: PushSolutionOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        // Shutdown: client_id is always Some; None indicates a bug.
        let client_id =
            client_id.ok_or_else(|| JDSError::shutdown(error::JDSErrorKind::ClientNotFound(0)))?;

        self.job_validator
            .handle_push_solution(client_id, msg)
            .await;

        Ok(())
    }
}

impl JobDeclarator {
    /// Refuses a declaration the JDS does not validate again, with `error_code`: its token is
    /// released and the validator drops what it kept for it, so nothing the declaration brought
    /// outlives the answer. Returns the `DeclareMiningJob.Error` to send.
    pub(super) fn refuse_declaration(
        &self,
        client_id: usize,
        request_id: u32,
        token: JdToken,
        error_code: &'static str,
    ) -> JobDeclarationOwned {
        self.token_manager.deallocate(token);
        self.job_validator
            .cleanup_declaration(client_id, request_id, token);
        JobDeclarationOwned::DeclareMiningJobError(DeclareMiningJobErrorOwned {
            request_id,
            error_code: error_code
                .as_bytes()
                .to_vec()
                .try_into()
                .expect("error code string must be valid B0_255"),
            error_details: Vec::new()
                .try_into()
                .expect("empty array must be valid B0_64K"),
        })
    }
}
