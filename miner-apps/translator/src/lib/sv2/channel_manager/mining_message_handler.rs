use crate::{
    error::{self, TproxyError, TproxyErrorKind, TproxyResult},
    sv2::channel_manager::{
        AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES, AGGREGATED_TPROXY_MAX_CHANNELS, ChannelManager,
        NON_AGGREGATED_TPROXY_MAX_CHANNELS,
    },
    utils::{AGGREGATED_CHANNEL_ID, AggregatedState, aggregated_upstream_user_identity},
};
use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{
            client::{extended::ExtendedChannel, group::GroupChannel},
            extranonce_manager::{
                ExtranonceAllocator, ExtranoncePrefix, MAX_EXTRANONCE_LEN, bytes_needed,
            },
        },
        handlers_sv2::{HandleMiningMessagesFromServerOwnedAsync, SupportedChannelTypes},
        mining_sv2::{
            CloseChannelOwned, MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_ERROR, MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
            NewExtendedMiningJobOwned, NewMiningJobOwned, OpenExtendedMiningChannelSuccessOwned,
            OpenMiningChannelErrorOwned, OpenStandardMiningChannelSuccessOwned,
            SetCustomMiningJobErrorOwned, SetCustomMiningJobSuccessOwned, SetExtranoncePrefixOwned,
            SetGroupChannelOwned, SetNewPrevHashOwned, SetTargetOwned, SubmitSharesErrorOwned,
            SubmitSharesSuccessOwned, UpdateChannelErrorOwned,
        },
        parsers_sv2::{MiningOwned, Tlv},
    },
    utils::types::{DownstreamId, Hashrate},
};
use tracing::{error, info, warn};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleMiningMessagesFromServerOwnedAsync for ChannelManager {
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
        self.negotiated_extensions
            .get()
            .map_err(TproxyError::shutdown)
    }

    async fn handle_open_standard_mining_channel_success(
        &mut self,
        _server_id: Option<usize>,
        m: OpenStandardMiningChannelSuccessOwned,
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
        m: OpenExtendedMiningChannelSuccessOwned,
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

        // Upstream IDs must never collide with tProxy's internal broadcast sentinel. The wire
        // channel and group IDs also share one namespace in every mode.
        let reserved_or_self_collision = m.channel_id == AGGREGATED_CHANNEL_ID
            || m.group_channel_id == AGGREGATED_CHANNEL_ID
            || m.channel_id == m.group_channel_id;
        // In non-aggregated mode, wire channel IDs are stored directly in the local maps, so also
        // reject collisions with any live channel or group before state can be overwritten.
        if reserved_or_self_collision
            || (!self.mode.is_aggregated()
                && (self.extended_channels.contains_key(&m.channel_id)
                    || self.group_channels.contains_key(&m.channel_id)
                    || self.extended_channels.contains_key(&m.group_channel_id)))
        {
            let conflicting_id = if m.channel_id == AGGREGATED_CHANNEL_ID
                || m.group_channel_id == AGGREGATED_CHANNEL_ID
            {
                AGGREGATED_CHANNEL_ID
            } else if self.extended_channels.contains_key(&m.group_channel_id) {
                m.group_channel_id
            } else {
                m.channel_id
            };
            error!(
                channel_id = m.channel_id,
                group_channel_id = m.group_channel_id,
                "Rejecting OpenExtendedMiningChannelSuccess with a colliding channel ID"
            );
            return Err(TproxyError::fallback(
                TproxyErrorKind::ChannelIdAlreadyInUse(conflicting_id),
            ));
        }

        let success = {
            info!(
                "Received: {:?}, user_identity: {}, nominal_hashrate: {}",
                m, user_identity, nominal_hashrate
            );

            let full_extranonce_size = m.extranonce_size as usize + m.extranonce_prefix.len();

            self.group_channels.with_mut_or_insert_with(
                m.group_channel_id,
                || GroupChannel::new(m.group_channel_id),
                |group_channel| {
                    group_channel
                        .add_channel_id(m.channel_id, full_extranonce_size)
                        .map_err(|e| {
                            error!("Failed to add channel id to group channel: {:?}", e);
                            TproxyError::fallback(
                                TproxyErrorKind::FailedToAddChannelIdToGroupChannel(e),
                            )
                        })
                },
            )?;

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
                    .set(Some(allocator))
                    .map_err(TproxyError::shutdown)?;
                self.aggregated_channel_state
                    .set(AggregatedState::Connected);

                let new_open_extended_mining_channel_success =
                    OpenExtendedMiningChannelSuccessOwned {
                        request_id: m.request_id,
                        channel_id: 1,
                        extranonce_prefix: downstream_extranonce_prefix_bytes
                            .try_into()
                            .map_err(TproxyError::shutdown)?,
                        extranonce_size: downstream_extranonce_len as u16,
                        target: m.target.clone(),
                        group_channel_id: m.group_channel_id,
                    };
                Ok::<OpenExtendedMiningChannelSuccessOwned, Self::Error>(
                    new_open_extended_mining_channel_success,
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
                    // No slack: forward `upstream_prefix` directly to the
                    // downstream. Its length equals the full prefix length,
                    // leaving no `local_prefix | local_index` bytes for share
                    // rewriting.
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

                let new_open_extended_mining_channel_success =
                    OpenExtendedMiningChannelSuccessOwned {
                        request_id: m.request_id,
                        channel_id: m.channel_id,
                        extranonce_prefix: downstream_prefix_bytes_for_success
                            .try_into()
                            .map_err(TproxyError::shutdown)?,
                        extranonce_size: downstream_extranonce_len as u16,
                        target: m.target.clone(),
                        group_channel_id: m.group_channel_id,
                    };
                Ok::<OpenExtendedMiningChannelSuccessOwned, Self::Error>(
                    new_open_extended_mining_channel_success,
                )
            }
        }?;

        self.channel_manager_io
            .sv1_server_sender
            .send(MiningOwned::OpenExtendedMiningChannelSuccess(
                success.clone(),
            ))
            .await
            .map_err(|e| {
                error!("Failed to send OpenExtendedMiningChannelSuccess: {:?}", e);
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })?;
        self.sv1_advertised_extranonce_prefixes.insert(
            success.channel_id,
            success.extranonce_prefix.to_owned_bytes(),
        );

        // In aggregated mode, serve any downstream requests that were buffered in
        // pending_channels while the upstream channel was being established (Pending state).
        if self.mode.is_aggregated() {
            let mut pending_requests: Vec<(u32, String, Hashrate, usize)> = Vec::new();
            self.pending_downstream_channels
                .for_each(|request_id, request| {
                    pending_requests.push((
                        request_id as u32,
                        request.0.clone(),
                        request.1,
                        request.2,
                    ));
                });
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
        m: OpenMiningChannelErrorOwned,
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
        m: UpdateChannelErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);
        Ok(())
    }

    async fn handle_close_channel(
        &mut self,
        _server_id: Option<usize>,
        m: CloseChannelOwned,
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
        let closed_channel_ids;

        // we're not in aggregated mode
        // was the message sent to a group channel?
        if let Some((_, group_channel)) = group_channel {
            closed_channel_ids = group_channel.get_channel_ids().copied().collect::<Vec<_>>();
            for channel_id in &closed_channel_ids {
                self.extended_channels.remove(channel_id);
            }
        // if the message was not sent to a group channel, and we're not working in
        // aggregated mode,
        } else if self.extended_channels.remove(&m.channel_id).is_some() {
            closed_channel_ids = vec![m.channel_id];
            // remove the channel from any group channels that contain it
            self.group_channels.for_each_mut(|_, group_channel| {
                if group_channel.has_channel_id(m.channel_id) {
                    group_channel.remove_channel_id(m.channel_id);
                }
            });
        } else {
            error!(
                "Channel Id not found: {}, ignoring CloseChannel message",
                m.channel_id
            );
            return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
        }

        // SV1 has no channel-level close message. Forward one close per affected extended channel
        // so Sv1Server can terminate the corresponding TCP connection instead of leaving a miner
        // submitting shares for a channel that no longer exists upstream.
        for channel_id in closed_channel_ids {
            self.sv1_advertised_extranonce_prefixes.remove(&channel_id);
            let mut close = m.clone();
            close.channel_id = channel_id;
            self.channel_manager_io
                .sv1_server_sender
                .send(MiningOwned::CloseChannel(close))
                .await
                .map_err(|error| {
                    error!(
                        channel_id,
                        "Failed to forward upstream CloseChannel to SV1 server: {error:?}"
                    );
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }

        Ok(())
    }

    async fn handle_set_extranonce_prefix(
        &mut self,
        _server_id: Option<usize>,
        m: SetExtranoncePrefixOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {}", m);

        let new_upstream_prefix = m.extranonce_prefix.to_owned_bytes();

        if self.mode.is_aggregated() {
            let (upstream_channel_id, upstream_rollable_extranonce_size) = self
                .extended_channels
                .with(&AGGREGATED_CHANNEL_ID, |channel| {
                    (
                        channel.get_channel_id(),
                        channel.get_rollable_extranonce_size(),
                    )
                })
                .ok_or_else(|| TproxyError::shutdown(TproxyErrorKind::ChannelNotFound))?;
            if m.channel_id != upstream_channel_id {
                warn!(
                    channel_id = m.channel_id,
                    upstream_channel_id,
                    "Ignoring SetExtranoncePrefix for unknown aggregated channel"
                );
                return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
            }

            let prefix_len = new_upstream_prefix.len();
            let total_extranonce_len = new_upstream_prefix
                .len()
                .checked_add(upstream_rollable_extranonce_size as usize)
                .ok_or_else(|| {
                    TproxyError::fallback(TproxyErrorKind::InvalidExtranonceSize {
                        prefix_len,
                        rollable_size: upstream_rollable_extranonce_size,
                    })
                })?;
            if total_extranonce_len > MAX_EXTRANONCE_LEN as usize {
                return Err(TproxyError::fallback(
                    TproxyErrorKind::InvalidExtranonceSize {
                        prefix_len,
                        rollable_size: upstream_rollable_extranonce_size,
                    },
                ));
            }

            self.aggregated_extranonce_allocator
                .with(|allocator| {
                    allocator
                        .as_mut()
                        .ok_or_else(|| {
                            TproxyError::shutdown(
                                TproxyErrorKind::MissingAggregatedExtranonceAllocator,
                            )
                        })?
                        .set_upstream_prefix(new_upstream_prefix.clone())
                        .map_err(|error| {
                            TproxyError::fallback(
                                TproxyErrorKind::AggregatedExtranonceAllocatorUpdateFailed(error),
                            )
                        })
                })
                .map_err(TproxyError::shutdown)??;

            // This sentinel channel stores the upstream wire value directly, so updating its
            // upstream-owned region replaces the whole prefix.
            self.extended_channels
                .with_mut(&AGGREGATED_CHANNEL_ID, |channel| {
                    channel.set_upstream_extranonce_prefix(&new_upstream_prefix)
                })
                .ok_or_else(|| TproxyError::shutdown(TproxyErrorKind::ChannelNotFound))?
                .map_err(|error| {
                    TproxyError::fallback(TproxyErrorKind::UpstreamExtranoncePrefixUpdateFailed {
                        channel_id: upstream_channel_id,
                        error,
                    })
                })?;

            let mut downstream_channel_ids = Vec::new();
            self.extended_channels.for_each(|channel_id, _| {
                if channel_id != AGGREGATED_CHANNEL_ID {
                    downstream_channel_ids.push(channel_id);
                }
            });

            for channel_id in downstream_channel_ids {
                let Some(update_result) = self.extended_channels.with_mut(
                    &channel_id,
                    |channel| -> TproxyResult<(), error::ChannelManager> {
                        // Allocated child channels keep `local_prefix | local_index` and their
                        // bitmap reservation while replacing only `upstream_prefix`.
                        channel
                            .set_upstream_extranonce_prefix(&new_upstream_prefix)
                            .map_err(|error| {
                                TproxyError::shutdown(
                                    TproxyErrorKind::UpstreamExtranoncePrefixUpdateFailed {
                                        channel_id,
                                        error,
                                    },
                                )
                            })?;
                        Ok(())
                    },
                ) else {
                    continue;
                };
                update_result?;
            }
            // Existing jobs retain their captured prefixes. The corresponding SV1 notifications
            // are emitted immediately before the first job that uses each new downstream prefix.
            return Ok(());
        }

        let channel_id = m.channel_id;
        let Some(update_result) = self.extended_channels.with_mut(
            &channel_id,
            |channel| -> TproxyResult<(), error::ChannelManager> {
                let rollable_size = channel.get_rollable_extranonce_size();
                // Include the preserved `local_prefix | local_index` regions when validating the
                // final extranonce layout.
                let current_prefix_len = channel.get_extranonce_prefix().len();
                let upstream_prefix_len = channel.upstream_prefix_len() as usize;
                let local_prefix_and_index_len = current_prefix_len
                    .checked_sub(upstream_prefix_len)
                    .ok_or_else(|| {
                        TproxyError::fallback(TproxyErrorKind::InvalidExtranonceSize {
                            prefix_len: current_prefix_len,
                            rollable_size,
                        })
                    })?;
                let prefix_len = new_upstream_prefix
                    .len()
                    .checked_add(local_prefix_and_index_len)
                    .ok_or_else(|| {
                        TproxyError::fallback(TproxyErrorKind::InvalidExtranonceSize {
                            prefix_len: new_upstream_prefix.len(),
                            rollable_size,
                        })
                    })?;
                let full_extranonce_len = prefix_len
                    .checked_add(rollable_size as usize)
                    .ok_or_else(|| {
                        TproxyError::fallback(TproxyErrorKind::InvalidExtranonceSize {
                            prefix_len,
                            rollable_size,
                        })
                    })?;
                if full_extranonce_len > MAX_EXTRANONCE_LEN as usize {
                    return Err(TproxyError::fallback(
                        TproxyErrorKind::InvalidExtranonceSize {
                            prefix_len,
                            rollable_size,
                        },
                    ));
                }

                // channels_sv2 replaces `upstream_prefix` while preserving any
                // `local_prefix | local_index` regions.
                channel
                    .set_upstream_extranonce_prefix(&new_upstream_prefix)
                    .map_err(|error| {
                        TproxyError::fallback(
                            TproxyErrorKind::UpstreamExtranoncePrefixUpdateFailed {
                                channel_id,
                                error,
                            },
                        )
                    })?;

                Ok(())
            },
        ) else {
            warn!(
                channel_id,
                "Ignoring SetExtranoncePrefix for unknown channel"
            );
            return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
        };

        update_result?;
        // Defer mining.set_extranonce until the exact job carrying this prefix is forwarded.

        Ok(())
    }

    async fn handle_submit_shares_success(
        &mut self,
        _server_id: Option<usize>,
        m: SubmitSharesSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {} ✅", m);

        // In aggregated mode, the Pool responds with the upstream channel ID, but the
        // channel is stored under AGGREGATED_CHANNEL_ID in the shared channel map.
        // In non-aggregated mode, m.channel_id matches the shared channel map key directly.
        let key = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            m.channel_id
        };

        // if None, the channel may be closed/missing, so we ignore this accounting update
        self.extended_channels.with_mut(&key, |ch| {
            ch.on_share_acknowledgement(m.new_submits_accepted_count, m.new_shares_sum);
        });

        Ok(())
    }

    async fn handle_submit_shares_error(
        &mut self,
        _server_id: Option<usize>,
        m: SubmitSharesErrorOwned,
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
        self.extended_channels.with_mut(&key, |ch| {
            ch.on_share_rejection(&error_code);
        });

        Ok(())
    }

    async fn handle_new_mining_job(
        &mut self,
        _server_id: Option<usize>,
        m: NewMiningJobOwned,
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
        m: NewExtendedMiningJobOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);

        // tProxy declares version rolling as required in SetupConnection. Most SV1 miners need it
        // to produce usable shares, so forwarding a job that disables it would make tProxy discard
        // otherwise valid miner work during local validation. Treat the inconsistent upstream
        // response as a connection failure before exposing the job to any downstream.
        if !m.version_rolling_allowed {
            error!(
                "Upstream sent a NewExtendedMiningJob with version rolling disabled after accepting it as a required feature"
            );
            return Err(TproxyError::fallback(
                TproxyErrorKind::VersionRollingNotAllowed,
            ));
        }

        let m_static = m.clone();

        // we update the channel states and keep track of the messages that need to be sent to the
        // SV1Server
        let new_extended_mining_job_messages_sv1_server = {
            let mut new_extended_mining_job_messages = Vec::new();

            // are we in aggregated mode?
            if self.mode.is_aggregated() {
                // Validate that the message is for the aggregated channel or its group
                let (aggregated_channel_id, full_extranonce_size) = self
                    .extended_channels
                    .with(&AGGREGATED_CHANNEL_ID, |aggregated_channel| {
                        (
                            aggregated_channel.get_channel_id(),
                            aggregated_channel.get_full_extranonce_size(),
                        )
                    })
                    .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                // here, we are assuming that since we are in aggregated mode, there should
                // be only one single group channel and the
                // aggregated channel must belong to it
                let mut group_channel_id = None;
                self.group_channels.for_each(|channel_id, _| {
                    group_channel_id.get_or_insert(channel_id);
                });
                let Some(group_channel_id) = group_channel_id else {
                    error!("Aggregated channel does not belong to any group channel");
                    return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                };

                // was the message sent to the aggregated channel?
                if aggregated_channel_id == m_static.channel_id
                    || group_channel_id == m_static.channel_id
                {
                    self.verify_payout_distribution(&m_static, full_extranonce_size)?;

                    self.extended_channels
                        .try_for_each_mut(|_, extended_channel| {
                            extended_channel
                                .on_new_extended_mining_job(m_static.clone())
                                .map_err(|e| {
                                    error!("Failed to process new extended mining job: {:?}", e);
                                    TproxyError::fallback(
                                        TproxyErrorKind::FailedToProcessNewExtendedMiningJob,
                                    )
                                })
                        })?;

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
            } else if let Some(messages) =
                self.group_channels
                    .with_mut(&m.channel_id, |group_channel| {
                        let full_extranonce_size =
                            group_channel.get_full_extranonce_size().ok_or_else(|| {
                                error!(
                                    "Group channel {} has no full extranonce size",
                                    m.channel_id
                                );
                                TproxyError::fallback(TproxyErrorKind::ChannelNotFound)
                            })?;
                        self.verify_payout_distribution(&m_static, full_extranonce_size)?;

                        // update group channel state
                        group_channel.on_new_extended_mining_job(m_static.clone());
                        let channel_ids: Vec<_> =
                            group_channel.get_channel_ids().copied().collect();
                        let mut messages = Vec::new();

                        // process the message for each individual channel on the group
                        for channel_id in channel_ids {
                            let message = self
                                .extended_channels
                                .with_mut(&channel_id, |channel| {
                                    let mut job = m_static.clone();
                                    job.channel_id = channel_id;

                                    // update each channel state
                                    channel.on_new_extended_mining_job(job.clone()).map_err(
                                        |e| {
                                            error!(
                                                "Failed to process new extended mining job: {:?}",
                                                e
                                            );
                                            TproxyError::fallback(
                                            TproxyErrorKind::FailedToProcessNewExtendedMiningJob,
                                        )
                                        },
                                    )?;

                                    Ok::<_, Self::Error>(if !job.is_future() {
                                        Some(job)
                                    } else {
                                        None
                                    })
                                })
                                .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))??;
                            if let Some(message) = message {
                                messages.push(message);
                            }
                        }
                        Ok::<_, Self::Error>(messages)
                    })
            {
                new_extended_mining_job_messages.extend(messages?);
            // if the message was not sent to a group channel, we need to check if we're
            // working in aggregated mode
            } else {
                let message = self
                    .extended_channels
                    .with_mut(&m_static.channel_id, |channel| {
                        self.verify_payout_distribution(
                            &m_static,
                            channel.get_full_extranonce_size(),
                        )?;

                        // update channel state
                        channel
                            .on_new_extended_mining_job(m_static.clone())
                            .map_err(|e| {
                                error!("Failed to process new extended mining job: {:?}", e);
                                TproxyError::fallback(
                                    TproxyErrorKind::FailedToProcessNewExtendedMiningJob,
                                )
                            })?;

                        Ok::<_, Self::Error>(if !m_static.is_future() {
                            Some(m_static.clone())
                        } else {
                            None
                        })
                    });
                let Some(message) = message else {
                    error!(
                        "Channel not found: {}, ignoring NewExtendedMiningJob message",
                        m_static.channel_id
                    );
                    return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                };

                // only send this message to the SV1Server if it's not a future job
                if let Some(message) = message? {
                    new_extended_mining_job_messages.push(message);
                }
            }
            Ok::<Vec<NewExtendedMiningJobOwned>, Self::Error>(new_extended_mining_job_messages)
        }?;

        // now we need to send the NewExtendedMiningJob message(s) to the SV1Server
        for message in new_extended_mining_job_messages_sv1_server {
            self.forward_job_to_sv1_server(message).await?;
        }
        Ok(())
    }

    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        m: SetNewPrevHashOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);
        let mut m_static = m.clone();

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
                        .with(&AGGREGATED_CHANNEL_ID, |channel| channel.get_channel_id())
                        .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                    // does aggregated channel belong to some group channel?
                    // here, we are assuming that since we are in aggregated mode, there
                    // should be only one single group channel
                    // and the aggregated channel must belong to it
                    let mut group_channel_id = None;
                    self.group_channels.for_each(|channel_id, _| {
                        group_channel_id.get_or_insert(channel_id);
                    });
                    let Some(group_channel_id) = group_channel_id else {
                        error!("Aggregated channel does not belong to any group channel");
                        return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                    };

                    // was the message sent to the aggregated channel?
                    if aggregated_channel_id == m.channel_id || group_channel_id == m.channel_id {
                        // update all extended channel states
                        self.extended_channels
                            .try_for_each_mut(|_, extended_channel| {
                                extended_channel
                                    .on_set_new_prev_hash(m_static.clone())
                                    .map_err(|e| {
                                        error!("Failed to set new prev hash: {:?}", e);
                                        TproxyError::fallback(
                                            TproxyErrorKind::FailedToProcessSetNewPrevHash,
                                        )
                                    })
                            })?;

                        // make sure the SetNewPrevHash message is sent to the aggregated
                        // channel
                        m_static.channel_id = AGGREGATED_CHANNEL_ID;
                        set_new_prev_hash_messages.push(m_static.clone());

                        // for the aggregated channel, send one NewExtendedMiningJob message
                        // to the SV1Server (get active job after updating all channels)
                        let mut new_extended_mining_job_message = self
                            .extended_channels
                            .with(&AGGREGATED_CHANNEL_ID, |channel| {
                                channel
                                    .get_active_job()
                                    .expect("active job must exist")
                                    .clone()
                            })
                            .expect("aggregated channel must exist");
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
                } else if let Some(messages) =
                    self.group_channels
                        .with_mut(&m.channel_id, |group_channel| {
                            // update group channel state
                            group_channel
                                .on_set_new_prev_hash(m_static.clone())
                                .map_err(|e| {
                                    error!("Failed to set new prev hash: {:?}", e);
                                    TproxyError::fallback(
                                        TproxyErrorKind::FailedToProcessSetNewPrevHash,
                                    )
                                })?;
                            let channel_ids: Vec<_> =
                                group_channel.get_channel_ids().copied().collect();
                            let mut set_new_prev_hash_messages = Vec::new();
                            let mut new_extended_mining_job_messages = Vec::new();

                            // there's no aggregated channel, so we need to process the message for
                            // each individual channel on the group
                            for channel_id in channel_ids {
                                let new_extended_mining_job_message = self
                                    .extended_channels
                                    .with_mut(&channel_id, |channel| {
                                        channel.on_set_new_prev_hash(m_static.clone()).map_err(
                                            |e| {
                                                error!("Failed to set new prev hash: {:?}", e);
                                                TproxyError::fallback(
                                                    TproxyErrorKind::FailedToProcessSetNewPrevHash,
                                                )
                                            },
                                        )?;

                                        let new_extended_mining_job_message = channel
                                            .get_active_job()
                                            .expect("active job must exist")
                                            .clone();
                                        Ok::<_, Self::Error>(new_extended_mining_job_message.0)
                                    })
                                    .ok_or(TproxyError::fallback(
                                        TproxyErrorKind::ChannelNotFound,
                                    ))??;

                                // for each extended channel, send one SetNewPrevHash message to
                                // the SV1Server
                                let mut set_new_prev_hash_message = m_static.clone();
                                set_new_prev_hash_message.channel_id = channel_id;
                                set_new_prev_hash_messages.push(set_new_prev_hash_message);
                                new_extended_mining_job_messages
                                    .push(new_extended_mining_job_message);
                            }

                            Ok::<_, Self::Error>((
                                set_new_prev_hash_messages,
                                new_extended_mining_job_messages,
                            ))
                        })
                {
                    let messages = messages?;
                    set_new_prev_hash_messages.extend(messages.0);
                    new_extended_mining_job_messages.extend(messages.1);
                // if the message was not sent to a group channel, and we're not in aggregated
                // mode, we need to process the message for a specific channel
                } else {
                    let messages =
                        self.extended_channels
                            .with_mut(&m_static.channel_id, |channel| {
                                channel
                                    .on_set_new_prev_hash(m_static.clone())
                                    .map_err(|e| {
                                        error!("Failed to set new prev hash: {:?}", e);
                                        TproxyError::fallback(
                                            TproxyErrorKind::FailedToProcessSetNewPrevHash,
                                        )
                                    })?;

                                let new_extended_mining_job_message = channel
                                    .get_active_job()
                                    .expect("active job must exist")
                                    .clone();
                                Ok::<_, Self::Error>(new_extended_mining_job_message.0)
                            });
                    let Some(new_extended_mining_job_message) = messages else {
                        // we got a nonsense channel id, we should log an error and ignore the
                        // message
                        warn!(
                            "Channel not found: {}, ignoring SetNewPrevHash message",
                            m_static.channel_id
                        );
                        return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                    };

                    // make sure the SetNewPrevHash message is sent to the channel
                    set_new_prev_hash_messages.push(m_static.clone());

                    // for the channel, send one NewExtendedMiningJob message to the SV1Server
                    new_extended_mining_job_messages.push(new_extended_mining_job_message?);
                }
                Ok::<(Vec<SetNewPrevHashOwned>, Vec<NewExtendedMiningJobOwned>), Self::Error>((
                    set_new_prev_hash_messages,
                    new_extended_mining_job_messages,
                ))
            }?;

        // we need to send the SetNewPrevHash message(s) to the SV1Server
        for message in set_new_prev_hash_messages_sv1_server {
            self.channel_manager_io
                .sv1_server_sender
                .send(MiningOwned::SetNewPrevHash(message))
                .await
                .map_err(|e| {
                    error!("Failed to send SetNewPrevHash: {:?}", e);
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }

        // we need to send the NewExtendedMiningJob message(s) to the SV1Server
        for message in new_extended_mining_job_messages_sv1_server {
            self.forward_job_to_sv1_server(message).await?;
        }

        Ok(())
    }

    async fn handle_set_custom_mining_job_success(
        &mut self,
        _server_id: Option<usize>,
        m: SetCustomMiningJobSuccessOwned,
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
        m: SetCustomMiningJobErrorOwned,
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
        m: SetTargetOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);

        let m_static = m.clone();

        // Update the channel targets in the channel manager
        let set_target_messages_sv1_server = {
            let mut set_target_messages = Vec::new();

            // are in aggregated mode?
            if self.mode.is_aggregated() {
                let aggregated_channel_id = self
                    .extended_channels
                    .with(&AGGREGATED_CHANNEL_ID, |channel| channel.get_channel_id())
                    .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                // does aggregated channel belong to some group channel?
                // here, we are assuming that since we are in aggregated mode, there should
                // be only one single group channel and the
                // aggregated channel must belong to it
                let mut group_channel_id = None;
                self.group_channels.for_each(|channel_id, _| {
                    group_channel_id.get_or_insert(channel_id);
                });
                let Some(group_channel_id) = group_channel_id else {
                    error!("Aggregated channel does not belong to any group channel");
                    return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                };

                // was the message sent to the aggregated channel?
                if aggregated_channel_id == m.channel_id || group_channel_id == m.channel_id {
                    // Update target for all extended channels (including AGGREGATED_CHANNEL_ID)
                    self.extended_channels.for_each_mut(|_, channel| {
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
            } else if let Some(channel_ids) =
                self.group_channels.with(&m.channel_id, |group_channel| {
                    group_channel.get_channel_ids().copied().collect::<Vec<_>>()
                })
            {
                // process the message for each individual channel on the group
                for channel_id in channel_ids {
                    self.extended_channels
                        .with_mut(&channel_id, |channel| {
                            channel.set_target(Target::from_le_bytes(m.maximum_target.to_array()));
                        })
                        .ok_or(TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?;

                    let mut message = m_static.clone();
                    message.channel_id = channel_id;
                    set_target_messages.push(message);
                }
            // if the message was not sent to a group channel, and we're not in aggregated
            // mode, we need to process the message for a specific channel
            } else {
                let Some(()) = self.extended_channels.with_mut(&m.channel_id, |channel| {
                    channel.set_target(Target::from_le_bytes(m.maximum_target.to_array()));
                }) else {
                    // we got a nonsense channel id, we should log an error and ignore the
                    // message
                    warn!(
                        "Channel not found: {}, ignoring SetTarget message",
                        m_static.channel_id
                    );
                    return Err(TproxyError::log(TproxyErrorKind::ChannelNotFound));
                };

                set_target_messages.push(m_static.clone());
            }

            Ok::<Vec<SetTargetOwned>, Self::Error>(set_target_messages)
        }?;

        // now we need to send the SetTarget message(s) to the SV1Server
        for message in set_target_messages_sv1_server {
            self.channel_manager_io
                .sv1_server_sender
                .send(MiningOwned::SetTarget(message))
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
        m: SetGroupChannelOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", m);

        if m.group_channel_id == AGGREGATED_CHANNEL_ID {
            error!(
                group_channel_id = m.group_channel_id,
                "Rejecting SetGroupChannel that uses tProxy's reserved broadcast identifier"
            );
            return Err(TproxyError::fallback(
                TproxyErrorKind::ChannelIdAlreadyInUse(m.group_channel_id),
            ));
        }

        let new_channel_ids = m.channel_ids.clone().into_inner();
        let aggregated_channel = if self.mode.is_aggregated() {
            Some(
                self.extended_channels
                    .with(&AGGREGATED_CHANNEL_ID, |channel| {
                        (channel.get_channel_id(), channel.get_full_extranonce_size())
                    })
                    .ok_or_else(|| TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?,
            )
        } else {
            None
        };

        // Validate the complete replacement before changing any existing group. In aggregated
        // mode, SetGroupChannel carries the real upstream channel ID, while the corresponding
        // local channel state is stored under AGGREGATED_CHANNEL_ID.
        if let Some((aggregated_channel_id, _)) = aggregated_channel {
            if m.group_channel_id == aggregated_channel_id {
                return Err(TproxyError::fallback(
                    TproxyErrorKind::ChannelIdAlreadyInUse(aggregated_channel_id),
                ));
            }
        } else if self.extended_channels.contains_key(&m.group_channel_id) {
            error!(
                group_channel_id = m.group_channel_id,
                "Rejecting SetGroupChannel that reinterprets an extended channel ID"
            );
            return Err(TproxyError::fallback(
                TproxyErrorKind::ChannelIdAlreadyInUse(m.group_channel_id),
            ));
        }

        let mut replacement_group = GroupChannel::new(m.group_channel_id);
        for channel_id in &new_channel_ids {
            if *channel_id == AGGREGATED_CHANNEL_ID {
                return Err(TproxyError::fallback(
                    TproxyErrorKind::ChannelIdAlreadyInUse(AGGREGATED_CHANNEL_ID),
                ));
            }

            let full_extranonce_size = match aggregated_channel {
                Some((aggregated_channel_id, full_extranonce_size))
                    if *channel_id == aggregated_channel_id =>
                {
                    full_extranonce_size
                }
                Some(_) => {
                    return Err(TproxyError::fallback(TproxyErrorKind::ChannelNotFound));
                }
                None => self
                    .extended_channels
                    .with(channel_id, |channel| channel.get_full_extranonce_size())
                    .ok_or_else(|| TproxyError::fallback(TproxyErrorKind::ChannelNotFound))?,
            };
            replacement_group
                .add_channel_id(*channel_id, full_extranonce_size)
                .map_err(|error| {
                    error!("Failed to add channel id to group channel: {error:?}");
                    TproxyError::fallback(TproxyErrorKind::FailedToAddChannelIdToGroupChannel(
                        error,
                    ))
                })?;
        }

        // Validation above makes this mutation phase infallible. Move the listed channels out of
        // their previous groups, remove the old definition of the target group, then install the
        // validated replacement.
        let mut groups_to_remove = Vec::new();
        self.group_channels.for_each_mut(|group_id, group| {
            for channel_id in &new_channel_ids {
                group.remove_channel_id(*channel_id);
            }
            if group.is_empty() {
                groups_to_remove.push(group_id);
            }
        });
        for group_id in groups_to_remove {
            self.group_channels.remove(&group_id);
        }
        self.group_channels.remove(&m.group_channel_id);
        if !new_channel_ids.is_empty() {
            self.group_channels
                .insert(m.group_channel_id, replacement_group);
        }

        Ok(())
    }
}

impl ChannelManager {
    #[allow(clippy::result_large_err)]
    fn verify_payout_distribution(
        &self,
        job: &NewExtendedMiningJobOwned,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TproxyMode, error::Action};
    use async_channel::{Receiver, unbounded};

    fn channel_manager_with_mode(mode: TproxyMode) -> (ChannelManager, Receiver<MiningOwned>) {
        let (upstream_sender, _upstream_receiver_for_test) = unbounded();
        let (_upstream_sender_for_test, upstream_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver_for_test) = unbounded();
        let (_sv1_server_sender_for_test, sv1_server_receiver) = unbounded();

        (
            ChannelManager::new(
                upstream_sender,
                upstream_receiver,
                sv1_server_sender,
                sv1_server_receiver,
                vec![],
                vec![],
                mode,
                #[cfg(feature = "monitoring")]
                true,
            ),
            sv1_server_receiver_for_test,
        )
    }

    fn channel_manager() -> (ChannelManager, Receiver<MiningOwned>) {
        channel_manager_with_mode(TproxyMode::NonAggregated)
    }

    fn open_success(
        request_id: u32,
        channel_id: u32,
        group_channel_id: u32,
        prefix_byte: u8,
        extranonce_size: u16,
    ) -> OpenExtendedMiningChannelSuccessOwned {
        OpenExtendedMiningChannelSuccessOwned {
            request_id,
            channel_id,
            target: [0xff; 32].into(),
            extranonce_size,
            extranonce_prefix: vec![prefix_byte; 4].try_into().unwrap(),
            group_channel_id,
        }
    }

    fn extended_channel(channel_id: u32) -> ExtendedChannel {
        ExtendedChannel::new(
            channel_id,
            format!("miner-{channel_id}"),
            ExtranoncePrefix::from_wire(vec![channel_id as u8; 4]).unwrap(),
            Target::from_le_bytes([0xff; 32]),
            1.0,
            true,
            4,
        )
    }

    fn assert_collision(error: TproxyError<error::ChannelManager>, channel_id: u32) {
        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::ChannelIdAlreadyInUse(id) if id == channel_id
        ));
    }

    #[tokio::test]
    async fn rejects_open_success_that_reuses_a_live_channel_id() {
        let (mut manager, sv1_server_receiver) = channel_manager();
        manager
            .pending_downstream_channels
            .insert(1, ("first-miner".to_string(), 1.0, 4));
        manager
            .handle_open_extended_mining_channel_success(
                None,
                open_success(1, 7, 100, 0xaa, 4),
                None,
            )
            .await
            .unwrap();
        sv1_server_receiver.recv().await.unwrap();

        manager
            .pending_downstream_channels
            .insert(2, ("second-miner".to_string(), 1.0, 6));
        let error = manager
            .handle_open_extended_mining_channel_success(
                None,
                open_success(2, 7, 200, 0xbb, 6),
                None,
            )
            .await
            .unwrap_err();

        assert_collision(error, 7);
        assert_eq!(
            manager
                .extended_channels
                .with(&7, |channel| channel.get_full_extranonce_size()),
            Some(8)
        );
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(7))
                .unwrap_or(false)
        );
        assert!(!manager.group_channels.contains_key(&200));
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_open_success_that_reinterprets_its_channel_as_a_group() {
        let (mut manager, sv1_server_receiver) = channel_manager();
        manager
            .pending_downstream_channels
            .insert(1, ("miner".to_string(), 1.0, 4));

        let error = manager
            .handle_open_extended_mining_channel_success(None, open_success(1, 7, 7, 0xaa, 4), None)
            .await
            .unwrap_err();

        assert_collision(error, 7);
        assert!(!manager.extended_channels.contains_key(&7));
        assert!(!manager.group_channels.contains_key(&7));
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_open_success_that_shadows_a_live_group() {
        let (mut manager, sv1_server_receiver) = channel_manager();
        manager.group_channels.insert(7, GroupChannel::new(7));
        manager
            .pending_downstream_channels
            .insert(1, ("miner".to_string(), 1.0, 4));

        let error = manager
            .handle_open_extended_mining_channel_success(
                None,
                open_success(1, 7, 100, 0xaa, 4),
                None,
            )
            .await
            .unwrap_err();

        assert_collision(error, 7);
        assert!(manager.group_channels.contains_key(&7));
        assert!(!manager.group_channels.contains_key(&100));
        assert!(!manager.extended_channels.contains_key(&7));
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_group_id_that_shadows_a_live_extended_channel() {
        let (mut manager, _sv1_server_receiver) = channel_manager();
        manager.extended_channels.insert(7, extended_channel(7));
        manager.extended_channels.insert(8, extended_channel(8));

        let error = manager
            .handle_set_group_channel(
                None,
                SetGroupChannelOwned {
                    group_channel_id: 7,
                    channel_ids: vec![8].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap_err();

        assert_collision(error, 7);
        assert!(!manager.group_channels.contains_key(&7));
        assert!(manager.extended_channels.contains_key(&7));
        assert!(manager.extended_channels.contains_key(&8));
    }

    #[tokio::test]
    async fn rejects_reserved_channel_id_in_aggregated_mode() {
        let (mut manager, sv1_server_receiver) = channel_manager_with_mode(TproxyMode::Aggregated);
        manager
            .pending_downstream_channels
            .insert(1, ("miner".to_string(), 1.0, 4));

        let error = manager
            .handle_open_extended_mining_channel_success(
                None,
                open_success(1, AGGREGATED_CHANNEL_ID, 100, 0xaa, 4),
                None,
            )
            .await
            .unwrap_err();

        assert_collision(error, AGGREGATED_CHANNEL_ID);
        assert!(manager.extended_channels.is_empty());
        assert!(manager.group_channels.is_empty());
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_reserved_group_id() {
        let (mut manager, sv1_server_receiver) = channel_manager();
        manager
            .pending_downstream_channels
            .insert(1, ("miner".to_string(), 1.0, 4));

        let error = manager
            .handle_open_extended_mining_channel_success(
                None,
                open_success(1, 7, AGGREGATED_CHANNEL_ID, 0xaa, 4),
                None,
            )
            .await
            .unwrap_err();

        assert_collision(error, AGGREGATED_CHANNEL_ID);
        assert!(manager.extended_channels.is_empty());
        assert!(manager.group_channels.is_empty());
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn invalid_set_group_channel_does_not_mutate_existing_groups() {
        let (mut manager, _sv1_server_receiver) = channel_manager();
        manager.extended_channels.insert(7, extended_channel(7));
        let mut original_group = GroupChannel::new(100);
        original_group.add_channel_id(7, 8).unwrap();
        manager.group_channels.insert(100, original_group);

        let error = manager
            .handle_set_group_channel(
                None,
                SetGroupChannelOwned {
                    group_channel_id: 200,
                    channel_ids: vec![7, 999].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelNotFound));
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(7))
                .unwrap_or(false)
        );
        assert!(!manager.group_channels.contains_key(&200));
    }

    #[tokio::test]
    async fn aggregated_set_group_channel_uses_the_wire_channel_id() {
        let (mut manager, _sv1_server_receiver) = channel_manager_with_mode(TproxyMode::Aggregated);
        manager
            .extended_channels
            .insert(AGGREGATED_CHANNEL_ID, extended_channel(7));
        let mut original_group = GroupChannel::new(100);
        original_group.add_channel_id(7, 8).unwrap();
        manager.group_channels.insert(100, original_group);

        manager
            .handle_set_group_channel(
                None,
                SetGroupChannelOwned {
                    group_channel_id: 200,
                    channel_ids: vec![7].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap();

        assert!(!manager.group_channels.contains_key(&100));
        assert!(
            manager
                .group_channels
                .with(&200, |group| group.has_channel_id(7))
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn invalid_aggregated_set_group_channel_preserves_existing_group() {
        let (mut manager, _sv1_server_receiver) = channel_manager_with_mode(TproxyMode::Aggregated);
        manager
            .extended_channels
            .insert(AGGREGATED_CHANNEL_ID, extended_channel(7));
        let mut original_group = GroupChannel::new(100);
        original_group.add_channel_id(7, 8).unwrap();
        manager.group_channels.insert(100, original_group);

        let error = manager
            .handle_set_group_channel(
                None,
                SetGroupChannelOwned {
                    group_channel_id: 200,
                    channel_ids: vec![8].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelNotFound));
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(7))
                .unwrap_or(false)
        );
        assert!(!manager.group_channels.contains_key(&200));
    }

    #[tokio::test]
    async fn aggregated_set_group_channel_rejects_reserved_group_id_without_mutation() {
        let (mut manager, _sv1_server_receiver) = channel_manager_with_mode(TproxyMode::Aggregated);
        manager
            .extended_channels
            .insert(AGGREGATED_CHANNEL_ID, extended_channel(7));
        let mut original_group = GroupChannel::new(100);
        original_group.add_channel_id(7, 8).unwrap();
        manager.group_channels.insert(100, original_group);

        let error = manager
            .handle_set_group_channel(
                None,
                SetGroupChannelOwned {
                    group_channel_id: AGGREGATED_CHANNEL_ID,
                    channel_ids: vec![7].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap_err();

        assert_collision(error, AGGREGATED_CHANNEL_ID);
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(7))
                .unwrap_or(false)
        );
        assert!(!manager.group_channels.contains_key(&AGGREGATED_CHANNEL_ID));
    }
}
