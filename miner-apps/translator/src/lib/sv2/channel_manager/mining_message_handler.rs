use crate::{
    error::{self, TproxyError, TproxyErrorKind},
    sv2::channel_manager::{
        ChannelManager, AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES, AGGREGATED_TPROXY_MAX_CHANNELS,
        NON_AGGREGATED_TPROXY_MAX_CHANNELS,
    },
    utils::{aggregated_upstream_user_identity, AggregatedState, AGGREGATED_CHANNEL_ID},
};
use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{
            client::{extended::ExtendedChannel, group::GroupChannel},
            extranonce_manager::{bytes_needed, ExtranonceAllocator, ExtranoncePrefix},
        },
        handlers_sv2::{HandleMiningMessagesFromServerAsync, SupportedChannelTypes},
        mining_sv2::{
            CloseChannel, NewExtendedMiningJob, NewMiningJob, OpenExtendedMiningChannelSuccess,
            OpenMiningChannelError, OpenStandardMiningChannelSuccess, SetCustomMiningJobError,
            SetCustomMiningJobSuccess, SetExtranoncePrefix, SetGroupChannel, SetNewPrevHash,
            SetTarget, SubmitSharesError, SubmitSharesSuccess, UpdateChannelError,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_ERROR, MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        },
        parsers_sv2::{Mining, Tlv},
    },
    utils::types::{DownstreamId, Hashrate},
};
use tracing::{error, info, warn};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromServerAsync for ChannelManager {
    type Error = TproxyError<error::ChannelManager>;

    fn get_channel_type_for_server(&self, _server_id: Option<usize>) -> SupportedChannelTypes {
        SupportedChannelTypes::GroupAndExtended
    }

    fn is_work_selection_enabled_for_server(&self, _server_id: Option<usize>) -> bool {
        false
    }

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(self
            .negotiated_extensions
            .super_safe_lock(|data| data.clone()))
    }

    async fn handle_open_standard_mining_channel_success(
        &mut self,
        _server_id: Option<usize>,
        m: OpenStandardMiningChannelSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        Err(TproxyError::log(TproxyErrorKind::UnexpectedMessage(
            0,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )))
    }

    async fn handle_open_extended_mining_channel_success(
        &mut self,
        _server_id: Option<usize>,
        m: OpenExtendedMiningChannelSuccess<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        // Retrieve the pending channel request data.
        // Both aggregated and non-aggregated modes store data in pending_downstream_channels, keyed
        // by request_id, so the lookup is identical for both.
        let (user_identity, nominal_hashrate, downstream_extranonce_len) = self
            .pending_downstream_channels
            .remove(&(m.request_id as DownstreamId))
            .ok_or_else(|| {
                error!("No pending channel found for request_id: {}", m.request_id);
                TproxyError::log(TproxyErrorKind::PendingChannelNotFound(m.request_id))
            })?
            .1;

        let success = {
            info!(
                "Received: {}, user_identity: {}, nominal_hashrate: {}",
                m, user_identity, nominal_hashrate
            );

            let full_extranonce_size = m.extranonce_size as usize + m.extranonce_prefix.len();

            // add the channel to the group channel
            match self.group_channels.get_mut(&m.group_channel_id) {
                Some(mut group_channel) => {
                    group_channel
                        .add_channel_id(m.channel_id, full_extranonce_size)
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            TproxyError::fallback(
                                TproxyErrorKind::FailedToAddChannelIdToGroupChannel(e),
                            )
                        })?;
                }
                None => {
                    let mut group_channel = GroupChannel::new(m.group_channel_id);
                    group_channel
                        .add_channel_id(m.channel_id, full_extranonce_size)
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            TproxyError::fallback(
                                TproxyErrorKind::FailedToAddChannelIdToGroupChannel(e),
                            )
                        })?;
                    self.group_channels
                        .insert(m.group_channel_id, group_channel);
                }
            }

            let upstream_prefix_bytes = m.extranonce_prefix.to_owned_bytes();
            let target = Target::from_le_bytes(m.target.to_array());
            let version_rolling = true; // we assume this is always true on extended channels

            if self.mode.is_aggregated() {
                // Aggregated: we asked upstream for `downstream_extranonce_len
                // + AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES` so the allocator's
                // `local_index` has room to uniquely address each multiplexed
                // downstream. Build the allocator with `max_channels =
                // AGGREGATED_TPROXY_MAX_CHANNELS` (2-byte index) and absorb
                // any extra slack upstream granted on top as zero-padding in
                // `local_prefix_bytes`.
                //
                // Resulting layout:
                //   [ upstream_prefix ][ local_prefix (padding) ][ local_index ][ rollable ]
                //        upstream              caller                 allocator     miner
                if (m.extranonce_size as usize)
                    < AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES as usize + downstream_extranonce_len
                {
                    error!(
                        "Upstream-granted rollable size ({} bytes) is smaller than minimum required ({} bytes) in aggregated mode",
                        m.extranonce_size,
                        AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES as usize + downstream_extranonce_len,
                    );
                    return Err(TproxyError::fallback(
                        TproxyErrorKind::OpenMiningChannelError,
                    ));
                }
                let full_extranonce_size =
                    upstream_prefix_bytes.len() as u8 + m.extranonce_size as u8;
                let local_index_bytes = bytes_needed(AGGREGATED_TPROXY_MAX_CHANNELS) as usize;
                let local_prefix_padding_len =
                    (m.extranonce_size as usize) - local_index_bytes - downstream_extranonce_len;
                let mut allocator = ExtranonceAllocator::from_upstream_prefix(
                    upstream_prefix_bytes,
                    vec![0u8; local_prefix_padding_len],
                    full_extranonce_size,
                    AGGREGATED_TPROXY_MAX_CHANNELS,
                )
                .map_err(|e| {
                    error!(
                        "Failed to create ExtranonceAllocator from upstream: {:?}",
                        e
                    );
                    TproxyError::fallback(TproxyErrorKind::OpenMiningChannelError)
                })?;
                let new_extranonce_prefix = allocator
                    .allocate_extended(downstream_extranonce_len)
                    .map_err(|e| {
                        error!("Failed to allocate extended extranonce prefix: {:?}", e);
                        TproxyError::fallback(TproxyErrorKind::OpenMiningChannelError)
                    })?;
                let downstream_extranonce_prefix_bytes: Vec<u8> =
                    new_extranonce_prefix.as_bytes().to_vec();

                // Store the upstream extended channel under AGGREGATED_CHANNEL_ID.
                // Other parts of the translator (job forwarding, target
                // updates, etc.) look up the upstream channel via this key.
                //
                // `expect` is safe: `allocator.upstream_prefix()` is bounded
                // by the allocator's `total_extranonce_len`, which is in
                // turn bounded by `MAX_EXTRANONCE_LEN` (checked at
                // allocator construction).
                let upstream_extranonce_prefix =
                    ExtranoncePrefix::from_wire(allocator.upstream_prefix().to_vec())
                        .expect("allocator upstream prefix is bounded by MAX_EXTRANONCE_LEN");
                let upstream_user_identity = aggregated_upstream_user_identity(&user_identity);
                let upstream_channel = ExtendedChannel::new(
                    m.channel_id,
                    upstream_user_identity,
                    upstream_extranonce_prefix,
                    target,
                    nominal_hashrate,
                    version_rolling,
                    m.extranonce_size,
                );
                self.extended_channels
                    .insert(AGGREGATED_CHANNEL_ID, upstream_channel);

                // Hand the allocator-minted prefix to the downstream channel
                // directly — its RAII release frees the bitmap slot on
                // channel drop. Widen `AllocatedExtranoncePrefix` to the
                // loose `ExtranoncePrefix` expected by the client-side
                // channel constructor; the allocation record (including
                // the bitmap back-reference) is preserved.
                let new_downstream_extended_channel = ExtendedChannel::new(
                    1,
                    user_identity.clone(),
                    new_extranonce_prefix.into(),
                    target,
                    nominal_hashrate,
                    true,
                    downstream_extranonce_len as u16,
                );
                self.extended_channels
                    .insert(1, new_downstream_extended_channel);
                // Keep the allocator alive; subsequent downstream channels in
                // this aggregated upstream draw from the same allocator and
                // share rewriting reads `upstream_prefix_len()` from it.
                self.aggregated_extranonce_allocator
                    .super_safe_lock(|slot| *slot = Some(allocator));
                self.aggregated_channel_state
                    .set(AggregatedState::Connected);

                let new_open_extended_mining_channel_success = OpenExtendedMiningChannelSuccess {
                    request_id: m.request_id,
                    channel_id: 1,
                    extranonce_prefix: downstream_extranonce_prefix_bytes
                        .try_into()
                        .map_err(TproxyError::shutdown)?,
                    extranonce_size: downstream_extranonce_len as u16,
                    target: m.target.clone(),
                    group_channel_id: m.group_channel_id,
                };
                Ok::<OpenExtendedMiningChannelSuccess<'static>, Self::Error>(
                    new_open_extended_mining_channel_success.into_static(),
                )
            } else {
                // Non-aggregated: we asked upstream for exactly
                // `downstream_extranonce_len` (no widening, since each
                // downstream has its own upstream channel and there is
                // nothing to multiplex).
                //
                // If upstream granted exactly what we asked
                // (`m.extranonce_size == downstream_extranonce_len`), there
                // is no slack to absorb: skip the allocator entirely, use the
                // upstream prefix verbatim as the downstream's extranonce1,
                // and let share rewriting be a no-op (the miner's
                // `extranonce2` already matches what upstream expects).
                //
                // If upstream granted more, build a `max_channels = 1`
                // allocator and absorb the slack as zero-padding so the miner
                // still rolls exactly `downstream_extranonce_len` bytes.
                if (m.extranonce_size as usize) < downstream_extranonce_len {
                    error!(
                        "Upstream-granted rollable size ({} bytes) is smaller than requested ({} bytes) in non-aggregated mode",
                        m.extranonce_size, downstream_extranonce_len,
                    );
                    return Err(TproxyError::fallback(
                        TproxyErrorKind::OpenMiningChannelError,
                    ));
                }

                let (downstream_prefix, downstream_prefix_bytes_for_success) = if (m.extranonce_size
                    as usize)
                    == downstream_extranonce_len
                {
                    // No slack: forward upstream's prefix directly to the
                    // downstream. No allocator is stored for this channel,
                    // which the share-rewrite path treats as "no
                    // rewriting needed" — `channel.upstream_prefix_len()`
                    // returns `None` for a wire-sourced prefix and the
                    // rewrite branch is skipped.
                    let prefix = ExtranoncePrefix::from_wire(upstream_prefix_bytes.clone())
                        .map_err(|e| {
                            error!("Upstream extranonce prefix rejected by from_wire: {:?}", e);
                            TproxyError::shutdown(TproxyErrorKind::OpenMiningChannelError)
                        })?;
                    (prefix, upstream_prefix_bytes)
                } else {
                    let local_index_bytes =
                        bytes_needed(NON_AGGREGATED_TPROXY_MAX_CHANNELS) as usize;
                    if (m.extranonce_size as usize) < local_index_bytes + downstream_extranonce_len
                    {
                        error!(
                            "Upstream-granted rollable size ({} bytes) leaves no room for allocator local_index in non-aggregated mode",
                            m.extranonce_size,
                        );
                        return Err(TproxyError::fallback(
                            TproxyErrorKind::OpenMiningChannelError,
                        ));
                    }
                    let full_extranonce_size =
                        upstream_prefix_bytes.len() as u8 + m.extranonce_size as u8;
                    let local_prefix_padding_len = (m.extranonce_size as usize)
                        - local_index_bytes
                        - downstream_extranonce_len;
                    // The allocator is a throwaway:
                    // `max_channels = NON_AGGREGATED_TPROXY_MAX_CHANNELS` (== 1),
                    // so it mints exactly one prefix and then becomes
                    // useless. Drop it right after `allocate_extended`.
                    // The prefix carries its own `upstream_prefix_len`
                    // (recorded at allocation time), which share rewriting
                    // reads back via `channel.upstream_prefix_len()` on the
                    // hot path, so no per-channel allocator state needs to
                    // persist.
                    let mut allocator = ExtranonceAllocator::from_upstream_prefix(
                        upstream_prefix_bytes,
                        vec![0u8; local_prefix_padding_len],
                        full_extranonce_size,
                        NON_AGGREGATED_TPROXY_MAX_CHANNELS,
                    )
                    .map_err(|e| {
                        error!(
                            "Failed to create ExtranonceAllocator from upstream: {:?}",
                            e
                        );
                        TproxyError::fallback(TproxyErrorKind::OpenMiningChannelError)
                    })?;
                    let prefix = allocator
                        .allocate_extended(downstream_extranonce_len)
                        .map_err(|e| {
                            error!("Failed to allocate extended extranonce prefix: {:?}", e);
                            TproxyError::fallback(TproxyErrorKind::OpenMiningChannelError)
                        })?;
                    let wire_bytes = prefix.as_bytes().to_vec();
                    // Widen the allocator-minted prefix to match the
                    // wire-sourced branch's `ExtranoncePrefix`; the
                    // `AllocatedExtranoncePrefix`'s allocation record is
                    // preserved through the conversion so the Drop-based
                    // bitmap release still fires (as a no-op here, since
                    // the throwaway allocator is dropped immediately).
                    (prefix.into(), wire_bytes)
                };

                let new_downstream_extended_channel = ExtendedChannel::new(
                    m.channel_id,
                    user_identity.clone(),
                    downstream_prefix,
                    target,
                    nominal_hashrate,
                    version_rolling,
                    downstream_extranonce_len as u16,
                );
                self.extended_channels
                    .insert(m.channel_id, new_downstream_extended_channel);

                let new_open_extended_mining_channel_success = OpenExtendedMiningChannelSuccess {
                    request_id: m.request_id,
                    channel_id: m.channel_id,
                    extranonce_prefix: downstream_prefix_bytes_for_success
                        .try_into()
                        .map_err(TproxyError::shutdown)?,
                    extranonce_size: downstream_extranonce_len as u16,
                    target: m.target.clone(),
                    group_channel_id: m.group_channel_id,
                };
                Ok::<OpenExtendedMiningChannelSuccess<'static>, Self::Error>(
                    new_open_extended_mining_channel_success.into_static(),
                )
            }
        }?;

        self.channel_manager_io
            .sv1_server_sender
            .send(Mining::OpenExtendedMiningChannelSuccess(success.clone()))
            .await
            .map_err(|e| {
                error!("Failed to send OpenExtendedMiningChannelSuccess: {:?}", e);
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })?;

        // In aggregated mode, serve any downstream requests that were buffered in
        // pending_channels while the upstream channel was being established (Pending state).
        if self.mode.is_aggregated() {
            let pending_requests: Vec<(u32, String, Hashrate, usize)> = self
                .pending_downstream_channels
                .iter()
                .map(|r| {
                    (
                        *r.key() as u32,
                        r.value().0.clone(),
                        r.value().1,
                        r.value().2,
                    )
                })
                .collect();
            self.pending_downstream_channels.clear();

            for (req_id, user_identity, hashrate, min_extranonce_size) in pending_requests {
                self.handle_downstream_channel_request_in_aggregated_mode(
                    req_id,
                    user_identity,
                    hashrate,
                    min_extranonce_size,
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn handle_open_mining_channel_error(
        &mut self,
        _server_id: Option<usize>,
        m: OpenMiningChannelError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        Err(TproxyError::fallback(
            TproxyErrorKind::OpenMiningChannelError,
        ))
    }

    async fn handle_update_channel_error(
        &mut self,
        _server_id: Option<usize>,
        m: UpdateChannelError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        Ok(())
    }

    async fn handle_close_channel(
        &mut self,
        _server_id: Option<usize>,
        m: CloseChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);
        // are we working in aggregated mode?
        if self.mode.is_aggregated() {
            // even if aggregated channel_id != m.channel_id, we should trigger fallback
            // because why would a sane server send a CloseChannel message to a different
            // channel?
            return Err(TproxyError::fallback(
                TproxyErrorKind::AggregatedChannelClosed,
            ));
        }

        let group_channel = self.group_channels.remove(&m.channel_id);

        // we're not in aggregated mode
        // was the message sent to a group channel?
        if let Some((_, group_channel)) = group_channel {
            for channel_id in group_channel.get_channel_ids() {
                self.extended_channels.remove(channel_id);
            }
        // if the message was not sent to a group channel, and we're not working in
        // aggregated mode,
        } else if self.extended_channels.contains_key(&m.channel_id) {
            // remove the channel from the extended channels map
            self.extended_channels.remove(&m.channel_id);

            // remove the channel from any group channels that contain it
            for mut group_channel in self.group_channels.iter_mut() {
                if group_channel.has_channel_id(m.channel_id) {
                    group_channel.remove_channel_id(m.channel_id);
                }
            }
        } else {
            error!(
                "Channel Id not found: {}, ignoring CloseChannel message",
                m.channel_id
            );
            return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
        }

        Ok(())
    }

    async fn handle_set_extranonce_prefix(
        &mut self,
        _server_id: Option<usize>,
        m: SetExtranoncePrefix<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        warn!(
            "⚠️ Cannot process SetExtranoncePrefix since set_extranonce is not supported for majority of sv1 clients. Ignoring."
        );
        Ok(())
    }

    async fn handle_submit_shares_success(
        &mut self,
        _server_id: Option<usize>,
        m: SubmitSharesSuccess,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {} ✅", m);

        // In aggregated mode, the Pool responds with the upstream channel ID, but the
        // channel is stored under AGGREGATED_CHANNEL_ID in the DashMap.
        // In non-aggregated mode, m.channel_id matches the DashMap key directly.
        let key = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            m.channel_id
        };

        // if None, the channel may be closed/missing, so we ignore this accounting update
        if let Some(mut ch) = self.extended_channels.get_mut(&key) {
            ch.on_share_acknowledgement(m.new_submits_accepted_count, m.new_shares_sum);
        }

        Ok(())
    }

    async fn handle_submit_shares_error(
        &mut self,
        _server_id: Option<usize>,
        m: SubmitSharesError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {} ❌", m);
        let error_code = m.error_code.as_utf8_or_hex();

        let key = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            m.channel_id
        };

        // if None, the channel may be closed/missing, so we ignore this accounting update
        if let Some(mut ch) = self.extended_channels.get_mut(&key) {
            ch.on_share_rejection(error_code);
        }

        Ok(())
    }

    async fn handle_new_mining_job(
        &mut self,
        _server_id: Option<usize>,
        m: NewMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        warn!(
            "⚠️ Cannot process NewMiningJob since Translator Proxy supports only extended mining jobs. Ignoring."
        );
        Ok(())
    }

    async fn handle_new_extended_mining_job(
        &mut self,
        _server_id: Option<usize>,
        m: NewExtendedMiningJob<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);
        let m_static = m.clone().into_static();

        // we update the channel states and keep track of the messages that need to be sent to the
        // SV1Server
        let new_extended_mining_job_messages_sv1_server = {
            let mut new_extended_mining_job_messages = Vec::new();

            // are we in aggregated mode?
            if self.mode.is_aggregated() {
                // Validate that the message is for the aggregated channel or its group
                let (aggregated_channel_id, full_extranonce_size) = {
                    let aggregated_channel = self
                        .extended_channels
                        .get(&AGGREGATED_CHANNEL_ID)
                        .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;
                    (
                        aggregated_channel.get_channel_id(),
                        aggregated_channel.get_full_extranonce_size(),
                    )
                };

                // here, we are assuming that since we are in aggregated mode, there should
                // be only one single group channel and the
                // aggregated channel must belong to it
                let group_channel = self.group_channels.iter().next();
                let Some(group_channel) = group_channel else {
                    error!("Aggregated channel does not belong to any group channel");
                    return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                };

                let group_channel_id = group_channel.get_group_channel_id();

                // was the message sent to the aggregated channel?
                if aggregated_channel_id == m_static.channel_id
                    || group_channel_id == m_static.channel_id
                {
                    self.verify_payout_distribution(&m_static, full_extranonce_size)?;

                    // update all extended channel states
                    for mut extended_channel in self.extended_channels.iter_mut() {
                        extended_channel
                            .on_new_extended_mining_job(m_static.clone())
                            .map_err(|e| {
                                error!("Failed to process new extended mining job: {:?}", e);
                                TproxyError::fallback(
                                    TproxyErrorKind::FailedToProcessNewExtendedMiningJob,
                                )
                            })?;
                    }

                    // only send this message to the SV1Server if it's not a future job
                    if !m_static.is_future() {
                        let mut new_extended_mining_job_message = m_static.clone();
                        new_extended_mining_job_message.channel_id = AGGREGATED_CHANNEL_ID; // this is done so that every aggregated downstream
                                                                                            // will receive the NewExtendedMiningJob message
                        new_extended_mining_job_messages.push(new_extended_mining_job_message);
                    }
                } else {
                    // we got a nonsense channel id, we should log an error and ignore the
                    // message
                    error!(
                        "Channel not found: {}, ignoring NewExtendedMiningJob message",
                        m_static.channel_id
                    );
                    return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                }
            // we're not in aggregated mode
            // was the message sent to a group channel?
            } else if let Some(mut group_channel) = self.group_channels.get_mut(&m.channel_id) {
                let full_extranonce_size =
                    group_channel.get_full_extranonce_size().ok_or_else(|| {
                        error!("Group channel {} has no full extranonce size", m.channel_id);
                        TproxyError::fallback(TproxyErrorKind::ChannelNotFound)
                    })?;
                self.verify_payout_distribution(&m_static, full_extranonce_size)?;

                // update group channel state
                group_channel.on_new_extended_mining_job(m_static.clone());

                // process the message for each individual channel on the group
                for channel_id in group_channel.get_channel_ids() {
                    let mut channel = self
                        .extended_channels
                        .get_mut(channel_id)
                        .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                    let mut job = m_static.clone();
                    job.channel_id = *channel_id;

                    // update each channel state
                    channel
                        .on_new_extended_mining_job(job.clone())
                        .map_err(|e| {
                            error!("Failed to process new extended mining job: {:?}", e);
                            TproxyError::fallback(
                                TproxyErrorKind::FailedToProcessNewExtendedMiningJob,
                            )
                        })?;

                    // only send this message to the SV1Server if it's not a future job
                    if !job.is_future() {
                        new_extended_mining_job_messages.push(job);
                    }
                }
            // if the message was not sent to a group channel, we need to check if we're
            // working in aggregated mode
            } else {
                let Some(mut channel) = self.extended_channels.get_mut(&m_static.channel_id) else {
                    // we got a nonsense channel id, we should log an error and ignore the
                    // message
                    error!(
                        "Channel not found: {}, ignoring NewExtendedMiningJob message",
                        m_static.channel_id
                    );
                    return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                };

                self.verify_payout_distribution(&m_static, channel.get_full_extranonce_size())?;

                // update channel state
                channel
                    .on_new_extended_mining_job(m_static.clone())
                    .map_err(|e| {
                        error!("Failed to process new extended mining job: {:?}", e);
                        TproxyError::fallback(TproxyErrorKind::FailedToProcessNewExtendedMiningJob)
                    })?;

                // only send this message to the SV1Server if it's not a future job
                if !m_static.is_future() {
                    let new_extended_mining_job_message = m_static.clone();
                    new_extended_mining_job_messages.push(new_extended_mining_job_message);
                }
            }
            Ok::<Vec<NewExtendedMiningJob<'static>>, Self::Error>(new_extended_mining_job_messages)
        }?;

        // now we need to send the NewExtendedMiningJob message(s) to the SV1Server
        for message in new_extended_mining_job_messages_sv1_server {
            self.channel_manager_io
                .sv1_server_sender
                .send(Mining::NewExtendedMiningJob(message))
                .await
                .map_err(|e| {
                    error!("Failed to send immediate NewExtendedMiningJob: {:?}", e);
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }
        Ok(())
    }

    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        m: SetNewPrevHash<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);
        let mut m_static = m.clone().into_static();

        // we update the channel states and keep track of the messages that need to be sent to the
        // SV1Server
        let (set_new_prev_hash_messages_sv1_server, new_extended_mining_job_messages_sv1_server) =
            {
                let mut set_new_prev_hash_messages = Vec::new();
                let mut new_extended_mining_job_messages = Vec::new();

                if self.mode.is_aggregated() {
                    // Validate that the message is for the aggregated channel or its group
                    let aggregated_channel_id = self
                        .extended_channels
                        .get(&AGGREGATED_CHANNEL_ID)
                        .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?
                        .get_channel_id();

                    // does aggregated channel belong to some group channel?
                    // here, we are assuming that since we are in aggregated mode, there
                    // should be only one single group channel
                    // and the aggregated channel must belong to it
                    let group_channel = self.group_channels.iter().next();
                    let Some(group_channel) = group_channel else {
                        error!("Aggregated channel does not belong to any group channel");
                        return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                    };

                    let group_channel_id = group_channel.get_group_channel_id();

                    // was the message sent to the aggregated channel?
                    if aggregated_channel_id == m.channel_id || group_channel_id == m.channel_id {
                        // update all extended channel states
                        for mut extended_channel in self.extended_channels.iter_mut() {
                            extended_channel
                                .on_set_new_prev_hash(m_static.clone())
                                .map_err(|e| {
                                    error!("Failed to set new prev hash: {:?}", e);
                                    TproxyError::fallback(
                                        TproxyErrorKind::FailedToProcessSetNewPrevHash,
                                    )
                                })?;
                        }

                        // make sure the SetNewPrevHash message is sent to the aggregated
                        // channel
                        m_static.channel_id = AGGREGATED_CHANNEL_ID;
                        set_new_prev_hash_messages.push(m_static.clone());

                        // for the aggregated channel, send one NewExtendedMiningJob message
                        // to the SV1Server (get active job after updating all channels)
                        let mut new_extended_mining_job_message = self
                            .extended_channels
                            .get(&AGGREGATED_CHANNEL_ID)
                            .expect("aggregated channel must exist")
                            .get_active_job()
                            .expect("active job must exist")
                            .clone();
                        new_extended_mining_job_message.0.channel_id = AGGREGATED_CHANNEL_ID;
                        new_extended_mining_job_messages.push(new_extended_mining_job_message.0);
                    } else {
                        // we got a nonsense channel id, we should log an error and ignore
                        // the message
                        warn!(
                            "Channel not found: {}, ignoring SetNewPrevHash message",
                            m_static.channel_id
                        );
                        return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                    }
                // we are not in aggregated mode.. was the message sent to a group channel?
                } else if let Some(mut group_channel) = self.group_channels.get_mut(&m.channel_id) {
                    // update group channel state
                    group_channel
                        .on_set_new_prev_hash(m_static.clone())
                        .map_err(|e| {
                            error!("Failed to set new prev hash: {:?}", e);
                            TproxyError::fallback(TproxyErrorKind::FailedToProcessSetNewPrevHash)
                        })?;

                    // there's no aggregated channel, so we need to process the message for each
                    // individual channel on the group
                    for channel_id in group_channel.get_channel_ids() {
                        let mut channel = self
                            .extended_channels
                            .get_mut(channel_id)
                            .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                        channel
                            .on_set_new_prev_hash(m_static.clone())
                            .map_err(|e| {
                                error!("Failed to set new prev hash: {:?}", e);
                                TproxyError::fallback(
                                    TproxyErrorKind::FailedToProcessSetNewPrevHash,
                                )
                            })?;

                        // for each extended channel, send one SetNewPrevHash message to the
                        // SV1Server
                        let mut set_new_prev_hash_message = m_static.clone();
                        set_new_prev_hash_message.channel_id = *channel_id;
                        set_new_prev_hash_messages.push(set_new_prev_hash_message);

                        // for each extended channel, send one NewExtendedMiningJob message to
                        // the SV1Server
                        let new_extended_mining_job_message = channel
                            .get_active_job()
                            .expect("active job must exist")
                            .clone();
                        new_extended_mining_job_messages.push(new_extended_mining_job_message.0);
                    }
                // if the message was not sent to a group channel, and we're not in aggregated
                // mode, we need to process the message for a specific channel
                } else {
                    let Some(mut channel) = self.extended_channels.get_mut(&m_static.channel_id)
                    else {
                        // we got a nonsense channel id, we should log an error and ignore the
                        // message
                        warn!(
                            "Channel not found: {}, ignoring SetNewPrevHash message",
                            m_static.channel_id
                        );
                        return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                    };

                    // update channel state
                    channel
                        .on_set_new_prev_hash(m_static.clone())
                        .map_err(|e| {
                            error!("Failed to set new prev hash: {:?}", e);
                            TproxyError::fallback(TproxyErrorKind::FailedToProcessSetNewPrevHash)
                        })?;

                    // make sure the SetNewPrevHash message is sent to the channel
                    set_new_prev_hash_messages.push(m_static.clone());

                    // for the channel, send one NewExtendedMiningJob message to the SV1Server
                    let new_extended_mining_job_message = channel
                        .get_active_job()
                        .expect("active job must exist")
                        .clone();
                    new_extended_mining_job_messages.push(new_extended_mining_job_message.0);
                }
                Ok::<
                    (
                        Vec<SetNewPrevHash<'static>>,
                        Vec<NewExtendedMiningJob<'static>>,
                    ),
                    Self::Error,
                >((set_new_prev_hash_messages, new_extended_mining_job_messages))
            }?;

        // we need to send the SetNewPrevHash message(s) to the SV1Server
        for message in set_new_prev_hash_messages_sv1_server {
            self.channel_manager_io
                .sv1_server_sender
                .send(Mining::SetNewPrevHash(message))
                .await
                .map_err(|e| {
                    error!("Failed to send SetNewPrevHash: {:?}", e);
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }

        // we need to send the NewExtendedMiningJob message(s) to the SV1Server
        for message in new_extended_mining_job_messages_sv1_server {
            self.channel_manager_io
                .sv1_server_sender
                .send(Mining::NewExtendedMiningJob(message))
                .await
                .map_err(|e| {
                    error!("Failed to send NewExtendedMiningJob: {:?}", e);
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }

        Ok(())
    }

    async fn handle_set_custom_mining_job_success(
        &mut self,
        _server_id: Option<usize>,
        m: SetCustomMiningJobSuccess,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        warn!(
            "⚠️ Cannot process SetCustomMiningJobSuccess since Translator Proxy does not support custom mining jobs. Ignoring."
        );
        Err(TproxyError::log(TproxyErrorKind::UnexpectedMessage(
            0,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        )))
    }

    async fn handle_set_custom_mining_job_error(
        &mut self,
        _server_id: Option<usize>,
        m: SetCustomMiningJobError<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        warn!(
            "⚠️ Cannot process SetCustomMiningJobError since Translator Proxy does not support custom mining jobs. Ignoring."
        );
        Err(TproxyError::log(TproxyErrorKind::UnexpectedMessage(
            0,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_ERROR,
        )))
    }

    async fn handle_set_target(
        &mut self,
        _server_id: Option<usize>,
        m: SetTarget<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);

        let m_static = m.clone().into_static();

        // Update the channel targets in the channel manager
        let set_target_messages_sv1_server = {
            let mut set_target_messages = Vec::new();

            // are in aggregated mode?
            if self.mode.is_aggregated() {
                let aggregated_channel_id = self
                    .extended_channels
                    .get(&AGGREGATED_CHANNEL_ID)
                    .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?
                    .get_channel_id();

                // does aggregated channel belong to some group channel?
                // here, we are assuming that since we are in aggregated mode, there should
                // be only one single group channel and the
                // aggregated channel must belong to it
                let group_channel = self.group_channels.iter().next();
                let Some(group_channel) = group_channel else {
                    error!("Aggregated channel does not belong to any group channel");
                    return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                };

                let group_channel_id = group_channel.get_group_channel_id();

                // was the message sent to the aggregated channel?
                if aggregated_channel_id == m.channel_id || group_channel_id == m.channel_id {
                    // Update target for all extended channels (including AGGREGATED_CHANNEL_ID)
                    self.extended_channels.iter_mut().for_each(|mut channel| {
                        channel.set_target(Target::from_le_bytes(m.maximum_target.to_array()));
                    });

                    let mut message = m_static.clone();
                    message.channel_id = AGGREGATED_CHANNEL_ID;
                    set_target_messages.push(message);
                } else {
                    // we got a nonsense channel id, we should log an error and ignore the
                    // message
                    warn!(
                        "Channel not found: {}, ignoring SetTarget message",
                        m_static.channel_id
                    );
                    return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                }

            // we are not in aggregated mode... was the message sent to a group channel?
            } else if let Some(group_channel) = self.group_channels.get(&m.channel_id) {
                // process the message for each individual channel on the group
                for channel_id in group_channel.get_channel_ids() {
                    let mut channel = self
                        .extended_channels
                        .get_mut(channel_id)
                        .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                    channel.set_target(Target::from_le_bytes(m.maximum_target.to_array()));

                    let mut message = m_static.clone();
                    message.channel_id = *channel_id;
                    set_target_messages.push(message);
                }
            // if the message was not sent to a group channel, and we're not in aggregated
            // mode, we need to process the message for a specific channel
            } else {
                let Some(mut channel) = self.extended_channels.get_mut(&m.channel_id) else {
                    // we got a nonsense channel id, we should log an error and ignore the
                    // message
                    warn!(
                        "Channel not found: {}, ignoring SetTarget message",
                        m_static.channel_id
                    );
                    return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                };

                channel.set_target(Target::from_le_bytes(m.maximum_target.to_array()));

                set_target_messages.push(m_static.clone());
            }

            Ok::<Vec<SetTarget<'static>>, Self::Error>(set_target_messages)
        }?;

        // now we need to send the SetTarget message(s) to the SV1Server
        for message in set_target_messages_sv1_server {
            self.channel_manager_io
                .sv1_server_sender
                .send(Mining::SetTarget(message))
                .await
                .map_err(|e| {
                    error!("Failed to send SetTarget: {:?}", e);
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }

        Ok(())
    }

    async fn handle_set_group_channel(
        &mut self,
        _server_id: Option<usize>,
        m: SetGroupChannel<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);

        // remove every channel from any group channels that end up empty
        let mut group_channels_to_remove = Vec::new();

        // check every group channel if it contains any of the channels in the new group
        // channel
        for mut channel in self.group_channels.iter_mut() {
            let group_channel_id = *channel.key();
            let group_channel = channel.value_mut();

            let channel_ids_to_remove = m.channel_ids.clone().into_inner();
            for channel_id in channel_ids_to_remove {
                group_channel.remove_channel_id(channel_id);
            }

            if group_channel.is_empty() {
                group_channels_to_remove.push(group_channel_id);
            }
        }

        // Now remove the empty group channels
        for group_channel_id in group_channels_to_remove {
            self.group_channels.remove(&group_channel_id);
        }

        // does the group channel already exist?
        match self.group_channels.get_mut(&m.group_channel_id) {
            // if yes, clean up any channels that are no longer in the new group channel
            Some(mut group_channel) => {
                let current_channel_ids: Vec<u32> =
                    group_channel.get_channel_ids().copied().collect();
                let new_channel_ids = m.channel_ids.clone().into_inner();

                // Remove channels that are no longer in the new list
                for channel_id in &current_channel_ids {
                    if !new_channel_ids.contains(channel_id) {
                        group_channel.remove_channel_id(*channel_id);
                    }
                }

                // Add all channels from the message (inner HashSet ingores duplicates)
                for channel_id in new_channel_ids {
                    let extended_channel = self
                        .extended_channels
                        .get(&channel_id)
                        .ok_or_else(|| TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                    let full_extranonce_size = extended_channel.get_full_extranonce_size();
                    group_channel
                        .add_channel_id(channel_id, full_extranonce_size)
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            TproxyError::fallback(
                                TproxyErrorKind::FailedToAddChannelIdToGroupChannel(e),
                            )
                        })?;
                }
            }
            // if no, create a new group channel, and add all the channels to it
            None => {
                let mut group_channel = GroupChannel::new(m.group_channel_id);

                // Add all channels to the newly created group channel
                for channel_id in m.channel_ids.clone().into_inner() {
                    let extended_channel = self
                        .extended_channels
                        .get(&channel_id)
                        .ok_or_else(|| TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                    let full_extranonce_size = extended_channel.get_full_extranonce_size();

                    group_channel
                        .add_channel_id(channel_id, full_extranonce_size)
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            TproxyError::fallback(
                                TproxyErrorKind::FailedToAddChannelIdToGroupChannel(e),
                            )
                        })?;
                }

                self.group_channels
                    .insert(m.group_channel_id, group_channel);
            }
        }

        Ok(())
    }
}

impl ChannelManager {
    #[allow(clippy::result_large_err)]
    fn verify_payout_distribution(
        &self,
        job: &NewExtendedMiningJob<'_>,
        full_extranonce_size: usize,
    ) -> Result<(), TproxyError<error::ChannelManager>> {
        if let Some(expected_payout_distribution) = self.expected_payout_distribution() {
            expected_payout_distribution
                .validate_coinbase_tx_parts(
                    job.coinbase_tx_prefix.as_bytes(),
                    job.coinbase_tx_suffix.as_bytes(),
                    full_extranonce_size,
                )
                .map_err(|e| {
                    error!("NewExtendedMiningJob failed payout verification: {e}");
                    TproxyError::fallback(TproxyErrorKind::PayoutVerificationFailed(e.to_string()))
                })?;
        }

        Ok(())
    }
}
