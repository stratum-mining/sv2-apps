use std::sync::atomic::Ordering;

use stratum_apps::stratum_core::{
    binary_sv2::Str0255,
    bitcoin::{consensus::Decodable, Amount, Target, TxOut},
    channels_sv2::{
        server::{
            error::{ExtendedChannelError, StandardChannelError},
            extended::ExtendedChannel,
            jobs::job_store::DefaultJobStore,
            share_accounting::{ShareValidationError, ShareValidationResult},
            standard::StandardChannel,
        },
        Vardiff, VardiffState,
    },
    extensions_sv2::{
        UserIdentity, EXTENSION_TYPE_WORKER_HASHRATE_TRACKING, TLV_FIELD_TYPE_USER_IDENTITY,
    },
    handlers_sv2::{HandleMiningMessagesFromClientAsync, SupportedChannelTypes},
    mining_sv2::*,
    parsers_sv2::{Mining, TemplateDistribution, Tlv, TlvField},
    template_distribution_sv2::SubmitSolution,
};
use tracing::{error, info};

use crate::{
    channel_manager::{ChannelManager, RouteMessageTo, CLIENT_SEARCH_SPACE_BYTES},
    error::{self, PoolError, PoolErrorKind},
    utils::create_close_channel_msg,
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromClientAsync for ChannelManager {
    type Error = PoolError<error::ChannelManager>;

    fn get_channel_type_for_client(&self, _client_id: Option<usize>) -> SupportedChannelTypes {
        SupportedChannelTypes::GroupAndExtended
    }

    fn is_work_selection_enabled_for_client(&self, _client_id: Option<usize>) -> bool {
        true
    }

    fn is_client_authorized(
        &self,
        _client_id: Option<usize>,
        _user_identity: &Str0255,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn get_negotiated_extensions_with_client(
        &self,
        client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        let negotiated_extensions =
            self.channel_manager_data
                .super_safe_lock(|channel_manager_data| {
                    channel_manager_data
                        .downstream
                        .get(&downstream_id)
                        .map(|downstream| {
                            downstream
                                .downstream_data
                                .super_safe_lock(|data| data.negotiated_extensions.clone())
                        })
                        .expect("negotiated_extensions must be present")
                });
        Ok(negotiated_extensions)
    }

    async fn handle_close_channel(
        &mut self,
        client_id: Option<usize>,
        msg: CloseChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received Close Channel: {msg}");
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        self.channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id)
                else {
                    return Err(PoolError::disconnect(
                        PoolErrorKind::DownstreamNotFound(downstream_id),
                        downstream_id,
                    ));
                };

                downstream
                    .downstream_data
                    .super_safe_lock(|downstream_data| {
                        downstream_data.standard_channels.remove(&msg.channel_id);
                        downstream_data.extended_channels.remove(&msg.channel_id);
                    });
                channel_manager_data
                    .vardiff
                    .remove(&(downstream_id, msg.channel_id).into());
                Ok(())
            })
    }

    async fn handle_open_standard_mining_channel(
        &mut self,
        client_id: Option<usize>,
        msg: OpenStandardMiningChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let request_id = msg.get_request_id_as_u32();
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        info!("Received OpenStandardMiningChannel: {}", msg);

        let messages = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id) else {
                return Err(PoolError::disconnect(PoolErrorKind::DownstreamIdNotFound, downstream_id));
            };

            if downstream.requires_custom_work.load(Ordering::SeqCst) {
                error!("OpenStandardMiningChannel: Standard Channels are not supported for this connection");
                let open_standard_mining_channel_error = OpenMiningChannelError {
                    request_id,
                    error_code: "standard-channels-not-supported-for-custom-work"
                        .to_string()
                        .try_into()
                        .expect("error code must be valid string"),
                };
                return Ok(vec![(downstream_id, Mining::OpenMiningChannelError(open_standard_mining_channel_error)).into()]);
            }

            let Some(last_future_template) = channel_manager_data.last_future_template.clone() else {
                return Err(PoolError::disconnect(PoolErrorKind::FutureTemplateNotPresent, downstream_id));
            };

            let Some(last_set_new_prev_hash_tdp) = channel_manager_data.last_new_prev_hash.clone() else {
                return Err(PoolError::disconnect(PoolErrorKind::LastNewPrevhashNotFound, downstream_id));
            };


            let pool_coinbase_output = TxOut {
                value: Amount::from_sat(last_future_template.coinbase_tx_value_remaining),
                script_pubkey: self.coinbase_reward_script.script_pubkey(),
            };

            downstream.downstream_data.super_safe_lock(|downstream_data| {
                let nominal_hash_rate = msg.nominal_hash_rate;
                let requested_max_target = Target::from_le_bytes(msg.max_target.inner_as_ref().try_into().unwrap());
                let extranonce_prefix = channel_manager_data.extranonce_prefix_factory_standard.next_prefix_standard().map_err(PoolError::shutdown)?;

                let channel_id = downstream_data.channel_id_factory.fetch_add(1, Ordering::SeqCst);
                let job_store = DefaultJobStore::new();

                let mut standard_channel = match StandardChannel::new_for_pool(channel_id, user_identity.to_string(), extranonce_prefix.to_vec(), requested_max_target, nominal_hash_rate, self.share_batch_size, self.shares_per_minute, job_store, self.pool_tag_string.clone()) {
                    Ok(channel) => channel,
                    Err(e) => match e {
                        StandardChannelError::InvalidNominalHashrate => {
                            error!("OpenMiningChannelError: invalid-nominal-hashrate");
                            let open_standard_mining_channel_error = OpenMiningChannelError {
                                request_id,
                                error_code: "invalid-nominal-hashrate"
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            return Ok(vec![(downstream_id, Mining::OpenMiningChannelError(open_standard_mining_channel_error)).into()]);
                        }
                        StandardChannelError::RequestedMaxTargetOutOfRange => {
                            error!("OpenMiningChannelError: max-target-out-of-range");
                            let open_standard_mining_channel_error = OpenMiningChannelError {
                                request_id,
                                error_code: "max-target-out-of-range"
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            return Ok(vec![(downstream_id, Mining::OpenMiningChannelError(open_standard_mining_channel_error)).into()]);
                        }
                        _ => {
                            error!("error in handle_open_standard_mining_channel: {:?}", e);
                            return Err(PoolError::disconnect(PoolErrorKind::ChannelErrorSender, downstream_id) );
                        }
                    },
                };

                let group_channel_id = downstream_data.group_channel.get_group_channel_id();
                let extranonce_prefix_size = standard_channel.get_extranonce_prefix().len();

                let open_standard_mining_channel_success = OpenStandardMiningChannelSuccess {
                    request_id: msg.request_id,
                    channel_id,
                    target: standard_channel.get_target().to_le_bytes().into(),
                    extranonce_prefix: standard_channel.get_extranonce_prefix().clone().try_into().expect("Extranonce_prefix must be valid"),
                    group_channel_id
                }.into_static();

                let mut  messages: Vec<RouteMessageTo> = Vec::new();

                messages.push((downstream_id, Mining::OpenStandardMiningChannelSuccess(open_standard_mining_channel_success)).into());

                let template_id = last_future_template.template_id;

                // create a future standard job based on the last future template
                standard_channel.on_new_template(last_future_template, vec![pool_coinbase_output.clone()]).map_err(PoolError::shutdown)?;
                let future_standard_job_id = standard_channel
                    .get_future_job_id_from_template_id(template_id)
                    .expect("future job id must exist");
                let future_standard_job = standard_channel
                    .get_future_job(future_standard_job_id)
                    .expect("future job must exist");
                let future_standard_job_message =
                    future_standard_job.get_job_message().clone().into_static();

                messages.push((downstream_id, Mining::NewMiningJob(future_standard_job_message)).into());
                let prev_hash = last_set_new_prev_hash_tdp.prev_hash.clone();
                let header_timestamp = last_set_new_prev_hash_tdp.header_timestamp;
                let n_bits = last_set_new_prev_hash_tdp.n_bits;
                let set_new_prev_hash_mining = SetNewPrevHash {
                    channel_id,
                    job_id: future_standard_job_id,
                    prev_hash,
                    min_ntime: header_timestamp,
                    nbits: n_bits,
                };

                standard_channel
                .on_set_new_prev_hash(last_set_new_prev_hash_tdp.clone()).map_err(PoolError::shutdown)?;

                messages.push((downstream_id, Mining::SetNewPrevHash(set_new_prev_hash_mining)).into());

                downstream_data.standard_channels.insert(channel_id, standard_channel);
                if !downstream.requires_standard_jobs.load(Ordering::SeqCst) {
                    downstream_data.group_channel.add_channel_id(channel_id, extranonce_prefix_size).map_err(|e| {
                        error!("Failed to add channel id to group channel: {:?}", e);
                        PoolError::shutdown(e)
                    })?;
                }
                let vardiff = VardiffState::new().map_err(PoolError::shutdown)?;
                channel_manager_data.vardiff.insert((downstream_id, channel_id).into(), vardiff);

                Ok(messages)
            })
        })?;

        for message in messages {
            message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }

    async fn handle_open_extended_mining_channel(
        &mut self,
        client_id: Option<usize>,
        msg: OpenExtendedMiningChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let request_id = msg.get_request_id_as_u32();
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        info!("Received OpenExtendedMiningChannel: {}", msg);

        let nominal_hash_rate = msg.nominal_hash_rate;
        let requested_max_target =
            Target::from_le_bytes(msg.max_target.inner_as_ref().try_into().unwrap());
        let requested_min_rollable_extranonce_size = msg.min_extranonce_size;

        let messages = self
            .channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id)
                else {
                    return Err(PoolError::disconnect(PoolErrorKind::DownstreamIdNotFound, downstream_id));
                };
                downstream
                    .downstream_data
                    .super_safe_lock(|downstream_data| {
                        let mut messages: Vec<RouteMessageTo> = Vec::new();

                        let extranonce_prefix = match channel_manager_data
                            .extranonce_prefix_factory_extended
                            .next_prefix_extended(requested_min_rollable_extranonce_size.into())
                        {
                            Ok(extranonce_prefix) => extranonce_prefix.to_vec(),
                            Err(_) => {
                                error!("OpenMiningChannelError: min-extranonce-size-too-large");
                                let open_extended_mining_channel_error = OpenMiningChannelError {
                                    request_id,
                                    error_code: "min-extranonce-size-too-large"
                                        .to_string()
                                        .try_into()
                                        .expect("error code must be valid string"),
                                };
                                return Ok(vec![(
                                    downstream_id,
                                    Mining::OpenMiningChannelError(
                                        open_extended_mining_channel_error,
                                    ),
                                )
                                    .into()]);
                            }
                        };

                        let channel_id = downstream_data
                            .channel_id_factory
                            .fetch_add(1, Ordering::SeqCst);
                        let job_store = DefaultJobStore::new();

                        let mut extended_channel = match ExtendedChannel::new_for_pool(
                            channel_id,
                            user_identity.to_string(),
                            extranonce_prefix.clone(),
                            requested_max_target,
                            nominal_hash_rate,
                            true, // version rolling always allowed
                            CLIENT_SEARCH_SPACE_BYTES as u16,
                            self.share_batch_size,
                            self.shares_per_minute,
                            job_store,
                            self.pool_tag_string.clone(),
                        ) {
                            Ok(channel) => channel,
                            Err(e) => match e {
                                ExtendedChannelError::InvalidNominalHashrate => {
                                    error!("OpenMiningChannelError: invalid-nominal-hashrate");
                                    let open_extended_mining_channel_error =
                                        OpenMiningChannelError {
                                            request_id,
                                            error_code: "invalid-nominal-hashrate"
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                    return Ok(vec![(
                                        downstream_id,
                                        Mining::OpenMiningChannelError(
                                            open_extended_mining_channel_error,
                                        ),
                                    )
                                        .into()]);
                                }
                                ExtendedChannelError::RequestedMaxTargetOutOfRange => {
                                    error!("OpenMiningChannelError: max-target-out-of-range");
                                    let open_extended_mining_channel_error =
                                        OpenMiningChannelError {
                                            request_id,
                                            error_code: "max-target-out-of-range"
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                    return Ok(vec![(
                                        downstream_id,
                                        Mining::OpenMiningChannelError(
                                            open_extended_mining_channel_error,
                                        ),
                                    )
                                        .into()]);
                                }
                                ExtendedChannelError::RequestedMinExtranonceSizeTooLarge => {
                                    error!("OpenMiningChannelError: min-extranonce-size-too-large");
                                    let open_extended_mining_channel_error =
                                        OpenMiningChannelError {
                                            request_id,
                                            error_code: "min-extranonce-size-too-large"
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                    return Ok(vec![(
                                        downstream_id,
                                        Mining::OpenMiningChannelError(
                                            open_extended_mining_channel_error,
                                        ),
                                    )
                                        .into()]);
                                }
                                e => {
                                    error!("error in handle_open_extended_mining_channel: {:?}", e);
                                    return Err(PoolError::disconnect(e, downstream_id))?;
                                }
                            },
                        };

                        let group_channel_id = downstream_data.group_channel.get_group_channel_id();

                        let open_extended_mining_channel_success =
                            OpenExtendedMiningChannelSuccess {
                                request_id,
                                channel_id,
                                target: extended_channel.get_target().to_le_bytes().into(),
                                extranonce_prefix: extended_channel
                                    .get_extranonce_prefix()
                                    .clone()
                                    .try_into().map_err(PoolError::shutdown)?,
                                extranonce_size: extended_channel.get_rollable_extranonce_size(),
                                group_channel_id,
                            }
                            .into_static();
                        info!("Sending OpenExtendedMiningChannel.Success (downstream_id: {downstream_id}): {open_extended_mining_channel_success}");

                        messages.push(
                            (
                                downstream_id,
                                Mining::OpenExtendedMiningChannelSuccess(
                                    open_extended_mining_channel_success,
                                ),
                            )
                                .into(),
                        );

                        let Some(last_set_new_prev_hash_tdp) =
                            channel_manager_data.last_new_prev_hash.clone()
                        else {
                            return Err(PoolError::disconnect(PoolErrorKind::LastNewPrevhashNotFound, downstream_id));
                        };

                        let Some(last_future_template) =
                            channel_manager_data.last_future_template.clone()
                        else {
                            return Err(PoolError::disconnect(PoolErrorKind::FutureTemplateNotPresent,downstream_id));
                        };

                        // if the client requires custom work, we don't need to send any extended
                        // jobs so we just process the SetNewPrevHash
                        // message
                        if downstream.requires_custom_work.load(Ordering::SeqCst) {
                            extended_channel.on_set_new_prev_hash(last_set_new_prev_hash_tdp).map_err(PoolError::shutdown)?;
                            // if the client does not require custom work, we need to send the
                            // future extended job
                            // and the SetNewPrevHash message
                        } else {
                            let pool_coinbase_output = TxOut {
                                value: Amount::from_sat(
                                    last_future_template.coinbase_tx_value_remaining,
                                ),
                                script_pubkey: self.coinbase_reward_script.script_pubkey(),
                            };

                            extended_channel.on_new_template(
                                last_future_template.clone(),
                                vec![pool_coinbase_output],
                            ).map_err(PoolError::shutdown)?;

                            let future_extended_job_id = extended_channel
                                .get_future_job_id_from_template_id(last_future_template.template_id)
                                .expect("future job id must exist");
                            let future_extended_job = extended_channel
                                .get_future_job(future_extended_job_id)
                                .expect("future job must exist");

                            let future_extended_job_message =
                                future_extended_job.get_job_message().clone().into_static();

                            // send this future job as new job message
                            // to be immediately activated with the subsequent SetNewPrevHash
                            // message
                            messages.push(
                                (
                                    downstream_id,
                                    Mining::NewExtendedMiningJob(future_extended_job_message),
                                )
                                    .into(),
                            );

                            // SetNewPrevHash message activates the future job
                            let prev_hash = last_set_new_prev_hash_tdp.prev_hash.clone();
                            let header_timestamp = last_set_new_prev_hash_tdp.header_timestamp;
                            let n_bits = last_set_new_prev_hash_tdp.n_bits;
                            let set_new_prev_hash_mining = SetNewPrevHash {
                                channel_id,
                                job_id: future_extended_job_id,
                                prev_hash,
                                min_ntime: header_timestamp,
                                nbits: n_bits,
                            };

                            extended_channel.on_set_new_prev_hash(last_set_new_prev_hash_tdp).map_err(PoolError::shutdown)?;

                            messages.push(
                                (
                                    downstream_id,
                                    Mining::SetNewPrevHash(set_new_prev_hash_mining),
                                )
                                    .into(),
                            );

                            let full_extranonce_size = extended_channel.get_full_extranonce_size();
                            downstream_data.group_channel.add_channel_id(channel_id, full_extranonce_size).map_err(|e| {
                                error!("Failed to add channel id to group channel: {:?}", e);
                                PoolError::shutdown(e)
                            })?;
                        }

                        downstream_data
                            .extended_channels
                            .insert(channel_id, extended_channel);
                        let vardiff = VardiffState::new().map_err(PoolError::shutdown)?;
                        channel_manager_data
                            .vardiff
                            .insert((downstream_id, channel_id).into(), vardiff);

                        Ok(messages)
                    })
            })?;

        for message in messages {
            message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }

    async fn handle_submit_shares_standard(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesStandard,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesStandard: {msg}");
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let messages = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let channel_id = msg.channel_id;

            let Some(downstream) = channel_manager_data.downstream.get(&downstream_id) else {
                return Err(PoolError::disconnect(PoolErrorKind::DownstreamNotFound(downstream_id), downstream_id));
            };

            downstream.downstream_data.super_safe_lock(|downstream_data| {
                let mut messages: Vec<RouteMessageTo> = Vec::new();
                let Some(standard_channel) = downstream_data.standard_channels.get_mut(&channel_id) else {
                    let submit_shares_error = SubmitSharesError {
                        channel_id,
                        sequence_number: msg.sequence_number,
                        error_code: "invalid-channel-id"
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    };
                    error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-channel-id ❌", downstream_id, channel_id, msg.sequence_number);
                    return Ok(vec![(downstream_id, Mining::SubmitSharesError(submit_shares_error)).into()]);
                };

                let Some(vardiff) = channel_manager_data.vardiff.get_mut(&(downstream_id, channel_id).into()) else {
                    return Ok(vec![(downstream_id, Mining::CloseChannel(create_close_channel_msg(channel_id, "invalid-channel-id"))).into()]);
                };

                let res = standard_channel.validate_share(msg.clone());
                vardiff.increment_shares_since_last_update();


                match res {
                    Ok(ShareValidationResult::Valid(share_hash)) => {
                        let share_accounting = standard_channel.get_share_accounting();
                        if share_accounting.should_acknowledge() {
                            let success = SubmitSharesSuccess {
                                channel_id,
                                last_sequence_number: share_accounting.get_last_share_sequence_number(),
                                new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                                new_shares_sum: share_accounting.get_last_batch_work_sum(),
                            };
                            info!("SubmitSharesStandard: {} ✅", success);
                            messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
                        } else {
                            let share_work = standard_channel.get_target().difficulty_float();
                            info!(
                                "SubmitSharesStandard: valid share | downstream_id: {}, channel_id: {}, sequence_number: {}, share_hash: {}, share_work: {} ✅",
                                downstream_id, channel_id, msg.sequence_number, share_hash, share_work
                            );
                        }

                    }
                    Ok(ShareValidationResult::BlockFound(share_hash, template_id, coinbase)) => {
                        info!("SubmitSharesStandard: 💰 Block Found!!! 💰{share_hash}");
                        // if we have a template id (i.e.: this was not a custom job)
                        // we can propagate the solution to the TP
                        if let Some(template_id) = template_id {
                            info!("SubmitSharesStandard: Propagating solution to the Template Provider.");
                            let solution = SubmitSolution {
                                template_id,
                                version: msg.version,
                                header_timestamp: msg.ntime,
                                header_nonce: msg.nonce,
                                coinbase_tx: coinbase.try_into().map_err(PoolError::shutdown)?,
                            };
                            messages.push(TemplateDistribution::SubmitSolution(solution).into());
                        }
                        let share_accounting = standard_channel.get_share_accounting();
                        let success = SubmitSharesSuccess {
                            channel_id,
                            last_sequence_number: share_accounting.get_last_share_sequence_number(),
                            new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                            new_shares_sum: share_accounting.get_last_batch_work_sum(),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
                    }
                    Err(ShareValidationError::Invalid) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-share ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "invalid-share"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };

                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::Stale) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: stale-share ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "stale-share"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::InvalidJobId) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-job-id ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "invalid-job-id"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::DoesNotMeetTarget) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: difficulty-too-low ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "difficulty-too-low"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::DuplicateShare) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: duplicate-share ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "duplicate-share"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(e) => {
                        return Err(PoolError::disconnect(e, downstream_id))?;
                    }
                }

                Ok(messages)
            })
        })?;

        for message in messages {
            message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }

    async fn handle_submit_shares_extended(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesExtended<'_>,
        tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesExtended: {msg}");
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        // Extract user_identity from TLV fields if the extension is negotiated
        let negotiated_extensions = self.get_negotiated_extensions_with_client(client_id);
        let user_identity = if negotiated_extensions
            .as_ref()
            .is_ok_and(|exts| exts.contains(&EXTENSION_TYPE_WORKER_HASHRATE_TRACKING))
        {
            tlv_fields.and_then(|tlvs| {
                tlvs.iter()
                    .find(|tlv| {
                        tlv.r#type.extension_type == EXTENSION_TYPE_WORKER_HASHRATE_TRACKING
                            && tlv.r#type.field_type == TLV_FIELD_TYPE_USER_IDENTITY
                    })
                    .and_then(|tlv| UserIdentity::from_tlv(tlv).ok())
            })
        } else {
            None
        };

        let messages = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let channel_id = msg.channel_id;
            let Some(downstream) = channel_manager_data.downstream.get(&downstream_id) else {
                return Err(PoolError::disconnect(PoolErrorKind::DownstreamNotFound(downstream_id), downstream_id));
            };

            downstream.downstream_data.super_safe_lock(|downstream_data| {
                let mut messages: Vec<RouteMessageTo> = Vec::new();
                let Some(extended_channel) = downstream_data.extended_channels.get_mut(&channel_id) else {
                    let error = SubmitSharesError {
                        channel_id,
                        sequence_number: msg.sequence_number,
                        error_code: "invalid-channel-id"
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    };
                    error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-channel-id ❌", downstream_id, channel_id, msg.sequence_number);
                    return Ok(vec![(downstream_id, Mining::SubmitSharesError(error)).into()]);
                };

                if let Some(_user_identity) = user_identity {
                    // here we have the UserIdentity TLV, so we can use it to enhance monitoring of individual miners in the future
                }

                let Some(vardiff) = channel_manager_data.vardiff.get_mut(&(downstream_id, channel_id).into()) else {
                    return Ok(vec![(downstream_id, Mining::CloseChannel(create_close_channel_msg(channel_id, "invalid-channel-id"))).into()]);
                };

                let res = extended_channel.validate_share(msg.clone());
                vardiff.increment_shares_since_last_update();

                match res {
                    Ok(ShareValidationResult::Valid(share_hash)) => {
                        let share_accounting = extended_channel.get_share_accounting();
                        if share_accounting.should_acknowledge() {
                            let success = SubmitSharesSuccess {
                                channel_id,
                                last_sequence_number: share_accounting.get_last_share_sequence_number(),
                                new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                                new_shares_sum: share_accounting.get_last_batch_work_sum(),
                            };
                            info!("SubmitSharesExtended: {} ✅", success);
                            messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
                        } else {
                            let share_work = extended_channel.get_target().difficulty_float();
                            info!(
                                "SubmitSharesExtended: valid share | downstream_id: {}, channel_id: {}, sequence_number: {}, share_hash: {}, share_work: {} ✅",
                                downstream_id, channel_id, msg.sequence_number, share_hash, share_work
                            );
                        }
                    }
                    Ok(ShareValidationResult::BlockFound(share_hash, template_id, coinbase)) => {
                        info!("SubmitSharesExtended: 💰 Block Found!!! 💰{share_hash}");
                        // if we have a template id (i.e.: this was not a custom job)
                        // we can propagate the solution to the TP
                        if let Some(template_id) = template_id {
                            info!("SubmitSharesExtended: Propagating solution to the Template Provider.");
                            let solution = SubmitSolution {
                                template_id,
                                version: msg.version,
                                header_timestamp: msg.ntime,
                                header_nonce: msg.nonce,
                                coinbase_tx: coinbase.try_into().map_err(PoolError::shutdown)?,
                            };
                            messages.push(TemplateDistribution::SubmitSolution(solution).into());
                        }
                        let share_accounting = extended_channel.get_share_accounting();
                        let success = SubmitSharesSuccess {
                            channel_id,
                            last_sequence_number: share_accounting.get_last_share_sequence_number(),
                            new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                            new_shares_sum: share_accounting.get_last_batch_work_sum(),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
                    }
                    Err(ShareValidationError::Invalid) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-share ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "invalid-share"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::Stale) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: stale-share ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "stale-share"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::InvalidJobId) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-job-id ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "invalid-job-id"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::DoesNotMeetTarget) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: difficulty-too-low ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "difficulty-too-low"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::DuplicateShare) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: duplicate-share ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "duplicate-share"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(ShareValidationError::BadExtranonceSize) => {
                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: bad-extranonce-size ❌", downstream_id, channel_id, msg.sequence_number);
                        let error = SubmitSharesError {
                            channel_id: msg.channel_id,
                            sequence_number: msg.sequence_number,
                            error_code: "bad-extranonce-size"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        messages.push((downstream_id, Mining::SubmitSharesError(error)).into());
                    }
                    Err(e) => {
                        return Err(PoolError::disconnect(e, downstream_id))?;
                    }
                }

                Ok(messages)
            })
        })?;

        for message in messages {
            message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }

    async fn handle_update_channel(
        &mut self,
        client_id: Option<usize>,
        msg: UpdateChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let messages: Vec<RouteMessageTo> = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let Some(downstream) = channel_manager_data.downstream.get(&downstream_id) else {
                return Err(PoolError::disconnect(PoolErrorKind::DownstreamNotFound(downstream_id), downstream_id));
            };

            downstream.downstream_data.super_safe_lock(|downstream_data| {
                let mut messages = Vec::new();
                let channel_id = msg.channel_id;
                let new_nominal_hash_rate = msg.nominal_hash_rate;
                let requested_maximum_target = Target::from_le_bytes(msg.maximum_target.inner_as_ref().try_into().unwrap());

                if let Some(standard_channel) = downstream_data.standard_channels.get_mut(&channel_id) {
                    let res = standard_channel
                                    .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                    match res {
                        Ok(_) => {}
                        Err(e) => {
                            error!("UpdateChannelError: {:?}", e);
                            match e {
                                StandardChannelError::InvalidNominalHashrate => {
                                    error!("UpdateChannelError: invalid-nominal-hashrate");
                                    let update_channel_error = UpdateChannelError {
                                        channel_id,
                                        error_code: "invalid-nominal-hashrate"
                                            .to_string()
                                            .try_into()
                                            .expect("error code must be valid string"),
                                    };
                                    messages.push((downstream_id, Mining::UpdateChannelError(update_channel_error)).into());
                                }
                                StandardChannelError::RequestedMaxTargetOutOfRange => {
                                    error!("UpdateChannelError: requested-max-target-out-of-range");
                                    let update_channel_error = UpdateChannelError {
                                        channel_id,
                                        error_code: "requested-max-target-out-of-range"
                                            .to_string()
                                            .try_into()
                                            .expect("error code must be valid string"),
                                    };
                                    messages.push((downstream_id, Mining::UpdateChannelError(update_channel_error)).into());
                                }
                                // We don't care about other variants as they are not 
                                // associated to Update channel, and we will never
                                // encounter it.
                                _ => unreachable!()
                            }
                        }
                    }
                    let new_target = standard_channel.get_target();
                    let set_target = SetTarget {
                        channel_id,
                        maximum_target: new_target.to_le_bytes().into(),
                    };
                    messages.push((downstream_id, Mining::SetTarget(set_target)).into());
                } else if let Some(extended_channel) = downstream_data.extended_channels.get_mut(&channel_id) {
                    let res = extended_channel
                                    .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                    match res {
                        Ok(_) => {}
                        Err(e) => {
                            error!("UpdateChannelError: {:?}", e);
                            match e {
                                ExtendedChannelError::InvalidNominalHashrate => {
                                    error!("UpdateChannelError: invalid-nominal-hashrate");
                                    let update_channel_error = UpdateChannelError {
                                        channel_id,
                                        error_code: "invalid-nominal-hashrate"
                                            .to_string()
                                            .try_into()
                                            .expect("error code must be valid string"),
                                    };
                                    messages.push((downstream_id, Mining::UpdateChannelError(update_channel_error)).into());
                                }
                                ExtendedChannelError::RequestedMaxTargetOutOfRange => {
                                    error!("UpdateChannelError: max-target-out-of-range");
                                    let update_channel_error = UpdateChannelError {
                                        channel_id,
                                        error_code: "max-target-out-of-range"
                                            .to_string()
                                            .try_into()
                                            .expect("error code must be valid string"),
                                    };
                                    messages.push((downstream_id, Mining::UpdateChannelError(update_channel_error)).into());
                                }
                                // We don't care about other variants as they are not 
                                // associated to Update channel, and we will never
                                // encounter it.
                                _ => unreachable!()
                            }
                        }
                    }
                    let new_target = extended_channel.get_target();
                    let set_target = SetTarget {
                        channel_id,
                        maximum_target: new_target.to_le_bytes().into(),
                    };
                    messages.push((downstream_id, Mining::SetTarget(set_target)).into());
                } else {
                    error!("UpdateChannelError: invalid-channel-id");
                    let update_channel_error = UpdateChannelError {
                        channel_id,
                        error_code: "invalid-channel-id"
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    };
                    messages.push((downstream_id, Mining::UpdateChannelError(update_channel_error)).into());
                }

                Ok(messages)
            })
        })?;

        for message in messages {
            message.forward(&self.channel_manager_channel).await;
        }

        Ok(())
    }

    async fn handle_set_custom_mining_job(
        &mut self,
        client_id: Option<usize>,
        msg: SetCustomMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        // this is a naive implementation, but ideally we should check the SetCustomMiningJob
        // message parameters, especially:
        // - the mining_job_token
        // - the amount of the pool payout output
        let custom_job_coinbase_outputs = Vec::<TxOut>::consensus_decode(
            &mut msg.coinbase_tx_outputs.inner_as_ref().to_vec().as_slice(),
        )
        .map_err(PoolError::shutdown)?;

        let message: RouteMessageTo =
            self.channel_manager_data
                .super_safe_lock(|channel_manager_data| {
                    // check that the script_pubkey from self.coinbase_reward_script
                    // is present in the custom job coinbase outputs
                    let missing_script = !custom_job_coinbase_outputs.iter().any(|pool_output| {
                        *pool_output.script_pubkey == *self.coinbase_reward_script.script_pubkey()
                    });

                    if missing_script {
                        error!("SetCustomMiningJobError: pool-payout-script-missing");

                        let error = SetCustomMiningJobError {
                            request_id: msg.request_id,
                            channel_id: msg.channel_id,
                            error_code: "pool-payout-script-missing"
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };

                        return Ok((downstream_id, Mining::SetCustomMiningJobError(error)).into());
                    }

                    let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id)
                    else {
                        return Err(PoolError::disconnect(
                            PoolErrorKind::DownstreamNotFound(downstream_id),
                            downstream_id,
                        ));
                    };

                    downstream
                        .downstream_data
                        .super_safe_lock(|downstream_data| {
                            let Some(extended_channel) =
                                downstream_data.extended_channels.get_mut(&msg.channel_id)
                            else {
                                error!("SetCustomMiningJobError: invalid-channel-id");
                                let error = SetCustomMiningJobError {
                                    request_id: msg.request_id,
                                    channel_id: msg.channel_id,
                                    error_code: "invalid-channel-id"
                                        .to_string()
                                        .try_into()
                                        .expect("error code must be valid string"),
                                };
                                return Ok(
                                    (downstream_id, Mining::SetCustomMiningJobError(error)).into()
                                );
                            };

                            // TOOD: Send a CustomMiningJobError and not disconnect.
                            let job_id = extended_channel
                                .on_set_custom_mining_job(msg.clone().into_static())
                                .map_err(|error| PoolError::disconnect(error, downstream_id))?;

                            let success = SetCustomMiningJobSuccess {
                                channel_id: msg.channel_id,
                                request_id: msg.request_id,
                                job_id,
                            };
                            Ok((downstream_id, Mining::SetCustomMiningJobSuccess(success)).into())
                        })
                })?;

        message.forward(&self.channel_manager_channel).await;
        Ok(())
    }
}
