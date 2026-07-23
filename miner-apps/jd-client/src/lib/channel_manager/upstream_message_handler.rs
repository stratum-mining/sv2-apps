use std::sync::atomic::Ordering;

use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{
            client::{error::ExtendedChannelError, extended::ExtendedChannel},
            extranonce_manager::{ExtranonceAllocator, ExtranoncePrefix, MAX_EXTRANONCE_LEN},
            outputs::deserialize_outputs,
            server::jobs::factory::JobFactory,
        },
        handlers_sv2::{
            HandleMiningMessagesFromClientAsync, HandleMiningMessagesFromServerAsync,
            SupportedChannelTypes,
        },
        mining_sv2::*,
        parsers_sv2::{AnyMessage, Mining, Tlv},
    },
    utils::types::Sv2Frame,
};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::{
        downstream_message_handler::RouteMessageTo, ChannelManager, DeclaredJob,
        JDC_LOCAL_PREFIX_BYTES, JDC_MAX_CHANNELS,
    },
    error::{self, JDCError, JDCErrorKind},
    utils::{create_close_channel_msg, validate_cached_share, UpstreamState},
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromServerAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions
            .with(|data| data.clone())
            .map_err(JDCError::shutdown)
    }

    fn get_channel_type_for_server(&self, _server_id: Option<usize>) -> SupportedChannelTypes {
        SupportedChannelTypes::Extended
    }
    fn is_work_selection_enabled_for_server(&self, _server_id: Option<usize>) -> bool {
        true
    }

    // Handles an unexpected `OpenStandardMiningChannelSuccess` message from the upstream.
    //
    // The Job Declarator Client (JDC) only supports extended channel when
    // communicating with upstream peer. Receiving a standard channel success
    // indicates either misbehavior or a protocol violation by the upstream.
    //
    // In such cases, the event is treated as malicious, and a fallback
    // (`UpstreamShutdownFallback`) is immediately triggered to protect the system.
    async fn handle_open_standard_mining_channel_success(
        &mut self,
        _server_id: Option<usize>,
        msg: OpenStandardMiningChannelSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        info!(
            "⚠️ JDC can only open extended channels with the upstream server, preparing fallback."
        );
        Err(JDCError::fallback(
            JDCErrorKind::OpenStandardMiningChannelError,
        ))
    }

    // Handles `OpenExtendedMiningChannelSuccess` messages from upstream.
    //
    // On success, this establishes a client-side extended channel:
    // - If initialization fails at any step, the upstream state is reverted from `Pending` to
    //   `NoChannel`.
    // - If initialization succeeds, we configure the extranonce factory, create a new
    //   `ExtendedChannel` and `JobFactory`, and update the upstream state from `Pending` to
    //   `Connected`.
    //
    // Once the upstream state transitions to `Connected`, all pending downstream requests are
    // processed, and downstream channels are opened accordingly.
    async fn handle_open_extended_mining_channel_success(
        &mut self,
        _server_id: Option<usize>,
        msg: OpenExtendedMiningChannelSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        let coinbase_outputs = self.coinbase_outputs.get().map_err(JDCError::shutdown)?;

        let outputs = deserialize_outputs(coinbase_outputs)
            .map_err(|_| JDCError::shutdown(JDCErrorKind::DeclaredJobHasBadCoinbaseOutputs))?;

        let reserved_downstream_rollable = self.reserved_downstream_rollable_extranonce_size;
        let min_extranonce_size =
            JDC_LOCAL_PREFIX_BYTES as u16 + reserved_downstream_rollable as u16;
        if msg.extranonce_size < min_extranonce_size {
            warn!(
                "Pool granted extranonce_size={} but JDC requires at least {} \
                 ({} bytes for its prefix + {} reserved rollable bytes for downstream), \
                 preparing fallback.",
                msg.extranonce_size,
                min_extranonce_size,
                JDC_LOCAL_PREFIX_BYTES,
                reserved_downstream_rollable,
            );
            return Err(JDCError::fallback(JDCErrorKind::ExtranonceSizeTooSmall));
        }

        let Some(hashrate) = self
            .pending_downstream_requests
            .with(|pending| pending.front().map(|request| request.hashrate()))
            .map_err(JDCError::shutdown)?
        else {
            self.upstream_state.set(UpstreamState::NoChannel);
            let close_channel =
                create_close_channel_msg(msg.channel_id, "downstream not available");
            let close_channel = Mining::CloseChannel(close_channel);
            let sv2_frame: Sv2Frame = AnyMessage::Mining(close_channel)
                .try_into()
                .map_err(JDCError::shutdown)?;
            self.channel_manager_io
                .upstream_sender
                .send(sv2_frame)
                .await
                .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
            return Ok(());
        };

        let prefix_len = msg.extranonce_prefix.len();
        let total_len = prefix_len as u16 + msg.extranonce_size;
        debug!(
            prefix_len,
            extranonce_size = msg.extranonce_size,
            total_len,
            "Calculated extranonce ranges"
        );

        let extranonce_allocator = match ExtranonceAllocator::from_upstream_prefix(
            msg.extranonce_prefix.to_owned_bytes(),
            Vec::new(),
            total_len as u8,
            JDC_MAX_CHANNELS,
        ) {
            Ok(allocator) => allocator,
            Err(e) => {
                warn!("Failed to build extranonce allocator: {e:?}");
                self.upstream_state.set(UpstreamState::NoChannel);
                let close_channel =
                    create_close_channel_msg(msg.channel_id, "downstream not available");
                let close_channel = Mining::CloseChannel(close_channel);
                let sv2_frame: Sv2Frame = AnyMessage::Mining(close_channel)
                    .try_into()
                    .map_err(JDCError::shutdown)?;
                self.channel_manager_io
                    .upstream_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
                return Ok(());
            }
        };

        let pool_tag_string = self.pool_tag_string.get().map_err(JDCError::shutdown)?;
        let job_factory =
            JobFactory::new(true, pool_tag_string, Some(self.miner_tag_string.clone()));
        let extranonce_prefix = ExtranoncePrefix::from_wire(msg.extranonce_prefix.to_owned_bytes())
            .expect("prefix length already validated by allocator");
        let mut extended_channel = ExtendedChannel::new(
            msg.channel_id,
            self.user_identity().to_string(),
            extranonce_prefix,
            Target::from_le_bytes(*msg.target.as_array()),
            hashrate,
            true,
            msg.extranonce_size,
        );

        if let Some(prevhash) = self.last_new_prev_hash.get().map_err(JDCError::shutdown)? {
            _ = extended_channel.on_chain_tip_update(prevhash.clone().into());
            debug!("Applied last_new_prev_hash to new extended channel");
        }

        let last_future_template = self
            .last_future_template
            .get()
            .map_err(JDCError::shutdown)?;
        let last_new_prev_hash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;
        let had_job_factory = self
            .job_factory
            .with(|job_factory| job_factory.is_some())
            .map_err(JDCError::shutdown)?;
        let set_custom_job = if self.mode.is_coinbase_only()
            && had_job_factory
            && last_future_template.is_some()
            && last_new_prev_hash.is_some()
        {
            if let Some(token) = self
                .allocate_tokens
                .with(|tokens| tokens.pop_front())
                .map_err(JDCError::shutdown)?
            {
                let template = last_future_template.expect("checked above");
                let prevhash = last_new_prev_hash.expect("checked above");
                let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
                let full_extranonce_size = extended_channel.get_full_extranonce_size();

                if let Ok(custom_job) = job_factory.new_custom_job(
                    extended_channel.get_channel_id(),
                    request_id,
                    token.mining_job_token,
                    prevhash.clone().into(),
                    template.clone(),
                    outputs,
                    full_extranonce_size,
                ) {
                    self.last_declare_job_store.insert(
                        request_id,
                        DeclaredJob {
                            declare_mining_job: None,
                            template: template.into_static(),
                            prev_hash: Some(prevhash.into_static()),
                            set_custom_mining_job: Some(custom_job.clone().into_static()),
                            coinbase_output: self
                                .coinbase_outputs
                                .get()
                                .map_err(JDCError::shutdown)?,
                            tx_list: vec![],
                        },
                    );
                    Some(custom_job)
                } else {
                    None
                }
            } else {
                warn!("No token available, discarding custom job");
                None
            }
        } else {
            None
        };

        let full_extranonce_size = extended_channel.get_full_extranonce_size();
        self.extranonce_allocator
            .set(extranonce_allocator)
            .map_err(JDCError::shutdown)?;
        self.upstream_channel
            .set(Some(extended_channel))
            .map_err(JDCError::shutdown)?;
        self.job_factory
            .set(Some(job_factory))
            .map_err(JDCError::shutdown)?;
        self.upstream_state.set(UpstreamState::Connected);

        let template = self
            .last_future_template
            .get()
            .map_err(JDCError::shutdown)?;
        self.downstream.try_for_each(|_, downstream| {
            downstream
                .group_channel
                .with(|group_channel| group_channel.set_full_extranonce_size(full_extranonce_size))
                .map_err(JDCError::shutdown)
        })?;

        info!("Extended mining channel successfully initialized");
        let channel_state = self.upstream_state.get();

        if channel_state == UpstreamState::Connected {
            if self.mode.is_full_template() {
                if let Some(template) = template {
                    self.request_transaction_data(template.template_id).await?;
                }
            }

            if self.mode.is_coinbase_only() {
                if let Some(custom_job) = set_custom_job {
                    let set_custom_job = Mining::SetCustomMiningJob(custom_job);
                    let sv2_frame: Sv2Frame = AnyMessage::Mining(set_custom_job)
                        .try_into()
                        .map_err(JDCError::shutdown)?;
                    self.channel_manager_io
                        .upstream_sender
                        .send(sv2_frame)
                        .await
                        .map_err(|_e| JDCError::fallback(JDCErrorKind::ChannelErrorSender))?;
                    _ = self.allocate_tokens(1).await;
                }
            }

            let pending_downstreams = self
                .pending_downstream_requests
                .with(std::mem::take)
                .map_err(JDCError::shutdown)?;

            for pending_downstream_message in pending_downstreams {
                let downstream_id = pending_downstream_message.downstream_id();
                let message = pending_downstream_message.message();
                self.handle_mining_message_from_client(Some(downstream_id), message, None)
                    .await?;
            }
        }

        Ok(())
    }

    // Handles `OpenMiningChannelError` messages received from upstream.
    //
    // Receiving this message is treated as malicious behavior, since JDC only supports
    // extended channels. When encountered, we immediately trigger the fallback mechanism
    // by transitioning the upstream state into a shutdown-fallback mode.
    async fn handle_open_mining_channel_error(
        &mut self,
        _server_id: Option<usize>,
        msg: OpenMiningChannelError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        warn!("⚠️ Cannot open extended channel with the upstream server, preparing fallback.");

        Err(JDCError::fallback(JDCErrorKind::OpenMiningChannelError))
    }

    // Handles `UpdateChannelError` messages from upstream.
    async fn handle_update_channel_error(
        &mut self,
        _server_id: Option<usize>,
        msg: UpdateChannelError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        Ok(())
    }

    // Handles `CloseChannel` messages from upstream.
    //
    // Upon receiving this message, the upstream channel is immediately closed and
    // the system transitions into the upstream shutdown fallback state.
    async fn handle_close_channel(
        &mut self,
        _server_id: Option<usize>,
        msg: CloseChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        self.upstream_channel
            .set(None)
            .map_err(JDCError::fallback)?;
        Err(JDCError::fallback(JDCErrorKind::CloseChannel))
    }

    // Handles `SetExtranoncePrefix` messages from upstream.
    //
    // When received, this updates the current extranonce prefix. Each active downstream channel is
    // then assigned a new extranonce prefix, and a corresponding `SetExtranoncePrefix` message
    // is sent downstream to synchronize state.
    async fn handle_set_extranonce_prefix(
        &mut self,
        _server_id: Option<usize>,
        msg: SetExtranoncePrefix<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        let mut messages_results: Vec<Result<RouteMessageTo, Self::Error>> = vec![];
        self.upstream_channel
            .with(|upstream_channel| -> Result<(), Self::Error> {
                if let Some(upstream_channel) = upstream_channel.as_mut() {
                    // Wire-sourced prefix: upstream could legitimately
                    // send a malformed (over-size) value. Treat as a
                    // protocol-level error and fall back.
                    let new_extranonce_prefix =
                        match ExtranoncePrefix::from_wire(msg.extranonce_prefix.to_owned_bytes()) {
                            Ok(p) => p,
                            Err(e) => {
                                warn!("Upstream SetExtranoncePrefix rejected: {e:?}");
                                return Err(JDCError::fallback(
                                    JDCErrorKind::ExtranonceSizeTooLarge,
                                ));
                            }
                        };
                    if let Err(e) = upstream_channel.set_extranonce_prefix(new_extranonce_prefix) {
                        return Err(JDCError::fallback(e));
                    }

                    let new_prefix_len = msg.extranonce_prefix.len();
                    let rollable_extranonce_size = upstream_channel.get_rollable_extranonce_size();
                    let full_extranonce_size = new_prefix_len + rollable_extranonce_size as usize;
                    if full_extranonce_size > MAX_EXTRANONCE_LEN as usize {
                        return Err(JDCError::fallback(JDCErrorKind::ExtranonceSizeTooLarge));
                    }

                    debug!(
                        new_prefix_len,
                        rollable_extranonce_size,
                        full_extranonce_size,
                        "Calculated extranonce ranges"
                    );
                    // `ExtranonceAllocator::from_upstream_prefix` validates on
                    // its own that the new upstream prefix leaves room for
                    // JDC's `local_index` (and therefore for downstream
                    // allocation). If it doesn't, we fall back.
                    let extranonce_allocator = match ExtranonceAllocator::from_upstream_prefix(
                        msg.extranonce_prefix.to_owned_bytes(),
                        Vec::new(),
                        full_extranonce_size as u8,
                        JDC_MAX_CHANNELS,
                    ) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!(
                                "Failed to build extranonce allocator from SetExtranoncePrefix \
                                     (new_prefix_len={}, full_extranonce_size={}): {e:?}",
                                new_prefix_len, full_extranonce_size
                            );
                            return Err(JDCError::fallback(e));
                        }
                    };

                    self.extranonce_allocator
                        .set(extranonce_allocator)
                        .map_err(JDCError::shutdown)?;

                    self.downstream.for_each(|downstream_id, downstream| {
                        downstream.standard_channels.for_each_mut(
                            |channel_id, standard_channel| match self
                                .extranonce_allocator
                                .with(|allocator| allocator.allocate_standard())
                            {
                                Ok(Ok(prefix)) => {
                                    let prefix_bytes = prefix.as_bytes().to_vec();
                                    if let Err(e) = standard_channel.set_extranonce_prefix(prefix) {
                                        messages_results.push(Err(JDCError::shutdown(e)));
                                        return;
                                    }
                                    let extranonce_prefix = match prefix_bytes.try_into() {
                                        Ok(p) => p,
                                        Err(e) => {
                                            messages_results.push(Err(JDCError::shutdown(e)));
                                            return;
                                        }
                                    };
                                    messages_results.push(Ok((
                                        downstream_id,
                                        Mining::SetExtranoncePrefix(SetExtranoncePrefix {
                                            channel_id,
                                            extranonce_prefix,
                                        }),
                                    )
                                        .into()));
                                }
                                Ok(Err(e)) => {
                                    messages_results
                                        .push(Err(JDCError::disconnect(e, downstream_id)));
                                }
                                Err(e) => {
                                    messages_results.push(Err(JDCError::shutdown(e)));
                                }
                            },
                        );
                        downstream.extended_channels.for_each_mut(
                            |channel_id, extended_channel| match self.extranonce_allocator.with(
                                |allocator| {
                                    allocator.allocate_extended(
                                        extended_channel.get_rollable_extranonce_size() as usize,
                                    )
                                },
                            ) {
                                Ok(Ok(prefix)) => {
                                    let prefix_bytes = prefix.as_bytes().to_vec();
                                    if let Err(e) = extended_channel.set_extranonce_prefix(prefix) {
                                        messages_results.push(Err(JDCError::shutdown(e)));
                                        return;
                                    }

                                    let extranonce_prefix = match prefix_bytes.try_into() {
                                        Ok(p) => p,
                                        Err(e) => {
                                            messages_results.push(Err(JDCError::shutdown(e)));
                                            return;
                                        }
                                    };
                                    messages_results.push(Ok((
                                        downstream_id,
                                        Mining::SetExtranoncePrefix(SetExtranoncePrefix {
                                            channel_id,
                                            extranonce_prefix,
                                        }),
                                    )
                                        .into()));
                                }
                                Ok(Err(e)) => {
                                    messages_results
                                        .push(Err(JDCError::disconnect(e, downstream_id)));
                                }
                                Err(e) => {
                                    messages_results.push(Err(JDCError::shutdown(e)));
                                }
                            },
                        );
                    });
                }
                Ok(())
            })
            .map_err(JDCError::shutdown)??;

        for message in messages_results.into_iter().flatten() {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }
        Ok(())
    }

    // Handles `SubmitSharesSuccess` messages from upstream.
    async fn handle_submit_shares_success(
        &mut self,
        _server_id: Option<usize>,
        msg: SubmitSharesSuccess,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {} ✅", msg);

        self.upstream_channel
            .with(|upstream_channel| {
                if let Some(upstream_channel) = upstream_channel.as_mut() {
                    upstream_channel.on_share_acknowledgement(
                        msg.new_submits_accepted_count,
                        msg.new_shares_sum,
                    );
                }
            })
            .map_err(JDCError::shutdown)?;

        Ok(())
    }

    // Handles `SubmitSharesError` messages from upstream.
    async fn handle_submit_shares_error(
        &mut self,
        _server_id: Option<usize>,
        msg: SubmitSharesError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {} ❌", msg);
        let error_code = msg.error_code.as_utf8_or_hex();

        self.upstream_channel
            .with(|upstream_channel| {
                if let Some(upstream_channel) = upstream_channel.as_mut() {
                    upstream_channel.on_share_rejection(error_code.clone());
                }
            })
            .map_err(JDCError::shutdown)?;

        Ok(())
    }

    // Handles `NewMiningJob` messages from upstream. JDC ignores it.
    async fn handle_new_mining_job(
        &mut self,
        _server_id: Option<usize>,
        msg: NewMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        warn!("⚠️ JDC does not expect jobs from the upstream server — ignoring.");
        Ok(())
    }

    // Handles `NewExtendedMiningJob` messages from upstream. JDC ignores it.
    async fn handle_new_extended_mining_job(
        &mut self,
        _server_id: Option<usize>,
        msg: NewExtendedMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        warn!("⚠️ JDC does not expect jobs from the upstream server — ignoring.");
        Ok(())
    }

    // Handles `SetNewPrevHash` messages from upstream. JDC ignores it.
    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        msg: SetNewPrevHash<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        warn!("⚠️ JDC does not expect prevhash updates from the upstream server — ignoring.");
        Ok(())
    }

    // Handles `SetCustomMiningJobSuccess` messages from upstream.
    //
    // On success:
    // - Updates the `job_id_to_template_id` mapping.
    // - Updates the channel state accordingly.
    // - Removes the associated `last_declare_job`, completing its lifecycle.
    async fn handle_set_custom_mining_job_success(
        &mut self,
        _server_id: Option<usize>,
        msg: SetCustomMiningJobSuccess,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {} ✅", msg);

        let mut shares_to_submit_upstream = Vec::new();

        let Some((_, last_declare_job)) = self.last_declare_job_store.remove(&msg.request_id)
        else {
            warn!(
                request_id = msg.request_id,
                "No matching declare job found for custom job success"
            );
            return Err(JDCError::fallback(JDCErrorKind::LastDeclareJobNotFound(
                msg.request_id,
            )));
        };

        let template_id = last_declare_job.template.template_id;
        let job_id = msg.job_id;
        self.template_id_to_upstream_job_id
            .insert(template_id, job_id);
        let cached_shares = self
            .cached_shares
            .remove(&template_id)
            .map(|(_, shares)| shares);
        let prev_hash = self.last_new_prev_hash.get().map_err(JDCError::shutdown)?;

        self.upstream_channel
            .with(|upstream_channel| -> Result<(), Self::Error> {
                let Some(upstream_channel) = upstream_channel.as_mut() else {
                    debug!("No upstream channel available");
                    return Err(JDCError::log(JDCErrorKind::UpstreamNotFound));
                };

                let Some(set_custom_job) = last_declare_job.set_custom_mining_job else {
                    error!("DeclareMiningJob present but SetCustomMiningJob missing");
                    return Err(JDCError::shutdown(JDCErrorKind::CustomJobError));
                };

                if let Err(e) =
                    upstream_channel.on_set_custom_mining_job_success(set_custom_job, msg)
                {
                    error!("SetCustomMiningJob.Success validation failed: {e:#?}");
                    return match e {
                        ExtendedChannelError::ChainTipMismatch => Err(JDCError::log(e)),
                        _ => Err(JDCError::fallback(e)),
                    };
                }

                if let (Some(prev_hash), Some(cached_shares)) = (prev_hash.as_ref(), cached_shares)
                {
                    debug!(
                        "Handling {} cached shares for template_id={}",
                        cached_shares.len(),
                        template_id
                    );
                    for mut share in cached_shares {
                        share.share.job_id = job_id;
                        validate_cached_share(
                            share.share,
                            upstream_channel,
                            prev_hash,
                            &self.sequence_number_factory,
                            &mut shares_to_submit_upstream,
                        );
                    }
                }
                Ok(())
            })
            .map_err(JDCError::shutdown)??;

        // The result can be safely ignored. A send failure usually means the channel
        // endpoint has been dropped (e.g., during disconnect or shutdown).
        // Lifecycle and error handling are managed elsewhere.
        for msg in shares_to_submit_upstream {
            _ = msg.forward(&self.channel_manager_io).await;
        }

        Ok(())
    }

    // Handles a `SetCustomMiningJobError` from upstream.
    //
    // Most of these errors are treated as malicious behavior and trigger the
    // fallback mechanism. However, `stale-chain-tip` can
    // happen during benign JD races when the chain tip changes between
    // declaration and custom job submission, so it is treated as non-fatal.
    async fn handle_set_custom_mining_job_error(
        &mut self,
        _server_id: Option<usize>,
        msg: SetCustomMiningJobError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);

        let error_code = msg.error_code.as_utf8_or_hex();
        if error_code == ERROR_CODE_SET_CUSTOM_MINING_JOB_STALE_CHAIN_TIP {
            warn!(
                "Received non-fatal SetCustomMiningJobError from upstream: stale-chain-tip (request_id={})",
                msg.request_id
            );
            if let Some((_, declared_job)) = self.last_declare_job_store.remove(&msg.request_id) {
                self.cached_shares
                    .remove(&declared_job.template.template_id);
            }
            return Ok(());
        }

        warn!(
            "⚠️ Upstream rejected the custom job with a SetCustomMiningJobError ❌. Starting fallback mechanism."
        );
        Err(JDCError::fallback(JDCErrorKind::CustomJobError))
    }

    // Handles a `SetTarget` message from upstream.
    //
    // Updates the corresponding upstream channel's target state.
    async fn handle_set_target(
        &mut self,
        _server_id: Option<usize>,
        msg: SetTarget<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);
        self.upstream_channel
            .with(|upstream_channel| {
                if let Some(upstream) = upstream_channel.as_mut() {
                    upstream.set_target(Target::from_le_bytes(
                        msg.maximum_target.clone().as_ref().try_into().unwrap(),
                    ));
                }
            })
            .map_err(JDCError::shutdown)?;
        Ok(())
    }

    // Handles `SetGroupChannel` messages from upstream. JDC ignores it.
    async fn handle_set_group_channel(
        &mut self,
        _server_id: Option<usize>,
        msg: SetGroupChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", msg);
        warn!("⚠️ JDC does not expect group channel updates from the upstream server — ignoring.");
        Ok(())
    }
}
