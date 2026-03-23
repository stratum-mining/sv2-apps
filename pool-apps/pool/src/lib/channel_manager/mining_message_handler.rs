use std::{convert::TryFrom, sync::atomic::Ordering};

use stratum_apps::stratum_core::{
    binary_sv2::Str0255,
    bitcoin::Target,
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

use jd_server_sv2::job_declarator::SetCustomMiningJobResponse;

use crate::{
    channel_manager::{ChannelManager, RouteMessageTo, CLIENT_SEARCH_SPACE_BYTES},
    error::{self, PoolError, PoolErrorKind},
    utils::{create_close_channel_msg, PayoutMode},
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
        let negotiated_extensions = self
            .downstream
            .with(&downstream_id, |downstream| {
                downstream.negotiated_extensions.get()
            })
            .expect("negotiated_extensions must be present");

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
        let found = self.downstream.with(&downstream_id, |downstream| {
            downstream.extended_channels.remove(&msg.channel_id);
            downstream.standard_channels.remove(&msg.channel_id);
        });
        if found.is_none() {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamNotFound(downstream_id),
                downstream_id,
            ));
        }

        self.vardiff.remove(&(downstream_id, msg.channel_id).into());
        Ok(())
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

        let mut messages: Vec<RouteMessageTo> = Vec::new();

        let Some(downstream) = self.downstream.with(&downstream_id, |d| d.clone()) else {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamIdNotFound,
                downstream_id,
            ));
        };

        if downstream.requires_custom_work.load(Ordering::SeqCst) {
            error!("Standard channels are not supported for this connection");
            let message: RouteMessageTo = (
                downstream_id,
                Mining::OpenMiningChannelError(OpenMiningChannelError {
                    request_id,
                    error_code: "standard-channels-not-supported-for-custom-work"
                        .to_string()
                        .try_into()
                        .expect("valid error code"),
                }),
            )
                .into();
            message.forward(&self.channel_manager_channel).await;
            return Ok(());
        }

        let payout_mode = match PayoutMode::try_from(user_identity.as_str()) {
            Ok(mode) => mode,
            Err(_) => {
                error!(
                    "Invalid user_identity '{}': does not match any supported identity format",
                    user_identity
                );
                let open_standard_mining_channel_error =
                    Mining::OpenMiningChannelError(OpenMiningChannelError {
                        request_id,
                        error_code: "invalid-user-identity"
                            .to_string()
                            .try_into()
                            .expect("valid error code"),
                    });
                let message: RouteMessageTo =
                    (downstream_id, open_standard_mining_channel_error).into();
                message.forward(&self.channel_manager_channel).await;
                return Ok(());
            }
        };

        let Some(last_future_template) = self.last_future_template.get() else {
            return Err(PoolError::disconnect(
                PoolErrorKind::FutureTemplateNotPresent,
                downstream_id,
            ));
        };

        let Some(last_set_new_prev_hash_tdp) = self.last_new_prev_hash.get() else {
            return Err(PoolError::disconnect(
                PoolErrorKind::LastNewPrevhashNotFound,
                downstream_id,
            ));
        };

        let coinbase_outputs = payout_mode.coinbase_outputs(
            last_future_template.coinbase_tx_value_remaining,
            &self.coinbase_reward_script,
        );

        let requested_max_target =
            Target::from_le_bytes(msg.max_target.inner_as_ref().try_into().unwrap());

        let extranonce_prefix = self
            .extranonce_prefix_factory_standard
            .with(|data| data.next_prefix_standard())
            .map_err(PoolError::shutdown)?;

        let channel_id = downstream.channel_id_factory.fetch_add(1, Ordering::SeqCst);

        let mut standard_channel = match StandardChannel::new_for_pool(
            channel_id,
            user_identity.to_string(),
            extranonce_prefix.to_vec(),
            requested_max_target,
            msg.nominal_hash_rate,
            self.share_batch_size,
            self.shares_per_minute,
            DefaultJobStore::new(),
            self.pool_tag_string.clone(),
        ) {
            Ok(channel) => channel,
            Err(e) => match e {
                StandardChannelError::InvalidNominalHashrate => {
                    error!("OpenMiningChannelError: invalid-nominal-hashrate");
                    let message: RouteMessageTo = (
                        downstream_id,
                        Mining::OpenMiningChannelError(OpenMiningChannelError {
                            request_id,
                            error_code: "invalid-nominal-hashrate"
                                .to_string()
                                .try_into()
                                .expect("valid error code"),
                        }),
                    )
                        .into();
                    message.forward(&self.channel_manager_channel).await;
                    return Ok(());
                }
                StandardChannelError::RequestedMaxTargetOutOfRange => {
                    error!("OpenMiningChannelError: max-target-out-of-range");
                    let message: RouteMessageTo = (
                        downstream_id,
                        Mining::OpenMiningChannelError(OpenMiningChannelError {
                            request_id,
                            error_code: "max-target-out-of-range"
                                .to_string()
                                .try_into()
                                .expect("valid error code"),
                        }),
                    )
                        .into();
                    message.forward(&self.channel_manager_channel).await;
                    return Ok(());
                }
                _ => {
                    error!("error in handle_open_standard_mining_channel: {:?}", e);
                    return Err(PoolError::disconnect(
                        PoolErrorKind::ChannelErrorSender,
                        downstream_id,
                    ));
                }
            },
        };

        let group_channel_id = downstream
            .group_channel
            .with(|group_channel| group_channel.get_group_channel_id());

        let extranonce_prefix_size = standard_channel.get_extranonce_prefix().len();

        let open_standard_mining_channel_success = OpenStandardMiningChannelSuccess {
            request_id: msg.request_id,
            channel_id,
            target: standard_channel.get_target().to_le_bytes().into(),
            extranonce_prefix: standard_channel
                .get_extranonce_prefix()
                .clone()
                .try_into()
                .expect("Extranonce_prefix must be valid"),
            group_channel_id,
        }
        .into_static();

        messages.push(
            (
                downstream_id,
                Mining::OpenStandardMiningChannelSuccess(open_standard_mining_channel_success),
            )
                .into(),
        );

        let template_id = last_future_template.template_id;

        // create a future standard job based on the last future template
        standard_channel
            .on_new_template(last_future_template, coinbase_outputs.clone())
            .map_err(PoolError::shutdown)?;

        let future_standard_job_id = standard_channel
            .get_future_job_id_from_template_id(template_id)
            .expect("future job id must exist");

        let future_standard_job = standard_channel
            .get_future_job(future_standard_job_id)
            .expect("future job must exist");

        let future_standard_job_message =
            future_standard_job.get_job_message().clone().into_static();

        messages.push(
            (
                downstream_id,
                Mining::NewMiningJob(future_standard_job_message),
            )
                .into(),
        );

        standard_channel
            .on_set_new_prev_hash(last_set_new_prev_hash_tdp.clone())
            .map_err(PoolError::shutdown)?;

        let set_new_prev_hash_mining = SetNewPrevHash {
            channel_id,
            job_id: future_standard_job_id,
            prev_hash: last_set_new_prev_hash_tdp.prev_hash.clone(),
            min_ntime: last_set_new_prev_hash_tdp.header_timestamp,
            nbits: last_set_new_prev_hash_tdp.n_bits,
        };

        messages.push(
            (
                downstream_id,
                Mining::SetNewPrevHash(set_new_prev_hash_mining),
            )
                .into(),
        );

        downstream
            .standard_channels
            .insert(channel_id, standard_channel);

        if !downstream.requires_standard_jobs.load(Ordering::SeqCst) {
            downstream.group_channel.with(|group_channel| {
                group_channel
                    .add_channel_id(channel_id, extranonce_prefix_size)
                    .map_err(|e| {
                        error!("Failed to add channel id to group channel: {:?}", e);
                        PoolError::shutdown(e)
                    })
            })?
        }

        self.vardiff.insert(
            (downstream_id, channel_id).into(),
            VardiffState::new().map_err(PoolError::shutdown)?,
        );

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

        let Some(downstream) = self.downstream.with(&downstream_id, |d| d.clone()) else {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamIdNotFound,
                downstream_id,
            ));
        };

        let mut messages: Vec<RouteMessageTo> = Vec::new();

        let requested_max_target =
            Target::from_le_bytes(msg.max_target.inner_as_ref().try_into().unwrap());

        let extranonce_prefix = match self
            .extranonce_prefix_factory_extended
            .with(|data| data.next_prefix_extended(msg.min_extranonce_size.into()))
        {
            Ok(extranonce_prefix) => extranonce_prefix.to_vec(),
            Err(_) => {
                error!("OpenMiningChannelError: min-extranonce-size-too-large");
                let message: RouteMessageTo = (
                    downstream_id,
                    Mining::OpenMiningChannelError(OpenMiningChannelError {
                        request_id,
                        error_code: "min-extranonce-size-too-large"
                            .to_string()
                            .try_into()
                            .expect("valid error code"),
                    }),
                )
                    .into();
                message.forward(&self.channel_manager_channel).await;
                return Ok(());
            }
        };

        let payout_mode = match PayoutMode::try_from(user_identity.as_str()) {
            Ok(mode) => mode,
            Err(_) => {
                error!(
                    "Invalid user_identity '{}': does not match any supported identity format",
                    user_identity
                );
                let open_standard_mining_channel_error =
                    Mining::OpenMiningChannelError(OpenMiningChannelError {
                        request_id,
                        error_code: "invalid-user-identity"
                            .to_string()
                            .try_into()
                            .expect("valid error code"),
                    });
                let message: RouteMessageTo =
                    (downstream_id, open_standard_mining_channel_error).into();
                message.forward(&self.channel_manager_channel).await;
                return Ok(());
            }
        };

        downstream.payout_mode.set(Some(payout_mode.clone()));

        let channel_id = downstream.channel_id_factory.fetch_add(1, Ordering::SeqCst);

        let mut extended_channel = match ExtendedChannel::new_for_pool(
            channel_id,
            user_identity.to_string(),
            extranonce_prefix.clone(),
            requested_max_target,
            msg.nominal_hash_rate,
            true, // version rolling always allowed
            CLIENT_SEARCH_SPACE_BYTES as u16,
            self.share_batch_size,
            self.shares_per_minute,
            DefaultJobStore::new(),
            self.pool_tag_string.clone(),
        ) {
            Ok(channel) => channel,
            Err(e) => match e {
                ExtendedChannelError::InvalidNominalHashrate => {
                    error!("OpenMiningChannelError: invalid-nominal-hashrate");
                    let message: RouteMessageTo = (
                        downstream_id,
                        Mining::OpenMiningChannelError(OpenMiningChannelError {
                            request_id,
                            error_code: "invalid-nominal-hashrate"
                                .to_string()
                                .try_into()
                                .expect("valid error code"),
                        }),
                    )
                        .into();
                    message.forward(&self.channel_manager_channel).await;
                    return Ok(());
                }
                ExtendedChannelError::RequestedMaxTargetOutOfRange => {
                    error!("OpenMiningChannelError: max-target-out-of-range");
                    let message: RouteMessageTo = (
                        downstream_id,
                        Mining::OpenMiningChannelError(OpenMiningChannelError {
                            request_id,
                            error_code: "max-target-out-of-range"
                                .to_string()
                                .try_into()
                                .expect("valid error code"),
                        }),
                    )
                        .into();
                    message.forward(&self.channel_manager_channel).await;
                    return Ok(());
                }
                ExtendedChannelError::RequestedMinExtranonceSizeTooLarge => {
                    error!("OpenMiningChannelError: min-extranonce-size-too-large");
                    let message: RouteMessageTo = (
                        downstream_id,
                        Mining::OpenMiningChannelError(OpenMiningChannelError {
                            request_id,
                            error_code: "min-extranonce-size-too-large"
                                .to_string()
                                .try_into()
                                .expect("valid error code"),
                        }),
                    )
                        .into();
                    message.forward(&self.channel_manager_channel).await;
                    return Ok(());
                }
                e => {
                    error!("error in handle_open_extended_mining_channel: {:?}", e);
                    return Err(PoolError::disconnect(e, downstream_id))?;
                }
            },
        };

        let group_channel_id = downstream
            .group_channel
            .with(|group_channel| group_channel.get_group_channel_id());

        let open_extended_mining_channel_success = OpenExtendedMiningChannelSuccess {
            request_id,
            channel_id,
            target: extended_channel.get_target().to_le_bytes().into(),
            extranonce_prefix: extended_channel
                .get_extranonce_prefix()
                .clone()
                .try_into()
                .map_err(PoolError::shutdown)?,
            extranonce_size: extended_channel.get_rollable_extranonce_size(),
            group_channel_id,
        }
        .into_static();

        info!("Sending OpenExtendedMiningChannel.Success (downstream_id: {downstream_id}): {open_extended_mining_channel_success}");

        messages.push(
            (
                downstream_id,
                Mining::OpenExtendedMiningChannelSuccess(open_extended_mining_channel_success),
            )
                .into(),
        );

        let Some(last_set_new_prev_hash_tdp) = self.last_new_prev_hash.get() else {
            return Err(PoolError::disconnect(
                PoolErrorKind::LastNewPrevhashNotFound,
                downstream_id,
            ));
        };

        let Some(last_future_template) = self.last_future_template.get() else {
            return Err(PoolError::disconnect(
                PoolErrorKind::FutureTemplateNotPresent,
                downstream_id,
            ));
        };

        // if the client requires custom work, we don't need to send any extended
        // jobs so we just process the SetNewPrevHash
        // message
        if downstream.requires_custom_work.load(Ordering::SeqCst) {
            extended_channel
                .on_set_new_prev_hash(last_set_new_prev_hash_tdp)
                .map_err(PoolError::shutdown)?;
            // if the client does not require custom work, we need to send the
            // future extended job
            // and the SetNewPrevHash message
        } else {
            let coinbase_outputs = payout_mode.coinbase_outputs(
                last_future_template.coinbase_tx_value_remaining,
                &self.coinbase_reward_script,
            );

            extended_channel
                .on_new_template(last_future_template.clone(), coinbase_outputs)
                .map_err(PoolError::shutdown)?;

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

            let set_new_prev_hash_mining = SetNewPrevHash {
                channel_id,
                job_id: future_extended_job_id,
                // SetNewPrevHash message activates the future job
                prev_hash: last_set_new_prev_hash_tdp.prev_hash.clone(),
                min_ntime: last_set_new_prev_hash_tdp.header_timestamp,
                nbits: last_set_new_prev_hash_tdp.n_bits,
            };

            extended_channel
                .on_set_new_prev_hash(last_set_new_prev_hash_tdp)
                .map_err(PoolError::shutdown)?;

            messages.push(
                (
                    downstream_id,
                    Mining::SetNewPrevHash(set_new_prev_hash_mining),
                )
                    .into(),
            );

            let full_extranonce_size = extended_channel.get_full_extranonce_size();

            downstream.group_channel.with(|group_channel| {
                group_channel
                    .add_channel_id(channel_id, full_extranonce_size)
                    .map_err(|e| {
                        error!("Failed to add channel id to group channel: {:?}", e);
                        PoolError::shutdown(e)
                    })
            })?;
        }

        downstream
            .extended_channels
            .insert(channel_id, extended_channel);

        self.vardiff.insert(
            (downstream_id, channel_id).into(),
            VardiffState::new().map_err(PoolError::shutdown)?,
        );

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

        let channel_id = msg.channel_id;

        let Some(downstream) = self.downstream.with(&downstream_id, |d| d.clone()) else {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamNotFound(downstream_id),
                downstream_id,
            ));
        };

        let mut messages: Vec<RouteMessageTo> = Vec::new();

        let Some(res) = downstream
            .standard_channels
            .with_mut(&channel_id, |channel| channel.validate_share(msg.clone()))
        else {
            error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-channel-id ❌", downstream_id, channel_id, msg.sequence_number);
            let submit_shares_error = SubmitSharesError {
                channel_id,
                sequence_number: msg.sequence_number,
                error_code: "invalid-channel-id"
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let message: RouteMessageTo = (
                downstream_id,
                Mining::SubmitSharesError(submit_shares_error),
            )
                .into();
            message.forward(&self.channel_manager_channel).await;
            return Ok(());
        };

        if self
            .vardiff
            .with_mut(&(downstream_id, channel_id).into(), |vardiff| {
                vardiff.increment_shares_since_last_update();
            })
            .is_none()
        {
            let msg: RouteMessageTo = (
                downstream_id,
                Mining::CloseChannel(create_close_channel_msg(channel_id, "invalid-channel-id")),
            )
                .into();

            msg.forward(&self.channel_manager_channel).await;
            return Ok(());
        }

        let Some((should_ack, last_seq, accepted, work_sum, target_diff)) =
            downstream.standard_channels.with(&channel_id, |ch| {
                let acc = ch.get_share_accounting();
                (
                    acc.should_acknowledge(),
                    acc.get_last_share_sequence_number(),
                    acc.get_last_batch_accepted(),
                    acc.get_last_batch_work_sum(),
                    ch.get_target().difficulty_float(),
                )
            })
        else {
            return Ok(());
        };

        match res {
            Ok(ShareValidationResult::Valid(share_hash)) => {
                if should_ack {
                    let success = SubmitSharesSuccess {
                        channel_id,
                        last_sequence_number: last_seq,
                        new_submits_accepted_count: accepted,
                        new_shares_sum: work_sum as u64,
                    };
                    info!("SubmitSharesStandard: {} ✅", success);
                    messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
                } else {
                    info!(
                            "SubmitSharesStandard: valid share | downstream_id: {}, channel_id: {}, sequence_number: {}, share_hash: {}, share_work: {} ✅",
                            downstream_id, channel_id, msg.sequence_number, share_hash, target_diff
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
                let success = SubmitSharesSuccess {
                    channel_id,
                    last_sequence_number: last_seq,
                    new_submits_accepted_count: accepted,
                    new_shares_sum: work_sum as u64,
                };
                messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
            }
            Err(e @ ShareValidationError::Invalid)
            | Err(e @ ShareValidationError::Stale)
            | Err(e @ ShareValidationError::InvalidJobId)
            | Err(e @ ShareValidationError::DoesNotMeetTarget)
            | Err(e @ ShareValidationError::DuplicateShare) => {
                let error_code = match e {
                    ShareValidationError::Invalid => "invalid-share",
                    ShareValidationError::Stale => "stale-share",
                    ShareValidationError::InvalidJobId => "invalid-job-id",
                    ShareValidationError::DoesNotMeetTarget => "difficulty-too-low",
                    ShareValidationError::DuplicateShare => "duplicate-share",
                    _ => unreachable!(),
                };

                error!(
                    "SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌",
                    downstream_id, channel_id, msg.sequence_number, error_code
                );

                let error = SubmitSharesError {
                    channel_id: msg.channel_id,
                    sequence_number: msg.sequence_number,
                    error_code: error_code
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

        let channel_id = msg.channel_id;
        let Some(downstream) = self.downstream.with(&downstream_id, |d| d.clone()) else {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamNotFound(downstream_id),
                downstream_id,
            ));
        };

        let mut messages: Vec<RouteMessageTo> = Vec::new();

        let Some(res) = downstream
            .extended_channels
            .with_mut(&channel_id, |channel| channel.validate_share(msg.clone()))
        else {
            error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: invalid-channel-id ❌", downstream_id, channel_id, msg.sequence_number);
            let error = SubmitSharesError {
                channel_id,
                sequence_number: msg.sequence_number,
                error_code: "invalid-channel-id"
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let message: RouteMessageTo = (downstream_id, Mining::SubmitSharesError(error)).into();
            message.forward(&self.channel_manager_channel).await;
            return Ok(());
        };

        if let Some(_user_identity) = user_identity {
            // here we have the UserIdentity TLV, so we can use it to enhance monitoring of
            // individual miners in the future
        }

        if self
            .vardiff
            .with_mut(&(downstream_id, channel_id).into(), |vardiff| {
                vardiff.increment_shares_since_last_update();
            })
            .is_none()
        {
            let message: RouteMessageTo = (
                downstream_id,
                Mining::CloseChannel(create_close_channel_msg(channel_id, "invalid-channel-id")),
            )
                .into();
            message.forward(&self.channel_manager_channel).await;
            return Ok(());
        }

        let Some((should_ack, last_seq, accepted, work_sum, target_diff)) =
            downstream.extended_channels.with(&channel_id, |ch| {
                let acc = ch.get_share_accounting();
                (
                    acc.should_acknowledge(),
                    acc.get_last_share_sequence_number(),
                    acc.get_last_batch_accepted(),
                    acc.get_last_batch_work_sum(),
                    ch.get_target().difficulty_float(),
                )
            })
        else {
            return Ok(());
        };

        match res {
            Ok(ShareValidationResult::Valid(share_hash)) => {
                if should_ack {
                    let success = SubmitSharesSuccess {
                        channel_id,
                        last_sequence_number: last_seq,
                        new_submits_accepted_count: accepted,
                        new_shares_sum: work_sum as u64,
                    };
                    info!("SubmitSharesExtended: {} ✅", success);
                    messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
                } else {
                    info!(
                            "SubmitSharesExtended: valid share | downstream_id: {}, channel_id: {}, sequence_number: {}, share_hash: {}, share_work: {} ✅",
                            downstream_id, channel_id, msg.sequence_number, share_hash, target_diff
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

                let success = SubmitSharesSuccess {
                    channel_id,
                    last_sequence_number: last_seq,
                    new_submits_accepted_count: accepted,
                    new_shares_sum: work_sum as u64,
                };
                messages.push((downstream_id, Mining::SubmitSharesSuccess(success)).into());
            }
            Err(e @ ShareValidationError::Invalid)
            | Err(e @ ShareValidationError::Stale)
            | Err(e @ ShareValidationError::InvalidJobId)
            | Err(e @ ShareValidationError::DoesNotMeetTarget)
            | Err(e @ ShareValidationError::DuplicateShare)
            | Err(e @ ShareValidationError::BadExtranonceSize) => {
                let error_code = match e {
                    ShareValidationError::Invalid => "invalid-share",
                    ShareValidationError::Stale => "stale-share",
                    ShareValidationError::InvalidJobId => "invalid-job-id",
                    ShareValidationError::DoesNotMeetTarget => "difficulty-too-low",
                    ShareValidationError::DuplicateShare => "duplicate-share",
                    ShareValidationError::BadExtranonceSize => "bad-extranonce-size",
                    _ => unreachable!(),
                };

                error!(
                    "SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌",
                    downstream_id, channel_id, msg.sequence_number, error_code
                );

                let error = SubmitSharesError {
                    channel_id: msg.channel_id,
                    sequence_number: msg.sequence_number,
                    error_code: error_code
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

        let Some(downstream) = self.downstream.with(&downstream_id, |d| d.clone()) else {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamNotFound(downstream_id),
                downstream_id,
            ));
        };

        let mut messages: Vec<RouteMessageTo> = Vec::new();
        let channel_id = msg.channel_id;
        let new_nominal_hash_rate = msg.nominal_hash_rate;
        let requested_maximum_target =
            Target::from_le_bytes(msg.maximum_target.inner_as_ref().try_into().unwrap());

        let standard_data = downstream
            .standard_channels
            .with_mut(&channel_id, |channel| {
                let res =
                    channel.update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                let new_target = channel.get_target();
                (res, new_target.to_le_bytes())
            });
        let extended_data = if standard_data.is_none() {
            downstream
                .extended_channels
                .with_mut(&channel_id, |channel| {
                    let res = channel
                        .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                    let new_target = channel.get_target();
                    (res, new_target.to_le_bytes())
                })
        } else {
            None
        };

        if let Some((res, new_target_bytes)) = standard_data {
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
                            messages.push(
                                (
                                    downstream_id,
                                    Mining::UpdateChannelError(update_channel_error),
                                )
                                    .into(),
                            );
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
                            messages.push(
                                (
                                    downstream_id,
                                    Mining::UpdateChannelError(update_channel_error),
                                )
                                    .into(),
                            );
                        }
                        // We don't care about other variants as they are not
                        // associated to Update channel, and we will never
                        // encounter it.
                        _ => unreachable!(),
                    }
                }
            }
            let set_target = SetTarget {
                channel_id,
                maximum_target: new_target_bytes.into(),
            };
            messages.push((downstream_id, Mining::SetTarget(set_target)).into());
        } else if let Some((res, new_target_bytes)) = extended_data {
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
                            messages.push(
                                (
                                    downstream_id,
                                    Mining::UpdateChannelError(update_channel_error),
                                )
                                    .into(),
                            );
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
                            messages.push(
                                (
                                    downstream_id,
                                    Mining::UpdateChannelError(update_channel_error),
                                )
                                    .into(),
                            );
                        }
                        // We don't care about other variants as they are not
                        // associated to Update channel, and we will never
                        // encounter it.
                        _ => unreachable!(),
                    }
                }
            }
            let set_target = SetTarget {
                channel_id,
                maximum_target: new_target_bytes.into(),
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
            messages.push(
                (
                    downstream_id,
                    Mining::UpdateChannelError(update_channel_error),
                )
                    .into(),
            );
        }

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

        let Some(ref mut job_declarator) = self.job_declarator else {
            let error = SetCustomMiningJobError {
                request_id: msg.request_id,
                channel_id: msg.channel_id,
                error_code: "jd-not-supported"
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let message: RouteMessageTo =
                (downstream_id, Mining::SetCustomMiningJobError(error)).into();
            message.forward(&self.channel_manager_channel).await;
            return Ok(());
        };

        let msg_static = msg.clone().into_static();

        // Step 1: Validate the custom job via JDS (token + job validation).
        let jds_response = job_declarator
            .handle_set_custom_mining_job(msg_static.clone(), _tlv_fields)
            .await
            .map_err(|e| PoolError::shutdown(PoolErrorKind::Jds(e.into())))?;

        if let SetCustomMiningJobResponse::Error(jds_err) = jds_response {
            let message: RouteMessageTo = (
                downstream_id,
                Mining::SetCustomMiningJobError(jds_err.into_static()),
            )
                .into();
            message.forward(&self.channel_manager_channel).await;
            return Ok(());
        }

        let Some(downstream) = self.downstream.with(&downstream_id, |d| d.clone()) else {
            return Err(PoolError::disconnect(
                PoolErrorKind::DownstreamNotFound(downstream_id),
                downstream_id,
            ));
        };

        // TOOD: Send a CustomMiningJobError and not disconnect.
        let job_id_result = downstream
            .extended_channels
            .with_mut(&msg.channel_id, |channel| {
                channel
                    .on_set_custom_mining_job(msg.clone().into_static())
                    .map_err(|error| PoolError::disconnect(error, downstream_id))
            });

        let job_id = match job_id_result {
            None => {
                error!("SetCustomMiningJobError: invalid-channel-id");
                let error = SetCustomMiningJobError {
                    request_id: msg.request_id,
                    channel_id: msg.channel_id,
                    error_code: "invalid-channel-id"
                        .to_string()
                        .try_into()
                        .expect("error code must be valid string"),
                };
                let message: RouteMessageTo =
                    (downstream_id, Mining::SetCustomMiningJobError(error)).into();
                message.forward(&self.channel_manager_channel).await;
                return Ok(());
            }
            Some(result) => result?,
        };

        let success = SetCustomMiningJobSuccess {
            channel_id: msg.channel_id,
            request_id: msg.request_id,
            job_id,
        };

        let message: RouteMessageTo =
            (downstream_id, Mining::SetCustomMiningJobSuccess(success)).into();
        message.forward(&self.channel_manager_channel).await;

        Ok(())
    }
}
