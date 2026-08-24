use std::{convert::TryFrom, sync::atomic::Ordering};
use stratum_apps::stratum_core::{
    mining_sv2::{
        CloseChannelOwned, OpenExtendedMiningChannelOwned, OpenExtendedMiningChannelSuccessOwned,
        OpenMiningChannelErrorOwned, OpenStandardMiningChannelOwned,
        OpenStandardMiningChannelSuccessOwned, SetCustomMiningJobErrorOwned,
        SetCustomMiningJobOwned, SetNewPrevHashOwned, SetTargetOwned, SubmitSharesErrorOwned,
        SubmitSharesExtendedOwned, UpdateChannelErrorOwned, UpdateChannelOwned,
    },
    parsers_sv2::{MiningOwned, TemplateDistributionOwned},
};

use stratum_apps::stratum_core::{
    binary_sv2::Str0255,
    bitcoin::Target,
    channels_sv2::{
        Vardiff, VardiffState,
        extranonce_manager::ExtranonceAllocatorError,
        server::{
            error::{ExtendedChannelError, StandardChannelError},
            extended::ExtendedChannel,
            share_accounting::{ShareValidationError, ShareValidationResult},
            standard::StandardChannel,
        },
    },
    extensions_sv2::{
        EXTENSION_TYPE_WORKER_HASHRATE_TRACKING, TLV_FIELD_TYPE_USER_IDENTITY, UserIdentity,
    },
    handlers_sv2::{HandleMiningMessagesFromClientOwnedAsync, SupportedChannelTypes},
    mining_sv2::*,
    parsers_sv2::{Tlv, TlvField},
    template_distribution_sv2::SubmitSolutionOwned,
};
use tracing::{error, info};

use jd_server_sv2::job_declarator::SetCustomMiningJobResponse;

use crate::{
    channel_manager::{CLIENT_SEARCH_SPACE_BYTES, ChannelManager, RouteMessageTo},
    error::{self, PoolError, PoolErrorKind},
    utils::{PayoutMode, PayoutModeError, create_close_channel_msg},
};

fn extranonce_allocation_error_code(
    error: ExtranonceAllocatorError,
) -> Result<&'static str, ExtranonceAllocatorError> {
    match error {
        ExtranonceAllocatorError::CapacityExhausted => {
            Ok(ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED)
        }
        ExtranonceAllocatorError::InvalidRollableSize => {
            Ok(ERROR_CODE_OPEN_MINING_CHANNEL_MIN_EXTRANONCE_SIZE_TOO_LARGE)
        }
        error => Err(error),
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromClientOwnedAsync for ChannelManager {
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
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .negotiated_extensions
                .get()
                .map_err(PoolError::shutdown)
        })
    }

    async fn handle_close_channel(
        &mut self,
        client_id: Option<usize>,
        msg: CloseChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received Close Channel: {msg}");
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .group_channel
                .with(|group_channel| {
                    if group_channel.has_channel_id(msg.channel_id) {
                        group_channel.remove_channel_id(msg.channel_id);
                    }
                })
                .map_err(PoolError::shutdown)?;
            downstream.standard_channels.remove(&msg.channel_id);
            downstream.extended_channels.remove(&msg.channel_id);
            Ok(())
        })?;
        self.vardiff.remove(&(downstream_id, msg.channel_id).into());
        Ok(())
    }

    async fn handle_open_standard_mining_channel(
        &mut self,
        client_id: Option<usize>,
        msg: OpenStandardMiningChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let request_id = msg.request_id;
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        info!("Received OpenStandardMiningChannel: {}", msg);

        let messages = self.with_registered_downstream(downstream_id, |downstream| {
                if downstream.requires_custom_work.load(Ordering::SeqCst) {
                    error!("OpenStandardMiningChannel: Standard Channels are not supported for this connection");
                    let open_standard_mining_channel_error = OpenMiningChannelErrorOwned {
                        request_id,
                        error_code: ERROR_CODE_OPEN_MINING_CHANNEL_STANDARD_CHANNELS_NOT_SUPPORTED_FOR_CUSTOM_WORK
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    };
                    return Ok(vec![(
                        downstream_id,
                        MiningOwned::OpenMiningChannelError(open_standard_mining_channel_error),
                    )
                        .into()]);
                }

                let Some(last_future_template) = self
                    .last_future_template
                    .get()
                    .map_err(PoolError::shutdown)?
                else {
                    return Err(PoolError::disconnect(
                        PoolErrorKind::FutureTemplateNotPresent,
                        downstream_id,
                    ));
                };

                let Some(last_set_new_prev_hash_tdp) =
                    self.last_new_prev_hash.get().map_err(PoolError::shutdown)?
                else {
                    return Err(PoolError::disconnect(
                        PoolErrorKind::LastNewPrevhashNotFound,
                        downstream_id,
                    ));
                };

                let payout_mode = match PayoutMode::try_from(user_identity.as_str()) {
                    Ok(mode) => mode,
                    Err(PayoutModeError::NoPayoutMode(_)) => PayoutMode::FullDonation,
                    Err(_) => {
                        error!(
                            "Invalid user_identity '{}': does not match any supported identity format",
                            user_identity
                        );
                        let open_standard_mining_channel_error = OpenMiningChannelErrorOwned {
                            request_id,
                            error_code: ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_USER_IDENTITY
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        return Ok(vec![(
                            downstream_id,
                            MiningOwned::OpenMiningChannelError(open_standard_mining_channel_error),
                        )
                            .into()]);
                    }
                };

                let coinbase_outputs = payout_mode.coinbase_outputs(
                    last_future_template.coinbase_tx_value_remaining,
                    &self.coinbase_reward_script,
                );

                downstream
                    .payout_mode
                    .set(Some(payout_mode))
                    .map_err(PoolError::shutdown)?;

                let nominal_hash_rate = msg.nominal_hash_rate;
                let requested_max_target = Target::from_le_bytes(msg.max_target.to_array());
                let extranonce_prefix = match self
                    .extranonce_allocator
                    .with(|allocator| allocator.allocate_standard())
                    .map_err(PoolError::shutdown)?
                {
                    Ok(prefix) => prefix,
                    Err(e) => {
                        error!(?e, "Failed to get extranonce prefix");
                        let error_code =
                            extranonce_allocation_error_code(e).map_err(PoolError::shutdown)?;
                        let open_standard_mining_channel_error = OpenMiningChannelErrorOwned {
                            request_id,
                            error_code: error_code
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        return Ok(vec![(
                            downstream_id,
                            MiningOwned::OpenMiningChannelError(
                                open_standard_mining_channel_error,
                            ),
                        )
                            .into()]);
                    }
                };

                let channel_id = downstream.channel_id_factory.fetch_add(1, Ordering::SeqCst);

                let mut standard_channel = match StandardChannel::new_for_pool(
                    channel_id,
                    user_identity.to_string(),
                    extranonce_prefix,
                    requested_max_target,
                    nominal_hash_rate,
                    self.share_batch_size,
                    self.shares_per_minute,
                    self.pool_tag_string.clone(),
                ) {
                    Ok(channel) => channel,
                    Err(e) => match e {
                        StandardChannelError::OpenChannelInvalidNominalHashrate(code) => {
                            error!("OpenMiningChannelError: {}", code);
                            let open_standard_mining_channel_error = OpenMiningChannelErrorOwned {
                                request_id,
                                error_code: code
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            return Ok(vec![(
                                downstream_id,
                                MiningOwned::OpenMiningChannelError(open_standard_mining_channel_error),
                            )
                                .into()]);
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
                    .with(|channel| channel.get_group_channel_id())
                    .map_err(PoolError::shutdown)?;
                let extranonce_prefix_size = standard_channel.get_extranonce_prefix().len();

                let open_standard_mining_channel_success = OpenStandardMiningChannelSuccessOwned {
                    request_id: msg.request_id,
                    channel_id,
                    target: standard_channel.get_target().to_le_bytes().into(),
                    extranonce_prefix: standard_channel
                        .get_extranonce_prefix()
                        .to_vec()
                        .try_into()
                        .expect("Extranonce_prefix must be valid"),
                    group_channel_id,
                }
                ;

                let mut messages: Vec<RouteMessageTo> = Vec::new();

                messages.push(
                    (
                        downstream_id,
                        MiningOwned::OpenStandardMiningChannelSuccess(
                            open_standard_mining_channel_success,
                        ),
                    )
                        .into(),
                );

                let template_id = last_future_template.template_id;

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
                    future_standard_job.get_job_message().clone();

                messages.push(
                    (
                        downstream_id,
                        MiningOwned::NewMiningJob(future_standard_job_message),
                    )
                        .into(),
                );
                let prev_hash = last_set_new_prev_hash_tdp.prev_hash.clone();
                let header_timestamp = last_set_new_prev_hash_tdp.header_timestamp;
                let n_bits = last_set_new_prev_hash_tdp.n_bits;
                let set_new_prev_hash_mining = SetNewPrevHashOwned {
                    channel_id,
                    job_id: future_standard_job_id,
                    prev_hash,
                    min_ntime: header_timestamp,
                    nbits: n_bits,
                };

                standard_channel
                    .on_set_new_prev_hash(last_set_new_prev_hash_tdp.clone())
                    .map_err(PoolError::shutdown)?;

                messages.push(
                    (
                        downstream_id,
                        MiningOwned::SetNewPrevHash(set_new_prev_hash_mining),
                    )
                        .into(),
                );

                downstream
                    .standard_channels
                    .insert(channel_id, standard_channel);
                if !downstream.requires_standard_jobs.load(Ordering::SeqCst) {
                    downstream
                        .group_channel
                        .with(|channel| channel.add_channel_id(channel_id, extranonce_prefix_size))
                        .map_err(PoolError::shutdown)?
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            PoolError::shutdown(e)
                        })?;
                }
                let vardiff = VardiffState::new().map_err(PoolError::shutdown)?;
                self.vardiff
                    .insert((downstream_id, channel_id).into(), vardiff);

                Ok(messages)
            })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    async fn handle_open_extended_mining_channel(
        &mut self,
        client_id: Option<usize>,
        msg: OpenExtendedMiningChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let request_id = msg.request_id;
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        info!("Received OpenExtendedMiningChannel: {}", msg);

        let nominal_hash_rate = msg.nominal_hash_rate;
        let requested_max_target = Target::from_le_bytes(msg.max_target.to_array());
        let requested_min_rollable_extranonce_size = msg.min_extranonce_size;

        let messages = self.with_registered_downstream(downstream_id, |downstream| {
                if downstream.requires_standard_jobs.load(Ordering::SeqCst) {
                    let open_extended_mining_channel_error = OpenMiningChannelErrorOwned {
                            request_id,
                            error_code: ERROR_CODE_OPEN_MINING_CHANNEL_EXTENDED_CHANNELS_NOT_SUPPORTED_FOR_STANDARD_JOBS
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                    return Ok(vec![(
                        downstream_id,
                        MiningOwned::OpenMiningChannelError(open_extended_mining_channel_error),
                    )
                        .into()]);
                }

                let mut messages: Vec<RouteMessageTo> = Vec::new();

                let extranonce_prefix = match self
                    .extranonce_allocator
                    .with(|allocator| {
                        allocator.allocate_extended(requested_min_rollable_extranonce_size.into())
                    })
                    .map_err(PoolError::shutdown)?
                {
                    Ok(prefix) => prefix,
                    Err(e) => {
                        error!(?e, "Failed to get extranonce prefix");
                        let error_code =
                            extranonce_allocation_error_code(e).map_err(PoolError::shutdown)?;
                        let open_extended_mining_channel_error = OpenMiningChannelErrorOwned {
                            request_id,
                            error_code: error_code
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        return Ok(vec![(
                            downstream_id,
                            MiningOwned::OpenMiningChannelError(
                                open_extended_mining_channel_error,
                            ),
                        )
                            .into()]);
                    }
                };

                let payout_mode = match PayoutMode::try_from(user_identity.as_str()) {
                    Ok(mode) => mode,
                    Err(PayoutModeError::NoPayoutMode(_)) => PayoutMode::FullDonation,
                    Err(_) => {
                        error!(
                            "Invalid user_identity '{}': does not match any supported identity format",
                            user_identity
                        );
                        let open_extended_mining_channel_error = OpenMiningChannelErrorOwned {
                            request_id,
                            error_code: ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_USER_IDENTITY
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        return Ok(vec![(
                            downstream_id,
                            MiningOwned::OpenMiningChannelError(open_extended_mining_channel_error),
                        )
                            .into()]);
                    }
                };

                downstream
                    .payout_mode
                    .set(Some(payout_mode.clone()))
                    .map_err(PoolError::shutdown)?;

                let channel_id = downstream.channel_id_factory.fetch_add(1, Ordering::SeqCst);

                let mut extended_channel = match ExtendedChannel::new_for_pool(
                    channel_id,
                    user_identity.to_string(),
                    extranonce_prefix,
                    requested_max_target,
                    nominal_hash_rate,
                    true, // version rolling always allowed
                    CLIENT_SEARCH_SPACE_BYTES as u16,
                    self.share_batch_size,
                    self.shares_per_minute,
                    self.pool_tag_string.clone(),
                ) {
                    Ok(channel) => channel,
                    Err(e) => match e {
                        ExtendedChannelError::OpenChannelInvalidNominalHashrate(code) => {
                            error!("OpenMiningChannelError: {}", code);
                            let open_extended_mining_channel_error = OpenMiningChannelErrorOwned {
                                request_id,
                                error_code: code
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            return Ok(vec![(
                                downstream_id,
                                MiningOwned::OpenMiningChannelError(open_extended_mining_channel_error),
                            )
                                .into()]);
                        }
                        ExtendedChannelError::RequestedMinExtranonceSizeTooLarge(code) => {
                            error!("OpenMiningChannelError: {}", code);
                            let open_extended_mining_channel_error = OpenMiningChannelErrorOwned {
                                request_id,
                                error_code: code
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            return Ok(vec![(
                                downstream_id,
                                MiningOwned::OpenMiningChannelError(open_extended_mining_channel_error),
                            )
                                .into()]);
                        }
                        e => {
                            error!("error in handle_open_extended_mining_channel: {:?}", e);
                            return Err(PoolError::disconnect(e, downstream_id));
                        }
                    },
                };

                let group_channel_id = downstream
                    .group_channel
                    .with(|channel| channel.get_group_channel_id())
                    .map_err(PoolError::shutdown)?;

                let open_extended_mining_channel_success = OpenExtendedMiningChannelSuccessOwned {
                    request_id,
                    channel_id,
                    target: extended_channel.get_target().to_le_bytes().into(),
                    extranonce_prefix: extended_channel
                        .get_extranonce_prefix()
                        .to_vec()
                        .try_into()
                        .map_err(PoolError::shutdown)?,
                    extranonce_size: extended_channel.get_rollable_extranonce_size(),
                    group_channel_id,
                }
                ;
                info!("Sending OpenExtendedMiningChannel.Success (downstream_id: {downstream_id}): {open_extended_mining_channel_success}");

                messages.push(
                    (
                        downstream_id,
                        MiningOwned::OpenExtendedMiningChannelSuccess(
                            open_extended_mining_channel_success,
                        ),
                    )
                        .into(),
                );

                let Some(last_set_new_prev_hash_tdp) =
                    self.last_new_prev_hash.get().map_err(PoolError::shutdown)?
                else {
                    return Err(PoolError::disconnect(
                        PoolErrorKind::LastNewPrevhashNotFound,
                        downstream_id,
                    ));
                };

                let Some(last_future_template) = self
                    .last_future_template
                    .get()
                    .map_err(PoolError::shutdown)?
                else {
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
                        future_extended_job.get_job_message().clone();

                    // send this future job as new job message
                    // to be immediately activated with the subsequent SetNewPrevHash
                    // message
                    messages.push(
                        (
                            downstream_id,
                            MiningOwned::NewExtendedMiningJob(future_extended_job_message),
                        )
                            .into(),
                    );

                    // SetNewPrevHash message activates the future job
                    let prev_hash = last_set_new_prev_hash_tdp.prev_hash.clone();
                    let header_timestamp = last_set_new_prev_hash_tdp.header_timestamp;
                    let n_bits = last_set_new_prev_hash_tdp.n_bits;
                    let set_new_prev_hash_mining = SetNewPrevHashOwned {
                        channel_id,
                        job_id: future_extended_job_id,
                        prev_hash,
                        min_ntime: header_timestamp,
                        nbits: n_bits,
                    };

                    extended_channel
                        .on_set_new_prev_hash(last_set_new_prev_hash_tdp)
                        .map_err(PoolError::shutdown)?;

                    messages.push(
                        (
                            downstream_id,
                            MiningOwned::SetNewPrevHash(set_new_prev_hash_mining),
                        )
                            .into(),
                    );

                    let full_extranonce_size = extended_channel.get_full_extranonce_size();
                    downstream
                        .group_channel
                        .with(|channel| channel.add_channel_id(channel_id, full_extranonce_size))
                        .map_err(PoolError::shutdown)?
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            PoolError::shutdown(e)
                        })?;
                }

                downstream
                    .extended_channels
                    .insert(channel_id, extended_channel);
                let vardiff = VardiffState::new().map_err(PoolError::shutdown)?;
                self.vardiff
                    .insert((downstream_id, channel_id).into(), vardiff);

                Ok(messages)
            })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                error!("Failed to forward message {e:?}");
            }
        }
        Ok(())
    }

    async fn handle_submit_shares_standard(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesStandardOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesStandard: {msg}");
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let channel_id = msg.channel_id;
        let vardiff_key = (downstream_id, channel_id).into();
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
                let messages = if !downstream.standard_channels.contains_key(&channel_id) {
                    let error = SubmitSharesErrorOwned {
                        channel_id,
                        sequence_number: msg.sequence_number,
                        error_code: ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    };
                    error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID);
                    vec![(downstream_id, MiningOwned::SubmitSharesError(error)).into()]
                } else if !self.vardiff.contains_key(&vardiff_key) {
                    vec![(
                        downstream_id,
                        MiningOwned::CloseChannel(create_close_channel_msg(
                            channel_id,
                            ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID,
                        )),
                    )
                        .into()]
                } else {
                    let validation =
                        downstream
                            .standard_channels
                            .with_mut(&channel_id, |standard_channel| {
                                let mut messages: Vec<RouteMessageTo> = Vec::new();
                                let res = standard_channel.validate_share(msg.clone());
                                match res {
                                    Ok(ShareValidationResult::Valid(share_hash)) => {
                                        let share_accounting = standard_channel.get_share_accounting();
                                        if share_accounting.should_acknowledge() {
                                            let success = SubmitSharesSuccessOwned {
                                                channel_id,
                                                last_sequence_number: share_accounting
                                                    .get_last_share_sequence_number(),
                                                new_submits_accepted_count: share_accounting
                                                    .get_last_batch_accepted(),
                                                new_shares_sum: share_accounting
                                                    .get_last_batch_work_sum(),
                                            };
                                            info!("SubmitSharesStandard: {} ✅", success);
                                            messages.push(
                                                (downstream_id, MiningOwned::SubmitSharesSuccess(success))
                                                    .into(),
                                            );
                                        } else {
                                            let share_work =
                                                standard_channel.get_target().difficulty_float();
                                            info!(
                                                "SubmitSharesStandard: valid share | downstream_id: {}, channel_id: {}, sequence_number: {}, share_hash: {}, share_work: {} ✅",
                                                downstream_id, channel_id, msg.sequence_number, share_hash, share_work
                                            );
                                        }
                                    }
                                    Ok(ShareValidationResult::BlockFound(
                                        share_hash,
                                        template_id,
                                        coinbase,
                                    )) => {
                                        info!("SubmitSharesStandard: 💰 Block Found!!! 💰{share_hash}");
                                        self.record_block_found();
                                        // if we have a template id (i.e.: this was not a custom job)
                                        // we can propagate the solution to the TP
                                        if let Some(template_id) = template_id {
                                            info!("SubmitSharesStandard: Propagating solution to the Template Provider.");
                                            let solution = SubmitSolutionOwned {
                                                template_id,
                                                version: msg.version,
                                                header_timestamp: msg.ntime,
                                                header_nonce: msg.nonce,
                                                coinbase_tx: coinbase
                                                    .try_into()
                                                    .map_err(PoolError::shutdown)?,
                                            };
                                            messages.push(
                                                TemplateDistributionOwned::SubmitSolution(solution)
                                                    .into(),
                                            );
                                        }
                                        let share_accounting = standard_channel.get_share_accounting();
                                        let success = SubmitSharesSuccessOwned {
                                            channel_id,
                                            last_sequence_number: share_accounting
                                                .get_last_share_sequence_number(),
                                            new_submits_accepted_count: share_accounting
                                                .get_last_batch_accepted(),
                                            new_shares_sum: share_accounting
                                                .get_last_batch_work_sum(),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesSuccess(success))
                                                .into(),
                                        );
                                    }
                                    Err(ShareValidationError::Invalid(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };

                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::Stale(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::InvalidJobId(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::DoesNotMeetTarget(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::DuplicateShare(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::VersionRollingNotAllowed(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(e) => {
                                        return Err(PoolError::disconnect(e, downstream_id));
                                    }
                                }

                                Ok(messages)
                            });
                    match validation {
                        Some(validation) => {
                            self.vardiff.with_mut(&vardiff_key, |vardiff| {
                                vardiff.increment_shares_since_last_update()
                            });
                            validation?
                        }
                        None => {
                            let submit_shares_error = SubmitSharesErrorOwned {
                                channel_id,
                                sequence_number: msg.sequence_number,
                                error_code: ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID);
                            vec![(
                                downstream_id,
                                MiningOwned::SubmitSharesError(submit_shares_error),
                            )
                                .into()]
                        }
                    }
                };

                Ok(messages)
            })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    async fn handle_submit_shares_extended(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesExtendedOwned,
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
        let vardiff_key = (downstream_id, channel_id).into();
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
                let messages = if !downstream.extended_channels.contains_key(&channel_id) {
                    let error = SubmitSharesErrorOwned {
                        channel_id,
                        sequence_number: msg.sequence_number,
                        error_code: ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID
                            .to_string()
                            .try_into()
                            .expect("error code must be valid string"),
                    };
                    error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID);
                    vec![(downstream_id, MiningOwned::SubmitSharesError(error)).into()]
                } else if !self.vardiff.contains_key(&vardiff_key) {
                    vec![(
                        downstream_id,
                        MiningOwned::CloseChannel(create_close_channel_msg(
                            channel_id,
                            ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID,
                        )),
                    )
                        .into()]
                } else {
                    if let Some(_user_identity) = user_identity {
                        // here we have the UserIdentity TLV, so we can use it to enhance monitoring of
                        // individual miners in the future
                    }
                    let validation =
                        downstream
                            .extended_channels
                            .with_mut(&channel_id, |extended_channel| {
                                let mut messages: Vec<RouteMessageTo> = Vec::new();
                                let res = extended_channel.validate_share(msg.clone());
                                match res {
                                    Ok(ShareValidationResult::Valid(share_hash)) => {
                                        let share_accounting = extended_channel.get_share_accounting();
                                        if share_accounting.should_acknowledge() {
                                            let success = SubmitSharesSuccessOwned {
                                                channel_id,
                                                last_sequence_number: share_accounting
                                                    .get_last_share_sequence_number(),
                                                new_submits_accepted_count: share_accounting
                                                    .get_last_batch_accepted(),
                                                new_shares_sum: share_accounting
                                                    .get_last_batch_work_sum(),
                                            };
                                            info!("SubmitSharesExtended: {} ✅", success);
                                            messages.push(
                                                (downstream_id, MiningOwned::SubmitSharesSuccess(success))
                                                    .into(),
                                            );
                                        } else {
                                            let share_work =
                                                extended_channel.get_target().difficulty_float();
                                            info!(
                                                "SubmitSharesExtended: valid share | downstream_id: {}, channel_id: {}, sequence_number: {}, share_hash: {}, share_work: {} ✅",
                                                downstream_id, channel_id, msg.sequence_number, share_hash, share_work
                                            );
                                        }
                                    }
                                    Ok(ShareValidationResult::BlockFound(
                                        share_hash,
                                        template_id,
                                        coinbase,
                                    )) => {
                                        info!("SubmitSharesExtended: 💰 Block Found!!! 💰{share_hash}");
                                        self.record_block_found();
                                        if let Some(template_id) = template_id {
                                            info!("SubmitSharesExtended: Propagating solution to the Template Provider.");
                                            let solution = SubmitSolutionOwned {
                                                template_id,
                                                version: msg.version,
                                                header_timestamp: msg.ntime,
                                                header_nonce: msg.nonce,
                                                coinbase_tx: coinbase
                                                    .try_into()
                                                    .map_err(PoolError::shutdown)?,
                                            };
                                            messages.push(
                                                TemplateDistributionOwned::SubmitSolution(solution)
                                                    .into(),
                                            );
                                        }
                                        let share_accounting = extended_channel.get_share_accounting();
                                        let success = SubmitSharesSuccessOwned {
                                            channel_id,
                                            last_sequence_number: share_accounting
                                                .get_last_share_sequence_number(),
                                            new_submits_accepted_count: share_accounting
                                                .get_last_batch_accepted(),
                                            new_shares_sum: share_accounting
                                                .get_last_batch_work_sum(),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesSuccess(success))
                                                .into(),
                                        );
                                    }
                                    Err(ShareValidationError::Invalid(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::Stale(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::InvalidJobId(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::DoesNotMeetTarget(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::DuplicateShare(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::BadExtranonceSize(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(ShareValidationError::VersionRollingNotAllowed(code)) => {
                                        error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, code);
                                        let error = SubmitSharesErrorOwned {
                                            channel_id: msg.channel_id,
                                            sequence_number: msg.sequence_number,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (downstream_id, MiningOwned::SubmitSharesError(error)).into(),
                                        );
                                    }
                                    Err(e) => {
                                        return Err(PoolError::disconnect(e, downstream_id));
                                    }
                                }

                                Ok(messages)
                            });
                    match validation {
                        Some(validation) => {
                            self.vardiff.with_mut(&vardiff_key, |vardiff| {
                                vardiff.increment_shares_since_last_update()
                            });
                            validation?
                        }
                        None => {
                            let error = SubmitSharesErrorOwned {
                                channel_id,
                                sequence_number: msg.sequence_number,
                                error_code: ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID
                                    .to_string()
                                    .try_into()
                                    .expect("error code must be valid string"),
                            };
                            error!("SubmitSharesError: downstream_id: {}, channel_id: {}, sequence_number: {}, error_code: {} ❌", downstream_id, channel_id, msg.sequence_number, ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID);
                            vec![(downstream_id, MiningOwned::SubmitSharesError(error)).into()]
                        }
                    }
                };

                Ok(messages)
            })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    async fn handle_update_channel(
        &mut self,
        client_id: Option<usize>,
        msg: UpdateChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        let channel_id = msg.channel_id;
        let new_nominal_hash_rate = msg.nominal_hash_rate;
        let requested_maximum_target = Target::from_le_bytes(msg.maximum_target.to_array());
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
            let mut messages: Vec<RouteMessageTo> = Vec::new();

            if downstream
                .standard_channels
                .with_mut(&channel_id, |standard_channel| {
                    let res = standard_channel
                        .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                    match res {
                        Ok(_) => {}
                        Err(e) => {
                            error!("UpdateChannelError: {:?}", e);
                            match e {
                                StandardChannelError::UpdateChannelInvalidNominalHashrate(code) => {
                                    error!("UpdateChannelError: {}", code);
                                    let update_channel_error = UpdateChannelErrorOwned {
                                        channel_id,
                                        error_code: code
                                            .to_string()
                                            .try_into()
                                            .expect("error code must be valid string"),
                                    };
                                    messages.push(
                                        (
                                            downstream_id,
                                            MiningOwned::UpdateChannelError(update_channel_error),
                                        )
                                            .into(),
                                    );
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                    let new_target = standard_channel.get_target();
                    let set_target = SetTargetOwned {
                        channel_id,
                        maximum_target: new_target.to_le_bytes().into(),
                    };
                    messages.push((downstream_id, MiningOwned::SetTarget(set_target)).into());
                })
                .is_none()
                && downstream
                    .extended_channels
                    .with_mut(&channel_id, |extended_channel| {
                        let res = extended_channel
                            .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                        match res {
                            Ok(_) => {}
                            Err(e) => {
                                error!("UpdateChannelError: {:?}", e);
                                match e {
                                    ExtendedChannelError::UpdateChannelInvalidNominalHashrate(
                                        code,
                                    ) => {
                                        error!("UpdateChannelError: {}", code);
                                        let update_channel_error = UpdateChannelErrorOwned {
                                            channel_id,
                                            error_code: code
                                                .to_string()
                                                .try_into()
                                                .expect("error code must be valid string"),
                                        };
                                        messages.push(
                                            (
                                                downstream_id,
                                                MiningOwned::UpdateChannelError(
                                                    update_channel_error,
                                                ),
                                            )
                                                .into(),
                                        );
                                    }
                                    _ => unreachable!(),
                                }
                            }
                        }
                        let new_target = extended_channel.get_target();
                        let set_target = SetTargetOwned {
                            channel_id,
                            maximum_target: new_target.to_le_bytes().into(),
                        };
                        messages.push((downstream_id, MiningOwned::SetTarget(set_target)).into());
                    })
                    .is_none()
            {
                error!("UpdateChannelError: invalid-channel-id");
                let update_channel_error = UpdateChannelErrorOwned {
                    channel_id,
                    error_code: ERROR_CODE_UPDATE_CHANNEL_INVALID_CHANNEL_ID
                        .to_string()
                        .try_into()
                        .expect("error code must be valid string"),
                };
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::UpdateChannelError(update_channel_error),
                    )
                        .into(),
                );
            }

            Ok(messages)
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    async fn handle_set_custom_mining_job(
        &mut self,
        client_id: Option<usize>,
        msg: SetCustomMiningJobOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let Some(ref mut job_declarator) = self.job_declarator else {
            let error = SetCustomMiningJobErrorOwned {
                request_id: msg.request_id,
                channel_id: msg.channel_id,
                error_code: ERROR_CODE_SET_CUSTOM_MINING_JOB_JD_NOT_SUPPORTED
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let message: RouteMessageTo =
                (downstream_id, MiningOwned::SetCustomMiningJobError(error)).into();
            message
                .forward(&self.channel_manager_io)
                .await
                .map_err(|e| PoolError::disconnect(e, downstream_id))?;
            return Ok(());
        };

        let msg_static = msg.clone();

        // Step 1: Validate the custom job via JDS (token + job validation).
        let jds_response = job_declarator
            .handle_set_custom_mining_job(msg_static.clone(), _tlv_fields)
            .await
            .map_err(|e| PoolError::shutdown(PoolErrorKind::Jds(e.into())))?;

        if let SetCustomMiningJobResponse::Error(jds_err) = jds_response {
            let message: RouteMessageTo =
                (downstream_id, MiningOwned::SetCustomMiningJobError(jds_err)).into();
            message
                .forward(&self.channel_manager_io)
                .await
                .map_err(|e| PoolError::disconnect(e, downstream_id))?;
            return Ok(());
        }

        // Step 2: JDS validated successfully — commit the job to the extended channel.
        let message: RouteMessageTo =
            self.with_registered_downstream(downstream_id, |downstream| {
                match downstream.extended_channels.with_mut(
                    &msg_static.channel_id,
                    |extended_channel| {
                        let job_id = extended_channel
                            .on_set_custom_mining_job(msg_static.clone())
                            .map_err(|error| PoolError::disconnect(error, downstream_id))?;

                        let success = SetCustomMiningJobSuccessOwned {
                            channel_id: msg_static.channel_id,
                            request_id: msg_static.request_id,
                            job_id,
                        };
                        Ok((
                            downstream_id,
                            MiningOwned::SetCustomMiningJobSuccess(success),
                        )
                            .into())
                    },
                ) {
                    Some(message) => message,
                    None => {
                        error!("SetCustomMiningJobError: invalid-channel-id");
                        let error = SetCustomMiningJobErrorOwned {
                            request_id: msg_static.request_id,
                            channel_id: msg_static.channel_id,
                            error_code: ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_CHANNEL_ID
                                .to_string()
                                .try_into()
                                .expect("error code must be valid string"),
                        };
                        Ok((downstream_id, MiningOwned::SetCustomMiningJobError(error)).into())
                    }
                }
            })?;

        message
            .forward(&self.channel_manager_io)
            .await
            .map_err(|e| PoolError::disconnect(e, downstream_id))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::stratum_core::channels_sv2::extranonce_manager::ExtranonceAllocator;

    #[test]
    fn capacity_exhaustion_has_specific_error_code() {
        let mut allocator = ExtranonceAllocator::new(vec![], 4, 1).unwrap();
        let _occupied_prefix = allocator.allocate_standard().unwrap();

        let error = allocator.allocate_standard().unwrap_err();

        assert_eq!(
            extranonce_allocation_error_code(error).unwrap(),
            ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED
        );
    }

    #[test]
    fn invalid_rollable_size_keeps_specific_error_code() {
        let mut allocator = ExtranonceAllocator::new(vec![], 4, 1).unwrap();
        let invalid_size = allocator.rollable_extranonce_size() as usize + 1;

        let error = allocator.allocate_extended(invalid_size).unwrap_err();

        assert_eq!(
            extranonce_allocation_error_code(error).unwrap(),
            ERROR_CODE_OPEN_MINING_CHANNEL_MIN_EXTRANONCE_SIZE_TOO_LARGE
        );
    }
}
