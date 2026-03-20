use std::sync::atomic::Ordering;

use stratum_apps::{
    stratum_core::{
        binary_sv2::Str0255,
        bitcoin::{hashes::sha256d, Amount, Target},
        channels_sv2::{
            client,
            outputs::deserialize_outputs,
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
        job_declaration_sv2::PushSolution,
        mining_sv2::*,
        parsers_sv2::{AnyMessage, JobDeclaration, Mining, TemplateDistribution, Tlv, TlvField},
        template_distribution_sv2::SubmitSolution,
    },
    utils::types::Sv2Frame,
};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::{
        ChannelManager, ChannelManagerChannel, SharesOrderedByDiff, FULL_EXTRANONCE_SIZE,
    },
    error::{self, JDCError, JDCErrorKind},
    jd_mode::{get_jd_mode, JdMode},
    utils::{add_share_to_cache, create_close_channel_msg},
};

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
pub enum RouteMessageTo<'a> {
    /// Route to the upstream (mining) channel.
    Upstream(Mining<'a>),
    /// Route to the job declarator subsystem.
    JobDeclarator(JobDeclaration<'a>),
    /// Route to the template provider subsystem.
    TemplateProvider(TemplateDistribution<'a>),
    /// Route to a specific downstream client by ID, along with its mining message.
    Downstream((usize, Mining<'a>)),
}

impl<'a> From<Mining<'a>> for RouteMessageTo<'a> {
    fn from(value: Mining<'a>) -> Self {
        Self::Upstream(value)
    }
}

impl<'a> From<JobDeclaration<'a>> for RouteMessageTo<'a> {
    fn from(value: JobDeclaration<'a>) -> Self {
        Self::JobDeclarator(value)
    }
}

impl<'a> From<TemplateDistribution<'a>> for RouteMessageTo<'a> {
    fn from(value: TemplateDistribution<'a>) -> Self {
        Self::TemplateProvider(value)
    }
}

impl<'a> From<(usize, Mining<'a>)> for RouteMessageTo<'a> {
    fn from(value: (usize, Mining<'a>)) -> Self {
        Self::Downstream(value)
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl RouteMessageTo<'_> {
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
    /// - [`RouteMessageTo::Upstream`] → Sends the mining message upstream, unless in
    ///   [`JdMode::SoloMining`].
    /// - [`RouteMessageTo::JobDeclarator`] → Sends the job declaration message to the JDS.
    /// - [`RouteMessageTo::TemplateProvider`] → Sends the template distribution message to the
    ///   template provider.
    pub async fn forward(
        self,
        channel_manager_channel: &ChannelManagerChannel,
    ) -> Result<(), JDCErrorKind> {
        match self {
            RouteMessageTo::Downstream((downstream_id, message)) => {
                let sender = channel_manager_channel
                    .downstream_sender
                    .super_safe_lock(|map| map.get(&downstream_id).cloned());
                if let Some(sender) = sender {
                    sender.send((message.into_static(), None)).await?;
                } else {
                    debug!("Dropping message for downstream {downstream_id}: no longer connected");
                }
            }
            RouteMessageTo::Upstream(message) => {
                if get_jd_mode() != JdMode::SoloMining {
                    let message_static = message.into_static();
                    let sv2_frame: Sv2Frame = AnyMessage::Mining(message_static).try_into()?;
                    channel_manager_channel
                        .upstream_sender
                        .send(sv2_frame)
                        .await?;
                }
            }
            RouteMessageTo::JobDeclarator(message) => {
                channel_manager_channel
                    .jd_sender
                    .send(message.into_static())
                    .await?;
            }
            RouteMessageTo::TemplateProvider(message) => {
                channel_manager_channel
                    .tp_sender
                    .send(message.into_static())
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromClientAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

    fn get_negotiated_extensions_with_client(
        &self,
        client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        let negotiated_extensions = self.channel_manager_data.super_safe_lock(|data| {
            data.downstream
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
        msg: CloseChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        self.channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                let Some(downstream) = channel_manager_data.downstream.get(&downstream_id) else {
                    error!(
                        "No downstream with channel_id: {:?} and downstream_id: {:?}, found",
                        msg.channel_id, downstream_id
                    );
                    return Err(JDCError::disconnect(
                        JDCErrorKind::DownstreamNotFound(downstream_id),
                        downstream_id,
                    ));
                };
                downstream.downstream_data.super_safe_lock(|data| {
                    data.extended_channels.remove(&msg.channel_id);
                    data.standard_channels.remove(&msg.channel_id);
                });
                channel_manager_data
                    .vardiff
                    .remove(&(downstream_id, msg.channel_id).into());
                Ok(())
            })
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
        msg: OpenStandardMiningChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let request_id = msg.get_request_id_as_u32();
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let coinbase_outputs = self
            .channel_manager_data
            .super_safe_lock(|data| data.coinbase_outputs.clone());

        let mut coinbase_outputs = deserialize_outputs(coinbase_outputs)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs))?;

        info!(downstream_id, "Received: {}", msg);

        let build_error = |code: &str| {
            Mining::OpenMiningChannelError(OpenMiningChannelError {
                request_id,
                error_code: code.to_string().try_into().expect("valid error code"),
            })
        };

        let messages: Vec<RouteMessageTo> =
            self.channel_manager_data
                .super_safe_lock(|channel_manager_data| {
                    let Some(last_future_template) =
                        channel_manager_data.last_future_template.clone()
                    else {
                        error!("Missing last_future_template, cannot open channel");
                        return Err(JDCError::disconnect(
                            JDCErrorKind::FutureTemplateNotPresent,
                            downstream_id,
                        ));
                    };

                    let Some(last_new_prev_hash) = channel_manager_data.last_new_prev_hash.clone()
                    else {
                        error!("Missing last_new_prev_hash, cannot open channel");
                        return Err(JDCError::disconnect(
                            JDCErrorKind::LastNewPrevhashNotFound,
                            downstream_id,
                        ));
                    };

                    let Some(downstream) = channel_manager_data.downstream.get(&downstream_id)
                    else {
                        error!(downstream_id, "Downstream not registered");
                        return Err(JDCError::disconnect(
                            JDCErrorKind::DownstreamNotFound(downstream_id),
                            downstream_id,
                        ));
                    };

                    coinbase_outputs[0].value =
                        Amount::from_sat(last_future_template.coinbase_tx_value_remaining);

                    downstream.downstream_data.super_safe_lock(|data| {
                        let mut messages: Vec<RouteMessageTo> = vec![];

                        let nominal_hash_rate = msg.nominal_hash_rate;
                        let requested_max_target = Target::from_le_bytes(
                            msg.max_target.inner_as_ref().try_into().unwrap(),
                        );

                        let group_channel_id = data.group_channel.get_group_channel_id();

                        let standard_channel_id =
                            data.channel_id_factory.fetch_add(1, Ordering::Relaxed);

                        let extranonce_prefix = match channel_manager_data
                            .extranonce_prefix_factory_standard
                            .next_prefix_standard()
                        {
                            Ok(p) => p,
                            Err(e) => {
                                error!(?e, "Failed to get extranonce prefix");
                                return Err(JDCError::shutdown(e));
                            }
                        };

                        let job_store = DefaultJobStore::new();
                        let mut standard_channel =
                            match StandardChannel::new_for_job_declaration_client(
                                standard_channel_id,
                                user_identity.to_string(),
                                extranonce_prefix.to_vec(),
                                requested_max_target,
                                nominal_hash_rate,
                                self.share_batch_size,
                                self.shares_per_minute,
                                job_store,
                                channel_manager_data.pool_tag_string.clone(),
                                self.miner_tag_string.clone(),
                            ) {
                                Ok(channel) => channel,
                                Err(e) => {
                                    error!(?e, "Failed to create standard channel");
                                    return match e {
                                        StandardChannelError::InvalidNominalHashrate => Ok(vec![(
                                            downstream_id,
                                            build_error("invalid-nominal-hashrate"),
                                        )
                                            .into()]),
                                        StandardChannelError::RequestedMaxTargetOutOfRange => {
                                            Ok(vec![(
                                                downstream_id,
                                                build_error("max-target-out-of-range"),
                                            )
                                                .into()])
                                        }
                                        other => Err(JDCError::disconnect(other, downstream_id)),
                                    };
                                }
                            };

                        let extranonce_prefix_size = standard_channel.get_extranonce_prefix().len();

                        let open_standard_mining_channel_success =
                            OpenStandardMiningChannelSuccess {
                                request_id: msg.request_id.clone(),
                                channel_id: standard_channel_id,
                                target: standard_channel.get_target().to_le_bytes().into(),
                                extranonce_prefix: standard_channel
                                    .get_extranonce_prefix()
                                    .clone()
                                    .try_into()
                                    .map_err(JDCError::shutdown)?,
                                group_channel_id,
                            }
                            .into_static();

                        messages.push(
                            (
                                downstream_id,
                                Mining::OpenStandardMiningChannelSuccess(
                                    open_standard_mining_channel_success,
                                ),
                            )
                                .into(),
                        );

                        if let Err(e) = standard_channel
                            .on_new_template(last_future_template.clone(), coinbase_outputs.clone())
                        {
                            error!(?e, "Failed to apply template to standard channel");
                            return Err(JDCError::shutdown(e));
                        }

                        let future_standard_job_id = standard_channel
                            .get_future_job_id_from_template_id(last_future_template.template_id)
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

                        let prev_hash = last_new_prev_hash.prev_hash.clone();
                        let header_timestamp = last_new_prev_hash.header_timestamp;
                        let n_bits = last_new_prev_hash.n_bits;
                        let set_new_prev_hash_mining = SetNewPrevHash {
                            channel_id: standard_channel_id,
                            job_id: future_standard_job_id,
                            prev_hash,
                            min_ntime: header_timestamp,
                            nbits: n_bits,
                        };

                        if let Err(e) =
                            standard_channel.on_set_new_prev_hash(last_new_prev_hash.clone())
                        {
                            error!(?e, "Failed to apply prevhash to standard channel");
                            return Err(JDCError::shutdown(e));
                        }
                        messages.push(
                            (
                                downstream_id,
                                Mining::SetNewPrevHash(set_new_prev_hash_mining),
                            )
                                .into(),
                        );

                        let vardiff =
                            VardiffState::new().expect("Vardiff state should instantiate.");

                        channel_manager_data
                            .vardiff
                            .insert((downstream_id, standard_channel_id).into(), vardiff);
                        data.standard_channels
                            .insert(standard_channel_id, standard_channel);

                        channel_manager_data
                            .downstream_channel_id_and_job_id_to_template_id
                            .insert(
                                (downstream_id, standard_channel_id, future_standard_job_id).into(),
                                last_future_template.template_id,
                            );
                        if !data.require_std_job {
                            data.group_channel
                                .add_channel_id(standard_channel_id, extranonce_prefix_size)
                                .map_err(|e| {
                                    error!("Failed to add channel id to group channel: {:?}", e);
                                    JDCError::shutdown(e)
                                })?;
                        }

                        Ok(messages)
                    })
                })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_channel).await {
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
        msg: OpenExtendedMiningChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let user_identity = msg.user_identity.as_utf8_or_hex();
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        info!(downstream_id, "Received: {}", msg);
        let request_id = msg.get_request_id_as_u32();

        let nominal_hash_rate = msg.nominal_hash_rate;
        let requested_max_target =
            Target::from_le_bytes(msg.max_target.inner_as_ref().try_into().unwrap());
        let requested_min_rollable_extranonce_size = msg.min_extranonce_size;

        let build_error = |code: &str| {
            Mining::OpenMiningChannelError(OpenMiningChannelError {
                request_id,
                error_code: code.to_string().try_into().expect("valid error code"),
            })
        };

        let messages = self
            .channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                let Some(last_future_template) = channel_manager_data.last_future_template.clone()
                else {
                    error!("No template to share");
                    return Err(JDCError::disconnect(
                        JDCErrorKind::FutureTemplateNotPresent,
                        downstream_id,
                    ));
                };

                let Some(last_new_prev_hash) = channel_manager_data.last_new_prev_hash.clone()
                else {
                    error!("No prevhash in system");
                    return Err(JDCError::disconnect(
                        JDCErrorKind::LastNewPrevhashNotFound,
                        downstream_id,
                    ));
                };

                let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id)
                else {
                    error!(downstream_id, "Downstream not found");
                    return Err(JDCError::disconnect(
                        JDCErrorKind::DownstreamNotFound(downstream_id),
                        downstream_id,
                    ));
                };

                downstream.downstream_data.super_safe_lock(|data| {
                        let mut messages: Vec<RouteMessageTo> = vec![];
                        let extended_channel_id =
                            data.channel_id_factory.fetch_add(1, Ordering::Relaxed);

                        let extranonce_prefix = match channel_manager_data
                            .extranonce_prefix_factory_extended
                            .next_prefix_extended(requested_min_rollable_extranonce_size.into())
                        {
                            Ok(p) => p,
                            Err(e) => {
                                error!(?e, "Extranonce prefix error");
                                return Ok(vec![(
                                    downstream_id,
                                    build_error("min-extranonce-size-too-large"),
                                )
                                    .into()]);
                            }
                        };

                        let job_store = DefaultJobStore::new();

                        let full_extranonce_size = channel_manager_data
                            .upstream_channel
                            .as_ref()
                            .map(|channel| channel.get_full_extranonce_size())
                            .unwrap_or(FULL_EXTRANONCE_SIZE); // Default to FULL_EXTRANONCE_SIZE if
                                                              // upstream channel is not present (solo mining mode)

                        let rollable_extranonce_size =
                            full_extranonce_size - extranonce_prefix.clone().to_vec().len();

                        let mut extended_channel =
                            match ExtendedChannel::new_for_job_declaration_client(
                                extended_channel_id,
                                user_identity.to_string(),
                                extranonce_prefix.into(),
                                requested_max_target,
                                nominal_hash_rate,
                                true,
                                rollable_extranonce_size as u16,
                                self.share_batch_size,
                                self.shares_per_minute,
                                job_store,
                                channel_manager_data.pool_tag_string.clone(),
                                self.miner_tag_string.clone(),
                            ) {
                                Ok(c) => c,
                                Err(e) => {
                                    error!(?e, "Failed to create ExtendedChannel");
                                    return match e {
                                        ExtendedChannelError::InvalidNominalHashrate => Ok(vec![(
                                            downstream_id,
                                            build_error("invalid-nominal-hashrate"),
                                        )
                                            .into()]),
                                        ExtendedChannelError::RequestedMaxTargetOutOfRange => {
                                            Ok(vec![(
                                                downstream_id,
                                                build_error("max-target-out-of-range"),
                                            )
                                                .into()])
                                        }
                                        other => Err(JDCError::disconnect(other, downstream_id)),
                                    };
                                }
                            };

                        let group_channel_id = data.group_channel.get_group_channel_id();

                        let open_extended_mining_channel_success =
                            OpenExtendedMiningChannelSuccess {
                                request_id,
                                channel_id: extended_channel_id,
                                target: extended_channel.get_target().to_le_bytes().into(),
                                extranonce_prefix: extended_channel
                                    .get_extranonce_prefix()
                                    .clone()
                                    .try_into()
                                    .expect("valid extranonce prefix"),
                                extranonce_size: extended_channel.get_rollable_extranonce_size(),
                                group_channel_id,
                            }
                            .into_static();

                        let full_extranonce_size = extended_channel.get_full_extranonce_size();

                        messages.push(
                            (
                                downstream_id,
                                Mining::OpenExtendedMiningChannelSuccess(
                                    open_extended_mining_channel_success,
                                ),
                            )
                                .into(),
                        );

                        let mut coinbase_outputs = match deserialize_outputs(
                            channel_manager_data.coinbase_outputs.clone(),
                        ) {
                            Ok(outputs) => outputs,
                            Err(_) => {
                                return Err(JDCError::shutdown(
                                    JDCErrorKind::ChannelManagerHasBadCoinbaseOutputs,
                                ))
                            }
                        };
                        coinbase_outputs[0].value =
                            Amount::from_sat(last_future_template.coinbase_tx_value_remaining);

                        // create a future extended job based on the last future template
                        if let Err(e) = extended_channel
                            .on_new_template(last_future_template.clone(), coinbase_outputs)
                        {
                            error!(?e, "Failed to apply template to extended channel");
                            return Err(JDCError::shutdown(e));
                        }

                        let future_extended_job_id = extended_channel
                            .get_future_job_id_from_template_id(last_future_template.template_id)
                            .expect("future job id must exist");
                        let future_extended_job = extended_channel
                            .get_future_job(future_extended_job_id)
                            .expect("future job must exist");

                        let future_extended_job_message =
                            future_extended_job.get_job_message().clone().into_static();

                        // send this future job as new job message
                        // to be immediately activated with the subsequent SetNewPrevHash message
                        messages.push(
                            (
                                downstream_id,
                                Mining::NewExtendedMiningJob(future_extended_job_message),
                            )
                                .into(),
                        );

                        // SetNewPrevHash message activates the future job
                        let prev_hash = last_new_prev_hash.prev_hash.clone();
                        let header_timestamp = last_new_prev_hash.header_timestamp;
                        let n_bits = last_new_prev_hash.n_bits;
                        let set_new_prev_hash_mining = SetNewPrevHash {
                            channel_id: extended_channel_id,
                            job_id: future_extended_job_id,
                            prev_hash,
                            min_ntime: header_timestamp,
                            nbits: n_bits,
                        };
                        if let Err(e) = extended_channel.on_set_new_prev_hash(last_new_prev_hash) {
                            error!(?e, "Failed to set prevhash on extended channel");
                            return Err(JDCError::shutdown(e));
                        }
                        messages.push(
                            (
                                downstream_id,
                                Mining::SetNewPrevHash(set_new_prev_hash_mining),
                            )
                                .into(),
                        );

                        let vardiff = VardiffState::new().expect("Vardiff should instantiate.");
                        data.extended_channels
                            .insert(extended_channel_id, extended_channel);

                        channel_manager_data
                            .downstream_channel_id_and_job_id_to_template_id
                            .insert(
                                (downstream_id, extended_channel_id, future_extended_job_id).into(),
                                last_future_template.template_id,
                            );
                        channel_manager_data
                            .vardiff
                            .insert((downstream_id, extended_channel_id).into(), vardiff);

                        data.group_channel
                            .add_channel_id(extended_channel_id, full_extranonce_size)
                            .map_err(|e| {
                                error!("Failed to add channel id to group channel: {:?}", e);
                                JDCError::shutdown(e)
                            })?;

                        Ok(messages)
                    })
            })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_channel).await {
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
        msg: UpdateChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let channel_id = msg.channel_id;
        let new_nominal_hash_rate = msg.nominal_hash_rate;
        let requested_maximum_target =
            Target::from_le_bytes(msg.maximum_target.inner_as_ref().try_into().unwrap());
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let messages = self
            .channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                let mut messages: Vec<RouteMessageTo> = vec![];

                if let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id) {
                    messages.extend_from_slice(&downstream.downstream_data.super_safe_lock(
                        |data| {
                            let mut messages: Vec<RouteMessageTo> = vec![];

                            let build_error = |code: &str| {
                                error!(channel_id, error_code = code, "UpdateChannelError");
                                Mining::UpdateChannelError(UpdateChannelError {
                                    channel_id,
                                    error_code: code
                                        .to_string()
                                        .try_into()
                                        .expect("valid error code"),
                                })
                            };

                            if let Some(standard_channel) =
                                data.standard_channels.get_mut(&channel_id)
                            {
                                let update_channel = standard_channel.update_channel(
                                    new_nominal_hash_rate,
                                    Some(requested_maximum_target),
                                );
                                let new_target = standard_channel.get_target();

                                if let Err(e) = update_channel {
                                    error!(channel_id, ?e, "StandardChannel update failed");

                                    let err_code = match e {
                                        StandardChannelError::InvalidNominalHashrate => {
                                            "invalid-nominal-hashrate"
                                        }
                                        StandardChannelError::RequestedMaxTargetOutOfRange => {
                                            "requested-max-target-out-of-range"
                                        }
                                        _ => "internal-error",
                                    };
                                    if err_code == "internal-error" {
                                        warn!("Failed to update extended channel {channel_id}");
                                    } else {
                                        return vec![(downstream_id, build_error(err_code)).into()];
                                    }
                                }

                                messages.push(
                                    (
                                        downstream_id,
                                        Mining::SetTarget(SetTarget {
                                            channel_id,
                                            maximum_target: new_target.to_le_bytes().into(),
                                        }),
                                    )
                                        .into(),
                                );
                            } else if let Some(extended_channel) =
                                data.extended_channels.get_mut(&channel_id)
                            {
                                let update_channel = extended_channel.update_channel(
                                    new_nominal_hash_rate,
                                    Some(requested_maximum_target),
                                );
                                let new_target = extended_channel.get_target();

                                if let Err(e) = update_channel {
                                    error!(channel_id, ?e, "StandardChannel update failed");
                                    let err_code = match e {
                                        ExtendedChannelError::InvalidNominalHashrate => {
                                            "invalid-nominal-hashrate"
                                        }
                                        ExtendedChannelError::RequestedMaxTargetOutOfRange => {
                                            "requested-max-target-out-of-range"
                                        }
                                        _ => "internal-error",
                                    };
                                    if err_code == "internal-error" {
                                        warn!("Failed to update extended channel {channel_id}");
                                    } else {
                                        return vec![(downstream_id, build_error(err_code)).into()];
                                    }
                                }

                                messages.push(
                                    (
                                        downstream_id,
                                        Mining::SetTarget(SetTarget {
                                            channel_id,
                                            maximum_target: new_target.to_le_bytes().into(),
                                        }),
                                    )
                                        .into(),
                                );
                            } else {
                                error!("UpdateChannelError: invalid-channel-id");
                                return vec![
                                    (downstream_id, build_error("invalid-channel-id")).into()
                                ];
                            }

                            messages
                        },
                    ));
                }

                let mut downstream_hashrate = 0.0;
                let mut min_target = Target::from_le_bytes([0xff; 32]);

                for (_, downstream) in channel_manager_data.downstream.iter() {
                    downstream.downstream_data.super_safe_lock(|data| {
                        let mut update_from_channel = |hashrate: f32, target: &Target| {
                            downstream_hashrate += hashrate;
                            min_target = std::cmp::min(*target, min_target);
                        };

                        for (_, channel) in data.standard_channels.iter() {
                            update_from_channel(
                                channel.get_nominal_hashrate(),
                                channel.get_target(),
                            );
                        }

                        for (_, channel) in data.extended_channels.iter() {
                            update_from_channel(
                                channel.get_nominal_hashrate(),
                                channel.get_target(),
                            );
                        }
                    });
                }

                if let Some(ref mut upstream_channel) = channel_manager_data.upstream_channel {
                    debug!(
                        "Checking upstream channel {} with hashrate {} and target {:?}",
                        upstream_channel.get_channel_id(),
                        upstream_channel.get_nominal_hashrate(),
                        upstream_channel.get_target()
                    );

                    // Update the upstream channel's nominal hashrate to reflect
                    // the aggregated downstream hashrate
                    upstream_channel.set_nominal_hashrate(downstream_hashrate);

                    info!("Sending update channel message upstream");
                    messages.push(
                        Mining::UpdateChannel(UpdateChannel {
                            channel_id: upstream_channel.get_channel_id(),
                            nominal_hash_rate: downstream_hashrate,
                            maximum_target: min_target.to_le_bytes().into(),
                        })
                        .into(),
                    )
                }

                messages
            });

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_channel).await {
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
    //
    // 3. If a block is found in solo mining mode, rotate the coinbase address.
    async fn handle_submit_shares_standard(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesStandard,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesStandard");
        let channel_id = msg.channel_id;
        let downstream_job_id = msg.job_id;
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");

        let build_error = |code: &str| {
            Mining::SubmitSharesError(SubmitSharesError {
                channel_id,
                sequence_number: msg.sequence_number,
                error_code: code.to_string().try_into().expect("valid error code"),
            })
        };

        let (messages, solo_block_found) = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id) else {
                warn!("No downstream found for downstream_id={downstream_id}");
                return Err(JDCError::disconnect(JDCErrorKind::DownstreamNotFound(downstream_id), downstream_id));
            };
            let Some(prev_hash) = channel_manager_data.last_new_prev_hash.as_ref() else {
                warn!("No prev_hash available yet, ignoring share");
                return Err(JDCError::disconnect(JDCErrorKind::LastNewPrevhashNotFound, downstream_id));
            };

            downstream.downstream_data.super_safe_lock(|data| {
                let mut messages: Vec<RouteMessageTo> = vec![];
                let mut solo_block_found = false;

                let Some(standard_channel) = data.standard_channels.get_mut(&channel_id) else {
                    error!("SubmitSharesError: channel_id: {channel_id}, sequence_number: {}, error_code: invalid-channel-id", msg.sequence_number);
                    return Ok((vec![(downstream_id, build_error("invalid-channel-id")).into()], false));
                };

                let Some(vardiff) = channel_manager_data.vardiff.get_mut(&(downstream_id, channel_id).into()) else {
                    return Ok((vec![(downstream_id, Mining::CloseChannel(create_close_channel_msg(channel_id, "invalid-channel-id"))).into()], false));
                };
                vardiff.increment_shares_since_last_update();
                let res = standard_channel.validate_share(msg.clone());
                let mut is_downstream_share_valid = false;
                let mut downstream_share_hash: Option<sha256d::Hash> = None;
                match res {
                    Ok(ShareValidationResult::Valid(share_hash)) => {
                        let share_accounting = standard_channel.get_share_accounting();
                        if share_accounting.should_acknowledge() {
                            let success = SubmitSharesSuccess {
                                channel_id,
                                last_sequence_number: share_accounting.get_last_share_sequence_number(),
                                new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                                new_shares_sum: share_accounting.get_last_batch_work_sum() as u64,
                            };
                            info!("SubmitSharesStandard on downstream channel: {} ✅", success);
                            messages.push((downstream.downstream_id, Mining::SubmitSharesSuccess(success)).into());
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
                            let solution = SubmitSolution {
                                template_id,
                                version: msg.version,
                                header_timestamp: msg.ntime,
                                header_nonce: msg.nonce,
                                coinbase_tx: coinbase.try_into().map_err(JDCError::shutdown)?,
                            };

                            messages.push(TemplateDistribution::SubmitSolution(solution.clone()).into());
                        }
                        let share_accounting = standard_channel.get_share_accounting().clone();
                        let success = SubmitSharesSuccess {
                            channel_id,
                            last_sequence_number: share_accounting.get_last_share_sequence_number(),
                            new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                            new_shares_sum: share_accounting.get_last_batch_work_sum() as u64,
                        };
                        messages.push((
                            downstream.downstream_id,
                            Mining::SubmitSharesSuccess(success),
                        ).into());

                        // Track block found in solo mining mode (no upstream channel)
                        if channel_manager_data.upstream_channel.is_none() {
                            solo_block_found = true;
                        }
                    }
                    Err(err) => {
                        let code = match err {
                            ShareValidationError::Invalid => "invalid-share",
                            ShareValidationError::Stale => "stale-share",
                            ShareValidationError::InvalidJobId => "invalid-job-id",
                            ShareValidationError::DoesNotMeetTarget => "difficulty-too-low",
                            ShareValidationError::DuplicateShare => "duplicate-share",
                            _ => unreachable!(),
                        };
                        error!("❌ SubmitSharesError: ch={}, seq={}, error={code}", channel_id, msg.sequence_number);
                        messages.push((downstream_id, build_error(code)).into());
                    }
                }

                if !is_downstream_share_valid {
                    return Ok((messages, solo_block_found));
                }

                if let Some(upstream_channel) = channel_manager_data.upstream_channel.as_mut() {
                    let key = (downstream_id, channel_id, downstream_job_id).into();

                    let template_id = channel_manager_data
                        .downstream_channel_id_and_job_id_to_template_id
                        .get(&key)
                        .copied();

                    let upstream_job_id = template_id
                        .and_then(|tid| channel_manager_data.template_id_to_upstream_job_id.get(&tid))
                        .copied();

                    let extranonce_prefix = standard_channel.get_extranonce_prefix();
                    let upstream_extranonce_prefix = upstream_channel.get_extranonce_prefix();
                    let extranonce = &extranonce_prefix[upstream_extranonce_prefix.len()..];

                    let mut upstream_message = SubmitSharesExtended {
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
                                    upstream_message.sequence_number = channel_manager_data.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                    info!(
                                        "SubmitSharesStandard, forwarding it to upstream: valid share | channel_id: {}, sequence_number: {}, share_hash: {}  ✅",
                                        channel_id, upstream_message.sequence_number, share_hash
                                    );
                                    messages.push(Mining::SubmitSharesExtended(upstream_message).into());
                                }
                                Ok(client::share_accounting::ShareValidationResult::BlockFound(share_hash)) => {
                                    upstream_message.sequence_number = channel_manager_data.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                    info!("SubmitSharesStandard forwarding it to upstream: 💰 Block Found!!! 💰{share_hash}");
                                    let push_solution = PushSolution {
                                        extranonce: standard_channel.get_extranonce_prefix().to_vec().try_into().map_err(JDCError::shutdown)?,
                                        ntime: upstream_message.ntime,
                                        nonce: upstream_message.nonce,
                                        version: upstream_message.version,
                                        nbits: prev_hash.n_bits,
                                        prev_hash: prev_hash.prev_hash.clone(),
                                    };
                                    messages.push(JobDeclaration::PushSolution(push_solution).into());
                                    messages.push(Mining::SubmitSharesExtended(upstream_message).into());
                                }
                                Err(err) => {
                                    let code = match err {
                                        client::share_accounting::ShareValidationError::Invalid => "invalid-share",
                                        client::share_accounting::ShareValidationError::Stale => "stale-share",
                                        client::share_accounting::ShareValidationError::InvalidJobId => "invalid-job-id",
                                        client::share_accounting::ShareValidationError::DoesNotMeetTarget => "difficulty-too-low",
                                        client::share_accounting::ShareValidationError::DuplicateShare => "duplicate-share",
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
                                let hash = downstream_share_hash
                                    .expect("downstream_share_hash must be set when downstream share is valid");
                                let entry = SharesOrderedByDiff::new(upstream_message.into_static(), hash);
                                let heap = channel_manager_data
                                    .cached_shares
                                    .entry(template_id)
                                    .or_default();
                                add_share_to_cache(heap, entry);
                            } else {
                                warn!(
                                        "SubmitSharesStandard: could not cache share, no template_id found for key (downstream_id={}, channel_id={}, downstream_job_id={})",
                                        downstream_id, channel_id, downstream_job_id
                                    );
                            }
                        }
                    }
                }

                Ok((messages, solo_block_found))
            })
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_channel).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        // Rotate coinbase address if block found in solo mining mode
        if solo_block_found {
            self.rotate_coinbase_address();
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
    //
    // 3. If a block is found in solo mining mode, rotate the coinbase address.
    async fn handle_submit_shares_extended(
        &mut self,
        client_id: Option<usize>,
        msg: SubmitSharesExtended<'_>,
        tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received SubmitSharesExtended");
        let channel_id = msg.channel_id;
        let downstream_job_id = msg.job_id;
        let downstream_id =
            client_id.expect("client_id must be present for downstream_id extraction");
        let negotiated_extensions = self.get_negotiated_extensions_with_client(client_id);

        let build_error = |code: &str| {
            Mining::SubmitSharesError(SubmitSharesError {
                channel_id,
                sequence_number: msg.sequence_number,
                error_code: code.to_string().try_into().expect("valid error code"),
            })
        };

        let (messages, solo_block_found) = self.channel_manager_data.super_safe_lock(|channel_manager_data| {
            let Some(downstream) = channel_manager_data.downstream.get_mut(&downstream_id) else {
                warn!("No downstream found for downstream_id={downstream_id}");
                return Err(JDCError::disconnect(JDCErrorKind::DownstreamNotFound(downstream_id), downstream_id));
            };
            let Some(prev_hash) = channel_manager_data.last_new_prev_hash.as_ref() else {
                warn!("No prev_hash available yet, ignoring share");
                return Err(JDCError::disconnect(JDCErrorKind::LastNewPrevhashNotFound, downstream_id));
            };
            downstream.downstream_data.super_safe_lock(|data| {
                let mut messages: Vec<RouteMessageTo> = vec![];
                let mut solo_block_found = false;

                let Some(extended_channel) = data.extended_channels.get_mut(&channel_id) else {
                    error!("SubmitSharesError: channel_id: {channel_id}, sequence_number: {}, error_code: invalid-channel-id", msg.sequence_number);
                    return Ok((vec![(downstream_id, build_error("invalid-channel-id")).into()], false));
                };
                // here we extract and set the user_identity from the TLV fields if the extension is negotiated
                let user_identity = if negotiated_extensions.as_ref().is_ok_and(|exts| exts.contains(&EXTENSION_TYPE_WORKER_HASHRATE_TRACKING)) {
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
                if let Some(_user_identity) = user_identity {
                    // here we have the UserIdentity TLV, so we can use it to enhance monitoring of individual miners in the future
                }

                let Some(vardiff) = channel_manager_data.vardiff.get_mut(&(downstream_id, channel_id).into()) else {
                    return Ok((vec![(downstream_id, Mining::CloseChannel(create_close_channel_msg(channel_id, "invalid-channel-id"))).into()], false));
                };
                vardiff.increment_shares_since_last_update();
                let res = extended_channel.validate_share(msg.clone());
                let mut is_downstream_share_valid = false;
                let mut downstream_share_hash: Option<sha256d::Hash> = None;
                match res {
                    Ok(ShareValidationResult::Valid(share_hash)) => {
                        let share_accounting = extended_channel.get_share_accounting();
                        if share_accounting.should_acknowledge() {
                            let success = SubmitSharesSuccess {
                                channel_id,
                                last_sequence_number: share_accounting.get_last_share_sequence_number(),
                                new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                                new_shares_sum: share_accounting.get_last_batch_work_sum() as u64,
                            };
                            info!("SubmitSharesExtended on downstream channel: {} ✅", success);
                            messages.push((downstream.downstream_id, Mining::SubmitSharesSuccess(success)).into());
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
                            let solution = SubmitSolution {
                                template_id,
                                version: msg.version,
                                header_timestamp: msg.ntime,
                                header_nonce: msg.nonce,
                                coinbase_tx: coinbase.try_into().map_err(JDCError::shutdown)?,
                            };
                            messages.push(TemplateDistribution::SubmitSolution(solution.clone()).into());
                        }
                        let share_accounting = extended_channel.get_share_accounting().clone();
                        let success = SubmitSharesSuccess {
                            channel_id,
                            last_sequence_number: share_accounting.get_last_share_sequence_number(),
                            new_submits_accepted_count: share_accounting.get_last_batch_accepted(),
                            new_shares_sum: share_accounting.get_last_batch_work_sum() as u64,
                        };
                        is_downstream_share_valid = true;
                        messages.push((
                            downstream.downstream_id,
                            Mining::SubmitSharesSuccess(success),
                        ).into());

                        // Track block found in solo mining mode (no upstream channel)
                        if channel_manager_data.upstream_channel.is_none() {
                            solo_block_found = true;
                        }
                    }
                    Err(err) => {
                        let code = match err {
                            ShareValidationError::Invalid => "invalid-share",
                            ShareValidationError::Stale => "stale-share",
                            ShareValidationError::InvalidJobId => "invalid-job-id",
                            ShareValidationError::DoesNotMeetTarget => "difficulty-too-low",
                            ShareValidationError::DuplicateShare => "duplicate-share",
                            ShareValidationError::BadExtranonceSize => "bad-extranonce-size",
                            _ => unreachable!(),
                        };
                        error!("❌ SubmitSharesError on downstream channel: ch={}, seq={}, error={code}", channel_id, msg.sequence_number);
                        messages.push((downstream_id, build_error(code)).into());
                    }
                }

                if !is_downstream_share_valid{
                    return Ok((messages, solo_block_found));
                }

                if let Some(upstream_channel) = channel_manager_data.upstream_channel.as_mut() {
                    let key = (downstream_id, channel_id, downstream_job_id).into();

                    let template_id = channel_manager_data
                        .downstream_channel_id_and_job_id_to_template_id
                        .get(&key)
                        .copied();

                    let upstream_job_id = template_id
                        .and_then(|tid| channel_manager_data.template_id_to_upstream_job_id.get(&tid))
                        .copied();

                    let extranonce_prefix = extended_channel.get_extranonce_prefix();
                    let upstream_extranonce_prefix = upstream_channel.get_extranonce_prefix();
                    let new_extranonce_prefix = &extranonce_prefix[upstream_extranonce_prefix.len()..];

                    let mut upstream_message = msg.clone();
                    upstream_message.channel_id = upstream_channel.get_channel_id();
                    // We assign sequence number later, when we validate the share
                    // and send it to upstream.
                    upstream_message.sequence_number = 0;

                    let mut extranonce = vec![];
                    extranonce.extend_from_slice(new_extranonce_prefix);
                    extranonce.extend_from_slice(&msg.extranonce.to_vec());
                    upstream_message.extranonce = extranonce.try_into().map_err(|e| JDCError::disconnect(e, downstream_id))?;

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
                                    upstream_message.sequence_number = channel_manager_data.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                    info!(
                                        "SubmitSharesExtended forwarding it to upstream: valid share | channel_id: {}, sequence_number: {}, share_hash: {}  ✅",
                                        channel_id, upstream_message.sequence_number, share_hash
                                    );
                                    messages.push(
                                        Mining::SubmitSharesExtended(upstream_message.into_static()).into(),
                                    );
                                }
                                Ok(client::share_accounting::ShareValidationResult::BlockFound(share_hash)) => {
                                    upstream_message.sequence_number = channel_manager_data.sequence_number_factory.fetch_add(1, Ordering::Relaxed);
                                    info!("SubmitSharesExtended forwarding it to upstream: 💰 Block Found!!! 💰{share_hash}");
                                    let mut channel_extranonce = upstream_channel.get_extranonce_prefix().to_vec();
                                    channel_extranonce.extend_from_slice(&upstream_message.extranonce.to_vec());
                                    let push_solution = PushSolution {
                                        extranonce: channel_extranonce.try_into().map_err(JDCError::shutdown)?,
                                        ntime: upstream_message.ntime,
                                        nonce: upstream_message.nonce,
                                        version: upstream_message.version,
                                        nbits: prev_hash.n_bits,
                                        prev_hash: prev_hash.prev_hash.clone(),
                                    };
                                    messages.push(JobDeclaration::PushSolution(push_solution.clone()).into());
                                    // TODO here we should add the user_identity TLV to the SubmitSharesExtended
                                    // frame in case the extension n.2 has been negotiated between JDC and Pool.
                                    messages.push(
                                        Mining::SubmitSharesExtended(upstream_message.into_static()).into(),
                                    );
                                }
                                Err(err) => {
                                    let code = match err {
                                        client::share_accounting::ShareValidationError::Invalid=>"invalid-share",
                                        client::share_accounting::ShareValidationError::Stale=>"stale-share",
                                        client::share_accounting::ShareValidationError::InvalidJobId=>"invalid-job-id",
                                        client::share_accounting::ShareValidationError::DoesNotMeetTarget=>"difficulty-too-low",
                                        client::share_accounting::ShareValidationError::DuplicateShare=>"duplicate-share",
                                        client::share_accounting::ShareValidationError::BadExtranonceSize=>"bad-extranonce-size",
                                    _ => unreachable!(),
                                    };
                                    debug!("❌ SubmitSharesError not forwarding it to upstream: ch={}, seq={}, error={code}", channel_id, upstream_message.sequence_number);
                                }
                            }
                        }
                        None =>{
                            debug!("Upstream job_id not yet known (still waiting for the SetCustomMiningJob.Success message), caching share");

                            if let Some(template_id) = template_id {
                                let hash = downstream_share_hash
                                    .expect("downstream_share_hash must be set when downstream share is valid");
                                let entry = SharesOrderedByDiff::new(upstream_message.into_static(), hash);
                                let heap = channel_manager_data
                                    .cached_shares
                                    .entry(template_id)
                                    .or_default();
                                add_share_to_cache(heap, entry);
                            } else {
                                warn!(
                                  "SubmitSharesExtended: could not cache share, no template_id found for key (downstream_id={}, channel_id={}, downstream_job_id={})",
                                  downstream_id, channel_id, downstream_job_id
                              );
                            }

                        }
                    }
                }

                Ok((messages, solo_block_found))
            })
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_channel).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        // Rotate coinbase address if block found in solo mining mode
        if solo_block_found {
            self.rotate_coinbase_address();
        }

        Ok(())
    }

    // Handles an incoming `SetCustomMiningJob` message from a downstream.
    async fn handle_set_custom_mining_job(
        &mut self,
        _client_id: Option<usize>,
        msg: SetCustomMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        Err(JDCError::log(JDCErrorKind::UnexpectedMessage(
            0,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
        )))
    }
}
