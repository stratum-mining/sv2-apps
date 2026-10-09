use std::sync::Arc;
use stratum_apps::stratum_core::{mining_sv2::UpdateChannelOwned, parsers_sv2::MiningOwned};

use crate::{
    error::{self, TproxyError, TproxyErrorKind, TproxyResult},
    sv1::{
        Sv1Server,
        downstream::Sv1ServerEvent,
        sv1_server::{PendingTargetUpdate, sv1_difficulty},
    },
};

use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{Vardiff, target::hash_rate_to_target},
        mining_sv2::SetTargetOwned,
    },
    utils::types::{ChannelId, DownstreamId, Hashrate},
};
use tracing::{debug, error, info, trace, warn};

enum AggregatedSnapshot {
    /// Aggregate hashrate and minimum target of all downstreams with an open channel.
    Active {
        total_hashrate: Hashrate,
        min_target: Target,
    },
    /// No downstream has an open channel yet.
    NoOpenChannels,
    /// Open channels exist, but no exact target could be computed for any of them.
    NoValidTargets,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1Server {
    /// Spawns the variable difficulty adjustment loop.
    ///
    /// This method implements the SV1 server's variable difficulty logic for all downstreams.
    /// Every 60 seconds, this method updates the difficulty state for each downstream.
    pub(super) async fn spawn_vardiff_loop(self: Arc<Self>) -> TproxyResult<(), error::Sv1Server> {
        info!("Variable difficulty adjustment enabled - starting vardiff loop");

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            info!("Starting vardiff loop for downstreams");

            self.handle_vardiff_updates().await?;
        }
    }

    /// Handles variable difficulty adjustments for all connected downstreams.
    ///
    /// This method implements the core vardiff logic:
    /// 1. For each downstream, calculate if a target update is needed
    /// 2. Always send UpdateChannel to keep upstream informed. Its `max_target` is the new exact
    ///    target (the hardest one across downstreams in aggregated mode), which bounds every
    ///    SetTarget the upstream may send for the channel once it accepts the update.
    /// 3. Compare the new target with the upstream target to decide when to send set_difficulty.
    ///    Bitcoin targets are ordered inversely to difficulty: a larger target is easier.
    ///    - If `new_target >= upstream_target`, advertise it immediately. This preserves every
    ///      upstream-valid share; additional easier shares are revalidated against the actual
    ///      upstream channel target by the channel manager and filtered locally.
    ///    - If `new_target < upstream_target`, wait for SetTarget. Advertising a harder target
    ///      early would prevent the miner from submitting shares that the upstream would still
    ///      accept and reward.
    /// 4. Handle aggregated vs non-aggregated modes for UpdateChannel messages
    pub(super) async fn handle_vardiff_updates(&self) -> TproxyResult<(), error::Sv1Server> {
        let mut immediate_updates = Vec::new();
        let mut all_updates = Vec::new(); // All updates will generate UpdateChannel messages

        self.vardiff.try_for_each_mut(|downstream_id, vardiff_state| {
            debug!("Updating vardiff for downstream_id: {}", downstream_id);
            let (channel_id, hashrate, target, upstream_target) = match self
                .with_registered_downstream(downstream_id, |downstream| {
                    downstream
                        .downstream_data
                        .with(|data| {
                            // It's safe to unwrap hashrate because we know that
                            // the downstream has a hashrate (we are
                            // doing vardiff)
                            (
                                data.channel_id,
                                data.hashrate.unwrap(),
                                data.target,
                                data.upstream_target,
                            )
                        })
                        .map_err(TproxyError::shutdown)
                }) {
                Ok(snapshot) => snapshot,
                Err(e) if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

            let Some(channel_id) = channel_id else {
                // Upstream channel closure and downstream cleanup are asynchronous. A vardiff
                // snapshot may briefly retain the downstream after its channel was cleared.
                debug!(
                    "Skipping vardiff update for downstream_id {} without an active channel",
                    downstream_id
                );
                return Ok(());
            };
            let new_hashrate_opt =
                vardiff_state.try_vardiff(hashrate, &target, self.shares_per_minute);

            match new_hashrate_opt {
                Ok(Some(new_hashrate)) => {
                    // Calculate new target based on new hashrate. A failure here is
                    // specific to this downstream's hashrate, so skip its update
                    // instead of shutting down the whole proxy.
                    let new_target: Target = match hash_rate_to_target(
                        new_hashrate as f64,
                        self.shares_per_minute as f64,
                    ) {
                        Ok(target) => target,
                        Err(e) => {
                            error!(
                                "Failed to calculate target for downstream {downstream_id} hashrate {new_hashrate}: {e:?}; skipping vardiff update"
                            );
                            return Ok(());
                        }
                    };
                    // Record the newest estimate now, even if the difficulty derived from it
                    // waits for the upstream. The validation target and reported hashrate only
                    // change when that difficulty reaches the miner.
                    if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| {
                                data.vardiff_hashrate = Some(new_hashrate);
                                data.stable_hashrate = false;
                            })
                            .map_err(crate::error::TproxyError::shutdown)
                    }) {
                        if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                            return Ok(());
                        }
                        return Err(e);
                    }
                    // All updates will be sent as UpdateChannel messages
                    all_updates.push((downstream_id, channel_id, new_target, new_hashrate));
                    // Determine if we should send set_difficulty immediately or wait
                    match upstream_target {
                        Some(upstream_target) => {
                            if new_target >= upstream_target {
                                // A larger target is easier. Advertise it immediately so the miner
                                // continues submitting every upstream-valid share; the channel
                                // manager filters any additional shares that miss the actual
                                // upstream target.
                                trace!(
                                    "✅ Target comparison: new_target ({}) >= upstream_target ({}) for downstream {}, will send mining.set_difficulty immediately",
                                    new_target, upstream_target, downstream_id
                                );
                                immediate_updates.push((downstream_id, new_target, new_hashrate));
                                // This update supersedes any parked pending target; drop
                                // it so a later SetTarget cannot resurrect an obsolete
                                // difficulty.
                                self.pending_target_updates.remove(&downstream_id);
                            } else {
                                // A smaller target is harder. Delay it until SetTarget confirms the
                                // upstream no longer accepts the shares this target would suppress.
                                trace!(
                                    "⏳ Target comparison: new_target ({}) < upstream_target ({}) for downstream {}, will delay mining.set_difficulty until SetTarget",
                                    new_target, upstream_target, downstream_id
                                );
                                self.pending_target_updates.insert(
                                    downstream_id,
                                    PendingTargetUpdate {
                                        new_target,
                                        new_hashrate,
                                    },
                                );
                            }
                        }
                        None => {
                            // No upstream target set yet, send set_difficulty immediately as fallback
                            trace!(
                                "No upstream target set for downstream {}, will send mining.set_difficulty immediately",
                                downstream_id
                            );
                            immediate_updates.push((downstream_id, new_target, new_hashrate));
                            // Same as above: the immediate update supersedes any parked
                            // pending target.
                            self.pending_target_updates.remove(&downstream_id);
                        }
                    }
                }
                Ok(None) => {
                    if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| {
                                data.stable_hashrate = true;
                            })
                            .map_err(crate::error::TproxyError::shutdown)
                    }) {
                        if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                            return Ok(());
                        }
                        return Err(e);
                    }
                }
                Err(e) => {
                    // A vardiff failure for one downstream should not take down the
                    // proxy; skip this downstream's update.
                    error!("Failed to update vardiff for downstream {downstream_id}: {e:?}; skipping");
                }
            }
            Ok(())
        })?;

        // Send UpdateChannel messages for ALL updates (both immediate and delayed)
        if !all_updates.is_empty() {
            self.send_update_channel_messages(all_updates).await?;
        }

        // Process immediate set_difficulty updates (for new_target >= upstream_target)
        for (downstream_id, target, hashrate) in immediate_updates {
            // Send set_difficulty message immediately
            let set_difficulty_msg = match sv1_difficulty(target, Some(hashrate)) {
                Ok(message) => message,
                Err(e) => {
                    error!(
                        "Failed to build immediate mining.set_difficulty for downstream {downstream_id}: {e:?}; skipping"
                    );
                    continue;
                }
            };
            if let Some(sender) = self
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .get_cloned(&downstream_id)
            {
                if let Err(e) = sender
                    .send(Sv1ServerEvent::SetDifficulty(set_difficulty_msg))
                    .await
                {
                    warn!(
                        "Failed to send immediate mining.set_difficulty message to downstream {downstream_id}: {e:?}; skipping (likely disconnected)"
                    );
                    continue;
                }
                trace!(
                    "Sent immediate mining.set_difficulty to downstream {downstream_id} (new_target >= upstream_target)",
                );
            }
        }

        Ok(())
    }

    /// Sends UpdateChannel messages for all target updates.
    ///
    /// Always sends UpdateChannel to keep upstream informed about target changes.
    /// Handles both aggregated and non-aggregated modes:
    /// - Aggregated: Send single UpdateChannel with minimum target and sum of hashrates
    /// - Non-aggregated: Send individual UpdateChannel for each downstream
    async fn send_update_channel_messages(
        &self,
        all_updates: Vec<(DownstreamId, ChannelId, Target, Hashrate)>, /* (downstream_id,
                                                                        * channel_id,
                                                                        * new_target,
                                                                        * new_hashrate) */
    ) -> TproxyResult<(), error::Sv1Server> {
        if self.mode.is_aggregated() {
            // Aggregated mode: Send single UpdateChannel with minimum target and total hashrate of
            // ALL downstreams
            self.send_aggregated_update_channel(all_updates).await
        } else {
            // Non-aggregated mode: Send individual UpdateChannel for each downstream
            self.send_non_aggregated_update_channels(all_updates).await
        }
    }

    pub(super) async fn send_aggregated_update_channel(
        &self,
        all_updates: Vec<(DownstreamId, ChannelId, Target, Hashrate)>,
    ) -> TproxyResult<(), error::Sv1Server> {
        // Nothing to do if we received no updates
        let Some((_, channel_id, _, _)) = all_updates.first() else {
            return Ok(());
        };

        let (total_hashrate, min_target) = match self.aggregated_downstream_snapshot()? {
            AggregatedSnapshot::Active {
                total_hashrate,
                min_target,
            } => (total_hashrate, min_target),
            AggregatedSnapshot::NoOpenChannels => return Ok(()),
            AggregatedSnapshot::NoValidTargets => {
                warn!("Skipping aggregated UpdateChannel: no exact downstream target is available");
                return Ok(());
            }
        };

        let update_channel = UpdateChannelOwned {
            channel_id: *channel_id,
            nominal_hash_rate: total_hashrate,
            max_target: min_target.to_le_bytes().into(),
        };

        debug!(
            "Sending aggregated UpdateChannel: channel_id={}, total_hashrate={}, min_target={}, vardiff_updates={}",
            channel_id,
            total_hashrate,
            min_target,
            all_updates.len()
        );

        self.sv1_server_io
            .channel_manager_sender
            .send((MiningOwned::UpdateChannel(update_channel), None))
            .await
            .map_err(|e| {
                error!("Failed to send aggregated UpdateChannel: {:?}", e);
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })
    }

    async fn send_non_aggregated_update_channels(
        &self,
        all_updates: Vec<(DownstreamId, ChannelId, Target, Hashrate)>,
    ) -> TproxyResult<(), error::Sv1Server> {
        for (downstream_id, channel_id, new_target, new_hashrate) in all_updates {
            let update_channel = UpdateChannelOwned {
                channel_id,
                nominal_hash_rate: new_hashrate,
                max_target: new_target.to_le_bytes().into(),
            };

            debug!(
                "Sending UpdateChannel for downstream {}: channel_id={}, hashrate={}, target={}",
                downstream_id, channel_id, new_hashrate, new_target
            );

            self.sv1_server_io
                .channel_manager_sender
                .send((MiningOwned::UpdateChannel(update_channel), None))
                .await
                .map_err(|e| {
                    error!(
                        "Failed to send UpdateChannel for downstream {}: {:?}",
                        downstream_id, e
                    );
                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                })?;
        }
        Ok(())
    }

    /// Returns aggregate difficulty state for downstreams with an opened mining channel.
    #[allow(clippy::result_large_err)]
    fn aggregated_downstream_snapshot(&self) -> TproxyResult<AggregatedSnapshot, error::Sv1Server> {
        let mut total_hashrate: Hashrate = 0.0;
        let mut min_target: Option<Target> = None;
        let mut has_open_downstream = false;
        let shares_per_minute = self.shares_per_minute as f64;

        self.downstreams.try_for_each(|downstream_id, downstream| {
            let hashrate = downstream
                .downstream_data
                .with(|data| {
                    data.channel_id.map(|_| {
                        data.vardiff_hashrate.unwrap_or_else(|| {
                            data.hashrate
                                .expect("vardiff implies downstream must have a hashrate")
                        })
                    })
                })
                .map_err(TproxyError::shutdown)?;

            let Some(hashrate) = hashrate else {
                trace!(
                    "Excluding downstream {downstream_id} from aggregated UpdateChannel: channel is not open"
                );
                return Ok(());
            };
            has_open_downstream = true;

            // UpdateChannel is upstream-facing, so rebuild the exact target from
            // hashrate instead of reusing the rounded SV1 advertised target.
            // A failure is specific to this downstream's hashrate, so exclude it
            // from the aggregate instead of shutting down the whole proxy.
            let target = match hash_rate_to_target(hashrate as f64, shares_per_minute) {
                Ok(target) => target,
                Err(e) => {
                    error!(
                        "Failed to calculate exact target for downstream {downstream_id} hashrate {hashrate}: {e:?}; excluding from aggregated UpdateChannel"
                    );
                    return Ok(());
                }
            };

            total_hashrate += hashrate;
            min_target = Some(match min_target {
                Some(current) => current.min(target),
                None => target,
            });
            Ok::<(), TproxyError<error::Sv1Server>>(())
        })?;

        if !has_open_downstream {
            return Ok(AggregatedSnapshot::NoOpenChannels);
        }

        Ok(match min_target {
            Some(min_target) => AggregatedSnapshot::Active {
                total_hashrate,
                min_target,
            },
            None => AggregatedSnapshot::NoValidTargets,
        })
    }

    /// Handles SetTarget messages from the ChannelManager.
    ///
    /// Aggregated mode: Single SetTarget updates all downstreams and processes all pending updates
    /// Non-aggregated mode: Each SetTarget updates one specific downstream and processes its
    /// pending update
    ///
    /// A SetTarget that releases no pending update, including one the upstream sent on its own
    /// initiative, leaves the advertised difficulty unchanged. That never leaves a miner harder
    /// than the upstream: the channel manager has already checked the target against the
    /// requested `max_target`, and a miner's advertised difficulty is never harder than the
    /// `max_target` requested for it.
    pub(super) async fn handle_set_target_message(
        &self,
        set_target: SetTargetOwned,
    ) -> TproxyResult<(), error::Sv1Server> {
        let new_upstream_target = Target::from_le_bytes(set_target.target.to_array());
        debug!(
            "Received SetTarget for channel {}: new_upstream_target = {}",
            set_target.channel_id, new_upstream_target
        );

        if self.mode.is_aggregated() {
            return self
                .handle_aggregated_set_target(new_upstream_target, set_target.channel_id)
                .await;
        }

        self.handle_non_aggregated_set_target(set_target.channel_id, new_upstream_target)
            .await
    }

    /// Handles SetTarget in aggregated mode.
    /// Updates all downstreams and processes all pending set_difficulty messages.
    async fn handle_aggregated_set_target(
        &self,
        new_upstream_target: Target,
        channel_id: ChannelId,
    ) -> TproxyResult<(), error::Sv1Server> {
        debug!("Aggregated mode: Updating upstream target for all downstreams");

        self.downstreams.try_for_each(|_, downstream| {
            downstream
                .downstream_data
                .with(|d| {
                    d.set_upstream_target(new_upstream_target, downstream.downstream_id);
                })
                .map_err(TproxyError::shutdown)
        })?;

        // Process ALL pending difficulty updates that can now be sent downstream
        let applicable_updates =
            self.get_pending_difficulty_updates(new_upstream_target, None, channel_id);

        self.send_pending_set_difficulty_messages_to_downstream(applicable_updates)
            .await
    }

    /// Handles SetTarget in non-aggregated mode.
    /// Updates the specific downstream and processes its pending set_difficulty message.
    async fn handle_non_aggregated_set_target(
        &self,
        channel_id: ChannelId,
        new_upstream_target: Target,
    ) -> TproxyResult<(), error::Sv1Server> {
        debug!(
            "Non-aggregated mode: Processing SetTarget for channel {}",
            channel_id
        );

        let Some(downstream_id) = self
            .channel_id_to_downstream_id
            .with(&channel_id, |downstream_id| *downstream_id)
        else {
            warn!("No downstream found for channel {}", channel_id);
            return Ok(());
        };

        if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|d| {
                    d.set_upstream_target(new_upstream_target, downstream_id);
                })
                .map_err(TproxyError::shutdown)
        }) {
            if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                warn!("No downstream found for downstream_id {}", downstream_id);
                return Ok(());
            }
            return Err(e);
        }

        trace!("Updated upstream target for downstream {}", downstream_id);

        let applicable_updates = self.get_pending_difficulty_updates(
            new_upstream_target,
            Some(downstream_id),
            channel_id,
        );

        self.send_pending_set_difficulty_messages_to_downstream(applicable_updates)
            .await
    }

    /// Gets pending updates that can now be applied based on the new upstream target.
    /// If downstream_id is provided, only returns updates for that specific downstream.
    /// Updates not yet satisfied by the new upstream target stay pending: the SetTarget
    /// may answer an older UpdateChannel, with the reply to the latest request still in
    /// flight, so dropping them would lose the newest difficulty for the downstream.
    fn get_pending_difficulty_updates(
        &self,
        new_upstream_target: Target,
        downstream_id: Option<DownstreamId>,
        channel_id: ChannelId,
    ) -> Vec<(DownstreamId, PendingTargetUpdate)> {
        let mut applicable_updates = Vec::new();

        self.pending_target_updates
            .retain(|pending_downstream_id, pending_update| {
                // Check if we should process this update
                let should_process = match downstream_id {
                    Some(downstream_id) => *pending_downstream_id == downstream_id,
                    None => true, // Process all in aggregated mode
                };

                if !should_process {
                    return true; // keep pending (not relevant for this SetTarget)
                }

                // It is safe to advertise the pending target once it is at least as easy as the
                // new upstream target. The miner will then submit every upstream-valid share;
                // anything easier is filtered locally by channel-manager validation.
                if pending_update.new_target >= new_upstream_target {
                    applicable_updates.push((*pending_downstream_id, *pending_update));
                    false // remove from pending map
                } else {
                    warn!(
                        "SetTarget target ({}) on channel {} does not yet satisfy pending target ({}) for downstream {}; keeping update pending",
                        new_upstream_target, channel_id, pending_update.new_target, pending_downstream_id
                    );
                    true // keep pending until a satisfying SetTarget arrives
                }
            });
        applicable_updates
    }

    /// Sends set_difficulty messages for all applicable pending updates.
    async fn send_pending_set_difficulty_messages_to_downstream(
        &self,
        difficulty_updates: Vec<(DownstreamId, PendingTargetUpdate)>,
    ) -> TproxyResult<(), error::Sv1Server> {
        for (
            downstream_id,
            PendingTargetUpdate {
                new_target,
                new_hashrate,
            },
        ) in difficulty_updates
        {
            let set_difficulty_msg = match sv1_difficulty(new_target, Some(new_hashrate)) {
                Ok(message) => message,
                Err(e) => {
                    error!(
                        "Failed to build mining.set_difficulty for downstream {downstream_id}: {e:?}; skipping"
                    );
                    continue;
                }
            };

            if let Some(sender) = self
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .get_cloned(&downstream_id)
            {
                if let Err(e) = sender
                    .send(Sv1ServerEvent::SetDifficulty(set_difficulty_msg))
                    .await
                {
                    warn!(
                        "Failed to send mining.set_difficulty to downstream {}: {:?}; skipping (likely disconnected)",
                        downstream_id, e
                    );
                    continue;
                }
                trace!("Sent SetDifficulty to downstream {}", downstream_id);
            }
        }
        Ok(())
    }

    /// Sends an UpdateChannel message for aggregated mode when a downstream channel opens or
    /// closes. Calculates total hashrate and minimum target among all active downstreams.
    pub async fn send_update_channel_on_downstream_state_change(
        &self,
    ) -> TproxyResult<(), error::Sv1Server> {
        if self.mode.is_non_aggregated() {
            return Ok(());
        }

        let update = match self.aggregated_downstream_snapshot()? {
            AggregatedSnapshot::Active {
                total_hashrate,
                min_target,
            } => UpdateChannelOwned {
                channel_id: 0, // ChannelManager will rewrite to upstream extended channel id
                nominal_hash_rate: total_hashrate,
                max_target: min_target.to_le_bytes().into(),
            },

            // Connected-but-unopened miners deliberately count as zero hashrate upstream.
            AggregatedSnapshot::NoOpenChannels => UpdateChannelOwned {
                channel_id: 0,
                nominal_hash_rate: 0.0,
                max_target: [0xFF; 32].into(),
            },
            AggregatedSnapshot::NoValidTargets => {
                warn!(
                    "Skipping aggregated UpdateChannel after downstream state change: no exact downstream target is available"
                );
                return Ok(());
            }
        };

        self.sv1_server_io
            .channel_manager_sender
            .send((MiningOwned::UpdateChannel(update), None))
            .await
            .map_err(|e| {
                error!(
                    "Failed to send UpdateChannel after downstream state change: {:?}",
                    e
                );
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })
    }
}
