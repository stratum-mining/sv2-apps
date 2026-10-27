use stratum_apps::{
    coinbase_output_constraints::coinbase_output_constraints_message,
    stratum_core::{
        binary_sv2::{B016MOwned, Seq064KOwned},
        bitcoin::{self, Amount, ScriptBuf, TxOut},
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
    channel_manager::{ChannelManager, CustomJobStage, WaitingWork},
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

    // Handles a successful `AllocateMiningJobToken` response from the JDS:
    // - drops an answer to a token request JDC never sent or already saw answered;
    // - checks the answer's coinbase outputs, and falls back if JDC can't use them;
    // - records the outputs as the ones the next templates are handed out with, and queues the
    //   token with them;
    // - grows the room the Template Provider reserves if the outputs need more, sending it
    //   `CoinbaseOutputConstraints` for that room;
    // - hands the token to work waiting for one.
    async fn handle_allocate_mining_job_token_success(
        &mut self,
        _server_id: Option<usize>,
        msg: AllocateMiningJobTokenSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        // Only the answer to a token request JDC sent and has not seen answered is acted on,
        // before it reaches the outputs or the token queue (see `MiningJobTokens`).
        let requested = self
            .mining_job_tokens
            .with(|tokens| tokens.answered(msg.request_id))
            .map_err(JDCError::shutdown)?;
        if !requested {
            warn!(
                request_id = msg.request_id,
                "Ignoring AllocateMiningJobToken.Success for a token request JDC did not send or already saw answered"
            );
            return Ok(());
        }

        // Everything about the answer that can fail is checked before any of it is kept, so
        // outputs JDC can't use leave its outputs, tokens and constraints as they were, and JDC
        // falls back from the JDS that handed them out.
        let (new_coinbase_outputs, constraints) =
            usable_coinbase_outputs(msg.coinbase_outputs.as_bytes()).map_err(JDCError::fallback)?;

        // The Template Provider gets constraints for the JDS's first outputs, and afterwards only
        // when outputs need more room than it reserves (see `OutputSets`).
        let room = self
            .output_sets
            .with(|output_sets| output_sets.update(new_coinbase_outputs, constraints))
            .map_err(JDCError::shutdown)?;
        self.mining_job_tokens
            .with(|tokens| tokens.queue(msg.clone()))
            .map_err(JDCError::shutdown)?;

        //
        // A template the Template Provider built before applying new constraints, but sent after
        // JDC sent them, can't be told apart: `NewTemplate` doesn't say which constraints it was
        // built under, so JDC hands it out with outputs it may have no room for. With the Template
        // Provider on the same host or LAN that window is under a round trip, and in full-template
        // mode the in-tree one closes it, answering the template's transaction-data request with
        // `stale-template-id`.
        if let Some(room) = room {
            debug!(
                max_additional_size = room.coinbase_output_max_additional_size,
                max_additional_sigops = room.coinbase_output_max_additional_sigops,
                "Computed coinbase output constraints"
            );
            self.channel_manager_io
                .tp_sender
                .send(TemplateDistributionOwned::CoinbaseOutputConstraints(room))
                .await
                .map_err(|_e| JDCError::shutdown(JDCErrorKind::ChannelErrorSender))?;
            info!("Sent updated CoinbaseOutputConstraints to TP channel");
        } else {
            debug!("Coinbase outputs fit the room reserved, skipping constraints update");
        }

        // A token goes first to work that was waiting for one: a declaration in full-template
        // mode, a custom job in coinbase-only mode. The work asked for a token of its own when it
        // started waiting, so this one is not replaced.
        let waiting = self
            .waiting_for_token
            .with(|waiting| waiting.next())
            .map_err(JDCError::shutdown)?;
        match waiting {
            Some(WaitingWork::Declaration {
                template,
                transaction_data,
            }) => {
                self.declare_template(template, transaction_data).await?;
            }
            Some(WaitingWork::CustomJob { template }) => {
                if let Some(custom_job) = self.build_coinbase_only_custom_job(&template).await? {
                    let message = MiningOwned::SetCustomMiningJob(custom_job);
                    let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))
                        .map_err(JDCError::shutdown)?;
                    self.channel_manager_io
                        .upstream_sender
                        .send(sv2_frame)
                        .await
                        .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
                }
            }
            None => {}
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

        // Only a declaration JDC sent and the JDS hasn't answered yet can be rejected. Any other
        // error answers nothing JDC is waiting for: one a prevhash hasn't activated yet was never
        // sent, and a coinbase-only custom job was never declared.
        let Some((rejected, _)) = self
            .custom_jobs
            .with(|jobs| jobs.declaration_answered(msg.request_id))
            .map_err(JDCError::shutdown)?
        else {
            warn!(
                request_id = msg.request_id,
                "Ignoring DeclareMiningJob.Error for a declaration JDC is not waiting on"
            );
            return Ok(());
        };

        let error_code = msg.error_code.as_utf8_or_hex();
        if error_code == ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP {
            warn!(
                "Received non-fatal DeclareMiningJobError from JDS: stale-chain-tip (request_id={})",
                msg.request_id
            );
            // Nothing else answers a rejected declaration, so it is gone, with its transactions,
            // and the shares cached for its template go too.
            self.cached_shares.remove(&rejected.template.template_id);
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
    // 1. Take the declared job of `request_id`, with the chain tip it was declared on: only a
    //    declaration JDC sent and the JDS hasn't answered yet is accepted, once. Any other success
    //    is ignored and logged.
    // 2. Use the job factory to create a new `SetCustomMiningJob` request on that chain tip,
    //    embedding the token provided by the JDS.
    // 3. Keep the job as proposed, with its custom job.
    // 4. Send the `SetCustomMiningJob` message to the upstream, ensuring the job is now distributed
    //    across the mining network.
    //
    // Without an upstream channel or job factory to build the custom job with, this handler
    // returns an error, and the job is not proposed.
    async fn handle_declare_mining_job_success(
        &mut self,
        _server_id: Option<usize>,
        msg: DeclareMiningJobSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let Some((mut job, prevhash)) = self
            .custom_jobs
            .with(|jobs| jobs.declaration_answered(msg.request_id))
            .map_err(JDCError::shutdown)?
        else {
            warn!(
                request_id = msg.request_id,
                "Ignoring DeclareMiningJob.Success for a declaration JDC is not waiting on"
            );
            return Ok(());
        };

        let outputs = job.coinbase_outputs.clone();

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

        let template_id = job.template.template_id;
        let Some(custom_job) = self
            .job_factory
            .with(|job_factory| {
                let job_factory = job_factory.as_mut()?;
                let custom_job = job_factory.new_custom_job(
                    upstream_channel_id,
                    msg.request_id,
                    msg.new_mining_job_token,
                    prevhash.into(),
                    job.template.clone(),
                    outputs,
                    full_extranonce_size,
                );
                Some(custom_job)
            })
            .map_err(JDCError::shutdown)?
        else {
            return Err(JDCError::log(JDCErrorKind::FailedToCreateCustomJob));
        };

        // A rejected custom job would leave the downstreams mining work whose shares could never
        // reach the Pool. JDC shuts down instead, as it does when a coinbase-only custom job can't
        // be built. The declaration was built from the same template and outputs, so a rejection
        // is not expected.
        let custom_job = custom_job.map_err(|e| {
            error!(
                error = ?e,
                template_id,
                "Failed to build the custom job of a declared template"
            );
            JDCError::shutdown(JDCErrorKind::FailedToCreateCustomJob)
        })?;

        job.stage = CustomJobStage::Proposed {
            custom_job: custom_job.clone(),
        };
        self.custom_jobs
            .with(|jobs| jobs.insert(job))
            .map_err(JDCError::shutdown)?;

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

        let transactions = self
            .custom_jobs
            .with(|jobs| jobs.transactions(request_id))
            .map_err(JDCError::shutdown)?;

        let Some(transactions) = transactions else {
            warn!(
                "No transaction list found for request_id={}",
                msg.request_id
            );
            return Err(JDCError::log(JDCErrorKind::LastDeclareJobNotFound(
                msg.request_id,
            )));
        };

        let full_tx_list: Vec<B016MOwned> = transactions
            .into_iter()
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

// The coinbase outputs a JDS token answer hands out, with the `CoinbaseOutputConstraints` that
// reserve room for them, if JDC can use them: they deserialize, and there is at least one, the
// first being the pool payout output JDC puts the template's reward in. Every one comes at 0
// sats, as the JDS hands them out (sv2-spec 06, `AllocateMiningJobToken.Success`).
fn usable_coinbase_outputs(
    serialized: &[u8],
) -> Result<(Vec<TxOut>, CoinbaseOutputConstraintsOwned), JDCErrorKind> {
    let outputs: Vec<TxOut> = bitcoin::consensus::deserialize(serialized)
        .map_err(|_| JDCErrorKind::AllocateMiningJobTokenSuccessCoinbaseOutputsError)?;
    if outputs.is_empty() {
        return Err(JDCErrorKind::AllocateMiningJobTokenSuccessNoCoinbaseOutputs);
    }
    if outputs.iter().any(|output| output.value != Amount::ZERO) {
        return Err(JDCErrorKind::AllocateMiningJobTokenSuccessCoinbaseOutputValue);
    }
    if coinbase_suffix_size(&outputs) > u16::MAX as usize {
        return Err(JDCErrorKind::AllocateMiningJobTokenSuccessCoinbaseOutputsTooLarge);
    }
    let constraints = coinbase_output_constraints_message(outputs.clone()).map_err(|e| {
        warn!(error = %e, "JDS coinbase outputs don't fit CoinbaseOutputConstraints");
        JDCErrorKind::CoinbaseOutputConstraints(e)
    })?;
    Ok((outputs, constraints))
}

// The input's sequence, which starts the coinbase's suffix.
const INPUT_SEQUENCE_SIZE: usize = 4;
// The witness commitment output's script: `OP_RETURN`, a 36-byte push, the 4-byte commitment
// header and the 32-byte commitment.
const WITNESS_COMMITMENT_SCRIPT_SIZE: usize = 38;
// The coinbase input's witness: one stack item, its length and the 32-byte reserved value.
const WITNESS_RESERVED_VALUE_SIZE: usize = 34;
// The locktime, which ends the coinbase.
const LOCKTIME_SIZE: usize = 4;

// The size of `DeclareMiningJob.coinbase_tx_suffix` for a coinbase paying `outputs`: the input's
// sequence, the outputs with the witness commitment a Template Provider adds (sv2-spec 07,
// `CoinbaseOutputConstraints`), the witness reserved value and the locktime. Every job JDC builds
// carries the outputs in a field of at most 64 KiB, and this one is the tightest.
fn coinbase_suffix_size(outputs: &[TxOut]) -> usize {
    let witness_commitment = TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(vec![0; WITNESS_COMMITMENT_SCRIPT_SIZE]),
    };
    let outputs: Vec<TxOut> = outputs
        .iter()
        .cloned()
        .chain([witness_commitment])
        .collect();
    INPUT_SEQUENCE_SIZE
        + bitcoin::consensus::serialize(&outputs).len()
        + WITNESS_RESERVED_VALUE_SIZE
        + LOCKTIME_SIZE
}
