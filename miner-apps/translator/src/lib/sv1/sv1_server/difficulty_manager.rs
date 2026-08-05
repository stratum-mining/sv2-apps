use std::sync::Arc;
use stratum_apps::stratum_core::{mining_sv2::UpdateChannelOwned, parsers_sv2::MiningOwned};

use crate::{
    error::{self, TproxyError, TproxyErrorKind, TproxyResult},
    sv1::{
        Sv1Server,
        sv1_server::{PendingTargetUpdate, SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING},
    },
};

use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{Vardiff, target::hash_rate_to_target},
        mining_sv2::SetTargetOwned,
        stratum_translation::sv2_to_sv1::{
            build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding,
            sv1_advertised_target_from_sv2_target,
        },
    },
    utils::types::{ChannelId, DownstreamId, Hashrate},
};
use tracing::{debug, error, info, trace, warn};

enum AggregatedSnapshot {
    Active {
        total_hashrate: Hashrate,
        min_target: Target,
    },
    NoDownstreams,
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
    /// 2. Always send UpdateChannel to keep upstream informed
    /// 3. Compare new target with upstream target to decide when to send set_difficulty:
    ///    - If new_target >= upstream_target: send set_difficulty immediately
    ///    - If new_target < upstream_target: wait for SetTarget response before sending
    ///      set_difficulty
    /// 4. Handle aggregated vs non-aggregated modes for UpdateChannel messages
    async fn handle_vardiff_updates(&self) -> TproxyResult<(), error::Sv1Server> {
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
                error!("Channel id is none for downstream_id: {}", downstream_id);
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
                    // Always update the downstream's pending target and hashrate
                    if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| {
                                // Store the advertised (pow2 rounded) target so share
                                // validation matches the difficulty the miner was sent.
                                data.set_pending_target(
                                    sv1_advertised_target_from_sv2_target(
                                        new_target,
                                        SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                                    )
                                    .unwrap_or(new_target),
                                    downstream.downstream_id,
                                );
                                data.set_pending_hashrate(
                                    Some(new_hashrate),
                                    downstream.downstream_id,
                                );
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
                                // Case 1: new_target >= upstream_target, send set_difficulty
                                // immediately
                                trace!(
                                    "✅ Target comparison: new_target ({}) >= upstream_target ({}) for downstream {}, will send mining.set_difficulty immediately",
                                    new_target, upstream_target, downstream_id
                                );
                                immediate_updates.push((
                                    channel_id,
                                    Some(downstream_id),
                                    new_target,
                                ));
                            } else {
                                // Case 2: new_target < upstream_target, delay set_difficulty until
                                // SetTarget
                                trace!(
                                    "⏳ Target comparison: new_target ({}) < upstream_target ({}) for downstream {}, will delay mining.set_difficulty until SetTarget",
                                    new_target, upstream_target, downstream_id
                                );
                                self.pending_target_updates
                                    .with(|data| {
                                        data.push(PendingTargetUpdate {
                                            downstream_id,
                                            new_target,
                                        })
                                    })
                                    .map_err(TproxyError::shutdown)?;
                            }
                        }
                        None => {
                            // No upstream target set yet, send set_difficulty immediately as fallback
                            trace!(
                                "No upstream target set for downstream {}, will send mining.set_difficulty immediately",
                                downstream_id
                            );
                            immediate_updates.push((channel_id, Some(downstream_id), new_target));
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
        for (_channel_id, downstream_id, target) in immediate_updates {
            let downstream_id = downstream_id.unwrap_or(0);
            // Send set_difficulty message immediately
            let set_difficulty_msg =
                match build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding(
                    target,
                    SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                ) {
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
                if let Err(e) = sender.send(set_difficulty_msg).await {
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

    async fn send_aggregated_update_channel(
        &self,
        all_updates: Vec<(DownstreamId, ChannelId, Target, Hashrate)>,
    ) -> TproxyResult<(), error::Sv1Server> {
        // Nothing to do if we received no updates
        let Some((_, channel_id, _, _)) = all_updates.first() else {
            return Ok(());
        };

        if self.downstreams.is_empty() {
            return Ok(());
        }

        let mut min_target: Option<Target> = None;
        let mut total_hashrate: Hashrate = 0.0;
        let shares_per_minute = self.shares_per_minute as f64;

        self.downstreams.try_for_each(|downstream_id, downstream| {
            let hashrate = downstream.downstream_data.with(|d| {
                d.pending_hashrate
                    .unwrap_or_else(|| d.hashrate.expect("vardiff implies hashrate"))
            }).map_err(TproxyError::shutdown)?;

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

            min_target = Some(match min_target {
                Some(current) => current.min(target),
                None => target,
            });

            total_hashrate += hashrate;
            Ok::<(), TproxyError<error::Sv1Server>>(())
        })?;

        let Some(min_target) = min_target else {
            warn!("Skipping aggregated UpdateChannel: no exact downstream target is available");
            return Ok(());
        };
        let downstream_count = self.downstreams.len();

        let update_channel = UpdateChannelOwned {
            channel_id: *channel_id,
            nominal_hash_rate: total_hashrate,
            maximum_target: min_target.to_le_bytes().into(),
        };

        debug!(
            "Sending aggregated UpdateChannel: channel_id={}, total_hashrate={}, min_target={}, downstreams={}, vardiff_updates={}",
            channel_id,
            total_hashrate,
            min_target,
            downstream_count,
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
                maximum_target: new_target.to_le_bytes().into(),
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

    /// Handles SetTarget messages from the ChannelManager.
    ///
    /// Aggregated mode: Single SetTarget updates all downstreams and processes all pending updates
    /// Non-aggregated mode: Each SetTarget updates one specific downstream and processes its
    /// pending update
    pub(super) async fn handle_set_target_message(
        &self,
        set_target: SetTargetOwned,
    ) -> TproxyResult<(), error::Sv1Server> {
        let new_upstream_target = Target::from_le_bytes(set_target.maximum_target.to_array());
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
            self.get_pending_difficulty_updates(new_upstream_target, None, channel_id)?;

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
        )?;

        self.send_pending_set_difficulty_messages_to_downstream(applicable_updates)
            .await
    }

    /// Gets pending updates that can now be applied based on the new upstream target.
    /// If downstream_id is provided, only returns updates for that specific downstream.
    /// Logs a warning if the upstream target is higher than any requested target.
    #[allow(clippy::result_large_err)]
    fn get_pending_difficulty_updates(
        &self,
        new_upstream_target: Target,
        downstream_id: Option<DownstreamId>,
        channel_id: ChannelId,
    ) -> TproxyResult<Vec<PendingTargetUpdate>, error::Sv1Server> {
        let mut applicable_updates = Vec::new();

        self.pending_target_updates.with(|data| {
            data.retain(|pending_update| {
                // Check if we should process this update
                let should_process = match downstream_id {
                    Some(downstream_id) => pending_update.downstream_id == downstream_id,
                    None => true, // Process all in aggregated mode
                };

                if !should_process {
                    return true; // keep in pending list (not relevant for this SetTarget)
                }

                if pending_update.new_target >= new_upstream_target {
                    // Target is acceptable, can apply immediately
                    applicable_updates.push(pending_update.clone());
                    false // remove from pending list
                } else {
                    // WARNING: Upstream gave us a target higher than what we requested
                    error!(
                        "❌ Protocol issue: SetTarget response has target ({}) which is higher than requested target ({}) in UpdateChannel for channel {}. Ignoring this pending update for downstream {}.",
                        new_upstream_target, pending_update.new_target, channel_id, pending_update.downstream_id
                    );
                    false // remove from pending list (don't keep invalid requests)
                }
            });
        }).map_err(TproxyError::shutdown)?;
        Ok(applicable_updates)
    }

    /// Sends set_difficulty messages for all applicable pending updates.
    async fn send_pending_set_difficulty_messages_to_downstream(
        &self,
        difficulty_updates: Vec<PendingTargetUpdate>,
    ) -> TproxyResult<(), error::Sv1Server> {
        for update in difficulty_updates {
            let set_difficulty_msg =
                match build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding(
                    update.new_target,
                    SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                ) {
                    Ok(message) => message,
                    Err(e) => {
                        error!(
                            "Failed to build mining.set_difficulty for downstream {}: {e:?}; skipping",
                            update.downstream_id
                        );
                        continue;
                    }
                };

            if let Some(sender) = self
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .get_cloned(&update.downstream_id)
            {
                if let Err(e) = sender.send(set_difficulty_msg).await {
                    warn!(
                        "Failed to send mining.set_difficulty to downstream {}: {:?}; skipping (likely disconnected)",
                        update.downstream_id, e
                    );
                    continue;
                }
                trace!("Sent SetDifficulty to downstream {}", update.downstream_id);
            }
        }
        Ok(())
    }

    /// Sends an UpdateChannel message for aggregated mode when downstream state changes
    /// (e.g., disconnect). Calculates total hashrate and minimum target among all remaining
    /// downstreams.
    pub async fn send_update_channel_on_downstream_state_change(
        &self,
    ) -> TproxyResult<(), error::Sv1Server> {
        if self.mode.is_non_aggregated() {
            return Ok(());
        }

        let is_empty = self.downstreams.is_empty();

        let snapshot = if is_empty {
            AggregatedSnapshot::NoDownstreams
        } else {
            let mut total_hashrate: Hashrate = 0.0;
            let mut min_target: Option<Target> = None;
            let shares_per_minute = self.shares_per_minute as f64;

            self.downstreams.try_for_each(|downstream_id, downstream| {
                let hashrate = downstream.downstream_data.with(|d| {
                    d.pending_hashrate.unwrap_or_else(|| {
                        d.hashrate
                            .expect("vardiff implies downstream must have a hashrate")
                    })
                }).map_err(TproxyError::shutdown)?;

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

            let Some(min_target) = min_target else {
                warn!(
                    "Skipping aggregated UpdateChannel after downstream state change: no exact downstream target is available"
                );
                return Ok(());
            };

            AggregatedSnapshot::Active {
                total_hashrate,
                min_target,
            }
        };

        let update = match snapshot {
            AggregatedSnapshot::Active {
                total_hashrate,
                min_target,
            } => UpdateChannelOwned {
                channel_id: 0, // ChannelManager will rewrite to upstream extended channel id
                nominal_hash_rate: total_hashrate,
                maximum_target: min_target.to_le_bytes().into(),
            },

            AggregatedSnapshot::NoDownstreams => UpdateChannelOwned {
                channel_id: 0,
                nominal_hash_rate: 0.0,
                maximum_target: [0xFF; 32].into(),
            },
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
