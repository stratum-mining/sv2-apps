use std::sync::atomic::Ordering;

use stratum_apps::stratum_core::{
    binary_sv2::{Seq064K, U256},
    bitcoin::{consensus, hashes::Hash, Amount, Transaction},
    channels_sv2::{chain_tip::ChainTip, outputs::deserialize_outputs},
    handlers_sv2::HandleTemplateDistributionMessagesFromServerAsync,
    job_declaration_sv2::DeclareMiningJob,
    mining_sv2::SetNewPrevHash as SetNewPrevHashMp,
    parsers_sv2::{JobDeclaration, Mining, Tlv},
    template_distribution_sv2::*,
};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::{ChannelManager, DeclaredJob},
    error::{self, JDCError, JDCErrorKind},
    utils::UpstreamState,
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleTemplateDistributionMessagesFromServerAsync for ChannelManager {
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
        msg: NewTemplate<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        self.template_store
            .insert(msg.template_id, msg.clone().into_static());
        if msg.future_template {
            self.last_future_template
                .set(Some(msg.clone().into_static()))
                .map_err(JDCError::shutdown)?;
        }

        let mut coinbase_outputs = deserialize_outputs(
            self.coinbase_outputs.get().map_err(JDCError::shutdown)?,
        )
        .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;
        coinbase_outputs[0].value = Amount::from_sat(msg.coinbase_tx_value_remaining);

        // Only request transaction data when the response can be consumed: building a
        // `DeclareMiningJob` requires the upstream channel and job factory, which only
        // exist once `UpstreamState::Connected`. Templates received before the channel
        // opens are still stored, and `handle_open_extended_mining_channel_success`
        // requests their transaction data once the channel is established.
        if self.mode.is_full_template() && self.upstream_state.get() == UpstreamState::Connected {
            self.request_transaction_data(msg.template_id).await?;
        }

        let upstream_ready = self
            .upstream_channel
            .with(|channel| channel.is_some())
            .map_err(JDCError::shutdown)?;
        let has_prev_hash = self
            .last_new_prev_hash
            .with(|prev_hash| prev_hash.is_some())
            .map_err(JDCError::shutdown)?;
        let has_job_factory = self
            .job_factory
            .with(|job_factory| job_factory.is_some())
            .map_err(JDCError::shutdown)?;

        let coinbase_only_token = if !msg.future_template
            && self.mode.is_coinbase_only()
            && upstream_ready
            && has_prev_hash
            && has_job_factory
        {
            self.allocate_tokens
                .with(|tokens| tokens.pop_front())
                .map_err(JDCError::shutdown)?
        } else {
            None
        };

        let upstream_info = self
            .upstream_channel
            .with(|channel| {
                channel
                    .as_ref()
                    .map(|channel| (channel.get_channel_id(), channel.get_full_extranonce_size()))
            })
            .map_err(JDCError::shutdown)?;
        let prevhash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;
        let coinbase_output_bytes = self.coinbase_outputs.get().map_err(JDCError::shutdown)?;

        let mut messages: Vec<crate::channel_manager::downstream_message_handler::RouteMessageTo> =
            Vec::new();

        self.downstream.try_for_each(|downstream_id, downstream| {
            let group_channel_job = downstream
                .group_channel
                .with(|group_channel| {
                    group_channel
                        .on_new_template(msg.clone().into_static(), coinbase_outputs.clone())
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

            if let (
                Some(token),
                Some((upstream_channel_id, full_extranonce_size)),
                Some(prevhash),
            ) = (&coinbase_only_token, upstream_info, prevhash.clone())
            {
                let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
                let custom_job = self
                    .job_factory
                    .with(|job_factory| {
                        let job_factory = job_factory
                            .as_mut()
                            .expect("job_factory checked before token extraction");
                        job_factory.new_custom_job(
                            upstream_channel_id,
                            request_id,
                            token.clone().mining_job_token,
                            prevhash.clone().into(),
                            msg.clone(),
                            coinbase_outputs.clone(),
                            full_extranonce_size,
                        )
                    })
                    .map_err(JDCError::shutdown)?;

                if let Ok(custom_job) = custom_job {
                    self.last_declare_job_store.insert(
                        request_id,
                        DeclaredJob {
                            declare_mining_job: None,
                            template: msg.clone().into_static(),
                            prev_hash: Some(prevhash),
                            set_custom_mining_job: Some(custom_job.clone().into_static()),
                            coinbase_output: coinbase_output_bytes.clone(),
                            tx_list: Vec::new(),
                        },
                    );
                    messages.push(Mining::SetCustomMiningJob(custom_job).into());
                }
            }

            let requires_standard_jobs = downstream.require_std_job.load(Ordering::Relaxed);
            let empty_group_channel = downstream
                .group_channel
                .with(|group_channel| group_channel.is_empty())
                .map_err(JDCError::shutdown)?;
            if !requires_standard_jobs && !empty_group_channel {
                messages.push(
                    (
                        downstream_id,
                        Mining::NewExtendedMiningJob(group_channel_job.get_job_message().clone()),
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
                            .on_new_template(msg.clone().into_static(), coinbase_outputs.clone())
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
                                    Mining::NewMiningJob(job.get_job_message().clone()),
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
                                    Mining::NewMiningJob(job.get_job_message().clone()),
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

        if coinbase_only_token.is_some() {
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
        msg: RequestTransactionDataError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
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
        msg: RequestTransactionDataSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        // Transaction data is only ever requested in full-template mode while the
        // upstream channel is open. Ignore anything else as unsolicited or late, so
        // stored templates stay available for the request sent when the channel opens.
        if !self.mode.is_full_template() || self.upstream_state.get() != UpstreamState::Connected {
            debug!(
                template_id = msg.template_id,
                "Ignoring unexpected RequestTransactionData.Success"
            );
            return Ok(());
        }

        let transactions_data = msg.transaction_list;
        let excess_data = msg.excess_data;

        let mut deserialized_outputs = deserialize_outputs(
            self.coinbase_outputs.get().map_err(JDCError::shutdown)?,
        )
        .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        let token = self
            .allocate_tokens
            .with(|tokens| tokens.pop_front())
            .map_err(JDCError::shutdown)?;
        let template_message = self
            .template_store
            .remove(&msg.template_id)
            .map(|(_, template)| template);
        let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
        let prevhash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;

        let Some(token) = token else {
            warn!(
                "No token available, discarding template id: {}",
                msg.template_id
            );
            _ = self.allocate_tokens(1).await;
            return Ok(());
        };
        _ = self.allocate_tokens(1).await;

        let Some(template_message) = template_message else {
            error!("Template not found, template id: {}", msg.template_id);
            return Err(JDCError::log(JDCErrorKind::TemplateNotFound(
                msg.template_id,
            )));
        };

        let mining_token = token.mining_job_token.clone();
        deserialized_outputs[0].value =
            Amount::from_sat(template_message.coinbase_tx_value_remaining);
        let reserialized_outputs = consensus::serialize(&deserialized_outputs);

        let tx_list: Vec<Transaction> = transactions_data
            .iter_bytes()
            .map(|raw_tx| consensus::deserialize(raw_tx).expect("invalid tx"))
            .collect();

        let wtxids_as_u256: Vec<U256<'static>> = tx_list
            .iter()
            .map(|tx| {
                let txid = tx.compute_wtxid();
                U256::from(*txid.as_byte_array())
            })
            .collect();
        let wtx_ids = Seq064K::new(wtxids_as_u256).map_err(JDCError::shutdown)?;

        let is_activated_future_template = template_message.future_template
            && prevhash
                .as_ref()
                .map(|prev_hash| prev_hash.template_id != template_message.template_id)
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
                            template_message.clone(),
                            deserialized_outputs.clone(),
                            full_extranonce_size,
                        )
                    {
                        let declare_job = DeclareMiningJob {
                            request_id,
                            mining_job_token: mining_token,
                            version: template_message.version,
                            coinbase_tx_prefix: coinbase_tx_prefix.try_into().unwrap(),
                            coinbase_tx_suffix: coinbase_tx_suffix.try_into().unwrap(),
                            wtxid_list: wtx_ids,
                            excess_data,
                        }
                        .into_static();

                        self.last_declare_job_store.insert(
                            request_id,
                            DeclaredJob {
                                declare_mining_job: Some(declare_job.clone()),
                                template: template_message,
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
                .send(JobDeclaration::DeclareMiningJob(declare_job))
                .await;
        }

        Ok(())
    }

    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        msg: SetNewPrevHash<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let outputs = deserialize_outputs(self.coinbase_outputs.get().map_err(JDCError::shutdown)?)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        self.cached_shares.clear();

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
                    .send(JobDeclaration::DeclareMiningJob(job))
                    .await
                    .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
            }
        }

        self.last_new_prev_hash
            .set(Some(msg.clone().into_static()))
            .map_err(JDCError::shutdown)?;
        self.last_declare_job_store.for_each_mut(|_, declared_job| {
            if declared_job.template.future_template
                && declared_job.template.template_id == msg.template_id
            {
                declared_job.prev_hash = Some(msg.clone().into_static());
                declared_job.template.future_template = false;
            }
        });

        let mut messages: Vec<crate::channel_manager::downstream_message_handler::RouteMessageTo> =
            Vec::new();
        let mut token_consumed = false;

        if self.mode.is_coinbase_only() && future_template.is_some() {
            if let Some(token) = self
                .allocate_tokens
                .with(|tokens| tokens.pop_front())
                .map_err(JDCError::shutdown)?
            {
                let upstream_info = self
                    .upstream_channel
                    .with(|channel| {
                        channel.as_ref().map(|channel| {
                            (channel.get_channel_id(), channel.get_full_extranonce_size())
                        })
                    })
                    .map_err(JDCError::shutdown)?;

                if let Some((upstream_channel_id, full_extranonce_size)) = upstream_info {
                    token_consumed = true;
                    let template = future_template
                        .clone()
                        .expect("future_template checked above");
                    let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
                    let chain_tip = ChainTip::new(
                        msg.prev_hash.clone().into_static(),
                        msg.n_bits,
                        msg.header_timestamp,
                    );

                    let custom_job = self
                        .job_factory
                        .with(|job_factory| {
                            let job_factory =
                                job_factory.as_mut().expect("job_factory should be present");
                            job_factory.new_custom_job(
                                upstream_channel_id,
                                request_id,
                                token.mining_job_token,
                                chain_tip,
                                template.clone(),
                                outputs.clone(),
                                full_extranonce_size,
                            )
                        })
                        .map_err(JDCError::shutdown)?;

                    if let Ok(custom_job) = custom_job {
                        self.last_declare_job_store.insert(
                            request_id,
                            DeclaredJob {
                                declare_mining_job: None,
                                template: template.into_static(),
                                prev_hash: Some(msg.clone().into_static()),
                                set_custom_mining_job: Some(custom_job.clone().into_static()),
                                coinbase_output: self
                                    .coinbase_outputs
                                    .get()
                                    .map_err(JDCError::shutdown)?,
                                tx_list: vec![],
                            },
                        );
                        messages.push(Mining::SetCustomMiningJob(custom_job).into());
                    }
                }
            }
        }

        self.downstream.try_for_each(|downstream_id, downstream| {
            let (group_channel_id, activated_group_job_id, empty_group_channel) = downstream
                .group_channel
                .with(|group_channel| {
                    group_channel
                        .on_set_new_prev_hash(msg.clone().into_static())
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
                        Mining::SetNewPrevHash(SetNewPrevHashMp {
                            channel_id: group_channel_id,
                            job_id: activated_group_job_id,
                            prev_hash: msg.prev_hash.clone(),
                            min_ntime: msg.header_timestamp,
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
                        .on_set_new_prev_hash(msg.clone().into_static())
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
                                Mining::SetNewPrevHash(SetNewPrevHashMp {
                                    channel_id,
                                    job_id: activated_standard_job_id,
                                    prev_hash: msg.prev_hash.clone(),
                                    min_ntime: msg.header_timestamp,
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
                        .on_set_new_prev_hash(msg.clone().into_static())
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
