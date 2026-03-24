use std::sync::atomic::Ordering;

use stratum_apps::stratum_core::{
    binary_sv2::{Seq064K, U256},
    bitcoin::{consensus, hashes::Hash, Amount, Transaction},
    channels_sv2::{chain_tip::ChainTip, outputs::deserialize_outputs},
    handlers_sv2::HandleTemplateDistributionMessagesFromServerAsync,
    job_declaration_sv2::DeclareMiningJob,
    mining_sv2::SetNewPrevHash as SetNewPrevHashMp,
    parsers_sv2::{JobDeclaration, Mining, TemplateDistribution, Tlv},
    template_distribution_sv2::*,
};
use tracing::{error, info, warn};

use crate::{
    channel_manager::{downstream_message_handler::RouteMessageTo, ChannelManager, DeclaredJob},
    error::{self, JDCError, JDCErrorKind},
    jd_mode::{get_jd_mode, JdMode},
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleTemplateDistributionMessagesFromServerAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(self
            .channel_manager_data
            .super_safe_lock(|data| data.negotiated_extensions.clone()))
    }

    // Handles a `NewTemplate` message from the Template Provider.
    //
    // Behavior depends on the JD mode:
    // - FullTemplate: sends a `RequestTransactionData` to start the declare-mining-job flow.
    // - CoinbaseOnly: sends a `SetCustomMiningJob` and continues with that flow.
    //
    // In both modes, the new template is stored and propagated to all
    // downstream channels, updating their state and dispatching the
    // appropriate mining job messages (standard, group, or extended).
    //
    // Also updates future/active template state and triggers token
    // allocation if needed.
    async fn handle_new_template(
        &mut self,
        _server_id: Option<usize>,
        msg: NewTemplate<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let coinbase_outputs = self.channel_manager_data.super_safe_lock(|data| {
            data.template_store
                .insert(msg.template_id, msg.clone().into_static());
            if msg.future_template {
                data.last_future_template = Some(msg.clone().into_static());
            }
            data.coinbase_outputs.clone()
        });

        let mut coinbase_outputs = deserialize_outputs(coinbase_outputs)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        if get_jd_mode() == JdMode::FullTemplate {
            let tx_data_request =
                TemplateDistribution::RequestTransactionData(RequestTransactionData {
                    template_id: msg.template_id,
                });

            self.channel_manager_channel
                .tp_sender
                .send(tx_data_request)
                .await
                .map_err(|_e| JDCError::shutdown(JDCErrorKind::ChannelErrorSender))?;
        }

        let (messages, token_consumed) = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let mut messages: Vec<RouteMessageTo> = Vec::new();
            coinbase_outputs[0].value = Amount::from_sat(msg.coinbase_tx_value_remaining);

            let coinbase_only_token = if !msg.future_template
                && get_jd_mode() == JdMode::CoinbaseOnly
                && channel_manager_data.upstream_channel.is_some()
                && channel_manager_data.last_new_prev_hash.is_some()
                && channel_manager_data.job_factory.is_some()
            {
                channel_manager_data.allocate_tokens.pop_front()
            } else {
                None
            };

            for (downstream_id, downstream) in channel_manager_data.downstream.iter_mut() {

                let messages_ = downstream.downstream_data.super_safe_lock(|data| {
                    data.group_channel.on_new_template(msg.clone().into_static(), coinbase_outputs.clone()).map_err(|e| {
                        tracing::error!("Error while adding template to group channel: {e:?}");
                        JDCError::shutdown(e)
                    })?;

                    let group_channel_job = match msg.future_template {
                        true => {
                            let future_job_id = data.group_channel.get_future_job_id_from_template_id(msg.template_id).expect("future job id must exist");
                            data.group_channel.get_future_job(future_job_id).expect("future job must exist")
                        }
                        false => {
                            data.group_channel.get_active_job().expect("active job must exist")
                        }
                    };

                    let mut messages: Vec<RouteMessageTo> = vec![];

                    // if we enter here, it means we are in Coinbase Only mode
                    // and we can already build the SetCustomMiningJob message
                    if let (Some(upstream_channel), Some(ref token)) = (
                        channel_manager_data.upstream_channel.as_mut(),
                        &coinbase_only_token,
                    ) {
                        let prevhash = channel_manager_data.last_new_prev_hash.clone()
    .expect("last_new_prev_hash checked in coinbase_only_token condition");
                        let request_id = channel_manager_data.request_id_factory.fetch_add(1, Ordering::Relaxed);
                        let job_factory = channel_manager_data.job_factory.as_mut().expect("job_factory checked in coinbase_only_token condition");
                        let full_extranonce_size = upstream_channel.get_full_extranonce_size();
                        let custom_job = job_factory.new_custom_job(upstream_channel.get_channel_id(), request_id, token.clone().mining_job_token, prevhash.clone().into(), msg.clone(), coinbase_outputs.clone(), full_extranonce_size);

                        if let Ok(custom_job) = custom_job {
                            let last_declare = DeclaredJob {
                                declare_mining_job: None,
                                template: msg.clone().into_static(),
                                prev_hash: Some(prevhash),
                                set_custom_mining_job: Some(custom_job.clone().into_static()),
                                coinbase_output: channel_manager_data.coinbase_outputs.clone(),
                                tx_list: Vec::new(),
                            };
                            channel_manager_data
                                .last_declare_job_store
                                .insert(request_id, last_declare);
                            messages.push(
                                Mining::SetCustomMiningJob(custom_job).into()
                            );
                        }
                    }

                    // if REQUIRES_STANDARD_JOBS is not set and the group channel is not empty,
                    // we need to send the NewExtendedMiningJob message to the group channel
                    let requires_standard_jobs = data.require_std_job;
                    let empty_group_channel = data.group_channel.get_channel_ids().is_empty();
                    if !requires_standard_jobs && !empty_group_channel {
                        messages.push((*downstream_id, Mining::NewExtendedMiningJob(group_channel_job.get_job_message().clone())).into());
                    }

                    // Extract group_job_id once for all channels that will use it
                    let group_job_id = group_channel_job.get_job_id();

                    // loop over every standard channel
                    // if REQUIRES_STANDARD_JOBS is not set, we need to call on_group_channel_job on each standard channel
                    // if REQUIRES_STANDARD_JOBS is set, we need to call on_new_template, and send individual NewMiningJob messages for each standard channel
                    for (channel_id, standard_channel) in data.standard_channels.iter_mut() {
                        if !requires_standard_jobs {
                            // update job ID to template ID mapping for standard channel
                            channel_manager_data
                                .downstream_channel_id_and_job_id_to_template_id
                                .insert(
                                    (*downstream_id, *channel_id, group_job_id).into(),
                                    msg.template_id,
                                );
                            // update the standard channel state with the group channel job
                            standard_channel.on_group_channel_job(group_channel_job.clone()).map_err(|e| {
                                tracing::error!("Error while adding group channel job to standard channel: {channel_id:?} {e:?}");
                                JDCError::shutdown(e)
                            })?;
                        } else {
                            standard_channel.on_new_template(msg.clone().into_static(), coinbase_outputs.clone()).map_err(|e| {
                                tracing::error!("Error while adding template to standard channel: {channel_id:?} {e:?}");
                                JDCError::shutdown(e)
                            })?;
                            match msg.future_template {
                                true => {
                                    let standard_job_id = standard_channel.get_future_job_id_from_template_id(msg.template_id).expect("future job id must exist");
                                    let standard_job = standard_channel.get_future_job(standard_job_id).expect("future job must exist");
                                    messages.push((*downstream_id, Mining::NewMiningJob(standard_job.get_job_message().clone())).into());
                                    // Update job ID to template ID mapping for standard channel
                                    channel_manager_data
                                        .downstream_channel_id_and_job_id_to_template_id
                                        .insert(
                                            (*downstream_id, *channel_id, standard_job_id).into(),
                                            msg.template_id,
                                        );
                                }
                                false => {
                                    let standard_job = standard_channel.get_active_job().expect("active job must exist");
                                    let active_job_id = standard_job.get_job_id();
                                    messages.push((*downstream_id, Mining::NewMiningJob(standard_job.get_job_message().clone())).into());
                                    // Update job ID to template ID mapping for standard channel
                                    channel_manager_data
                                        .downstream_channel_id_and_job_id_to_template_id
                                        .insert(
                                            (*downstream_id, *channel_id, active_job_id).into(),
                                            msg.template_id,
                                        );
                                }
                            }
                        }
                    }

                    // loop over every extended channel, and call on_group_channel_job on each extended channel
                    for (channel_id, extended_channel) in data.extended_channels.iter_mut() {
                        // update job ID to template ID mapping for extended channel
                        channel_manager_data
                            .downstream_channel_id_and_job_id_to_template_id
                            .insert(
                                (*downstream_id, *channel_id, group_job_id).into(),
                                msg.template_id,
                            );

                        // update the extended channel state with the group channel job
                        extended_channel.on_group_channel_job(group_channel_job.clone()).map_err(|e| {
                            tracing::error!("Error while adding group channel job to extended channel: {channel_id:?} {e:?}");
                            JDCError::shutdown(e)
                        })?;
                    }

                    Ok::<Vec<RouteMessageTo>, Self::Error>(messages)

                })?;
                messages.extend(messages_);
            }
            Ok::<(Vec<RouteMessageTo>, bool), Self::Error>((messages, coinbase_only_token.is_some()))
        })?;

        if token_consumed {
            _ = self.allocate_tokens(1).await;
        }

        for message in messages {
            let _ = message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }

    // Handles a `RequestTransactionDataError` message from the Template Provider.
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
            "template-id-not-found" | "stale-template-id"
        ) {
            return Ok(());
        }
        Err(JDCError::log(JDCErrorKind::TxDataError))
    }

    // Handles a `RequestTransactionDataSuccess` message from the Template Provider.
    //
    // Flow:
    // - If the template is not a future template, immediately declare a mining job to JDS.
    // - If the template is a future template:
    //   - Check if the current `prevhash` activates this template.
    //   - If activated → proceed with the normal declare job flow.
    //   - If not activated → cache it as a declare job for later propagation.
    async fn handle_request_tx_data_success(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestTransactionDataSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let transactions_data = msg.transaction_list;
        let excess_data = msg.excess_data;

        let coinbase_outputs = self
            .channel_manager_data
            .super_safe_lock(|data| data.coinbase_outputs.clone());

        let mut deserialized_outputs = deserialize_outputs(coinbase_outputs)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        let (token, template_message, request_id, prevhash) =
            self.channel_manager_data.super_safe_lock(|data| {
                (
                    data.allocate_tokens.pop_front(),
                    data.template_store.remove(&msg.template_id),
                    data.request_id_factory.fetch_add(1, Ordering::Relaxed),
                    data.last_new_prev_hash.clone(),
                )
            });

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
            .to_vec()
            .iter()
            .map(|raw_tx| consensus::deserialize(raw_tx).expect("invalid tx"))
            .collect();

        let wtxids_as_u256: Vec<U256<'static>> = tx_list
            .iter()
            .map(|tx| {
                let txid = tx.compute_wtxid();
                let byte_array: [u8; 32] = *txid.as_byte_array();
                U256::Owned(byte_array.to_vec())
            })
            .collect();

        let wtx_ids = Seq064K::new(wtxids_as_u256).map_err(JDCError::shutdown)?;
        let is_activated_future_template = template_message.future_template
            && prevhash
                .map(|prev_hash| prev_hash.template_id != template_message.template_id)
                .unwrap_or(true);

        let declare_job = self.channel_manager_data.super_safe_lock(|data| {
            let job_factory = data.job_factory.as_mut()?;
            let full_extranonce_size = data.upstream_channel.as_mut()?.get_full_extranonce_size();

            if let Ok((coinbase_tx_prefix, coinbase_tx_suffix)) = job_factory
                .new_coinbase_tx_prefix_and_suffix(
                    template_message.clone(),
                    deserialized_outputs.clone(),
                    full_extranonce_size,
                )
            {
                let version = template_message.version;

                let declare_job = DeclareMiningJob {
                    request_id,
                    mining_job_token: mining_token.to_vec().try_into().unwrap(),
                    version,
                    coinbase_tx_prefix: coinbase_tx_prefix.try_into().unwrap(),
                    coinbase_tx_suffix: coinbase_tx_suffix.try_into().unwrap(),
                    wtxid_list: wtx_ids,
                    excess_data: excess_data.to_vec().try_into().unwrap(),
                };

                let last_declare = DeclaredJob {
                    declare_mining_job: Some(declare_job.clone()),
                    template: template_message,
                    prev_hash: data.last_new_prev_hash.clone(),
                    set_custom_mining_job: None,
                    coinbase_output: reserialized_outputs,
                    tx_list: transactions_data.to_vec(),
                };

                data.last_declare_job_store.insert(request_id, last_declare);

                return Some(declare_job);
            }
            None
        });

        if is_activated_future_template {
            return Ok(());
        }

        if let Some(declare_job) = declare_job {
            let message = JobDeclaration::DeclareMiningJob(declare_job);
            _ = self.channel_manager_channel.jd_sender.send(message).await;
        }

        Ok(())
    }

    // Handles a `SetNewPrevHash` message:
    //
    // - Check `declare_job_cache` to see if the `prevhash` activates a future template.
    // - In FullTemplate mode → send a `DeclareMiningJob`.
    // - In CoinbaseOnly mode → send a `CustomMiningJob` for the activated future template.
    // - Update the upstream channel state.
    // - Update all downstream channels and propagate the new `prevhash` via `SetNewPrevHash`.
    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        msg: SetNewPrevHash<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let coinbase_outputs = self
            .channel_manager_data
            .super_safe_lock(|data| data.coinbase_outputs.clone());

        self.channel_manager_data
            .super_safe_lock(|data| data.cached_shares.clear());

        let outputs = deserialize_outputs(coinbase_outputs)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        let (future_template, declare_job) = self.channel_manager_data.super_safe_lock(|data| {
            if let Some(upstream_channel) = data.upstream_channel.as_mut() {
                if let Err(e) = upstream_channel.on_chain_tip_update(msg.clone().into()) {
                    error!(
                        "Couldn't update chaintip of the upstream channel: {msg}, error: {e:#?}"
                    );
                }
            }

            let declare_job = data
                .last_declare_job_store
                .values()
                .find(|declared_job| {
                    Some(declared_job.template.template_id)
                        == data.last_future_template.as_ref().map(|t| t.template_id)
                })
                .map(|declared_job| declared_job.declare_mining_job.clone());

            (data.last_future_template.clone(), declare_job)
        });

        if get_jd_mode() == JdMode::FullTemplate {
            if let Some(Some(job)) = declare_job {
                let message = JobDeclaration::DeclareMiningJob(job);

                self.channel_manager_channel
                    .jd_sender
                    .send(message)
                    .await
                    .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
            }
        }

        let (messages, token_consumed) = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            channel_manager_data.last_new_prev_hash = Some(msg.clone().into_static());
            channel_manager_data.last_declare_job_store.iter_mut().for_each(|(_k, v)| {
                if v.template.future_template && v.template.template_id == msg.template_id {
                    v.prev_hash = Some(msg.clone().into_static());
                    v.template.future_template = false;
                }
            });

            let mut messages: Vec<RouteMessageTo> = vec![];
            let mut token_consumed = false;

            if let Some(ref mut upstream_channel) = channel_manager_data.upstream_channel {
                _ = upstream_channel.on_chain_tip_update(msg.clone().into());

                if get_jd_mode() == JdMode::CoinbaseOnly
                    && channel_manager_data.job_factory.is_some()
                    && future_template.is_some()
                {
                    if let Some(token) = channel_manager_data.allocate_tokens.pop_front() {
                        token_consumed = true;
                        let job_factory = channel_manager_data.job_factory.as_mut().expect("job_factory checked above");
                        let template = future_template.clone().expect("future_template checked above");
                        let request_id = channel_manager_data.request_id_factory.fetch_add(1, Ordering::Relaxed);
                        let chain_tip = ChainTip::new(
                            msg.prev_hash.clone().into_static(),
                            msg.n_bits,
                            msg.header_timestamp,
                        );

                        let full_extranonce_size = upstream_channel.get_full_extranonce_size();

                        if let Ok(custom_job) = job_factory.new_custom_job(
                            upstream_channel.get_channel_id(),
                            request_id,
                            token.mining_job_token,
                            chain_tip,
                            template.clone(),
                            outputs,
                            full_extranonce_size,
                        ) {
                            let last_declare = DeclaredJob {
                                declare_mining_job: None,
                                template: template.into_static(),
                                prev_hash: Some(msg.clone().into_static()),
                                set_custom_mining_job: Some(custom_job.clone().into_static()),
                                coinbase_output: channel_manager_data.coinbase_outputs.clone(),
                                tx_list: vec![],
                            };

                            channel_manager_data.last_declare_job_store.insert(request_id, last_declare);
                            messages.push(Mining::SetCustomMiningJob(custom_job).into());
                        }
                    }
                }
            }

            for (downstream_id, downstream) in channel_manager_data.downstream.iter_mut() {
                let downstream_messages = downstream.downstream_data.super_safe_lock(|data| {
                    let mut messages: Vec<RouteMessageTo> = vec![];

                    // call on_set_new_prev_hash on the group channel to update the channel state
                    data.group_channel.on_set_new_prev_hash(msg.clone().into_static()).map_err(|e| {
                        tracing::error!("Error while adding new prev hash to group channel: {e:?}");
                        JDCError::fallback(e)
                    })?;

                    // did SetupConnection have the REQUIRES_STANDARD_JOBS flags set?
                    // if no, and the group channel is not empty, we need to send the SetNewPrevHash to the group channel
                    let requires_standard_jobs = data.require_std_job;
                    let empty_group_channel = data.group_channel.get_channel_ids().is_empty();
                    if !requires_standard_jobs && !empty_group_channel {
                        let group_channel_id = data.group_channel.get_group_channel_id();

                        let activated_group_job_id = data.group_channel.get_active_job().expect("active job must exist").get_job_id();

                        // Update job ID to template ID mapping for all channels using the group channel
                        // This is critical when a future template becomes active
                        for (channel_id, _) in data.standard_channels.iter() {
                            channel_manager_data
                                .downstream_channel_id_and_job_id_to_template_id
                                .insert(
                                    (*downstream_id, *channel_id, activated_group_job_id).into(),
                                    msg.template_id,
                                );
                        }
                        for (channel_id, _) in data.extended_channels.iter() {
                            channel_manager_data
                                .downstream_channel_id_and_job_id_to_template_id
                                .insert(
                                    (*downstream_id, *channel_id, activated_group_job_id).into(),
                                    msg.template_id,
                                );
                        }

                        let group_set_new_prev_hash_message = SetNewPrevHashMp {
                            channel_id: group_channel_id,
                            job_id: activated_group_job_id,
                            prev_hash: msg.prev_hash.clone(),
                            min_ntime: msg.header_timestamp,
                            nbits: msg.n_bits,
                        };
                        messages.push((*downstream_id, Mining::SetNewPrevHash(group_set_new_prev_hash_message)).into());
                    }

                    for (channel_id, standard_channel) in data.standard_channels.iter_mut() {
                        // call on_set_new_prev_hash on the standard channel to update the channel state
                        standard_channel.on_set_new_prev_hash(msg.clone().into_static()).map_err(|e| {
                            tracing::error!("Error while adding new prev hash to standard channel: {channel_id:?} {e:?}");
                            JDCError::fallback(e)
                        })?;

                        // did SetupConnection have the REQUIRES_STANDARD_JOBS flags set?
                        // if yes, we need to send the SetNewPrevHashMp to the standard channel
                        if data.require_std_job {
                            let activated_standard_job_id = standard_channel.get_active_job().expect("active job must exist").get_job_id();

                            // Update job ID to template ID mapping for this standard channel
                            // This is critical when a future template becomes active
                            channel_manager_data
                                .downstream_channel_id_and_job_id_to_template_id
                                .insert(
                                    (*downstream_id, *channel_id, activated_standard_job_id).into(),
                                    msg.template_id,
                                );

                            let standard_set_new_prev_hash_message = SetNewPrevHashMp {
                                channel_id: *channel_id,
                                job_id: activated_standard_job_id,
                                prev_hash: msg.prev_hash.clone(),
                                min_ntime: msg.header_timestamp,
                                nbits: msg.n_bits,
                            };
                            messages.push((*downstream_id, Mining::SetNewPrevHash(standard_set_new_prev_hash_message)).into());
                        }
                    }

                    // loop over every extended channel, and call on_set_new_prev_hash on each extended channel to update the channel state
                    // we're already sending the SetNewPrevHash message to the group channel
                    for (channel_id, extended_channel) in data.extended_channels.iter_mut() {
                        extended_channel.on_set_new_prev_hash(msg.clone().into_static()).map_err(|e| {
                            tracing::error!("Error while adding new prev hash to extended channel: {channel_id:?} {e:?}");
                            JDCError::fallback(e)
                        })?;
                    }

                    Ok::<Vec<RouteMessageTo>, Self::Error>(messages)
                })?;

                messages.extend(downstream_messages);
            }

            Ok::<(Vec<RouteMessageTo>, bool), Self::Error>((messages, token_consumed))
        })?;

        if token_consumed {
            _ = self.allocate_tokens(1).await;
        }

        for message in messages {
            let _ = message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }
}
