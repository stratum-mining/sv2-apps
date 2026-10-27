use std::sync::atomic::Ordering;
use stratum_apps::stratum_core::{
    binary_sv2::{Seq064KOwned, U256Owned},
    job_declaration_sv2::{AllocateMiningJobTokenSuccessOwned, DeclareMiningJobOwned},
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
    channel_manager::{ChannelManager, DeclaredJob, WaitingWork},
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

        let token = self
            .allocate_tokens
            .with(|tokens| tokens.pop_front())
            .map_err(JDCError::shutdown)?;
        let Some(token) = token else {
            return self
                .wait_for_token(WaitingWork::Declaration {
                    template,
                    transaction_data: msg,
                })
                .await;
        };
        _ = self.allocate_tokens(1).await;

        self.declare_template(template, msg, token).await
    }

    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        msg: SetNewPrevHashTdpOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

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
        let future_template_id = future_template.as_ref().map(|t| t.template_id);
        let mut declare_job = None;
        self.last_declare_job_store.for_each(|_, declared_job| {
            if Some(declared_job.template.template_id) == future_template_id {
                declare_job = Some(declared_job.declare_mining_job.clone());
            }
        });

        if self.mode.is_full_template() {
            if let Some(Some(job)) = declare_job {
                self.channel_manager_io
                    .jd_sender
                    .send(JobDeclarationOwned::DeclareMiningJob(job))
                    .await
                    .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
            }
        }

        self.last_new_prev_hash
            .set(Some(msg.clone()))
            .map_err(JDCError::shutdown)?;
        self.last_declare_job_store.for_each_mut(|_, declared_job| {
            if declared_job.template.future_template
                && declared_job.template.template_id == msg.template_id
            {
                declared_job.prev_hash = Some(msg.clone());
                declared_job.template.future_template = false;
            }
        });

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
    // Declares `template` to the JDS with `token`, from the transaction data the Template Provider
    // sent for it, and records the declaration. A future template's declaration is held back for
    // the prevhash that activates it, which sends it.
    pub(super) async fn declare_template(
        &self,
        template: NewTemplateOwned,
        transaction_data: RequestTransactionDataSuccessOwned,
        token: AllocateMiningJobTokenSuccessOwned,
    ) -> Result<(), JDCError<error::ChannelManager>> {
        let transactions_data = transaction_data.transaction_list;
        let excess_data = transaction_data.excess_data;
        let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
        let prevhash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;

        let mining_token = token.mining_job_token.clone();
        let deserialized_outputs = self.coinbase_outputs_for(&template)?;
        let reserialized_outputs = consensus::serialize(&deserialized_outputs);

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

        let is_activated_future_template = template.future_template
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

        let declare_job = if let Some(full_extranonce_size) = upstream_full_extranonce_size {
            self.job_factory
                .with(|job_factory| {
                    let job_factory = job_factory.as_mut()?;
                    if let Ok((coinbase_tx_prefix, coinbase_tx_suffix)) = job_factory
                        .new_coinbase_tx_prefix_and_suffix(
                            template.clone(),
                            deserialized_outputs.clone(),
                            full_extranonce_size,
                        )
                    {
                        let declare_job = DeclareMiningJobOwned {
                            request_id,
                            mining_job_token: mining_token,
                            version: template.version,
                            coinbase_tx_prefix: coinbase_tx_prefix.try_into().unwrap(),
                            coinbase_tx_suffix: coinbase_tx_suffix.try_into().unwrap(),
                            wtxid_list: wtx_ids,
                            excess_data,
                        };

                        self.last_declare_job_store.insert(
                            request_id,
                            DeclaredJob {
                                declare_mining_job: Some(declare_job.clone()),
                                template,
                                prev_hash: prevhash,
                                set_custom_mining_job: None,
                                coinbase_output: reserialized_outputs,
                                tx_list: transactions_data
                                    .iter_bytes()
                                    .map(|tx| tx.to_vec())
                                    .collect(),
                            },
                        );

                        return Some(declare_job);
                    }
                    None
                })
                .map_err(JDCError::shutdown)?
        } else {
            None
        };

        if is_activated_future_template {
            return Ok(());
        }

        if let Some(declare_job) = declare_job {
            _ = self
                .channel_manager_io
                .jd_sender
                .send(JobDeclarationOwned::DeclareMiningJob(declare_job))
                .await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::{Receiver, unbounded};
    use stratum_apps::{
        config_helpers::CoinbaseRewardScript,
        key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
        stratum_core::{
            binary_sv2::B0255Owned,
            bitcoin::{Amount, ScriptBuf, Target, TxOut},
            channels_sv2::{
                client::extended::ExtendedChannel, extranonce_manager::ExtranoncePrefix,
                server::jobs::factory::JobFactory,
            },
            codec_sv2::EncodableFrame as _,
            job_declaration_sv2::AllocateMiningJobTokenSuccessOwned,
            mining_sv2::SetCustomMiningJobOwned,
            parsers_sv2::{Mining, TemplateDistributionOwned},
        },
        tp_type::TemplateProviderType,
        utils::types::{InboundFrame, OutboundFrame},
    };

    use stratum_apps::stratum_core::handlers_sv2::HandleJobDeclarationMessagesFromServerOwnedAsync;

    use crate::{
        channel_manager::{MAX_REQUESTED_TEMPLATES, MAX_WAITING_FOR_TOKEN},
        config::{ConfigJDCMode, JobDeclaratorClientConfig, PoolConfig, ProtocolConfig},
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
    // on the chain tip of `prev_hash(1)`, holding one token, with no downstream connected.
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
            jds_coinbase_outputs(),
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

        // What the runtime and the JDS handshake leave behind once JDC works through a Pool.
        jd_mode.activate();
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
        manager
            .allocate_tokens
            .with(|tokens| {
                tokens.push_back(AllocateMiningJobTokenSuccessOwned {
                    request_id: 0,
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

    fn queued_tokens(manager: &ChannelManager) -> usize {
        manager.allocate_tokens.with(|tokens| tokens.len()).unwrap()
    }

    // The custom jobs recorded for `template_id`.
    fn recorded_custom_jobs(
        manager: &ChannelManager,
        template_id: u64,
    ) -> Vec<SetCustomMiningJobOwned> {
        let mut custom_jobs = Vec::new();
        manager.last_declare_job_store.for_each(|_, declared_job| {
            if declared_job.template.template_id == template_id {
                custom_jobs.extend(declared_job.set_custom_mining_job.clone());
            }
        });
        custom_jobs
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
            manager.allocate_tokens.with(|tokens| tokens.len()).unwrap(),
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

        let mut recorded = None;
        manager.last_declare_job_store.for_each(|_, declared_job| {
            if declared_job.template.template_id == 2 {
                recorded = Some(declared_job.coinbase_output.clone());
            }
        });
        let recorded = recorded.expect("the custom job is recorded");
        assert_eq!(
            consensus::deserialize::<Vec<TxOut>>(&recorded).unwrap(),
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
            manager.allocate_tokens.with(|tokens| tokens.len()).unwrap(),
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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

    // A token the JDS hands JDC.
    fn token_success() -> AllocateMiningJobTokenSuccessOwned {
        AllocateMiningJobTokenSuccessOwned {
            request_id: 7,
            mining_job_token: token(),
            coinbase_outputs: jds_coinbase_outputs().try_into().unwrap(),
        }
    }

    // The templates JDC has recorded a declaration for, in ascending order.
    fn declared_templates(manager: &ChannelManager) -> Vec<u64> {
        let mut templates = Vec::new();
        manager.last_declare_job_store.for_each(|_, declared_job| {
            if declared_job.declare_mining_job.is_some() {
                templates.push(declared_job.template.template_id);
            }
        });
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
            .await
            .unwrap();
        assert_eq!(declared_templates(&manager), vec![3]);
        for _ in 0..2 {
            manager
                .handle_allocate_mining_job_token_success(None, token_success(), None)
                .await
                .unwrap();
        }

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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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

        for _ in templates.clone() {
            manager
                .handle_allocate_mining_job_token_success(None, token_success(), None)
                .await
                .unwrap();
        }

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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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

        for _ in 0..2 {
            manager
                .handle_allocate_mining_job_token_success(None, token_success(), None)
                .await
                .unwrap();
        }

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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
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
            .allocate_tokens
            .with(|tokens| tokens.clear())
            .unwrap();
        for template_id in [2, 3] {
            manager
                .handle_new_template(None, template(template_id, false), None)
                .await
                .unwrap();
        }

        manager
            .handle_allocate_mining_job_token_success(None, token_success(), None)
            .await
            .unwrap();
        assert!(recorded_custom_jobs(&manager, 2).is_empty());
        assert_eq!(recorded_custom_jobs(&manager, 3).len(), 1);
        for _ in 0..2 {
            manager
                .handle_allocate_mining_job_token_success(None, token_success(), None)
                .await
                .unwrap();
        }

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
            .allocate_tokens
            .with(|tokens| tokens.clear())
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
            .handle_allocate_mining_job_token_success(None, token_success(), None)
            .await
            .unwrap();

        assert_eq!(custom_jobs_sent(&sent_to_pool_receiver), 0);
        assert!(recorded_custom_jobs(&manager, 2).is_empty());
        assert_eq!(queued_tokens(&manager), 1);
    }
}
