use std::sync::atomic::Ordering;
use stratum_apps::stratum_core::{
    binary_sv2::{Seq064KOwned, U256Owned},
    job_declaration_sv2::DeclareMiningJobOwned,
    parsers_sv2::{JobDeclarationOwned, MiningOwned},
    template_distribution_sv2::{
        NewTemplateOwned, RequestTransactionDataErrorOwned, RequestTransactionDataSuccessOwned,
    },
};

use stratum_apps::stratum_core::{
    bitcoin::{Transaction, consensus, hashes::Hash},
    handlers_sv2::HandleTemplateDistributionMessagesFromServerOwnedAsync,
    mining_sv2::SetNewPrevHashOwned as SetNewPrevHashMp,
    parsers_sv2::Tlv,
    template_distribution_sv2::{SetNewPrevHashOwned as SetNewPrevHashTdpOwned, *},
};
use tracing::{error, info, warn};

use crate::{
    channel_manager::{ChannelManager, CustomJob, CustomJobStage, WaitingWork},
    error::{self, JDCError, JDCErrorKind},
    utils::UpstreamState,
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleTemplateDistributionMessagesFromServerOwnedAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions
            .with(|data| data.clone())
            .map_err(JDCError::shutdown)
    }

    async fn handle_new_template(
        &mut self,
        _server_id: Option<usize>,
        msg: NewTemplateOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        // JDC hands the template out with the outputs it holds now: they go in the downstream jobs
        // built below, and every job built from the template later, downstream or upstream, pays
        // them.
        self.output_sets
            .with(|output_sets| output_sets.hand_out(msg.template_id))
            .map_err(JDCError::shutdown)?;

        if msg.future_template {
            self.last_future_template
                .set(Some(msg.clone()))
                .map_err(JDCError::shutdown)?;
        }

        let coinbase_outputs = self.coinbase_outputs_for(&msg)?;

        // Only request transaction data when the response can be consumed: building a
        // `DeclareMiningJob` requires the upstream channel and job factory, which only
        // exist once `UpstreamState::Connected`. Of the templates received before the channel
        // opens, the downstreams are handed the last future template once it does, and
        // `handle_open_extended_mining_channel_success` requests its transaction data then.
        if self.mode.is_full_template() && self.upstream_state.get() == UpstreamState::Connected {
            self.request_transaction_data(&msg).await?;
        }

        // Every downstream mines this template under the same custom job on the single upstream
        // channel, so the job is built, and a token spent on it, once per template.
        let custom_job = if !msg.future_template && self.mode.is_coinbase_only() {
            self.build_coinbase_only_custom_job(&msg).await?
        } else {
            None
        };
        let token_spent = custom_job.is_some();

        let mut messages: Vec<crate::channel_manager::downstream_message_handler::RouteMessageTo> =
            Vec::new();
        if let Some(custom_job) = custom_job {
            messages.push(MiningOwned::SetCustomMiningJob(custom_job).into());
        }

        self.downstream.try_for_each(|downstream_id, downstream| {
            let group_channel_job = downstream
                .group_channel
                .with(|group_channel| {
                    group_channel
                        .on_new_template(msg.clone(), coinbase_outputs.clone())
                        .map_err(|e| {
                            tracing::error!("Error while adding template to group channel: {e:?}");
                            JDCError::shutdown(e)
                        })?;

                    let job = if msg.future_template {
                        let future_job_id = group_channel
                            .get_future_job_id_from_template_id(msg.template_id)
                            .expect("future job id must exist");
                        group_channel
                            .get_future_job(future_job_id)
                            .expect("future job must exist")
                    } else {
                        group_channel
                            .get_active_job()
                            .expect("active job must exist")
                    };
                    Ok::<_, Self::Error>(job.clone())
                })
                .map_err(JDCError::shutdown)??;

            let requires_standard_jobs = downstream.require_std_job.load(Ordering::Relaxed);
            let empty_group_channel = downstream
                .group_channel
                .with(|group_channel| group_channel.is_empty())
                .map_err(JDCError::shutdown)?;
            if !requires_standard_jobs && !empty_group_channel {
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::NewExtendedMiningJob(group_channel_job.get_job_message().clone()),
                    )
                        .into(),
                );
            }

            let group_job_id = group_channel_job.get_job_id();

            downstream
                .standard_channels
                .try_for_each_mut(|channel_id, standard_channel| {
                    if !requires_standard_jobs {
                        self.downstream_channel_id_and_job_id_to_template_id.insert(
                            (downstream_id, channel_id, group_job_id).into(),
                            msg.template_id,
                        );
                        standard_channel
                            .on_group_channel_job(group_channel_job.clone())
                            .map_err(|e| {
                                tracing::error!(
                                    "Error while adding group channel job to standard channel: {channel_id:?} {e:?}"
                                );
                                JDCError::shutdown(e)
                            })?;
                    } else {
                        standard_channel
                            .on_new_template(msg.clone(), coinbase_outputs.clone())
                            .map_err(|e| {
                                tracing::error!(
                                    "Error while adding template to standard channel: {channel_id:?} {e:?}"
                                );
                                JDCError::shutdown(e)
                            })?;

                        let job_id = if msg.future_template {
                            let job_id = standard_channel
                                .get_future_job_id_from_template_id(msg.template_id)
                                .expect("future job id must exist");
                            let job = standard_channel
                                .get_future_job(job_id)
                                .expect("future job must exist");
                            messages.push(
                                (
                                    downstream_id,
                                    MiningOwned::NewMiningJob(job.get_job_message().clone()),
                                )
                                    .into(),
                            );
                            job_id
                        } else {
                            let job = standard_channel
                                .get_active_job()
                                .expect("active job must exist");
                            let job_id = job.get_job_id();
                            messages.push(
                                (
                                    downstream_id,
                                    MiningOwned::NewMiningJob(job.get_job_message().clone()),
                                )
                                    .into(),
                            );
                            job_id
                        };

                        self.downstream_channel_id_and_job_id_to_template_id
                            .insert((downstream_id, channel_id, job_id).into(), msg.template_id);
                    }
                    Ok::<(), Self::Error>(())
                })?;

            downstream
                .extended_channels
                .try_for_each_mut(|channel_id, extended_channel| {
                    self.downstream_channel_id_and_job_id_to_template_id.insert(
                        (downstream_id, channel_id, group_job_id).into(),
                        msg.template_id,
                    );
                    extended_channel
                        .on_group_channel_job(group_channel_job.clone())
                        .map_err(|e| {
                            tracing::error!(
                                "Error while adding group channel job to extended channel: {channel_id:?} {e:?}"
                            );
                            JDCError::shutdown(e)
                        })?;
                    Ok::<(), Self::Error>(())
                })?;
            Ok::<(), Self::Error>(())
        })?;

        if token_spent {
            _ = self.allocate_tokens(1).await;
        }

        for message in messages {
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    async fn handle_request_tx_data_error(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestTransactionDataErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        // An error answers the request as finally as a success does, and its template goes with
        // it.
        self.take_requested_template(msg.template_id)?;
        let error_code = msg.error_code.as_utf8_or_hex();

        if matches!(
            error_code.as_str(),
            ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND
                | ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID
        ) {
            return Ok(());
        }
        Err(JDCError::log(JDCErrorKind::TxDataError))
    }

    async fn handle_request_tx_data_success(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestTransactionDataSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        // Only the answer for a template whose transaction data JDC requested and has not
        // received yet is acted on, and taking the template out makes a duplicate answer one JDC
        // never asked for. Requests are only sent in full-template mode while the upstream channel
        // is open, and a fallback starts with none, so anything else is dropped here, before it
        // can reach the token queue.
        let Some(template) = self.take_requested_template(msg.template_id)? else {
            warn!(
                template_id = msg.template_id,
                "Ignoring RequestTransactionData.Success for a template whose data was not requested"
            );
            return Ok(());
        };

        let token_spent = self.declare_template(template, msg).await?;
        if token_spent {
            _ = self.allocate_tokens(1).await;
        }
        Ok(())
    }

    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        msg: SetNewPrevHashTdpOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        // A prevhash naming the template the chain tip already activated is that chain tip again:
        // everything it would change, the first one changed.
        let repeated = self
            .last_new_prev_hash
            .with(|tip| {
                tip.as_ref()
                    .is_some_and(|tip| tip.template_id == msg.template_id)
            })
            .map_err(JDCError::shutdown)?;
        if repeated {
            warn!(
                template_id = msg.template_id,
                "Ignoring SetNewPrevHash for the template the chain tip already activated"
            );
            return Ok(());
        }

        self.cached_shares.clear();

        // Templates still waiting for their transaction data were built on the previous chain
        // tip, except the one this prevhash activates.
        let mut released = Vec::new();
        self.requested_templates
            .with(|requested| {
                requested.retain(|template| {
                    let keep = template.template_id == msg.template_id;
                    if !keep {
                        released.push(template.template_id);
                    }
                    keep
                })
            })
            .map_err(JDCError::shutdown)?;
        for template_id in released {
            warn!(
                template_id,
                "Releasing template waiting for its transaction data: the chain tip moved past it"
            );
        }

        // Work waiting for a token was built on the previous chain tip, except the template this
        // prevhash activates, which becomes the new tip's.
        let dropped = self
            .waiting_for_token
            .with(|waiting| waiting.on_chain_tip(msg.template_id))
            .map_err(JDCError::shutdown)?;
        for waiting in dropped {
            warn!(
                template_id = waiting.template().template_id,
                "Dropping work waiting for a token: the chain tip moved past it"
            );
        }
        self.output_sets
            .with(|output_sets| output_sets.on_chain_tip(msg.template_id))
            .map_err(JDCError::shutdown)?;

        self.upstream_channel
            .with(|upstream_channel| {
                if let Some(upstream_channel) = upstream_channel.as_mut() {
                    _ = upstream_channel.on_chain_tip_update(msg.clone().into());
                }
            })
            .map_err(JDCError::shutdown)?;

        let future_template = self
            .last_future_template
            .get()
            .map_err(JDCError::shutdown)?;
        // The template this prevhash names is declared now, if its declaration waited for it.
        let declaration = self
            .custom_jobs
            .with(|jobs| jobs.declare(&msg))
            .map_err(JDCError::shutdown)?;
        if let Some(declaration) = declaration {
            self.channel_manager_io
                .jd_sender
                .send(JobDeclarationOwned::DeclareMiningJob(declaration))
                .await
                .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
        }

        self.last_new_prev_hash
            .set(Some(msg.clone()))
            .map_err(JDCError::shutdown)?;

        let mut messages: Vec<crate::channel_manager::downstream_message_handler::RouteMessageTo> =
            Vec::new();
        // The activated template goes on the upstream channel under its own custom job. With no
        // upstream channel to send it on yet, nothing is built and the token stays queued.
        let custom_job = match &future_template {
            Some(template) if self.mode.is_coinbase_only() => {
                self.build_coinbase_only_custom_job(template).await?
            }
            _ => None,
        };
        let token_consumed = custom_job.is_some();
        if let Some(custom_job) = custom_job {
            messages.push(MiningOwned::SetCustomMiningJob(custom_job).into());
        }

        self.downstream.try_for_each(|downstream_id, downstream| {
            let (group_channel_id, activated_group_job_id, empty_group_channel) = downstream
                .group_channel
                .with(|group_channel| {
                    group_channel
                        .on_set_new_prev_hash(msg.clone())
                        .map_err(|e| {
                            tracing::error!(
                                "Error while adding new prev hash to group channel: {e:?}"
                            );
                            JDCError::fallback(e)
                        })?;
                    Ok::<_, Self::Error>((
                        group_channel.get_group_channel_id(),
                        group_channel
                            .get_active_job()
                            .expect("active job must exist")
                            .get_job_id(),
                        group_channel.is_empty(),
                    ))
                })
                .map_err(JDCError::shutdown)??;

            let requires_standard_jobs = downstream.require_std_job.load(Ordering::Relaxed);
            if !requires_standard_jobs && !empty_group_channel {
                downstream.standard_channels.for_each(|channel_id, _| {
                    self.downstream_channel_id_and_job_id_to_template_id.insert(
                        (downstream_id, channel_id, activated_group_job_id).into(),
                        msg.template_id,
                    );
                });
                downstream.extended_channels.for_each(|channel_id, _| {
                    self.downstream_channel_id_and_job_id_to_template_id.insert(
                        (downstream_id, channel_id, activated_group_job_id).into(),
                        msg.template_id,
                    );
                });

                messages.push(
                    (
                        downstream_id,
                        MiningOwned::SetNewPrevHash(SetNewPrevHashMp {
                            channel_id: group_channel_id,
                            job_id: activated_group_job_id,
                            prev_hash: msg.prev_hash.clone(),
                            ntime_start: msg.ntime_start,
                            nbits: msg.n_bits,
                        }),
                    )
                        .into(),
                );
            }

            downstream
                .standard_channels
                .try_for_each_mut(|channel_id, standard_channel| {
                    standard_channel
                        .on_set_new_prev_hash(msg.clone())
                        .map_err(|e| {
                            tracing::error!(
                                "Error while adding new prev hash to standard channel: {channel_id:?} {e:?}"
                            );
                            JDCError::fallback(e)
                        })?;

                    if requires_standard_jobs {
                        let activated_standard_job_id = standard_channel
                            .get_active_job()
                            .expect("active job must exist")
                            .get_job_id();
                        self.downstream_channel_id_and_job_id_to_template_id.insert(
                            (downstream_id, channel_id, activated_standard_job_id).into(),
                            msg.template_id,
                        );
                        messages.push(
                            (
                                downstream_id,
                                MiningOwned::SetNewPrevHash(SetNewPrevHashMp {
                                    channel_id,
                                    job_id: activated_standard_job_id,
                                    prev_hash: msg.prev_hash.clone(),
                                    ntime_start: msg.ntime_start,
                                    nbits: msg.n_bits,
                                }),
                            )
                                .into(),
                        );
                    }
                    Ok::<(), Self::Error>(())
                })?;

            downstream
                .extended_channels
                .try_for_each_mut(|_, extended_channel| {
                    extended_channel
                        .on_set_new_prev_hash(msg.clone())
                        .map_err(|e| {
                            tracing::error!(
                                "Error while adding new prev hash to extended channel: {e:?}"
                            );
                            JDCError::fallback(e)
                        })
                })?;
            Ok::<(), Self::Error>(())
        })?;

        if token_consumed {
            _ = self.allocate_tokens(1).await;
        }

        for message in messages {
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl ChannelManager {
    // Declares `template` to the JDS, from the transaction data the Template Provider sent for it,
    // with the token for its upstream job, and records the declaration. A future template's
    // declaration is held back for the prevhash that activates it, which sends it.
    //
    // A template has a single declaration, so one whose custom job exists already gets none. The
    // token is taken from the queue only once the declaration is built. Without one, the template
    // waits for a token (see `wait_for_token`), and the token that arrives declares it. Returns
    // whether a token was spent.
    pub(super) async fn declare_template(
        &self,
        template: NewTemplateOwned,
        transaction_data: RequestTransactionDataSuccessOwned,
    ) -> Result<bool, JDCError<error::ChannelManager>> {
        if self
            .custom_jobs
            .with(|jobs| jobs.contains(template.template_id))
            .map_err(JDCError::shutdown)?
        {
            warn!(
                template_id = template.template_id,
                "Not declaring a template that already has a custom job"
            );
            return Ok(false);
        }
        let Some(token) = self.token_for(&template)? else {
            self.wait_for_token(WaitingWork::Declaration {
                template,
                transaction_data,
            })
            .await?;
            return Ok(false);
        };

        let transactions_data = transaction_data.transaction_list;
        let excess_data = transaction_data.excess_data;
        let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
        let prevhash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;

        let mining_token = token.mining_job_token.clone();
        let deserialized_outputs = self.coinbase_outputs_for(&template)?;

        let tx_list: Vec<Transaction> = transactions_data
            .iter_bytes()
            .map(consensus::deserialize)
            .collect::<Result<_, _>>()
            .map_err(|e| {
                error!(
                    error = ?e,
                    template_id = template.template_id,
                    "Template provider sent a malformed transaction"
                );
                JDCError::shutdown(e)
            })?;

        let wtxids_as_u256: Vec<U256Owned> = tx_list
            .iter()
            .map(|tx| {
                let txid = tx.compute_wtxid();
                U256Owned::from(*txid.as_byte_array())
            })
            .collect();
        let wtx_ids = Seq064KOwned::new(wtxids_as_u256).map_err(JDCError::shutdown)?;

        let awaits_activation = template.future_template
            && prevhash
                .as_ref()
                .map(|prev_hash| prev_hash.template_id != template.template_id)
                .unwrap_or(true);

        let upstream_full_extranonce_size = self
            .upstream_channel
            .with(|channel| {
                channel
                    .as_ref()
                    .map(|channel| channel.get_full_extranonce_size())
            })
            .map_err(JDCError::shutdown)?;

        let coinbase_parts = match upstream_full_extranonce_size {
            Some(full_extranonce_size) => self
                .job_factory
                .with(|job_factory| {
                    job_factory.as_mut().map(|job_factory| {
                        job_factory.new_coinbase_tx_prefix_and_suffix(
                            template.clone(),
                            deserialized_outputs.clone(),
                            full_extranonce_size,
                        )
                    })
                })
                .map_err(JDCError::shutdown)?,
            None => None,
        };
        let Some(coinbase_parts) = coinbase_parts else {
            return Ok(false);
        };
        // A template whose declaration can't be built would leave the downstreams mining work
        // whose shares could never reach the Pool. JDC shuts down instead, as it does when a
        // coinbase-only custom job can't be built. The JDS's outputs were checked when its token
        // answer arrived, so only Template Provider input gets here.
        let (coinbase_tx_prefix, coinbase_tx_suffix) = coinbase_parts.map_err(|e| {
            error!(
                error = ?e,
                template_id = template.template_id,
                "Failed to build a job declaration"
            );
            JDCError::shutdown(JDCErrorKind::FailedToDeclareJob)
        })?;
        let (Ok(coinbase_tx_prefix), Ok(coinbase_tx_suffix)) =
            (coinbase_tx_prefix.try_into(), coinbase_tx_suffix.try_into())
        else {
            error!(
                template_id = template.template_id,
                "A job declaration's coinbase does not fit DeclareMiningJob"
            );
            return Err(JDCError::shutdown(JDCErrorKind::FailedToDeclareJob));
        };

        let declare_job = DeclareMiningJobOwned {
            request_id,
            mining_job_token: mining_token,
            version: template.version,
            coinbase_tx_prefix,
            coinbase_tx_suffix,
            wtxid_list: wtx_ids,
            excess_data,
        };
        let transactions = transactions_data
            .iter_bytes()
            .map(|tx| tx.to_vec())
            .collect();
        // A template no prevhash has activated yet is declared by the one that does. So is one
        // arriving while JDC has no chain tip at all, which a Template Provider following the
        // protocol never sends: its declaration is never sent without the chain tip it is on.
        let stage = match prevhash {
            Some(prev_hash) if !awaits_activation => CustomJobStage::Declared {
                prev_hash,
                transactions: Some(transactions),
            },
            _ => CustomJobStage::Undeclared {
                declaration: declare_job.clone(),
                transactions,
            },
        };
        let declared = matches!(stage, CustomJobStage::Declared { .. });
        self.mining_job_tokens
            .with(|tokens| tokens.spend(token.request_id))
            .map_err(JDCError::shutdown)?;
        self.custom_jobs
            .with(|jobs| {
                jobs.insert(CustomJob {
                    request_id,
                    template,
                    coinbase_outputs: deserialized_outputs,
                    stage,
                })
            })
            .map_err(JDCError::shutdown)?;
        if !declared {
            return Ok(true);
        }

        _ = self
            .channel_manager_io
            .jd_sender
            .send(JobDeclarationOwned::DeclareMiningJob(declare_job))
            .await;

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::{Receiver, unbounded};
    use stratum_apps::{
        coinbase_output_constraints::coinbase_output_constraints_message,
        config_helpers::CoinbaseRewardScript,
        key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
        stratum_core::{
            binary_sv2::B0255Owned,
            bitcoin::{Amount, ScriptBuf, Target, TxOut},
            channels_sv2::{
                client::extended::ExtendedChannel,
                extranonce_manager::ExtranoncePrefix,
                server::{group::GroupChannel, jobs::factory::JobFactory},
            },
            codec_sv2::EncodableFrame as _,
            job_declaration_sv2::{
                AllocateMiningJobTokenSuccessOwned, DeclareMiningJobErrorOwned,
                DeclareMiningJobSuccessOwned, ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
                ProvideMissingTransactionsOwned,
            },
            mining_sv2::{
                ERROR_CODE_SET_CUSTOM_MINING_JOB_STALE_CHAIN_TIP, SetCustomMiningJobErrorOwned,
                SetCustomMiningJobOwned, SetCustomMiningJobSuccessOwned,
            },
            parsers_sv2::{Mining, TemplateDistributionOwned},
        },
        tp_type::TemplateProviderType,
        utils::types::{InboundFrame, OutboundFrame},
    };

    use stratum_apps::stratum_core::handlers_sv2::HandleJobDeclarationMessagesFromServerOwnedAsync;

    use crate::{
        channel_manager::{
            MAX_REQUESTED_TEMPLATES, MAX_WAITING_FOR_TOKEN, MiningJobTokens, OutputSets,
            SOLO_FULL_EXTRANONCE_SIZE,
        },
        config::{ConfigJDCMode, JobDeclaratorClientConfig, PoolConfig, ProtocolConfig},
        downstream::Downstream,
        error::Action,
        jd_mode::JDMode,
    };

    const TEMPLATE_VALUE: u64 = 5_000_000_000;

    // What a channel manager under test sends each of its peers.
    struct Sent {
        pool: Receiver<OutboundFrame>,
        jds: Receiver<JobDeclarationOwned>,
        tp: Receiver<TemplateDistributionOwned>,
    }

    // A coinbase-only channel manager from `channel_manager_in`. Returns it with what it sends
    // the Pool and what it sends the JDS.
    async fn channel_manager() -> (
        ChannelManager,
        Receiver<OutboundFrame>,
        Receiver<JobDeclarationOwned>,
    ) {
        let (manager, sent) = channel_manager_in(ConfigJDCMode::CoinbaseOnly).await;
        (manager, sent.pool, sent.jds)
    }

    // A channel manager in `mode`. Outside solo mining it mines through an open upstream channel
    // on the chain tip of `prev_hash(1)`, holding one token and the JDS's outputs, with no
    // downstream connected.
    async fn channel_manager_in(mode: ConfigJDCMode) -> (ChannelManager, Sent) {
        let config = JobDeclaratorClientConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            ProtocolConfig::new(
                2,
                2,
                CoinbaseRewardScript::from_descriptor("raw(51)").unwrap(),
            ),
            10.0,
            1,
            PoolConfig::new(
                Secp256k1PublicKey::try_from(
                    "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72".to_string(),
                )
                .unwrap(),
                Secp256k1SecretKey::try_from(
                    "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n".to_string(),
                )
                .unwrap(),
            ),
            60,
            TemplateProviderType::Sv2Tp {
                address: "127.0.0.1:0".to_string(),
                public_key: None,
            },
            vec![],
            "test-jdc".to_string(),
            Some(mode),
            vec![],
            vec![],
            None,
            None,
            None,
        );
        let (upstream_sender, sent_to_pool) = unbounded();
        let (_, upstream_receiver) = unbounded();
        let (jd_sender, sent_to_jds) = unbounded();
        let (_, jd_receiver) = unbounded();
        let (tp_sender, sent_to_tp) = unbounded();
        let (_, tp_receiver) = unbounded();
        let (_, downstream_receiver) = unbounded();
        let jd_mode = JDMode::new(mode);

        let manager = ChannelManager::new(
            config,
            upstream_sender,
            upstream_receiver,
            jd_sender,
            jd_receiver,
            tp_sender,
            tp_receiver,
            downstream_receiver,
            parsed(&jds_coinbase_outputs()),
            vec![],
            vec![],
            jd_mode.clone(),
        )
        .await
        .unwrap();
        let sent = Sent {
            pool: sent_to_pool,
            jds: sent_to_jds,
            tp: sent_to_tp,
        };
        if mode == ConfigJDCMode::SoloMining {
            return (manager, sent);
        }

        // What the runtime and the JDS handshake leave behind once JDC works through a Pool, and
        // the JDS has answered a token request.
        jd_mode.activate();
        manager
            .output_sets
            .with(|output_sets| {
                output_sets.update(
                    parsed(&jds_coinbase_outputs()),
                    coinbase_output_constraints_message(parsed(&jds_coinbase_outputs())).unwrap(),
                )
            })
            .unwrap();
        manager.set_user_identity("test-user".to_string());
        manager.upstream_state.set(UpstreamState::Connected);
        manager
            .upstream_channel
            .set(Some(
                ExtendedChannel::new(
                    7,
                    "test-user".to_string(),
                    ExtranoncePrefix::from_wire(vec![0, 0, 0, 1]).unwrap(),
                    Target::from_le_bytes([0xff; 32]),
                    1.0,
                    true,
                    12,
                    None,
                )
                .unwrap(),
            ))
            .unwrap();
        manager
            .job_factory
            .set(Some(JobFactory::new(
                true,
                None,
                Some("test-jdc".to_string()),
            )))
            .unwrap();
        manager.last_new_prev_hash.set(Some(prev_hash(1))).unwrap();
        let request_id = manager.request_id_factory.fetch_add(1, Ordering::Relaxed);
        manager
            .mining_job_tokens
            .with(|tokens| {
                tokens.queue(AllocateMiningJobTokenSuccessOwned {
                    request_id,
                    mining_job_token: token(),
                    coinbase_outputs: jds_coinbase_outputs().try_into().unwrap(),
                })
            })
            .unwrap();
        (manager, sent)
    }

    fn token() -> B0255Owned {
        0_u64.to_le_bytes().to_vec().try_into().unwrap()
    }

    // The serialized `outputs`, parsed.
    fn parsed(outputs: &[u8]) -> Vec<TxOut> {
        consensus::deserialize(outputs).unwrap()
    }

    // Coinbase outputs as a JDS hands them out, for JDC to fill in from each template.
    fn jds_coinbase_outputs() -> Vec<u8> {
        consensus::serialize(&vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::from_hex("0014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc")
                .unwrap(),
        }])
    }

    fn template(template_id: u64, future_template: bool) -> NewTemplateOwned {
        NewTemplateOwned {
            template_id,
            future_template,
            version: 536_870_912,
            coinbase_tx_version: 2,
            coinbase_prefix: vec![82, 0].try_into().unwrap(),
            coinbase_tx_input_sequence: u32::MAX,
            coinbase_tx_value_remaining: TEMPLATE_VALUE,
            coinbase_tx_outputs_count: 1,
            coinbase_tx_outputs: vec![
                0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113, 209,
                222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153, 98, 180,
                139, 235, 216, 54, 151, 78, 140, 249,
            ]
            .try_into()
            .unwrap(),
            coinbase_tx_locktime: 0,
            merkle_path: vec![].try_into().unwrap(),
        }
    }

    // A template whose coinbase prefix leaves no room in the scriptSig for JDC's tags and
    // extranonce, which the job factory rejects.
    fn oversized_template(template_id: u64, future_template: bool) -> NewTemplateOwned {
        let mut template = template(template_id, future_template);
        template.coinbase_prefix = vec![0; 100].try_into().unwrap();
        template
    }

    fn prev_hash(template_id: u64) -> SetNewPrevHashTdpOwned {
        SetNewPrevHashTdpOwned {
            template_id,
            prev_hash: [template_id as u8; 32].into(),
            ntime_start: 1_700_000_000,
            n_bits: 0x1d00ffff,
            target: [0xff; 32].into(),
        }
    }

    // The outputs JDC hands the next template out with.
    fn current_outputs(manager: &ChannelManager) -> Vec<u8> {
        manager
            .output_sets
            .with(|output_sets| output_sets.current().serialized.clone())
            .unwrap()
    }

    fn queued_tokens(manager: &ChannelManager) -> usize {
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.len())
            .unwrap()
    }

    // The custom job JDC keeps for `template_id`, if any.
    fn recorded_job(manager: &ChannelManager, template_id: u64) -> Option<CustomJob> {
        manager
            .custom_jobs
            .with(|jobs| jobs.0.get(&template_id).cloned())
            .unwrap()
    }

    // The custom jobs recorded as proposed for `template_id`.
    fn recorded_custom_jobs(
        manager: &ChannelManager,
        template_id: u64,
    ) -> Vec<SetCustomMiningJobOwned> {
        match recorded_job(manager, template_id).map(|job| job.stage) {
            Some(CustomJobStage::Proposed { custom_job }) => vec![custom_job],
            _ => vec![],
        }
    }

    fn sent_to_pool(receiver: &Receiver<OutboundFrame>) -> Vec<MiningOwned> {
        std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|frame| {
                let mut bytes = vec![0; frame.encoded_length()];
                frame.encode_into(&mut bytes).unwrap();
                let mut frame = InboundFrame::from_bytes(bytes.into()).unwrap();
                let message_type = frame.header().msg_type();
                Mining::try_from((message_type, frame.payload()))
                    .unwrap()
                    .into_owned()
            })
            .collect()
    }

    // The custom job belongs to the template and the upstream channel, not to whoever mines it,
    // so it goes out with no downstream connected too.
    #[tokio::test]
    async fn template_with_no_downstream_connected_sends_its_custom_job() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        let sent = sent_to_pool(&sent_to_pool_receiver);
        let [MiningOwned::SetCustomMiningJob(custom_job)] = sent.as_slice() else {
            panic!("expected one custom job, the Pool got {sent:?}");
        };
        assert_eq!(custom_job.token, token());
    }

    // A template whose custom job is rejected shuts JDC down rather than leave its downstreams
    // mining work whose shares could never reach the Pool, and nothing goes to the Pool.
    #[tokio::test]
    async fn template_whose_custom_job_is_rejected_shuts_jdc_down() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;

        let error = manager
            .handle_new_template(None, oversized_template(2, false), None)
            .await
            .unwrap_err();

        assert!(matches!(error.kind, JDCErrorKind::FailedToCreateCustomJob));
        assert!(matches!(error.action, Action::Shutdown));
        assert!(sent_to_pool(&sent_to_pool_receiver).is_empty());
    }

    // The downstream channels were handed an activated template's jobs when it arrived. Their
    // shares have nowhere upstream to go without its custom job, so a rejected one shuts JDC
    // down, and nothing goes to the Pool.
    #[tokio::test]
    async fn activation_whose_custom_job_is_rejected_shuts_jdc_down() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;
        manager
            .last_future_template
            .set(Some(oversized_template(2, true)))
            .unwrap();

        let error = manager
            .handle_set_new_prev_hash(None, prev_hash(2), None)
            .await
            .unwrap_err();

        assert!(matches!(error.kind, JDCErrorKind::FailedToCreateCustomJob));
        assert!(matches!(error.action, Action::Shutdown));
        assert!(sent_to_pool(&sent_to_pool_receiver).is_empty());
    }

    // Between a future template and the prevhash that activates it, the chain tip is still the
    // previous block's, so the template gets no custom job and its token stays queued. Opening
    // the upstream channel in that window asks for this job.
    #[tokio::test]
    async fn future_template_the_chain_tip_has_not_activated_gets_no_custom_job() {
        let (manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;

        let custom_job = manager
            .build_coinbase_only_custom_job(&template(2, true))
            .await
            .unwrap();

        assert!(custom_job.is_none());
        assert_eq!(
            manager
                .mining_job_tokens
                .with(|tokens| tokens.queue.len())
                .unwrap(),
            1
        );
        assert!(sent_to_pool(&sent_to_pool_receiver).is_empty());
    }

    // A coinbase-only custom job is recorded with the outputs it pays, the template's reward
    // included, as a declaration is.
    #[tokio::test]
    async fn custom_job_is_recorded_with_the_outputs_it_pays() {
        let (mut manager, _sent_to_pool, _sent_to_jds) = channel_manager().await;

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        let recorded = recorded_job(&manager, 2)
            .expect("the custom job is recorded")
            .coinbase_outputs;
        assert_eq!(
            recorded,
            manager.coinbase_outputs_for(&template(2, false)).unwrap()
        );
    }

    // A prevhash that activates a template other than the last future one gives that future
    // template no custom job on the new chain tip, and spends no token on it.
    #[tokio::test]
    async fn activation_of_another_template_builds_no_custom_job() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;
        manager
            .last_future_template
            .set(Some(template(2, true)))
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();

        assert!(sent_to_pool(&sent_to_pool_receiver).is_empty());
        assert_eq!(
            manager
                .mining_job_tokens
                .with(|tokens| tokens.queue.len())
                .unwrap(),
            1
        );
    }

    // An activation takes a token only for a custom job it built, so one whose custom job is
    // rejected leaves the token queued.
    #[tokio::test]
    async fn activation_whose_custom_job_is_rejected_keeps_its_token() {
        let (mut manager, _sent_to_pool, sent_to_jds) = channel_manager().await;
        manager
            .last_future_template
            .set(Some(oversized_template(2, true)))
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(2), None)
            .await
            .unwrap_err();

        assert_eq!(queued_tokens(&manager), 1);
        assert!(sent_to_jds.try_recv().is_err());
    }

    // An activation's custom job that cannot be handed to the Pool sender keeps its token
    // recorded with the job, not queued again for another one.
    #[tokio::test]
    async fn activation_custom_job_the_pool_cannot_receive_keeps_its_token_recorded() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;
        drop(sent_to_pool_receiver);
        manager
            .last_future_template
            .set(Some(template(2, true)))
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(2), None)
            .await
            .unwrap();

        let recorded = recorded_custom_jobs(&manager, 2);
        let [custom_job] = recorded.as_slice() else {
            panic!("expected one recorded custom job, got {recorded:?}");
        };
        assert_eq!(custom_job.token, token());
        assert_eq!(queued_tokens(&manager), 0);
    }

    // The templates waiting for their transaction data, oldest first.
    fn requested_templates(manager: &ChannelManager) -> Vec<u64> {
        manager
            .requested_templates
            .with(|requested| {
                requested
                    .iter()
                    .map(|template| template.template_id)
                    .collect()
            })
            .unwrap()
    }

    // Solo and coinbase-only modes never ask for transaction data, and nothing else reads a stored
    // template, so they keep none of the templates they are sent.
    #[tokio::test]
    async fn modes_that_request_no_transaction_data_keep_no_template() {
        for mode in [ConfigJDCMode::SoloMining, ConfigJDCMode::CoinbaseOnly] {
            let (mut manager, _sent) = channel_manager_in(mode).await;

            for template_id in 2..10 {
                manager
                    .handle_new_template(None, template(template_id, template_id % 2 == 0), None)
                    .await
                    .unwrap();
            }

            assert!(requested_templates(&manager).is_empty());
        }
    }

    // A full-template JDC asks for no transaction data before its upstream channel opens, so it
    // keeps none of the templates it is sent until then.
    #[tokio::test]
    async fn templates_before_the_upstream_channel_opens_are_not_kept() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.upstream_state.set(UpstreamState::Pending);

        for template_id in 2..10 {
            manager
                .handle_new_template(None, template(template_id, false), None)
                .await
                .unwrap();
        }

        assert!(sent.tp.try_recv().is_err());
        assert!(requested_templates(&manager).is_empty());
    }

    // A template is kept until its transaction-data request is answered, and an error answers it
    // as finally as a success does.
    #[tokio::test]
    async fn an_error_for_its_transaction_data_releases_the_template() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        assert_eq!(requested_templates(&manager), vec![2]);

        manager
            .handle_request_tx_data_error(
                None,
                RequestTransactionDataErrorOwned {
                    template_id: 2,
                    error_code: ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID
                        .to_string()
                        .try_into()
                        .unwrap(),
                },
                None,
            )
            .await
            .unwrap();

        assert!(requested_templates(&manager).is_empty());
    }

    // A new chain tip releases the templates built on the previous one that still wait for their
    // transaction data, and keeps the one it activates.
    #[tokio::test]
    async fn a_new_chain_tip_releases_the_templates_it_leaves_behind() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();

        assert_eq!(requested_templates(&manager), vec![3]);
    }

    // However many requests go unanswered, only the newest templates wait for their transaction
    // data, up to a fixed number.
    #[tokio::test]
    async fn templates_waiting_for_transaction_data_are_capped() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        let templates = 2..2 + MAX_REQUESTED_TEMPLATES as u64 + 4;

        for template_id in templates.clone() {
            manager
                .handle_new_template(None, template(template_id, false), None)
                .await
                .unwrap();
        }

        let newest: Vec<u64> = templates.skip(4).collect();
        assert_eq!(requested_templates(&manager), newest);
    }

    fn transaction_data(template_id: u64) -> RequestTransactionDataSuccessOwned {
        RequestTransactionDataSuccessOwned {
            template_id,
            excess_data: vec![].try_into().unwrap(),
            transaction_list: vec![].try_into().unwrap(),
        }
    }

    // Solo mode never asks the Template Provider for transaction data, so it ignores any it is
    // sent, without reaching for a JDS it does not have.
    #[tokio::test]
    async fn solo_mode_ignores_transaction_data_it_never_requested() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::SoloMining).await;

        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        assert!(sent.jds.try_recv().is_err());
    }

    // Transaction data JDC never asked for is dropped before it reaches the token queue, so it
    // cannot make JDC ask the JDS for tokens. An answer to a request made before a fallback,
    // which builds a new channel manager, is one of these.
    #[tokio::test]
    async fn transaction_data_never_requested_asks_the_jds_for_no_token() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();

        for template_id in [2, 3] {
            manager
                .handle_request_tx_data_success(None, transaction_data(template_id), None)
                .await
                .unwrap();
        }

        assert!(sent.jds.try_recv().is_err());
    }

    // Transaction data JDC never asked for leaves the token queue and the requests still waiting
    // for their answer as they were, and declares nothing.
    #[tokio::test]
    async fn transaction_data_never_requested_leaves_tokens_and_requests_alone() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        manager
            .handle_request_tx_data_success(None, transaction_data(3), None)
            .await
            .unwrap();

        assert_eq!(queued_tokens(&manager), 1);
        assert_eq!(requested_templates(&manager), vec![2]);
        assert!(sent.jds.try_recv().is_err());
    }

    // The answer to a request is acted on once. A duplicate of it declares nothing more and
    // leaves the token queue alone.
    #[tokio::test]
    async fn duplicate_transaction_data_is_acted_on_once() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        assert!(matches!(
            sent.tp.try_recv(),
            Ok(TemplateDistributionOwned::RequestTransactionData(_))
        ));

        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        let declarations = std::iter::from_fn(|| sent.jds.try_recv().ok())
            .filter(|message| matches!(message, JobDeclarationOwned::DeclareMiningJob(_)))
            .count();
        assert_eq!(declarations, 1);

        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        assert!(sent.jds.try_recv().is_err());
    }

    // An error answers a request as finally as a success does, so transaction data that follows
    // it is dropped before it reaches the token queue.
    #[tokio::test]
    async fn transaction_data_after_an_error_for_its_request_is_dropped() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_error(
                None,
                RequestTransactionDataErrorOwned {
                    template_id: 2,
                    error_code: ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID
                        .to_string()
                        .try_into()
                        .unwrap(),
                },
                None,
            )
            .await
            .unwrap();

        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        assert_eq!(queued_tokens(&manager), 1);
        assert!(sent.jds.try_recv().is_err());
    }

    // A token the JDS hands JDC, answering the oldest of JDC's token requests not answered yet.
    fn token_success(manager: &ChannelManager) -> AllocateMiningJobTokenSuccessOwned {
        let request_id = manager
            .mining_job_tokens
            .with(|tokens| tokens.requests.iter().min().copied())
            .unwrap()
            .expect("JDC has a token request to answer");
        AllocateMiningJobTokenSuccessOwned {
            request_id,
            mining_job_token: token(),
            coinbase_outputs: jds_coinbase_outputs().try_into().unwrap(),
        }
    }

    // The templates JDC keeps a declaration for that the JDS hasn't answered yet, in ascending
    // order.
    fn declared_templates(manager: &ChannelManager) -> Vec<u64> {
        let mut templates: Vec<u64> = manager
            .custom_jobs
            .with(|jobs| {
                jobs.0
                    .values()
                    .filter(|job| {
                        matches!(
                            job.stage,
                            CustomJobStage::Undeclared { .. } | CustomJobStage::Declared { .. }
                        )
                    })
                    .map(|job| job.template.template_id)
                    .collect()
            })
            .unwrap();
        templates.sort();
        templates
    }

    fn declarations_sent(receiver: &Receiver<JobDeclarationOwned>) -> usize {
        std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|message| matches!(message, JobDeclarationOwned::DeclareMiningJob(_)))
            .count()
    }

    // Transaction data that finds no token waits for one, and asks the JDS for a token of its
    // own. The token that arrives next declares the template, without a new one, and is not
    // replaced: the waiting work already asked for it.
    #[tokio::test]
    async fn requested_template_waiting_for_a_token_is_declared_once_one_arrives() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        assert!(matches!(
            sent.jds.try_recv(),
            Ok(JobDeclarationOwned::AllocateMiningJobToken(_))
        ));
        assert!(sent.jds.try_recv().is_err());

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        let sent_after_token: Vec<_> = std::iter::from_fn(|| sent.jds.try_recv().ok()).collect();
        assert!(matches!(
            sent_after_token.as_slice(),
            [JobDeclarationOwned::DeclareMiningJob(_)]
        ));
        assert_eq!(declared_templates(&manager), vec![2]);
    }

    // A future template whose transaction data waits for a token through its own activation is
    // declared, and sent, by the token that arrives after it.
    #[tokio::test]
    async fn activated_template_waiting_for_a_token_is_declared_once_one_arrives() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, true), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(2), None)
            .await
            .unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        assert_eq!(declarations_sent(&sent.jds), 1);
        assert_eq!(declared_templates(&manager), vec![2]);
    }

    // Work built on a chain tip can no longer be mined once the tip moves, so a new tip ends its
    // wait: the token that arrives next declares nothing and stays queued.
    #[tokio::test]
    async fn waiting_work_the_chain_tip_moved_past_is_dropped() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();
        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        assert_eq!(declarations_sent(&sent.jds), 0);
        assert!(declared_templates(&manager).is_empty());
        assert_eq!(queued_tokens(&manager), 1);
    }

    // Downstreams mine every template of the current chain tip, so each one whose transaction
    // data finds no token waits, and the tokens that arrive declare them newest first, each once.
    #[tokio::test]
    async fn every_waiting_template_is_declared_newest_first() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        for template_id in [2, 3] {
            manager
                .handle_new_template(None, template(template_id, false), None)
                .await
                .unwrap();
        }
        for template_id in [2, 3] {
            manager
                .handle_request_tx_data_success(None, transaction_data(template_id), None)
                .await
                .unwrap();
        }

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert_eq!(declared_templates(&manager), vec![3]);
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        // One more token, with no work left waiting for it: it stays queued.
        manager.allocate_tokens(1).await.unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        assert_eq!(declarations_sent(&sent.jds), 2);
        assert_eq!(declared_templates(&manager), vec![2, 3]);
        assert_eq!(queued_tokens(&manager), 1);
    }

    // However long tokens take to arrive, only the newest templates of the current chain tip
    // wait for one, up to a fixed number. A template pushing out the oldest takes over its token
    // request instead of asking for another.
    #[tokio::test]
    async fn templates_waiting_for_a_token_are_capped() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        let templates = 2..2 + MAX_WAITING_FOR_TOKEN as u64 + 1;
        for template_id in templates.clone() {
            manager
                .handle_new_template(None, template(template_id, false), None)
                .await
                .unwrap();
        }
        for template_id in templates.clone() {
            manager
                .handle_request_tx_data_success(None, transaction_data(template_id), None)
                .await
                .unwrap();
        }
        let token_requests = std::iter::from_fn(|| sent.jds.try_recv().ok())
            .filter(|message| matches!(message, JobDeclarationOwned::AllocateMiningJobToken(_)))
            .count();
        assert_eq!(token_requests, MAX_WAITING_FOR_TOKEN);

        for _ in 0..MAX_WAITING_FOR_TOKEN {
            manager
                .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
                .await
                .unwrap();
        }
        // One more token, with no work left waiting for it: it stays queued.
        manager.allocate_tokens(1).await.unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        let newest: Vec<u64> = templates.skip(1).collect();
        assert_eq!(declared_templates(&manager), newest);
        assert_eq!(queued_tokens(&manager), 1);
    }

    // A future template's transaction data that waits for a token is declared by a token that
    // arrives before its activation, but the declaration only goes to the JDS once a prevhash
    // activates the template.
    #[tokio::test]
    async fn future_template_waiting_for_a_token_is_sent_once_activated() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, true), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert_eq!(declared_templates(&manager), vec![2]);
        assert_eq!(declarations_sent(&sent.jds), 0);

        manager
            .handle_set_new_prev_hash(None, prev_hash(2), None)
            .await
            .unwrap();
        assert_eq!(declarations_sent(&sent.jds), 1);
    }

    // Only one future template waits for a token: a newer one takes its place, and only the newer
    // one is declared.
    #[tokio::test]
    async fn newer_future_template_replaces_a_waiting_one() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        for template_id in [2, 3] {
            manager
                .handle_new_template(None, template(template_id, true), None)
                .await
                .unwrap();
        }
        for template_id in [2, 3] {
            manager
                .handle_request_tx_data_success(None, transaction_data(template_id), None)
                .await
                .unwrap();
        }

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        // One more token, with no work left waiting for it: it stays queued.
        manager.allocate_tokens(1).await.unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        assert_eq!(declared_templates(&manager), vec![3]);
        assert_eq!(queued_tokens(&manager), 1);
    }

    fn custom_jobs_sent(receiver: &Receiver<OutboundFrame>) -> usize {
        sent_to_pool(receiver)
            .iter()
            .filter(|message| matches!(message, MiningOwned::SetCustomMiningJob(_)))
            .count()
    }

    // In coinbase-only mode too, a template that finds no token waits for one, and asks the JDS
    // for a token of its own. The token that arrives next builds and sends the custom job, and is
    // not replaced.
    #[tokio::test]
    async fn coinbase_only_template_waiting_for_a_token_gets_its_custom_job_once_one_arrives() {
        let (mut manager, sent_to_pool_receiver, sent_to_jds) = channel_manager().await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 0);
        assert!(matches!(
            sent_to_jds.try_recv(),
            Ok(JobDeclarationOwned::AllocateMiningJobToken(_))
        ));
        assert!(sent_to_jds.try_recv().is_err());

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 1);
        assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);
        assert!(sent_to_jds.try_recv().is_err());
    }

    // A template activated while JDC holds no token gets its custom job from the token that
    // arrives next.
    #[tokio::test]
    async fn coinbase_only_activated_template_waiting_for_a_token_gets_its_custom_job() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, true), None)
            .await
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(2), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 0);

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 1);
        assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);
    }

    // Every coinbase-only template of the current chain tip that finds no token waits, and the
    // tokens that arrive build their custom jobs newest first, each once.
    #[tokio::test]
    async fn coinbase_only_every_waiting_template_gets_its_custom_job_newest_first() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        for template_id in [2, 3] {
            manager
                .handle_new_template(None, template(template_id, false), None)
                .await
                .unwrap();
        }

        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert!(recorded_custom_jobs(&manager, 2).is_empty());
        assert_eq!(recorded_custom_jobs(&manager, 3).len(), 1);
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        // One more token, with no work left waiting for it: it stays queued.
        manager.allocate_tokens(1).await.unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 2);
        assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);
        assert_eq!(recorded_custom_jobs(&manager, 3).len(), 1);
        assert_eq!(queued_tokens(&manager), 1);
    }

    // A coinbase-only template waiting for a token is dropped when the chain tip moves past it,
    // so the token that arrives next builds nothing for it and stays queued.
    #[tokio::test]
    async fn coinbase_only_waiting_template_the_chain_tip_moved_past_is_dropped() {
        let (mut manager, sent_to_pool_receiver, _sent_to_jds) = channel_manager().await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();

        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 0);
        assert!(recorded_custom_jobs(&manager, 2).is_empty());
        assert_eq!(queued_tokens(&manager), 1);
    }

    // An answer to a token request is acted on once: a second copy of it adds no token. JDC
    // starts with one token queued, and ends with that one and the one it asked for.
    #[tokio::test]
    async fn replayed_token_answer_is_queued_once() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.allocate_tokens(1).await.unwrap();
        let Ok(JobDeclarationOwned::AllocateMiningJobToken(request)) = sent.jds.try_recv() else {
            panic!("JDC asks the JDS for a token");
        };
        let answer = AllocateMiningJobTokenSuccessOwned {
            request_id: request.request_id,
            mining_job_token: token(),
            coinbase_outputs: jds_coinbase_outputs().try_into().unwrap(),
        };

        for _ in 0..2 {
            manager
                .handle_allocate_mining_job_token_success(None, answer.clone(), None)
                .await
                .unwrap();
        }

        assert_eq!(queued_tokens(&manager), 2);
    }

    // An answer to a token request JDC never sent changes nothing: no token is queued, JDC keeps
    // its outputs, and the Template Provider gets no new constraints.
    #[tokio::test]
    async fn token_answer_jdc_never_asked_for_changes_nothing() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        let outputs = current_outputs(&manager);
        let other_outputs = consensus::serialize(&vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::new(),
        }]);

        manager
            .handle_allocate_mining_job_token_success(
                None,
                AllocateMiningJobTokenSuccessOwned {
                    request_id: 7,
                    mining_job_token: token(),
                    coinbase_outputs: other_outputs.try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap();

        assert_eq!(queued_tokens(&manager), 1);
        assert_eq!(current_outputs(&manager), outputs);
        assert!(sent.tp.try_recv().is_err());
    }

    // A token answered for `request_id`, carrying the serialized `outputs`.
    fn token_answer(request_id: u32, outputs: Vec<u8>) -> AllocateMiningJobTokenSuccessOwned {
        AllocateMiningJobTokenSuccessOwned {
            request_id,
            mining_job_token: token(),
            coinbase_outputs: outputs.try_into().unwrap(),
        }
    }

    // The request IDs of the queued tokens, oldest first.
    fn queued_requests(tokens: &MiningJobTokens) -> Vec<u32> {
        tokens.queue.iter().map(|token| token.request_id).collect()
    }

    // An answer is accepted for a request JDC sent, once: a second answer to it, or one to a
    // request JDC never sent, finds no request.
    #[test]
    fn a_token_request_is_answered_once() {
        let mut tokens = MiningJobTokens::default();
        tokens.request(4);

        assert!(!tokens.answered(5));
        assert!(tokens.answered(4));
        assert!(!tokens.answered(4));
    }

    // A job spends the token it was built with even when the queue changed in between: that
    // token is taken out, or nothing if it is gone already.
    #[test]
    fn a_job_spends_the_token_it_was_built_with() {
        let mut tokens = MiningJobTokens::default();
        for request_id in [4, 5, 6] {
            tokens.queue(token_answer(request_id, jds_coinbase_outputs()));
        }

        let built_with = tokens.peek_for(&jds_coinbase_outputs()).unwrap().request_id;
        assert_eq!(built_with, 4);
        // Another job spends the same token first.
        tokens.spend(4);
        tokens.spend(built_with);
        assert_eq!(queued_requests(&tokens), vec![5, 6]);

        tokens.spend(6);
        assert_eq!(queued_requests(&tokens), vec![5]);
    }

    // A job takes the oldest token that came with the outputs it pays, and the oldest of all when
    // none did.
    #[test]
    fn a_job_takes_the_oldest_token_that_came_with_its_outputs() {
        let mut tokens = MiningJobTokens::default();
        for (request_id, hash) in [(4, 0x11), (5, 0x22), (6, 0x22)] {
            tokens.queue(token_answer(request_id, payout_outputs(hash)));
        }

        assert_eq!(
            tokens.peek_for(&payout_outputs(0x22)).unwrap().request_id,
            5
        );
        assert_eq!(
            tokens.peek_for(&payout_outputs(0x33)).unwrap().request_id,
            4
        );
        tokens.spend(4);
        tokens.spend(5);
        assert_eq!(
            tokens.peek_for(&payout_outputs(0x22)).unwrap().request_id,
            6
        );
        tokens.spend(6);
        assert!(tokens.peek_for(&payout_outputs(0x22)).is_none());
    }

    // Outputs whose sigop cost doesn't fit `CoinbaseOutputConstraints` make JDC fall back rather
    // than reserve fewer sigops than they cost, and the Template Provider gets no constraints. A
    // thousand `OP_CHECKMULTISIG`s cost 80,000 sigops, more than its 16-bit field can carry.
    #[tokio::test]
    async fn outputs_costing_more_sigops_than_constraints_carry_fall_back() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.allocate_tokens(1).await.unwrap();
        let mut answer = token_success(&manager);
        answer.coinbase_outputs = consensus::serialize(&vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::from_bytes(vec![0xae; 1000]),
        }])
        .try_into()
        .unwrap();

        let error = manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Fallback));
        assert!(sent.tp.try_recv().is_err());
    }

    // The room reserved for the JDS's outputs is their serialized size alone, even with enough of
    // them to grow the coinbase's output count from one byte to three: the Template Provider
    // reserves the output count's worst case itself (sv2-spec 07, `CoinbaseOutputConstraints`).
    #[tokio::test]
    async fn constraints_reserve_the_outputs_size_alone() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.allocate_tokens(1).await.unwrap();
        let outputs: Vec<TxOut> = (0..253)
            .map(|_| TxOut {
                value: Amount::from_sat(0),
                script_pubkey: ScriptBuf::new(),
            })
            .collect();
        let mut answer = token_success(&manager);
        answer.coinbase_outputs = consensus::serialize(&outputs).try_into().unwrap();

        manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap();

        let Ok(TemplateDistributionOwned::CoinbaseOutputConstraints(constraints)) =
            sent.tp.try_recv()
        else {
            panic!("JDC sends the Template Provider constraints for the new outputs");
        };
        let outputs_size: usize = outputs.iter().map(TxOut::size).sum();
        assert_eq!(
            constraints.coinbase_output_max_additional_size as usize,
            outputs_size
        );
    }

    // The largest outputs a JDS can hand out are those whose declaration, with the template's
    // witness commitment, fills `DeclareMiningJob.coinbase_tx_suffix` exactly: they are accepted
    // and declared. One byte more is refused.
    #[tokio::test]
    async fn largest_usable_outputs_fill_a_declaration_exactly() {
        let outputs = |script_len: usize| {
            consensus::serialize(&vec![TxOut {
                value: Amount::from_sat(0),
                script_pubkey: ScriptBuf::from_bytes(
                    [vec![0x6a], vec![0; script_len - 1]].concat(),
                ),
            }])
        };

        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.allocate_tokens(1).await.unwrap();
        let mut answer = token_success(&manager);
        answer.coinbase_outputs = outputs(65_435).try_into().unwrap();
        let error = manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap_err();
        assert!(matches!(error.action, Action::Fallback));

        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.allocate_tokens(1).await.unwrap();
        let mut answer = token_success(&manager);
        answer.coinbase_outputs = outputs(65_434).try_into().unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap();
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        let declaration = std::iter::from_fn(|| sent.jds.try_recv().ok())
            .find_map(|message| match message {
                JobDeclarationOwned::DeclareMiningJob(declaration) => Some(declaration),
                _ => None,
            })
            .expect("JDC declares the template");
        assert_eq!(declaration.coinbase_tx_suffix.len(), u16::MAX as usize);
    }

    // A declaration the job factory can't build, here because the template leaves no room in the
    // scriptSig for JDC's tags, shuts JDC down, as a coinbase-only custom job does, and nothing is
    // declared.
    #[tokio::test]
    async fn declaration_that_cant_be_built_shuts_jdc_down() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, oversized_template(2, false), None)
            .await
            .unwrap();

        let error = manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Shutdown));
        assert_eq!(declarations_sent(&sent.jds), 0);
    }

    // A custom job the job factory rejects after the JDS accepted its declaration shuts JDC down,
    // as a coinbase-only custom job does, and nothing goes to the Pool.
    #[tokio::test]
    async fn custom_job_rejected_after_its_declaration_shuts_jdc_down() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        let outputs = manager.coinbase_outputs_for(&template(2, false)).unwrap();
        manager
            .custom_jobs
            .with(|jobs| {
                jobs.insert(CustomJob {
                    request_id: 7,
                    template: oversized_template(2, false),
                    coinbase_outputs: outputs,
                    stage: CustomJobStage::Declared {
                        prev_hash: prev_hash(1),
                        transactions: Some(Vec::new()),
                    },
                })
            })
            .unwrap();

        let error = manager
            .handle_declare_mining_job_success(
                None,
                stratum_apps::stratum_core::job_declaration_sv2::DeclareMiningJobSuccessOwned {
                    request_id: 7,
                    new_mining_job_token: token(),
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Shutdown));
        assert!(sent_to_pool(&sent.pool).is_empty());
    }

    // A token answer whose outputs JDC can't use makes JDC fall back, and changes nothing: no
    // token is queued, JDC keeps its outputs, and the Template Provider gets no constraints.
    #[tokio::test]
    async fn unusable_token_answers_fall_back_and_change_nothing() {
        let output = |sats, script: Vec<u8>| TxOut {
            value: Amount::from_sat(sats),
            script_pubkey: ScriptBuf::from_bytes(script),
        };
        let unusable_outputs = [
            // Not a list of outputs: a CompactSize cut short.
            vec![0xfd],
            // No output to pay the template's reward into.
            consensus::serialize(&Vec::<TxOut>::new()),
            // A value in the pool payout output, or in another one.
            consensus::serialize(&vec![output(1, vec![])]),
            consensus::serialize(&vec![output(0, vec![]), output(1, vec![])]),
            // More sigops than `CoinbaseOutputConstraints` can carry.
            consensus::serialize(&vec![output(0, vec![0xae; 1000])]),
            // Too big for a job's coinbase: one output filling the whole 64 KiB field.
            consensus::serialize(&vec![output(0, [vec![0x6a], vec![0; 65_522]].concat())]),
        ];

        for coinbase_outputs in unusable_outputs {
            let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
            let outputs = current_outputs(&manager);
            manager.allocate_tokens(1).await.unwrap();
            let mut answer = token_success(&manager);
            answer.coinbase_outputs = coinbase_outputs.try_into().unwrap();

            let error = manager
                .handle_allocate_mining_job_token_success(None, answer, None)
                .await
                .unwrap_err();

            assert!(matches!(error.action, Action::Fallback));
            assert_eq!(queued_tokens(&manager), 1);
            assert_eq!(current_outputs(&manager), outputs);
            assert!(sent.tp.try_recv().is_err());
        }
    }

    // A payout output to `hash`, the way a JDS or the miner's configuration hands one out.
    fn payout_outputs(hash: u8) -> Vec<u8> {
        consensus::serialize(&vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::from_bytes([&[0x00, 0x14][..], &[hash; 20]].concat()),
        }])
    }

    // The script of the first of the serialized `outputs`, where a job pays the template's reward.
    fn payout_script(outputs: &[u8]) -> ScriptBuf {
        consensus::deserialize::<Vec<TxOut>>(outputs).unwrap()[0]
            .script_pubkey
            .clone()
    }

    // The outputs JDC recorded with its declaration of `template_id`.
    fn declared_outputs(manager: &ChannelManager, template_id: u64) -> Vec<u8> {
        consensus::serialize(
            &recorded_job(manager, template_id)
                .expect("the template is declared")
                .coinbase_outputs,
        )
    }

    // A template is declared with the outputs it was handed out with, even when the JDS hands out
    // others before its transaction data arrives, so that the declared job pays what the
    // template's downstream jobs pay. The next template pays the new outputs.
    #[tokio::test]
    async fn template_is_declared_with_the_outputs_it_was_handed_out_with() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager.allocate_tokens(1).await.unwrap();
        let mut answer = token_success(&manager);
        answer.coinbase_outputs = payout_outputs(0x11).try_into().unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap();

        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        manager
            .handle_new_template(None, template(3, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(3), None)
            .await
            .unwrap();

        assert_eq!(
            payout_script(&declared_outputs(&manager, 2)),
            payout_script(&jds_coinbase_outputs())
        );
        assert_eq!(
            payout_script(&declared_outputs(&manager, 3)),
            payout_script(&payout_outputs(0x11))
        );
    }

    // A coinbase-only template waiting for a token gets its custom job with the outputs it was
    // handed out with, even when the token that pays for it brings different ones.
    #[tokio::test]
    async fn waiting_coinbase_only_template_keeps_the_outputs_it_was_handed_out_with() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::CoinbaseOnly).await;
        manager
            .mining_job_tokens
            .with(|tokens| tokens.queue.clear())
            .unwrap();
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        let mut answer = token_success(&manager);
        answer.coinbase_outputs = payout_outputs(0x11).try_into().unwrap();

        manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap();

        let custom_jobs = recorded_custom_jobs(&manager, 2);
        assert_eq!(
            payout_script(custom_jobs[0].coinbase_tx_outputs.as_bytes()),
            payout_script(&jds_coinbase_outputs())
        );
    }

    // JDC holds the token it started with, which came with `jds_coinbase_outputs`, and then gets
    // token 1 with other outputs, which the templates handed out from then on pay.
    async fn queue_token_with_other_outputs(
        manager: &mut ChannelManager,
    ) -> AllocateMiningJobTokenSuccessOwned {
        manager.allocate_tokens(1).await.unwrap();
        let mut answer = token_success(manager);
        answer.mining_job_token = 1_u64.to_le_bytes().to_vec().try_into().unwrap();
        answer.coinbase_outputs = payout_outputs(0x11).try_into().unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, answer.clone(), None)
            .await
            .unwrap();
        answer
    }

    fn queued_token_ids(manager: &ChannelManager) -> Vec<B0255Owned> {
        manager
            .mining_job_tokens
            .with(|tokens| {
                tokens
                    .queue
                    .iter()
                    .map(|token| token.mining_job_token.clone())
                    .collect()
            })
            .unwrap()
    }

    // A declaration takes the token that came with the outputs in the template's downstream jobs
    // over an older token that came with others, which stays queued.
    #[tokio::test]
    async fn declaration_takes_a_token_that_came_with_its_outputs() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        let answer = queue_token_with_other_outputs(&mut manager).await;

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        let declaration = std::iter::from_fn(|| sent.jds.try_recv().ok())
            .find_map(|message| match message {
                JobDeclarationOwned::DeclareMiningJob(declaration) => Some(declaration),
                _ => None,
            })
            .expect("JDC declares the template");
        assert_eq!(declaration.mining_job_token, answer.mining_job_token);
        assert_eq!(queued_token_ids(&manager), vec![token()]);
    }

    // A coinbase-only custom job takes the token that came with the outputs in the template's
    // downstream jobs over an older token that came with others, which stays queued.
    #[tokio::test]
    async fn custom_job_takes_a_token_that_came_with_its_outputs() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::CoinbaseOnly).await;
        let answer = queue_token_with_other_outputs(&mut manager).await;

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        assert_eq!(
            recorded_custom_jobs(&manager, 2)[0].token,
            answer.mining_job_token
        );
        assert_eq!(queued_token_ids(&manager), vec![token()]);
    }

    // Before the JDS's first answer, templates are handed out with the miner's own outputs: solo
    // work, which gets no custom job even with a token in hand, and asks for none. The first
    // answer sends the Template Provider constraints for the JDS's outputs, and the template the
    // chain tip moves to next goes upstream with them.
    #[tokio::test]
    async fn work_handed_out_before_the_first_jds_answer_stays_solo() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::CoinbaseOnly).await;
        manager
            .output_sets
            .set(OutputSets::new(parsed(&payout_outputs(0x22))))
            .unwrap();

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent.pool), 0);
        assert_eq!(queued_tokens(&manager), 1);
        assert!(sent.jds.try_recv().is_err());

        manager.allocate_tokens(1).await.unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert!(matches!(
            sent.tp.try_recv(),
            Ok(TemplateDistributionOwned::CoinbaseOutputConstraints(_))
        ));

        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();
        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();
        let custom_jobs = recorded_custom_jobs(&manager, 3);
        assert_eq!(custom_jobs.len(), 1);
        assert_eq!(
            payout_script(custom_jobs[0].coinbase_tx_outputs.as_bytes()),
            payout_script(&jds_coinbase_outputs())
        );
    }

    // A full-template JDC asks for no transaction data of a template handed out with the miner's
    // own outputs: that work is never declared.
    #[tokio::test]
    async fn solo_work_requests_no_transaction_data() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .output_sets
            .set(OutputSets::new(parsed(&payout_outputs(0x22))))
            .unwrap();

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();

        assert!(sent.tp.try_recv().is_err());
        assert!(requested_templates(&manager).is_empty());
    }

    // Each set of outputs covers the templates from the first one handed out with it, and a new
    // chain tip keeps only the sets the templates from it on can be paying.
    #[test]
    fn output_sets_follow_template_ids_and_chain_tips_prune_them() {
        let outputs = |hash| parsed(&payout_outputs(hash));
        let room = coinbase_output_constraints_message(outputs(1)).unwrap();
        let mut sets = OutputSets::new(outputs(0));
        sets.hand_out(1);
        sets.update(outputs(1), room);
        sets.hand_out(2);
        sets.hand_out(3);
        sets.update(outputs(2), room);
        // No template was handed out with `outputs(2)`, so `outputs(3)` replaces it.
        sets.update(outputs(3), room);
        sets.hand_out(5);

        assert_eq!(sets.of(1).outputs, outputs(0));
        assert_eq!(sets.of(3).outputs, outputs(1));
        assert_eq!(sets.of(4).outputs, outputs(1));
        assert_eq!(sets.of(5).outputs, outputs(3));
        assert_eq!(sets.of(5).serialized, payout_outputs(3));
        assert_eq!(sets.sets.len(), 3);

        sets.on_chain_tip(3);
        assert_eq!(sets.sets.len(), 2);
        sets.on_chain_tip(5);
        assert_eq!(sets.sets.len(), 1);
    }

    // JDC asks the JDS for a token, and the JDS answers with `outputs`.
    async fn answer_with_outputs(manager: &mut ChannelManager, outputs: Vec<u8>) {
        manager.allocate_tokens(1).await.unwrap();
        let mut answer = token_success(manager);
        answer.coinbase_outputs = outputs.try_into().unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, answer, None)
            .await
            .unwrap();
    }

    // Outputs that fit the room the Template Provider already reserves, like a new payout address
    // of the same size, send it no constraints. The templates handed out next pay them.
    #[tokio::test]
    async fn outputs_fitting_the_reserved_room_send_no_constraints() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;

        answer_with_outputs(&mut manager, payout_outputs(0x11)).await;

        assert!(sent.tp.try_recv().is_err());
        assert_eq!(current_outputs(&manager), payout_outputs(0x11));
    }

    // Outputs needing more room than the Template Provider reserves send it constraints for that
    // room, and the reservation never shrinks: later outputs that fit it, smaller or not, send
    // nothing.
    #[tokio::test]
    async fn the_reserved_room_only_grows() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        let two_payouts: Vec<TxOut> = [0x11, 0x33]
            .into_iter()
            .flat_map(|hash| consensus::deserialize::<Vec<TxOut>>(&payout_outputs(hash)).unwrap())
            .collect();
        let taproot_payout = consensus::serialize(&vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::from_bytes([&[0x51, 0x20][..], &[0x44; 32]].concat()),
        }]);

        answer_with_outputs(&mut manager, consensus::serialize(&two_payouts)).await;
        let Ok(TemplateDistributionOwned::CoinbaseOutputConstraints(constraints)) =
            sent.tp.try_recv()
        else {
            panic!("JDC asks the Template Provider for more room");
        };
        let two_payouts_size: usize = two_payouts.iter().map(TxOut::size).sum();
        assert_eq!(
            constraints.coinbase_output_max_additional_size as usize,
            two_payouts_size
        );

        answer_with_outputs(&mut manager, payout_outputs(0x55)).await;
        answer_with_outputs(&mut manager, taproot_payout).await;
        assert!(sent.tp.try_recv().is_err());
    }

    // The room for the miner's own outputs gives way to the JDS's first outputs whatever their
    // size. From then on it only grows: outputs that fit it send nothing, and outputs needing more
    // in either field get the larger of the room and their need in both.
    #[test]
    fn the_reserved_room_grows_only_for_outputs_that_need_more() {
        let outputs = |hash| parsed(&payout_outputs(hash));
        let constraints = |size, sigops| CoinbaseOutputConstraintsOwned {
            coinbase_output_max_additional_size: size,
            coinbase_output_max_additional_sigops: sigops,
        };
        let mut sets = OutputSets::new(outputs(0x00));

        let rooms = [
            sets.update(outputs(0x11), constraints(10, 4)),
            sets.update(outputs(0x22), constraints(10, 4)),
            sets.update(outputs(0x33), constraints(8, 0)),
            sets.update(outputs(0x44), constraints(12, 0)),
            sets.update(outputs(0x55), constraints(0, 8)),
        ];

        assert_eq!(
            rooms,
            [
                Some(constraints(10, 4)),
                None,
                None,
                Some(constraints(12, 4)),
                Some(constraints(12, 8)),
            ]
        );
    }

    // The request ID of the job JDC recorded for `template_id`.
    fn recorded_request_id(manager: &ChannelManager, template_id: u64) -> u32 {
        recorded_job(manager, template_id)
            .expect("JDC recorded a job for the template")
            .request_id
    }

    fn stale_chain_tip(request_id: u32) -> DeclareMiningJobErrorOwned {
        DeclareMiningJobErrorOwned {
            request_id,
            error_code: ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP
                .to_string()
                .try_into()
                .unwrap(),
            error_details: vec![].try_into().unwrap(),
        }
    }

    // A declaration the JDS rejects as stale gets no other answer, so it goes with its
    // transactions and the shares cached for its template, while JDC carries on.
    #[tokio::test]
    async fn stale_rejection_releases_the_declaration() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        manager.cached_shares.with_mut_or_default(2, |_| {});

        manager
            .handle_declare_mining_job_error(
                None,
                stale_chain_tip(recorded_request_id(&manager, 2)),
                None,
            )
            .await
            .unwrap();

        assert!(declared_templates(&manager).is_empty());
        assert!(!manager.cached_shares.contains_key(&2));
    }

    // A stale rejection naming work JDC never declared to the JDS leaves it alone: a declaration a
    // prevhash hasn't activated yet was never sent, and a coinbase-only custom job is the Pool's.
    #[tokio::test]
    async fn stale_rejection_leaves_work_never_declared_alone() {
        let (mut manager, _sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(3), None)
            .await
            .unwrap();
        manager
            .handle_declare_mining_job_error(
                None,
                stale_chain_tip(recorded_request_id(&manager, 3)),
                None,
            )
            .await
            .unwrap();
        assert_eq!(declared_templates(&manager), vec![3]);

        let (mut manager, _sent_to_pool, _sent_to_jds) = channel_manager().await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_declare_mining_job_error(
                None,
                stale_chain_tip(recorded_request_id(&manager, 2)),
                None,
            )
            .await
            .unwrap();
        assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);
    }

    fn declaration_success(request_id: u32) -> DeclareMiningJobSuccessOwned {
        DeclareMiningJobSuccessOwned {
            request_id,
            new_mining_job_token: token(),
        }
    }

    fn custom_job_success(request_id: u32, channel_id: u32) -> SetCustomMiningJobSuccessOwned {
        SetCustomMiningJobSuccessOwned {
            channel_id,
            request_id,
            job_id: 9,
        }
    }

    fn custom_job_stale(request_id: u32, channel_id: u32) -> SetCustomMiningJobErrorOwned {
        SetCustomMiningJobErrorOwned {
            channel_id,
            request_id,
            error_code: ERROR_CODE_SET_CUSTOM_MINING_JOB_STALE_CHAIN_TIP
                .to_string()
                .try_into()
                .unwrap(),
        }
    }

    // The Pool answers JDC's custom job with `message`.
    async fn pool_success(
        manager: &mut ChannelManager,
        message: SetCustomMiningJobSuccessOwned,
    ) -> Result<(), JDCError<error::ChannelManager>> {
        use stratum_apps::stratum_core::handlers_sv2::HandleMiningMessagesFromServerOwnedAsync;
        manager
            .handle_set_custom_mining_job_success(None, message, None)
            .await
    }

    // The Pool rejects JDC's custom job with `message`.
    async fn pool_error(
        manager: &mut ChannelManager,
        message: SetCustomMiningJobErrorOwned,
    ) -> Result<(), JDCError<error::ChannelManager>> {
        use stratum_apps::stratum_core::handlers_sv2::HandleMiningMessagesFromServerOwnedAsync;
        manager
            .handle_set_custom_mining_job_error(None, message, None)
            .await
    }

    // A full-template JDC whose declaration of template 2 the JDS hasn't answered yet.
    async fn manager_with_a_declaration() -> (ChannelManager, Sent, u32) {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        let request_id = recorded_request_id(&manager, 2);
        (manager, sent, request_id)
    }

    // A template's declaration is sent once: declaring it again finds it sent already, and sends
    // nothing rather than the declaration again with a token the JDS has consumed.
    #[tokio::test]
    async fn a_declaration_is_sent_once() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(3), None)
            .await
            .unwrap();
        assert_eq!(declarations_sent(&sent.jds), 0);

        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();
        assert_eq!(declarations_sent(&sent.jds), 1);

        let declaration = manager
            .custom_jobs
            .with(|jobs| jobs.declare(&prev_hash(3)))
            .unwrap();
        assert!(declaration.is_none());
    }

    // A template declared while JDC has no chain tip, which only a Template Provider breaking the
    // protocol brings about, waits for a prevhash instead of going to the JDS without the chain
    // tip its custom job would be built on.
    #[tokio::test]
    async fn a_declaration_without_a_chain_tip_is_not_sent() {
        let (manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager.last_new_prev_hash.set(None).unwrap();

        manager
            .declare_template(template(2, false), transaction_data(2))
            .await
            .unwrap();

        assert_eq!(declarations_sent(&sent.jds), 0);
        let job = recorded_job(&manager, 2).expect("the template is recorded");
        assert!(matches!(job.stage, CustomJobStage::Undeclared { .. }));
    }

    // A Template Provider repeating a template ID, which the protocol rules out, brings the
    // template's transaction data again. The template keeps the declaration it has, and the token
    // a second one would have spent stays queued.
    #[tokio::test]
    async fn a_repeated_template_keeps_its_declaration_and_its_token() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();
        assert_eq!(declarations_sent(&sent.jds), 1);
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        assert_eq!(queued_tokens(&manager), 1);

        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(2), None)
            .await
            .unwrap();

        assert_eq!(declarations_sent(&sent.jds), 0);
        assert_eq!(queued_tokens(&manager), 1);
    }

    // In coinbase-only mode a template has one custom job: building it again finds it built
    // already, and neither builds another nor spends another token.
    #[tokio::test]
    async fn coinbase_only_template_gets_one_custom_job() {
        let (mut manager, sent_to_pool, _sent_to_jds) = channel_manager().await;
        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();

        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent_to_pool), 1);
        manager.allocate_tokens(1).await.unwrap();
        manager
            .handle_allocate_mining_job_token_success(None, token_success(&manager), None)
            .await
            .unwrap();
        let tokens = queued_tokens(&manager);

        let custom_job = manager
            .build_coinbase_only_custom_job(&template(3, true))
            .await
            .unwrap();
        assert!(custom_job.is_none());
        assert_eq!(queued_tokens(&manager), tokens);
    }

    // A prevhash naming the template the chain tip already activated changes nothing, with a
    // downstream whose group channel activated that template too: no declaration goes out again,
    // the shares cached on the chain tip stay, and JDC carries on.
    #[tokio::test]
    async fn a_repeated_prevhash_changes_nothing() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        let group_channel = GroupChannel::new_for_job_declaration_client(
            1,
            SOLO_FULL_EXTRANONCE_SIZE as usize,
            true,
            None,
            "test-jdc".to_string(),
        )
        .unwrap();
        manager
            .downstream
            .insert(1, Downstream::for_test(1, group_channel));
        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(3), None)
            .await
            .unwrap();
        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();
        assert_eq!(declarations_sent(&sent.jds), 1);
        manager.cached_shares.with_mut_or_default(3, |_| {});

        manager
            .handle_set_new_prev_hash(None, prev_hash(3), None)
            .await
            .unwrap();

        assert_eq!(declarations_sent(&sent.jds), 0);
        assert!(manager.cached_shares.contains_key(&3));
    }

    // The JDS accepts a declaration once: a replayed success finds the custom job proposed
    // already, and sends the Pool no second one.
    #[tokio::test]
    async fn replayed_declaration_success_proposes_once() {
        let (mut manager, sent, request_id) = manager_with_a_declaration().await;

        manager
            .handle_declare_mining_job_success(None, declaration_success(request_id), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent.pool), 1);
        manager
            .handle_declare_mining_job_success(None, declaration_success(request_id), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent.pool), 0);
    }

    // The Pool's answers concern only a custom job JDC proposed. A success or a stale rejection
    // for a declaration the JDS hasn't accepted yet, or for an unknown request, changes nothing,
    // and the declaration goes on as before.
    #[tokio::test]
    async fn pool_answers_for_jobs_it_was_not_proposed_change_nothing() {
        let (mut manager, sent, request_id) = manager_with_a_declaration().await;

        for request in [request_id, request_id + 100] {
            pool_success(&mut manager, custom_job_success(request, 7))
                .await
                .unwrap();
            pool_error(&mut manager, custom_job_stale(request, 7))
                .await
                .unwrap();
        }
        assert_eq!(declared_templates(&manager), vec![2]);

        manager
            .handle_declare_mining_job_success(None, declaration_success(request_id), None)
            .await
            .unwrap();
        assert_eq!(custom_jobs_sent(&sent.pool), 1);
        assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);
    }

    fn custom_job_refused(request_id: u32, channel_id: u32) -> SetCustomMiningJobErrorOwned {
        SetCustomMiningJobErrorOwned {
            channel_id,
            request_id,
            error_code: "invalid-mining-job-token".to_string().try_into().unwrap(),
        }
    }

    // A full-template JDC whose custom job for template 2 the Pool hasn't answered yet, on the
    // upstream channel 7.
    async fn manager_with_a_proposal() -> (ChannelManager, u32) {
        let (mut manager, _sent, request_id) = manager_with_a_declaration().await;
        manager
            .handle_declare_mining_job_success(None, declaration_success(request_id), None)
            .await
            .unwrap();
        (manager, request_id)
    }

    // A Pool answer whose request ID is a custom job JDC proposed, but which names another channel
    // than the custom job went on, is one JDC can't use: a success and an error, stale or not,
    // each make it fall back, and the custom job is left as it was.
    #[tokio::test]
    async fn pool_answers_on_another_channel_fall_back() {
        let (mut manager, request_id) = manager_with_a_proposal().await;
        let error = pool_success(&mut manager, custom_job_success(request_id, 8))
            .await
            .unwrap_err();
        assert!(matches!(error.action, Action::Fallback));
        assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);

        for answer in [custom_job_stale, custom_job_refused] {
            let (mut manager, request_id) = manager_with_a_proposal().await;
            let error = pool_error(&mut manager, answer(request_id, 8))
                .await
                .unwrap_err();
            assert!(matches!(error.action, Action::Fallback));
            assert_eq!(recorded_custom_jobs(&manager, 2).len(), 1);
        }
    }

    // A custom job the Pool acknowledges keeps the upstream job ID its shares go upstream with,
    // and a replayed acknowledgement changes nothing.
    #[tokio::test]
    async fn acknowledged_custom_job_keeps_its_upstream_job_id() {
        let (mut manager, _sent_to_pool, _sent_to_jds) = channel_manager().await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        let request_id = recorded_request_id(&manager, 2);
        // The upstream channel knows the chain tip the custom job is on, as it does once JDC
        // handles a prevhash.
        manager
            .upstream_channel
            .with(|channel| {
                channel
                    .as_mut()
                    .unwrap()
                    .on_chain_tip_update(prev_hash(1).into())
                    .unwrap()
            })
            .unwrap();

        for _ in 0..2 {
            pool_success(&mut manager, custom_job_success(request_id, 7))
                .await
                .unwrap();
        }

        let upstream_job_id = manager
            .custom_jobs
            .with(|jobs| jobs.upstream_job_id(2))
            .unwrap();
        assert_eq!(upstream_job_id, Some(9));
    }

    fn missing_transactions(request_id: u32) -> ProvideMissingTransactionsOwned {
        ProvideMissingTransactionsOwned {
            request_id,
            unknown_tx_position_list: vec![0].try_into().unwrap(),
        }
    }

    fn disclosures_sent(receiver: &Receiver<JobDeclarationOwned>) -> usize {
        std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|message| {
                matches!(
                    message,
                    JobDeclarationOwned::ProvideMissingTransactionsSuccess(_)
                )
            })
            .count()
    }

    // A declaration's transactions are disclosed once, while the JDS hasn't answered the
    // declaration: a repeated request gets nothing more.
    #[tokio::test]
    async fn missing_transactions_are_disclosed_once() {
        let (mut manager, sent, request_id) = manager_with_a_declaration().await;

        for _ in 0..2 {
            manager
                .handle_provide_missing_transactions(None, missing_transactions(request_id), None)
                .await
                .unwrap();
        }

        assert_eq!(disclosures_sent(&sent.jds), 1);
    }

    // Transactions are disclosed only for a declaration JDC sent and the JDS hasn't answered yet:
    // not for one a prevhash hasn't activated yet, which was never sent, not once the JDS has
    // accepted the declaration, and never for a coinbase-only custom job.
    #[tokio::test]
    async fn missing_transactions_are_disclosed_only_for_a_pending_declaration() {
        let (mut manager, sent) = channel_manager_in(ConfigJDCMode::FullTemplate).await;
        manager
            .handle_new_template(None, template(3, true), None)
            .await
            .unwrap();
        manager
            .handle_request_tx_data_success(None, transaction_data(3), None)
            .await
            .unwrap();
        let held = recorded_request_id(&manager, 3);
        manager
            .handle_provide_missing_transactions(None, missing_transactions(held), None)
            .await
            .unwrap();
        assert_eq!(disclosures_sent(&sent.jds), 0);

        let (mut manager, sent, request_id) = manager_with_a_declaration().await;
        manager
            .handle_declare_mining_job_success(None, declaration_success(request_id), None)
            .await
            .unwrap();
        manager
            .handle_provide_missing_transactions(None, missing_transactions(request_id), None)
            .await
            .unwrap();
        assert_eq!(disclosures_sent(&sent.jds), 0);

        let (mut manager, _sent_to_pool, sent_to_jds) = channel_manager().await;
        manager
            .handle_new_template(None, template(2, false), None)
            .await
            .unwrap();
        let proposed = recorded_request_id(&manager, 2);
        manager
            .handle_provide_missing_transactions(None, missing_transactions(proposed), None)
            .await
            .unwrap();
        assert_eq!(disclosures_sent(&sent_to_jds), 0);
    }
}
