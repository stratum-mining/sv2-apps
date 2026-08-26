use std::sync::atomic::Ordering;
use stratum_apps::stratum_core::{
    job_declaration_sv2::PushSolutionOwned,
    mining_sv2::{
        CloseChannelOwned, OpenExtendedMiningChannelOwned, OpenExtendedMiningChannelSuccessOwned,
        OpenMiningChannelErrorOwned, OpenStandardMiningChannelOwned,
        OpenStandardMiningChannelSuccessOwned, SetCustomMiningJobOwned, SetNewPrevHashOwned,
        SetTargetOwned, SubmitSharesErrorOwned, SubmitSharesExtendedOwned,
        SubmitSharesStandardOwned, SubmitSharesSuccessOwned, UpdateChannelErrorOwned,
        UpdateChannelOwned,
    },
};

use stratum_apps::{
    stratum_core::{
        binary_sv2::Str0255,
        bitcoin::{Amount, Target, hashes::sha256d},
        channels_sv2::{
            Vardiff, VardiffState, client,
            extranonce_manager::ExtranonceAllocatorError,
            outputs::deserialize_outputs,
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
        parsers_sv2::{
            AnyMessageOwned, JobDeclarationOwned, MiningOwned, TemplateDistributionOwned, Tlv,
            TlvField,
        },
        template_distribution_sv2::SubmitSolutionOwned,
    },
    utils::types::OutboundFrame,
};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::{
        ChannelManager, ChannelManagerIo, SOLO_FULL_EXTRANONCE_SIZE, SharesOrderedByDiff,
    },
    error::{self, JDCError, JDCErrorKind},
    utils::{add_share_to_cache, create_close_channel_msg},
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

/// `RouteMessageTo` is an abstraction used to route protocol messages
/// to the appropriate subsystem connected to the JDC.
///
/// Instead of manually handling routing logic for each message type,
/// this enum provides a unified interface. Each variant represents
/// a possible destination:
///
/// - [`RouteMessageTo::Upstream`] → For messages intended for the upstream.
/// - [`RouteMessageTo::JobDeclarator`] → For job declaration messages sent to the JDS.
/// - [`RouteMessageTo::TemplateProvider`] → For template distribution messages sent to the template
///   provider.
/// - [`RouteMessageTo::Downstream`] → For messages destined to a specific downstream client,
///   identified by its `u32` downstream ID.
#[derive(Clone)]
pub enum RouteMessageTo {
    /// Route to the upstream (mining) channel.
    Upstream(MiningOwned),
    /// Route to the job declarator subsystem.
    JobDeclarator(JobDeclarationOwned),
    /// Route to the template provider subsystem.
    TemplateProvider(TemplateDistributionOwned),
    /// Route to a specific downstream client by ID, along with its mining message.
    Downstream((usize, MiningOwned)),
}

impl From<MiningOwned> for RouteMessageTo {
    fn from(value: MiningOwned) -> Self {
        Self::Upstream(value)
    }
}

impl From<JobDeclarationOwned> for RouteMessageTo {
    fn from(value: JobDeclarationOwned) -> Self {
        Self::JobDeclarator(value)
    }
}

impl From<TemplateDistributionOwned> for RouteMessageTo {
    fn from(value: TemplateDistributionOwned) -> Self {
        Self::TemplateProvider(value)
    }
}

impl From<(usize, MiningOwned)> for RouteMessageTo {
    fn from(value: (usize, MiningOwned)) -> Self {
        Self::Downstream(value)
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl RouteMessageTo {
    /// Forwards the message to its corresponding destination channel.
    ///
    /// The result of this method can generally be ignored. A send failure
    /// typically indicates that one end of the channel is no longer present,
    /// which can occur during disconnections or lifecycle transitions.
    /// Such conditions are handled elsewhere by the system’s lifecycle
    /// and error management logic.
    ///
    /// The routing is handled as follows:
    /// - [`RouteMessageTo::Downstream`] → Sends the mining message to the specified downstream
    ///   client.
    /// - [`RouteMessageTo::Upstream`] → Forwards mining message upstream. In solo mode,
    ///   upstream-directed messages should not be produced.
    /// - [`RouteMessageTo::JobDeclarator`] → Sends the job declaration message to the JDS.
    /// - [`RouteMessageTo::TemplateProvider`] → Sends the template distribution message to the
    ///   template provider.
    pub async fn forward(self, channel_manager_io: &ChannelManagerIo) -> Result<(), JDCErrorKind> {
        match self {
            RouteMessageTo::Downstream((downstream_id, message)) => {
                let sender = channel_manager_io
                    .downstream_sender
                    .get_cloned(&downstream_id);
                if let Some(sender) = sender {
                    sender.send((message, None)).await?;
                } else {
                    debug!("Dropping message for downstream {downstream_id}: no longer connected");
                }
            }
            RouteMessageTo::Upstream(message) => {
                let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))?;
                channel_manager_io.upstream_sender.send(sv2_frame).await?;
            }
            RouteMessageTo::JobDeclarator(message) => {
                channel_manager_io.jd_sender.send(message).await?;
            }
            RouteMessageTo::TemplateProvider(message) => {
                channel_manager_io.tp_sender.send(message).await?;
            }
        }
        Ok(())
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromClientOwnedAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

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
                .map_err(JDCError::shutdown)
        })
    }

    fn get_channel_type_for_client(&self, _client_id: Option<usize>) -> SupportedChannelTypes {
        SupportedChannelTypes::GroupAndExtended
    }
    fn is_work_selection_enabled_for_client(&self, _client_id: Option<usize>) -> bool {
        false
    }
    fn is_client_authorized(
        &self,
        _client_id: Option<usize>,
        _user_identity: &Str0255,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    // Handles a `CloseChannel` message:
    // - Look up the downstream associated with the given `channel_id`.
    // - If found, remove the channel from its `extended_channels` and `standard_channels`.
    // - If not found, return an appropriate error.
    async fn handle_close_channel(
        &mut self,
        client_id: Option<usize>,
        msg: CloseChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .group_channel
                .with(|gc| {
                    if gc.has_channel_id(msg.channel_id) {
                        gc.remove_channel_id(msg.channel_id);
                    }
                })
                .map_err(JDCError::shutdown)?;
            downstream.extended_channels.remove(&msg.channel_id);
            downstream.standard_channels.remove(&msg.channel_id);
            Ok(())
        })?;
        self.vardiff.remove(&(downstream_id, msg.channel_id).into());
        Ok(())
    }

    // Handles an `OpenStandardMiningChannel` message from a downstream.
    //
    // Steps:
    // 1. Parse the `downstream_id` from the `user_identity`.
    // 2. Create a new `StandardChannel` for the downstream.
    // 3. Ensure a valid `GroupChannel` exists (create one if needed).
    // 4. Apply the latest future template and prevhash to both group and standard channels.
    // 5. Send the following messages back to the downstream:
    //    - `OpenStandardMiningChannelSuccess`
    //    - `NewMiningJob`
    //    - `SetNewPrevHash`
    // 6. Update the downstream state, including:
    //    - Channel manager mappings
    //    - Standard and group channel registrations
    //    - Vardiff state
    //
    // Returns an error if any step fails, such as missing templates, invalid identity,
    // or failure to apply updates to channels.
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

        let coinbase_outputs = self.coinbase_outputs.get().map_err(JDCError::shutdown)?;

        let mut coinbase_outputs = deserialize_outputs(coinbase_outputs)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        info!(downstream_id, "Received: {}", msg);

        let build_error = |code: &str| {
            MiningOwned::OpenMiningChannelError(OpenMiningChannelErrorOwned {
                request_id,
                error_code: code.try_into().expect("valid error code"),
            })
        };

        let last_future_template = self
            .last_future_template
            .get()
            .map_err(JDCError::shutdown)?;
        let Some(last_future_template) = last_future_template else {
            error!("Missing last_future_template, cannot open channel");
            return Err(JDCError::disconnect(
                JDCErrorKind::FutureTemplateNotPresent,
                downstream_id,
            ));
        };

        let last_new_prev_hash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;
        let Some(last_new_prev_hash) = last_new_prev_hash else {
            error!("Missing last_new_prev_hash, cannot open channel");
            return Err(JDCError::disconnect(
                JDCErrorKind::LastNewPrevhashNotFound,
                downstream_id,
            ));
        };

        coinbase_outputs[0].value =
            Amount::from_sat(last_future_template.coinbase_tx_value_remaining);
        let nominal_hash_rate = msg.nominal_hash_rate;
        let requested_max_target = Target::from_le_bytes(msg.max_target.to_array());
        let pool_tag_string = self.pool_tag_string.get().map_err(JDCError::shutdown)?;
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
            let group_channel_id = downstream
                .group_channel
                .with(|channel| channel.get_group_channel_id())
                .map_err(JDCError::shutdown)?;

            let standard_channel_id = downstream
                .channel_id_factory
                .fetch_add(1, Ordering::Relaxed);

            let extranonce_prefix = match self
                .extranonce_allocator
                .with(|allocator| allocator.allocate_standard())
                .map_err(JDCError::shutdown)?
            {
                Ok(prefix) => prefix,
                Err(e) => {
                    error!(?e, "Failed to get extranonce prefix");
                    let error_code =
                        extranonce_allocation_error_code(e).map_err(JDCError::shutdown)?;
                    return Ok(vec![(downstream_id, build_error(error_code)).into()]);
                }
            };

            let mut messages: Vec<RouteMessageTo> = Vec::new();
            let standard_channel = match StandardChannel::new_for_job_declaration_client(
                standard_channel_id,
                user_identity.to_string(),
                extranonce_prefix,
                requested_max_target,
                nominal_hash_rate,
                self.share_batch_size,
                self.shares_per_minute,
                pool_tag_string,
                self.miner_tag_string.clone(),
                self.max_past_jobs,
            ) {
                Ok(standard_channel) => Some(standard_channel),
                Err(e) => {
                    error!(?e, "Failed to create standard channel");
                    match e {
                        StandardChannelError::OpenChannelInvalidNominalHashrate(code) => {
                            messages.push((downstream_id, build_error(code)).into());
                            None
                        }
                        other => return Err(JDCError::disconnect(other, downstream_id)),
                    }
                }
            };

            if let Some(mut standard_channel) = standard_channel {
                let extranonce_prefix_size = standard_channel.get_extranonce_prefix().len();
                let open_standard_mining_channel_success = OpenStandardMiningChannelSuccessOwned {
                    request_id: msg.request_id,
                    channel_id: standard_channel_id,
                    target: standard_channel.get_target().to_le_bytes().into(),
                    extranonce_prefix: standard_channel
                        .get_extranonce_prefix()
                        .to_vec()
                        .try_into()
                        .map_err(JDCError::shutdown)?,
                    group_channel_id,
                };

                messages.push(
                    (
                        downstream_id,
                        MiningOwned::OpenStandardMiningChannelSuccess(
                            open_standard_mining_channel_success,
                        ),
                    )
                        .into(),
                );

                standard_channel
                    .on_new_template(last_future_template.clone(), coinbase_outputs.clone())
                    .map_err(|e| {
                        error!(?e, "Failed to apply template to standard channel");
                        JDCError::shutdown(e)
                    })?;

                let future_standard_job_id = standard_channel
                    .get_future_job_id_from_template_id(last_future_template.template_id)
                    .expect("future job id must exist");
                let future_standard_job_message = standard_channel
                    .get_future_job(future_standard_job_id)
                    .expect("future job must exist")
                    .get_job_message()
                    .clone();
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::NewMiningJob(future_standard_job_message),
                    )
                        .into(),
                );

                let set_new_prev_hash_mining = SetNewPrevHashOwned {
                    channel_id: standard_channel_id,
                    job_id: future_standard_job_id,
                    prev_hash: last_new_prev_hash.prev_hash.clone(),
                    min_ntime: last_new_prev_hash.header_timestamp,
                    nbits: last_new_prev_hash.n_bits,
                };

                standard_channel
                    .on_set_new_prev_hash(last_new_prev_hash)
                    .map_err(|e| {
                        error!(?e, "Failed to apply prevhash to standard channel");
                        JDCError::shutdown(e)
                    })?;
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::SetNewPrevHash(set_new_prev_hash_mining),
                    )
                        .into(),
                );

                self.vardiff.insert(
                    (downstream_id, standard_channel_id).into(),
                    VardiffState::new().expect("Vardiff state should instantiate."),
                );
                downstream
                    .standard_channels
                    .insert(standard_channel_id, standard_channel);
                self.downstream_channel_id_and_job_id_to_template_id.insert(
                    (downstream_id, standard_channel_id, future_standard_job_id).into(),
                    last_future_template.template_id,
                );

                if !downstream.require_std_job.load(Ordering::Relaxed) {
                    downstream
                        .group_channel
                        .with(|channel| {
                            channel.add_channel_id(standard_channel_id, extranonce_prefix_size)
                        })
                        .map_err(JDCError::shutdown)?
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            JDCError::shutdown(e)
                        })?;
                }
            }

            Ok(messages)
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    // Handles an `OpenExtendedMiningChannel` request from a downstream.
    //
    // Workflow:
    // 1. Extract the `downstream_id` from `user_identity`.
    // 2. Create a new `ExtendedChannel` with the requested parameters.
    // 3. Send back to the downstream:
    //    - `OpenExtendedMiningChannelSuccess`
    //    - `NewExtendedMiningJob` (based on the latest future template)
    //    - `SetNewPrevHash` (to immediately activate the job)
    // 4. Update internal state, including:
    //    - Extended channel registry
    //    - Downstream/channel mappings
    //    - Vardiff state
    //
    // Returns an error if the downstream is missing, template/prevhash are unavailable,
    // or if extended channel creation fails.
    async fn handle_open_extended_mining_channel(
        &mut self,
        client_id: Option<usize>,
        msg: OpenExtendedMiningChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        info!(downstream_id, "Received: {}", msg);
        let request_id = msg.request_id;

        let nominal_hash_rate = msg.nominal_hash_rate;
        let requested_max_target = Target::from_le_bytes(msg.max_target.to_array());
        let requested_min_rollable_extranonce_size = msg.min_extranonce_size;

        let build_error = |code: &str| {
            MiningOwned::OpenMiningChannelError(OpenMiningChannelErrorOwned {
                request_id,
                error_code: code.try_into().expect("valid error code"),
            })
        };

        let last_future_template = self
            .last_future_template
            .get()
            .map_err(JDCError::shutdown)?;
        let Some(last_future_template) = last_future_template else {
            return Err(JDCError::disconnect(
                JDCErrorKind::FutureTemplateNotPresent,
                downstream_id,
            ));
        };
        let last_new_prev_hash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;
        let Some(last_new_prev_hash) = last_new_prev_hash else {
            return Err(JDCError::disconnect(
                JDCErrorKind::LastNewPrevhashNotFound,
                downstream_id,
            ));
        };
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
            let mut messages: Vec<RouteMessageTo> = Vec::new();
            let mut extended_channel = None;
            let mut extended_channel_id = None;

            if downstream.require_std_job.load(Ordering::Relaxed) {
                messages.push(
                    (
                        downstream_id,
                        build_error(
                            ERROR_CODE_OPEN_MINING_CHANNEL_EXTENDED_CHANNELS_NOT_SUPPORTED_FOR_STANDARD_JOBS,
                        ),
                    )
                        .into(),
                );
            } else {
                let next_extended_channel_id =
                    downstream.channel_id_factory.fetch_add(1, Ordering::Relaxed);
                let extranonce_prefix = match self
                    .extranonce_allocator
                    .with(|allocator| {
                        allocator.allocate_extended(requested_min_rollable_extranonce_size.into())
                    })
                    .map_err(JDCError::shutdown)?
                {
                    Ok(prefix) => Some(prefix),
                    Err(e) => {
                        error!(?e, "Extranonce prefix error");
                        let error_code =
                            extranonce_allocation_error_code(e).map_err(JDCError::shutdown)?;
                        messages.push((downstream_id, build_error(error_code)).into());
                        None
                    }
                };

                if let Some(extranonce_prefix) = extranonce_prefix {
                    let full_extranonce_size = self
                        .upstream_channel
                        .with(|channel| {
                            channel
                                .as_ref()
                                .map(|channel| channel.get_full_extranonce_size())
                                .unwrap_or(SOLO_FULL_EXTRANONCE_SIZE as usize)
                        })
                        .map_err(JDCError::shutdown)?;
                    let rollable_extranonce_size = full_extranonce_size - extranonce_prefix.len();
                    let pool_tag_string = self.pool_tag_string.get().map_err(JDCError::shutdown)?;

                    let new_extended_channel = match ExtendedChannel::new_for_job_declaration_client(
                        next_extended_channel_id,
                        user_identity.to_string(),
                        extranonce_prefix,
                        requested_max_target,
                        nominal_hash_rate,
                        true,
                        rollable_extranonce_size as u16,
                        self.share_batch_size,
                        self.shares_per_minute,
                        pool_tag_string,
                        self.miner_tag_string.clone(),
                        self.max_past_jobs,
                    ) {
                        Ok(channel) => Some(channel),
                        Err(e) => {
                            error!(?e, "Failed to create ExtendedChannel");
                            match e {
                                ExtendedChannelError::OpenChannelInvalidNominalHashrate(code) => {
                                    messages.push((downstream_id, build_error(code)).into());
                                    None
                                }
                                other => return Err(JDCError::disconnect(other, downstream_id)),
                            }
                        }
                    };

                    if let Some(channel) = new_extended_channel {
                        extended_channel_id = Some(next_extended_channel_id);
                        extended_channel = Some(channel);
                    }
                }
            }

            if let Some(mut extended_channel) = extended_channel {
                let extended_channel_id = extended_channel_id
                    .expect("extended_channel_id must be set when channel exists");
                let group_channel_id = downstream
                    .group_channel
                    .with(|channel| channel.get_group_channel_id())
                    .map_err(JDCError::shutdown)?;
                let open_extended_mining_channel_success = OpenExtendedMiningChannelSuccessOwned {
                    request_id,
                    channel_id: extended_channel_id,
                    target: extended_channel.get_target().to_le_bytes().into(),
                    extranonce_prefix: extended_channel
                        .get_extranonce_prefix()
                        .to_vec()
                        .try_into()
                        .expect("valid extranonce prefix"),
                    extranonce_size: extended_channel.get_rollable_extranonce_size(),
                    group_channel_id,
                }
                ;

                let full_extranonce_size = extended_channel.get_full_extranonce_size();
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::OpenExtendedMiningChannelSuccess(
                            open_extended_mining_channel_success,
                        ),
                    )
                        .into(),
                );

                let mut coinbase_outputs =
                    deserialize_outputs(self.coinbase_outputs.get().map_err(JDCError::shutdown)?)
                        .map_err(|_| {
                            JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs)
                        })?;
                coinbase_outputs[0].value =
                    Amount::from_sat(last_future_template.coinbase_tx_value_remaining);
                extended_channel
                    .on_new_template(last_future_template.clone(), coinbase_outputs)
                    .map_err(|e| {
                        error!(?e, "Failed to apply template to extended channel");
                        JDCError::shutdown(e)
                    })?;

                let future_extended_job_id = extended_channel
                    .get_future_job_id_from_template_id(last_future_template.template_id)
                    .expect("future job id must exist");
                let future_extended_job_message = extended_channel
                    .get_future_job(future_extended_job_id)
                    .expect("future job must exist")
                    .get_job_message()
                    .clone()
                    ;
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::NewExtendedMiningJob(future_extended_job_message),
                    )
                        .into(),
                );

                let prev_hash = last_new_prev_hash.prev_hash.clone();
                let header_timestamp = last_new_prev_hash.header_timestamp;
                let n_bits = last_new_prev_hash.n_bits;
                let set_new_prev_hash_mining = SetNewPrevHashOwned {
                    channel_id: extended_channel_id,
                    job_id: future_extended_job_id,
                    prev_hash,
                    min_ntime: header_timestamp,
                    nbits: n_bits,
                };
                extended_channel
                    .on_set_new_prev_hash(last_new_prev_hash)
                    .map_err(|e| {
                        error!(?e, "Failed to set prevhash on extended channel");
                        JDCError::shutdown(e)
                    })?;
                messages.push(
                    (
                        downstream_id,
                        MiningOwned::SetNewPrevHash(set_new_prev_hash_mining),
                    )
                        .into(),
                );

                downstream
                    .extended_channels
                    .insert(extended_channel_id, extended_channel);
                self.downstream_channel_id_and_job_id_to_template_id.insert(
                    (downstream_id, extended_channel_id, future_extended_job_id).into(),
                    last_future_template.template_id,
                );
                self.vardiff.insert(
                    (downstream_id, extended_channel_id).into(),
                    VardiffState::new().expect("Vardiff should instantiate."),
                );
                downstream
                    .group_channel
                    .with(|channel| {
                        channel.add_channel_id(extended_channel_id, full_extranonce_size)
                    })
                    .map_err(JDCError::shutdown)?
                    .map_err(|e| {
                        error!("Failed to add channel id to group channel: {:?}", e);
                        JDCError::shutdown(e)
                    })?;
            }

            Ok(messages)
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    // Handles an `UpdateChannel` message from a downstream.
    //
    // Workflow:
    // 1. Update the target for the corresponding downstream channel (standard or extended).
    //    - On success, reply with a `SetTarget`.
    //    - On failure, return an `UpdateChannelError`.
    // 2. Recompute aggregate downstream state:
    //    - Sum all downstream nominal hashrates.
    //    - Determine the minimum target across all downstream channels.
    // 3. Propagate the update upstream by sending an `UpdateChannel` with the aggregated hashrate
    //    and minimum target.
    //
    // Returns an error if the downstream channel is missing or update
    // validation fails.
    async fn handle_update_channel(
        &mut self,
        client_id: Option<usize>,
        msg: UpdateChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let channel_id = msg.channel_id;
        let new_nominal_hash_rate = msg.nominal_hash_rate;
        let requested_maximum_target = Target::from_le_bytes(msg.maximum_target.to_array());
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let build_error = |code: &str| {
            error!(channel_id, error_code = code, "UpdateChannelError");
            MiningOwned::UpdateChannelError(UpdateChannelErrorOwned {
                channel_id,
                error_code: code.to_string().try_into().expect("valid error code"),
            })
        };
        let mut messages = self.with_registered_downstream(downstream_id, |downstream| {
            let channel_messages = if let Some(messages_) =
                downstream
                    .standard_channels
                    .with_mut(&channel_id, |standard_channel| {
                        let mut messages: Vec<RouteMessageTo> = vec![];
                        let update_channel = standard_channel
                            .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                        let new_target = standard_channel.get_target();

                        if let Err(e) = update_channel {
                            error!(channel_id, ?e, "StandardChannel update failed");
                            let err_code = match e {
                                StandardChannelError::UpdateChannelInvalidNominalHashrate(code) => {
                                    code
                                }
                                _ => "internal-error",
                            };
                            if err_code != "internal-error" {
                                return vec![(downstream_id, build_error(err_code)).into()];
                            }
                            warn!("Failed to update standard channel {channel_id}");
                        }

                        messages.push(
                            (
                                downstream_id,
                                MiningOwned::SetTarget(SetTargetOwned {
                                    channel_id,
                                    maximum_target: new_target.to_le_bytes().into(),
                                }),
                            )
                                .into(),
                        );
                        messages
                    }) {
                messages_
            } else if let Some(messages_) =
                downstream
                    .extended_channels
                    .with_mut(&channel_id, |extended_channel| {
                        let mut messages: Vec<RouteMessageTo> = vec![];
                        let update_channel = extended_channel
                            .update_channel(new_nominal_hash_rate, Some(requested_maximum_target));
                        let new_target = extended_channel.get_target();

                        if let Err(e) = update_channel {
                            error!(channel_id, ?e, "ExtendedChannel update failed");
                            let err_code = match e {
                                ExtendedChannelError::UpdateChannelInvalidNominalHashrate(code) => {
                                    code
                                }
                                _ => "internal-error",
                            };
                            if err_code != "internal-error" {
                                return vec![(downstream_id, build_error(err_code)).into()];
                            }
                            warn!("Failed to update extended channel {channel_id}");
                        }

                        messages.push(
                            (
                                downstream_id,
                                MiningOwned::SetTarget(SetTargetOwned {
                                    channel_id,
                                    maximum_target: new_target.to_le_bytes().into(),
                                }),
                            )
                                .into(),
                        );
                        messages
                    })
            {
                messages_
            } else {
                vec![
                    (
                        downstream_id,
                        build_error(ERROR_CODE_UPDATE_CHANNEL_INVALID_CHANNEL_ID),
                    )
                        .into(),
                ]
            };
            Ok(channel_messages)
        })?;

        let mut downstream_hashrate = 0.0;
        let mut min_target = Target::from_le_bytes([0xff; 32]);
        self.downstream.for_each(|_, downstream| {
            let mut update_from_channel = |hashrate: f32, target: &Target| {
                downstream_hashrate += hashrate;
                min_target = std::cmp::min(*target, min_target);
            };

            downstream.standard_channels.for_each(|_, channel| {
                update_from_channel(channel.get_nominal_hashrate(), channel.get_target());
            });

            downstream.extended_channels.for_each(|_, channel| {
                update_from_channel(channel.get_nominal_hashrate(), channel.get_target());
            });
        });

        self.upstream_channel
            .with(|upstream_channel| {
                if let Some(upstream_channel) = upstream_channel.as_mut() {
                    debug!(
                        "Checking upstream channel {} with hashrate {} and target {:?}",
                        upstream_channel.get_channel_id(),
                        upstream_channel.get_nominal_hashrate(),
                        upstream_channel.get_target()
                    );

                    upstream_channel.set_nominal_hashrate(downstream_hashrate);
                    info!("Sending update channel message upstream");
                    messages.push(
                        MiningOwned::UpdateChannel(UpdateChannelOwned {
                            channel_id: upstream_channel.get_channel_id(),
                            nominal_hash_rate: downstream_hashrate,
                            maximum_target: min_target.to_le_bytes().into(),
                        })
                        .into(),
                    );
                }
            })
            .map_err(JDCError::shutdown)?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    // Handles a `SubmitSharesStandard` message from a downstream.
    //
    // Steps:
    // 1. Validate the share against the downstream channel.
    //    - On error, respond with `SubmitSharesError`.
    //    - On success, acknowledge with `SubmitSharesSuccess` (and optionally a block found).
    //
    // 2. If the share is valid, attempt to forward it upstream:
    //    - Translate the share into an upstream `SubmitSharesExtended`.
    //    - Validate with the upstream channel.
    //    - Forward valid shares (or block solutions) upstream.
    async fn handle_submit_shares_standard(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesStandardOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesStandard");
        let channel_id = msg.channel_id;
        let downstream_job_id = msg.job_id;
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let build_error = |code: &str| {
            MiningOwned::SubmitSharesError(SubmitSharesErrorOwned {
                channel_id,
                sequence_number: msg.sequence_number,
                error_code: code.try_into().expect("valid error code"),
            })
        };

        let Some(prev_hash) = self.last_new_prev_hash.get().map_err(JDCError::shutdown)? else {
            warn!("No prev_hash available yet, ignoring share");
            return Err(JDCError::disconnect(
                JDCErrorKind::LastNewPrevhashNotFound,
                downstream_id,
            ));
        };

        let vardiff_key = (downstream_id, channel_id).into();
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
            let validation = downstream.standard_channels.with_mut(&channel_id, |standard_channel| {
                let mut messages: Vec<RouteMessageTo> = vec![];

                let res = standard_channel.validate_share(msg.clone());
                let mut is_downstream_share_valid = false;
                let mut downstream_share_hash: Option<sha256d::Hash> = None;
                match res {
                    Ok(ShareValidationResult::Valid(share_hash)) => {
                        let share_accounting = standard_channel.get_share_accounting();
                        if share_accounting.should_acknowledge() {
                            let success = SubmitSharesSuccessOwned {
                                channel_id,
                                last_sequence_number: share_accounting.get_last_share_sequence_number(),
                                new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                                new_shares_sum: share_accounting.get_last_batch_work_sum(),
                            };
                            info!("SubmitSharesStandard on downstream channel: {} ✅", success);
                            messages.push((downstream.downstream_id, MiningOwned::SubmitSharesSuccess(success)).into());
                        } else {
                            info!(
                                "SubmitSharesStandard on downstream channel: valid share | channel_id: {}, sequence_number: {}, share_hash: {} ☑️",
                                channel_id, msg.sequence_number, share_hash
                            );
                        }
                        downstream_share_hash = Some(share_hash);
                        is_downstream_share_valid = true;
                    }
                    Ok(ShareValidationResult::BlockFound(share_hash, template_id, coinbase)) => {
                        info!("SubmitSharesStandard on downstream channel: 💰 Block Found!!! 💰{share_hash}");
                        downstream_share_hash = Some(share_hash);
                        is_downstream_share_valid = true;
                        if let Some(template_id) = template_id {
                            info!("SubmitSharesStandard: Propagating solution to the Template Provider.");
                            let solution = SubmitSolutionOwned {
                                template_id,
                                version: msg.version,
                                header_timestamp: msg.ntime,
                                header_nonce: msg.nonce,
                                coinbase_tx: coinbase.try_into().map_err(JDCError::shutdown)?,
                            };
                            messages.push(TemplateDistributionOwned::SubmitSolution(solution).into());
                        }
                        let share_accounting = standard_channel.get_share_accounting().clone();
                        let success = SubmitSharesSuccessOwned {
                            channel_id,
                            last_sequence_number: share_accounting.get_last_share_sequence_number(),
                            new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                            new_shares_sum: share_accounting.get_last_batch_work_sum(),
                        };
                        messages.push((downstream.downstream_id, MiningOwned::SubmitSharesSuccess(success)).into());
                    }
                    Err(ShareValidationError::SeenSharesBudgetExhausted) => {
                        // share rate far outran the channel's configured expectation, filling the
                        // per-tip dedup cache; no error code to report, so close the downstream.
                        warn!("closing downstream {}: channel {} exhausted its accepted-share dedup budget 🛑", downstream_id, channel_id);
                        return Err(JDCError::disconnect(
                            ShareValidationError::SeenSharesBudgetExhausted,
                            downstream_id,
                        ));
                    }
                    Err(err) => {
                        let code = match err {
                            ShareValidationError::Invalid(code) => code,
                            ShareValidationError::Stale(code) => code,
                            ShareValidationError::InvalidJobId(code) => code,
                            ShareValidationError::DoesNotMeetTarget(code) => code,
                            ShareValidationError::DuplicateShare(code) => code,
                            ShareValidationError::VersionRollingNotAllowed(code) => code,
                            _ => unreachable!(),
                        };
                        error!(
                            "❌ SubmitSharesError: ch={}, seq={}, error={code}",
                            channel_id, msg.sequence_number
                        );
                        messages.push((downstream_id, build_error(code)).into());
                    }
                }

                if !is_downstream_share_valid {
                    return Ok(messages);
                }

                let mapping_key = (downstream_id, channel_id, downstream_job_id).into();
                let template_id = self
                    .downstream_channel_id_and_job_id_to_template_id
                    .get_cloned(&mapping_key);
                let upstream_job_id = template_id
                    .and_then(|tid| self.template_id_to_upstream_job_id.get_cloned(&tid));

                self.upstream_channel
                    .with(|maybe_upstream_channel| -> Result<(), Self::Error> {
                        let Some(upstream_channel) = maybe_upstream_channel.as_mut() else {
                            return Ok(());
                        };

                        let extranonce_prefix = standard_channel.get_extranonce_prefix();
                        let upstream_extranonce_prefix = upstream_channel.get_extranonce_prefix();
                        let extranonce = &extranonce_prefix[upstream_extranonce_prefix.len()..];

                        let mut upstream_message = SubmitSharesExtendedOwned {
                            channel_id: upstream_channel.get_channel_id(),
                            job_id: 0, // set later if known
                            extranonce: extranonce.to_vec().try_into().map_err(JDCError::shutdown)?,
                            nonce: msg.nonce,
                            ntime: msg.ntime,
                            // We assign sequence number later, when we validate the share
                            // and send it to upstream.
                            sequence_number: 0,
                            version: msg.version,
                        };

                        match upstream_job_id {
                            // The presence of an `upstream_job_id` indicates that the upstream
                            // has acknowledged the custom job (`SetCustomMiningJob.Success` was received by JDC) 
                            // and is ready to accept shares for it.
                            //
                            // We use optimistic mining: downstream miners are instructed to start
                            // mining before the upstream acknowledgement arrives. Once the custom job
                            // is acknowledged, shares can be safely submitted.
                            //
                            // See the Job Declaration Modes section for details:
                            // https://stratumprotocol.org/specification/06-job-declaration-protocol/#63-job-declaration-modes
                            Some(upstream_job_id) => {
                                upstream_message.job_id = upstream_job_id;
                                match upstream_channel.validate_share(upstream_message.clone()) {
                                    Ok(client::share_accounting::ShareValidationResult::Valid(share_hash)) => {
                                        upstream_message.sequence_number =
                                            self.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                            info!(
                                        "SubmitSharesStandard, forwarding it to upstream: valid share | channel_id: {}, sequence_number: {}, share_hash: {}  ✅",
                                        channel_id, upstream_message.sequence_number, share_hash
                                    );
                                        messages.push(MiningOwned::SubmitSharesExtended(upstream_message).into());
                                    }
                                    Ok(client::share_accounting::ShareValidationResult::BlockFound(share_hash)) => {
                                        upstream_message.sequence_number =
                                            self.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                        info!("SubmitSharesStandard forwarding it to upstream: 💰 Block Found!!! 💰{share_hash}");
                                        let push_solution = PushSolutionOwned {
                                            extranonce: standard_channel
                                                .get_extranonce_prefix()
                                                .to_vec()
                                                .try_into()
                                                .map_err(JDCError::shutdown)?,
                                            ntime: upstream_message.ntime,
                                            nonce: upstream_message.nonce,
                                            version: upstream_message.version,
                                            nbits: prev_hash.n_bits,
                                            prev_hash: prev_hash.prev_hash.clone(),
                                        };
                                        messages.push(JobDeclarationOwned::PushSolution(push_solution).into());
                                        messages.push(MiningOwned::SubmitSharesExtended(upstream_message).into());
                                    }
                                    Err(err) => {
                                        let code = match err {
                                        client::share_accounting::ShareValidationError::Invalid(code) => code,
                                        client::share_accounting::ShareValidationError::Stale(code) => code,
                                        client::share_accounting::ShareValidationError::InvalidJobId(code) => code,
                                        client::share_accounting::ShareValidationError::DoesNotMeetTarget(code) => code,
                                        client::share_accounting::ShareValidationError::DuplicateShare(code) => code,
                                        client::share_accounting::ShareValidationError::VersionRollingNotAllowed(code) => code,
                                        _ => unreachable!(),
                                    };
                                    debug!("❌ SubmitSharesError not forwarding it to upstream: ch={}, seq={}, error={code}", channel_id, upstream_message.sequence_number);
                                    }
                                }
                            }
                            None => {
                                debug!(
                                    "SubmitSharesStandard: upstream job_id not yet known (still waiting for the SetCustomMiningJob.Success message), caching share (channel_id={}, downstream_job_id={})",
                                    channel_id, downstream_job_id
                                );
                                if let Some(template_id) = template_id {
                                    let hash = downstream_share_hash.expect(
                                        "downstream_share_hash must be set when downstream share is valid",
                                    );
                                    let entry =
                                        SharesOrderedByDiff::new(upstream_message, hash);
                                    self.cached_shares.with_mut_or_default(template_id, |heap| {
                                        add_share_to_cache(heap, entry);
                                    });
                                } else {
                                    warn!(
                                        "SubmitSharesStandard: could not cache share, no template_id found for key (downstream_id={}, channel_id={}, downstream_job_id={})",
                                        downstream_id, channel_id, downstream_job_id
                                    );
                                }
                            }
                        }

                        Ok(())
                    })
                    .map_err(JDCError::shutdown)??;

                Ok(messages)
            });
            match validation {
                Some(validation) => {
                    if self
                        .vardiff
                        .with_mut(&vardiff_key, |vardiff| {
                            vardiff.increment_shares_since_last_update();
                        })
                        .is_none()
                    {
                        return Ok(vec![(
                            downstream_id,
                            MiningOwned::CloseChannel(create_close_channel_msg(
                                channel_id,
                                "invalid-channel-id",
                            )),
                        )
                            .into()]);
                    }
                    validation
                }
                None => Ok(vec![(
                    downstream_id,
                    build_error(ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID),
                )
                    .into()]),
            }
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    // Handles a `SubmitSharesExtended` message from a downstream.
    //
    // Steps:
    // 1. Validate the share against the downstream channel.
    //    - On error, respond with `SubmitSharesError`.
    //    - On success, acknowledge with `SubmitSharesSuccess` (and optionally a block found).
    //
    // 2. If the share is valid, attempt to forward it upstream:
    //    - Translate the share into an upstream `SubmitSharesExtended`.
    //    - Validate with the upstream channel.
    //    - Forward valid shares (or block solutions) upstream.
    async fn handle_submit_shares_extended(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesExtendedOwned,
        tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesExtended");
        let channel_id = msg.channel_id;
        let downstream_job_id = msg.job_id;
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        let negotiated_extensions = self.get_negotiated_extensions_with_client(client_id);

        let build_error = |code: &str| {
            MiningOwned::SubmitSharesError(SubmitSharesErrorOwned {
                channel_id,
                sequence_number: msg.sequence_number,
                error_code: code.try_into().expect("valid error code"),
            })
        };

        let Some(prev_hash) = self.last_new_prev_hash.get().map_err(JDCError::shutdown)? else {
            warn!("No prev_hash available yet, ignoring share");
            return Err(JDCError::disconnect(
                JDCErrorKind::LastNewPrevhashNotFound,
                downstream_id,
            ));
        };

        let vardiff_key = (downstream_id, channel_id).into();
        let messages = self.with_registered_downstream(downstream_id, |downstream| {
            let validation = downstream.extended_channels.with_mut(&channel_id, |extended_channel| {
                let mut messages: Vec<RouteMessageTo> = vec![];
                // here we extract and set the user_identity from the TLV fields if the extension is negotiated
                let _user_identity = if negotiated_extensions
                    .as_ref()
                    .is_ok_and(|exts| exts.contains(&EXTENSION_TYPE_WORKER_HASHRATE_TRACKING))
                {
                    tlv_fields.and_then(|tlvs| {
                        tlvs.iter()
                            .find(|tlv| {
                                tlv.r#type.extension_type
                                    == EXTENSION_TYPE_WORKER_HASHRATE_TRACKING
                                    && tlv.r#type.field_type == TLV_FIELD_TYPE_USER_IDENTITY
                            })
                            .and_then(|tlv| UserIdentity::from_tlv(tlv).ok())
                    })
                } else {
                    None
                };

                let res = extended_channel.validate_share(msg.clone());
                let mut is_downstream_share_valid = false;
                let mut downstream_share_hash: Option<sha256d::Hash> = None;
                match res {
                    Ok(ShareValidationResult::Valid(share_hash)) => {
                        let share_accounting = extended_channel.get_share_accounting();
                        if share_accounting.should_acknowledge() {
                            let success = SubmitSharesSuccessOwned {
                                channel_id,
                                last_sequence_number: share_accounting.get_last_share_sequence_number(),
                                new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                                new_shares_sum: share_accounting.get_last_batch_work_sum(),
                            };
                            info!("SubmitSharesExtended on downstream channel: {} ✅", success);
                            messages.push((downstream.downstream_id, MiningOwned::SubmitSharesSuccess(success)).into());
                        } else {
                            info!(
                                "SubmitSharesExtended on downstream channel: valid share | channel_id: {}, sequence_number: {}, share_hash: {} ☑️",
                                channel_id, msg.sequence_number, share_hash
                            );
                        }
                        downstream_share_hash = Some(share_hash);
                        is_downstream_share_valid = true;
                    }
                    Ok(ShareValidationResult::BlockFound(share_hash, template_id, coinbase)) => {
                        info!("SubmitSharesExtended on downstream channel: 💰 Block Found!!! 💰{share_hash}");
                        downstream_share_hash = Some(share_hash);
                        if let Some(template_id) = template_id {
                            info!("SubmitSharesExtended: Propagating solution to the Template Provider.");
                            let solution = SubmitSolutionOwned {
                                template_id,
                                version: msg.version,
                                header_timestamp: msg.ntime,
                                header_nonce: msg.nonce,
                                coinbase_tx: coinbase.try_into().map_err(JDCError::shutdown)?,
                            };
                            messages.push(TemplateDistributionOwned::SubmitSolution(solution).into());
                        }
                        let share_accounting = extended_channel.get_share_accounting().clone();
                        let success = SubmitSharesSuccessOwned {
                            channel_id,
                            last_sequence_number: share_accounting.get_last_share_sequence_number(),
                            new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                            new_shares_sum: share_accounting.get_last_batch_work_sum(),
                        };
                        is_downstream_share_valid = true;
                        messages.push((downstream.downstream_id, MiningOwned::SubmitSharesSuccess(success)).into());
                    }
                    Err(ShareValidationError::SeenSharesBudgetExhausted) => {
                        // share rate far outran the channel's configured expectation, filling the
                        // per-tip dedup cache; no error code to report, so close the downstream.
                        warn!("closing downstream {}: channel {} exhausted its accepted-share dedup budget 🛑", downstream_id, channel_id);
                        return Err(JDCError::disconnect(
                            ShareValidationError::SeenSharesBudgetExhausted,
                            downstream_id,
                        ));
                    }
                    Err(err) => {
                        let code = match err {
                            ShareValidationError::Invalid(code) => code,
                            ShareValidationError::Stale(code) => code,
                            ShareValidationError::InvalidJobId(code) => code,
                            ShareValidationError::DoesNotMeetTarget(code) => code,
                            ShareValidationError::DuplicateShare(code) => code,
                            ShareValidationError::BadExtranonceSize(code) => code,
                            ShareValidationError::VersionRollingNotAllowed(code) => code,
                            _ => unreachable!(),
                        };
                        error!(
                            "❌ SubmitSharesError on downstream channel: ch={}, seq={}, error={code}",
                            channel_id, msg.sequence_number
                        );
                        messages.push((downstream_id, build_error(code)).into());
                    }
                }

                if !is_downstream_share_valid {
                    return Ok(messages);
                }

                let mapping_key = (downstream_id, channel_id, downstream_job_id).into();
                let template_id = self
                    .downstream_channel_id_and_job_id_to_template_id
                    .get_cloned(&mapping_key);
                let upstream_job_id = template_id
                    .and_then(|tid| self.template_id_to_upstream_job_id.get_cloned(&tid));

                self.upstream_channel
                    .with(|maybe_upstream_channel| -> Result<(), Self::Error> {
                        let Some(upstream_channel) = maybe_upstream_channel.as_mut() else {
                            return Ok(());
                        };

                        let extranonce_prefix = extended_channel.get_extranonce_prefix();
                        let upstream_extranonce_prefix = upstream_channel.get_extranonce_prefix();
                        let new_extranonce_prefix =
                            &extranonce_prefix[upstream_extranonce_prefix.len()..];

                        let mut upstream_message = msg.clone();
                        upstream_message.channel_id = upstream_channel.get_channel_id();
                        upstream_message.sequence_number = 0;

                        let mut extranonce = vec![];
                        extranonce.extend_from_slice(new_extranonce_prefix);
                        extranonce.extend_from_slice(msg.extranonce.as_bytes());
                        upstream_message.extranonce = extranonce
                            .try_into()
                            .map_err(|e| JDCError::disconnect(e, downstream_id))?;

                        match upstream_job_id {
                            // The presence of an `upstream_job_id` indicates that the upstream
                            // has acknowledged the custom job (`SetCustomMiningJob.Success` was received by JDC) 
                            // and is ready to accept shares for it.
                            //
                            // We use optimistic mining: downstream miners are instructed to start
                            // mining before the upstream acknowledgement arrives. Once the custom job
                            // is acknowledged, shares can be safely submitted.
                            //
                            // See the Job Declaration Modes section for details:
                            // https://stratumprotocol.org/specification/06-job-declaration-protocol/#63-job-declaration-modes
                            Some(upstream_job_id) => {
                                upstream_message.job_id = upstream_job_id;
                                match upstream_channel.validate_share(upstream_message.clone()) {
                                    Ok(client::share_accounting::ShareValidationResult::Valid(share_hash)) => {
                                        upstream_message.sequence_number =
                                            self.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                        info!(
                                            "SubmitSharesExtended forwarding it to upstream: valid share | channel_id: {}, sequence_number: {}, share_hash: {}  ✅",
                                            channel_id, upstream_message.sequence_number, share_hash
                                        );
                                        messages.push(
                                            MiningOwned::SubmitSharesExtended(upstream_message).into(),
                                        );
                                    }
                                    Ok(client::share_accounting::ShareValidationResult::BlockFound(share_hash)) => {
                                        upstream_message.sequence_number =
                                            self.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                        info!("SubmitSharesExtended forwarding it to upstream: 💰 Block Found!!! 💰{share_hash}");
                                        let mut channel_extranonce =
                                            upstream_channel.get_extranonce_prefix().to_vec();
                                        channel_extranonce
                                            .extend_from_slice(upstream_message.extranonce.as_bytes());
                                        let push_solution = PushSolutionOwned {
                                            extranonce: channel_extranonce
                                                .try_into()
                                                .map_err(JDCError::shutdown)?,
                                            ntime: upstream_message.ntime,
                                            nonce: upstream_message.nonce,
                                            version: upstream_message.version,
                                            nbits: prev_hash.n_bits,
                                            prev_hash: prev_hash.prev_hash.clone(),
                                        };
                                        messages.push(JobDeclarationOwned::PushSolution(push_solution).into());
                                        messages.push(
                                            MiningOwned::SubmitSharesExtended(upstream_message).into(),
                                        );
                                    }
                                    Err(err) => {
                                        let code = match err {
                                            client::share_accounting::ShareValidationError::Invalid(code) => code,
                                            client::share_accounting::ShareValidationError::Stale(code) => code,
                                            client::share_accounting::ShareValidationError::InvalidJobId(code) => code,
                                            client::share_accounting::ShareValidationError::DoesNotMeetTarget(code) => code,
                                            client::share_accounting::ShareValidationError::DuplicateShare(code) => code,
                                            client::share_accounting::ShareValidationError::BadExtranonceSize(code) => code,
                                            client::share_accounting::ShareValidationError::VersionRollingNotAllowed(code) => code,
                                            _ => unreachable!(),
                                        };
                                        debug!(
                                            "❌ SubmitSharesError not forwarding it to upstream: ch={}, seq={}, error={code}",
                                            channel_id, upstream_message.sequence_number
                                        );
                                    }
                                }
                            }
                            None => {
                                debug!("Upstream job_id not yet known (still waiting for the SetCustomMiningJob.Success message), caching share");
                                if let Some(template_id) = template_id {
                                    let hash = downstream_share_hash.expect(
                                        "downstream_share_hash must be set when downstream share is valid",
                                    );
                                    let entry =
                                        SharesOrderedByDiff::new(upstream_message, hash);
                                    self.cached_shares.with_mut_or_default(template_id, |heap| {
                                        add_share_to_cache(heap, entry);
                                    });
                                } else {
                                    warn!(
                                        "SubmitSharesExtended: could not cache share, no template_id found for key (downstream_id={}, channel_id={}, downstream_job_id={})",
                                        downstream_id, channel_id, downstream_job_id
                                    );
                                }
                            }
                        }

                        Ok(())
                    })
                    .map_err(JDCError::shutdown)??;

                Ok(messages)
            });
            match validation {
                Some(validation) => {
                    if self
                        .vardiff
                        .with_mut(&vardiff_key, |vardiff| {
                            vardiff.increment_shares_since_last_update();
                        })
                        .is_none()
                    {
                        return Ok(vec![(
                            downstream_id,
                            MiningOwned::CloseChannel(create_close_channel_msg(
                                channel_id,
                                "invalid-channel-id",
                            )),
                        )
                            .into()]);
                    }
                    validation
                }
                None => Ok(vec![(
                    downstream_id,
                    build_error(ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID),
                )
                    .into()]),
            }
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    // Handles an incoming `SetCustomMiningJob` message from a downstream.
    async fn handle_set_custom_mining_job(
        &mut self,
        _client_id: Option<usize>,
        msg: SetCustomMiningJobOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        Err(JDCError::log(JDCErrorKind::UnexpectedMessage(
            0,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
        )))
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
