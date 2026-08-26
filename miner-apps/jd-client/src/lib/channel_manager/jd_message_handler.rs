use stratum_apps::{
    stratum_core::{
        binary_sv2::{B016MOwned, Seq064KOwned},
        bitcoin::{
            self, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
            absolute::LockTime, transaction::Version,
        },
        channels_sv2::outputs::deserialize_outputs,
        handlers_sv2::HandleJobDeclarationMessagesFromServerOwnedAsync,
        job_declaration_sv2::{
            AllocateMiningJobTokenSuccessOwned, DeclareMiningJobErrorOwned,
            DeclareMiningJobSuccessOwned, ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
            ProvideMissingTransactionsOwned, ProvideMissingTransactionsSuccessOwned,
        },
        parsers_sv2::{
            AnyMessageOwned, JobDeclarationOwned, MiningOwned, TemplateDistributionOwned, Tlv,
        },
        template_distribution_sv2::CoinbaseOutputConstraintsOwned,
    },
    utils::types::OutboundFrame,
};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::ChannelManager,
    error::{self, JDCError, JDCErrorKind},
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleJobDeclarationMessagesFromServerOwnedAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions
            .with(|data| data.clone())
            .map_err(JDCError::shutdown)
    }

    // Handles a successful `AllocateMiningJobToken` response from the JDS.
    //
    // When the JDS confirms job token allocation:
    // - Updates the channel manager state with the newly issued token.
    // - Checks whether the JDS has provided updated coinbase outputs.
    //   - If outputs have changed, recalculates the corresponding size and sigops constraints.
    //   - Sends an updated `CoinbaseOutputConstraints` message to the Template Provider to ensure
    //     the new coinbase rules are enforced.
    // - If outputs are unchanged, skips recomputation and continues as normal.
    async fn handle_allocate_mining_job_token_success(
        &mut self,
        _server_id: Option<usize>,
        msg: AllocateMiningJobTokenSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let new_coinbase_outputs = msg.coinbase_outputs.to_owned_bytes();
        let coinbase_changed = self
            .coinbase_outputs
            .with(|coinbase_outputs| {
                let changed = *coinbase_outputs != new_coinbase_outputs;
                *coinbase_outputs = new_coinbase_outputs.clone();
                changed
            })
            .map_err(JDCError::shutdown)?;
        self.allocate_tokens
            .with(|tokens| tokens.push_back(msg.clone()))
            .map_err(JDCError::shutdown)?;

        if coinbase_changed {
            info!("Coinbase outputs from JDS changed, recalculating constraints");
            let deserialized_jds_coinbase_outputs: Vec<TxOut> =
                bitcoin::consensus::deserialize(msg.coinbase_outputs.as_bytes())
                    .map_err(JDCError::shutdown)?;

            let max_additional_size: usize = deserialized_jds_coinbase_outputs
                .iter()
                .map(|o| o.size())
                .sum();

            // create a dummy coinbase transaction with the empty output
            // this is used to calculate the sigops of the coinbase output
            let dummy_coinbase = Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint::null(),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::from(vec![vec![0; 32]]),
                }],
                output: deserialized_jds_coinbase_outputs,
            };

            let max_additional_sigops = dummy_coinbase.total_sigop_cost(|_| None) as u16;

            debug!(
                max_additional_size,
                max_additional_sigops, "Computed coinbase output constraints"
            );

            let coinbase_output_constraints_message =
                TemplateDistributionOwned::CoinbaseOutputConstraints(
                    CoinbaseOutputConstraintsOwned {
                        coinbase_output_max_additional_size: max_additional_size as u32,
                        coinbase_output_max_additional_sigops: max_additional_sigops,
                    },
                );

            self.channel_manager_io
                .tp_sender
                .send(coinbase_output_constraints_message)
                .await
                .map_err(|_e| JDCError::shutdown(JDCErrorKind::ChannelErrorSender))?;

            info!("Sent updated CoinbaseOutputConstraints to TP channel");
        } else {
            debug!("Coinbase outputs unchanged, skipping constraints update");
        }

        Ok(())
    }

    // Handles a `DeclareMiningJobError` response from the JDS.
    //
    // Receiving this error is treated as a malicious or invalid upstream behavior,
    // since it indicates the JDS has rejected a declared mining job request.
    //
    // Upon receiving it:
    // - Triggers the fallback mechanism by signaling a shutdown through the status channel, causing
    //   the Job Declarator Client to enter `JobDeclaratorShutdownFallback`.
    //
    // This ensures that the system does not continue relying on a potentially
    // untrustworthy or misbehaving JDS, and instead fails over to a safer state.
    async fn handle_declare_mining_job_error(
        &mut self,
        _server_id: Option<usize>,
        msg: DeclareMiningJobErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);

        let error_code = msg.error_code.as_utf8_or_hex();
        if error_code == ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP {
            warn!(
                "Received non-fatal DeclareMiningJobError from JDS: stale-chain-tip (request_id={})",
                msg.request_id
            );
            return Ok(());
        }

        warn!(
            "⚠️ JDS refused the declared job with a DeclareMiningJobError ❌. Starting fallback mechanism."
        );
        Err(JDCError::fallback(JDCErrorKind::DeclareMiningJobError))
    }

    // Handles a `DeclareMiningJobSuccess` message from the JDS.
    //
    // Receiving this message means the JDS has accepted the declared mining job,
    // giving us the green light to propagate it upstream.
    //
    // The steps are:
    // 1. Look up the last declared job using the `request_id`.
    // 2. Validate that a `prevhash` exists and retrieve job details.
    // 3. Use the job factory to create a new `SetCustomMiningJob` request, embedding the token
    //    provided by the JDS.
    // 4. Update the channel manager state with the newly created custom job.
    // 5. Send the `SetCustomMiningJob` message to the upstream, ensuring the job is now distributed
    //    across the mining network.
    //
    // If any required data (like `prevhash` or the last declared job) is missing,
    // this handler returns an error to prevent propagation of an incomplete job.
    async fn handle_declare_mining_job_success(
        &mut self,
        _server_id: Option<usize>,
        msg: DeclareMiningJobSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let Some(last_declare_job) = self.last_declare_job_store.get_cloned(&msg.request_id) else {
            error!(
                "No last_declare_job found for request_id={}",
                msg.request_id
            );
            return Err(JDCError::log(JDCErrorKind::LastDeclareJobNotFound(
                msg.request_id,
            )));
        };

        let Some(prevhash) = last_declare_job.prev_hash else {
            error!("Prevhash not found for request_id = {}", msg.request_id);
            return Err(JDCError::log(JDCErrorKind::LastNewPrevhashNotFound));
        };

        let outputs = match deserialize_outputs(last_declare_job.coinbase_output.clone()) {
            Ok(outputs) => outputs,
            Err(_) => {
                return Err(JDCError::shutdown(
                    JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs,
                ));
            }
        };

        let upstream_channel_info = self
            .upstream_channel
            .with(|upstream_channel| {
                upstream_channel
                    .as_ref()
                    .map(|channel| (channel.get_channel_id(), channel.get_full_extranonce_size()))
            })
            .map_err(JDCError::shutdown)?;

        let Some((upstream_channel_id, full_extranonce_size)) = upstream_channel_info else {
            return Err(JDCError::log(JDCErrorKind::FailedToCreateCustomJob));
        };

        let Some(custom_job) = self
            .job_factory
            .with(|job_factory| {
                let job_factory = job_factory.as_mut()?;
                let custom_job = job_factory.new_custom_job(
                    upstream_channel_id,
                    msg.request_id,
                    msg.new_mining_job_token,
                    prevhash.into(),
                    last_declare_job.template,
                    outputs,
                    full_extranonce_size,
                );
                Some(custom_job)
            })
            .map_err(JDCError::shutdown)?
        else {
            return Err(JDCError::log(JDCErrorKind::FailedToCreateCustomJob));
        };

        let custom_job =
            custom_job.map_err(|_e| JDCError::log(JDCErrorKind::FailedToCreateCustomJob))?;

        let updated = self
            .last_declare_job_store
            .with_mut(&msg.request_id, |value| {
                value.set_custom_mining_job = Some(custom_job.clone());
            });
        if updated.is_none() {
            return Err(JDCError::log(JDCErrorKind::LastDeclareJobNotFound(
                msg.request_id,
            )));
        }

        let channel_id = custom_job.channel_id;

        debug!("Sending SetCustomMiningJob to the upstream with channel_id: {channel_id}");
        let message = MiningOwned::SetCustomMiningJob(custom_job);
        let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))
            .map_err(JDCError::shutdown)?;
        self.channel_manager_io
            .upstream_sender
            .send(sv2_frame)
            .await
            .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;

        info!("Successfully sent SetCustomMiningJob to the upstream with channel_id: {channel_id}");
        Ok(())
    }

    // Handles a `ProvideMissingTransactions` request from the JDS.
    //
    // The JDS provides a list of transaction positions it could not resolve.
    // We then:
    // - Retrieve the full transaction list for the given `request_id`.
    // - Identify which transactions are missing based on the provided positions.
    // - Collect and package those transactions into a `ProvideMissingTransactionsSuccess`.
    // - Send the response back to the JDS.
    async fn handle_provide_missing_transactions(
        &mut self,
        _server_id: Option<usize>,
        msg: ProvideMissingTransactionsOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let request_id = msg.request_id;

        info!("Received: {}", msg);

        let tx_store_entry = self.last_declare_job_store.get_cloned(&request_id);

        let Some(entry) = tx_store_entry else {
            warn!(
                "No transaction list found for request_id={}",
                msg.request_id
            );
            return Err(JDCError::log(JDCErrorKind::LastDeclareJobNotFound(
                msg.request_id,
            )));
        };

        let full_tx_list: Vec<B016MOwned> = entry
            .tx_list
            .iter()
            .cloned()
            .map(|raw| raw.try_into().map_err(JDCError::shutdown))
            .collect::<Result<_, _>>()?;

        let unknown_positions: Vec<u16> = msg.unknown_tx_position_list.into_inner();
        debug!(
            total_known = full_tx_list.len(),
            unknown_positions = unknown_positions.len(),
            "Resolving missing transactions"
        );

        let missing_txns: Vec<B016MOwned> = unknown_positions
            .iter()
            .filter_map(|&pos| full_tx_list.get(pos as usize).cloned())
            .collect();

        if missing_txns.is_empty() {
            warn!("No matching transactions found for request_id={request_id}");
        }

        let response = ProvideMissingTransactionsSuccessOwned {
            request_id: msg.request_id,
            transaction_list: Seq064KOwned::new(missing_txns).map_err(JDCError::shutdown)?,
        };
        let message = JobDeclarationOwned::ProvideMissingTransactionsSuccess(response);

        self.channel_manager_io
            .jd_sender
            .send(message)
            .await
            .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;

        info!(
            "Successfully sent ProvideMissingTransactionsSuccess to the JDS with request_id: {request_id}"
        );

        Ok(())
    }
}
