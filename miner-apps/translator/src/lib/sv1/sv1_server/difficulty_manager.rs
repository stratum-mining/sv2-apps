use std::sync::Arc;
use stratum_apps::stratum_core::{mining_sv2::UpdateChannelOwned, parsers_sv2::MiningOwned};

use crate::{
    error::{self, Action, TproxyError, TproxyErrorKind, TproxyResult},
    sv1::{
        Sv1Server,
        downstream::{DownstreamData, Sv1ServerEvent},
        sv1_server::sv1_difficulty,
    },
};

use stratum_apps::{
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{
            Vardiff,
            target::{hash_rate_from_target, hash_rate_to_target},
        },
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
        info!("Variable difficulty adjustment enabled, starting vardiff loop");

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            debug!("Starting vardiff loop for downstreams");

            self.handle_vardiff_updates().await?;
        }
    }

    /// Handles variable difficulty adjustments for all connected downstreams.
    ///
    /// For each downstream whose hashrate estimate changed, records the new estimate and the
    /// target vardiff wants for it, sends UpdateChannel to keep the upstream informed (one per
    /// downstream, or a single one for the aggregated channel), and updates the difficulty
    /// advertised to the miner, see [`Self::update_difficulty`].
    pub(super) async fn handle_vardiff_updates(&self) -> TproxyResult<(), error::Sv1Server> {
        let mut updates = Vec::new();

        self.vardiff.try_for_each_mut(|downstream_id, vardiff_state| {
            debug!("Updating vardiff for downstream_id: {}", downstream_id);
            let (channel_id, hashrate, target) = match self
                .with_registered_downstream(downstream_id, |downstream| {
                    downstream
                        .downstream_data
                        .with(|data| {
                            // It's safe to unwrap hashrate because we know that
                            // the downstream has a hashrate (we are
                            // doing vardiff)
                            (data.channel_id, data.hashrate.unwrap(), data.target)
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
                            warn!(
                                downstream_id,
                                hashrate = new_hashrate,
                                error = ?e,
                                "Failed to calculate target from hashrate (skipping vardiff update)"
                            );
                            return Ok(());
                        }
                    };
                    if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| {
                                data.hashrate = Some(new_hashrate);
                                data.desired_target = Some(new_target);
                                data.stable_hashrate = false;
                            })
                            .map_err(crate::error::TproxyError::shutdown)
                    }) {
                        if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                            return Ok(());
                        }
                        return Err(e);
                    }
                    updates.push((downstream_id, channel_id, new_target, new_hashrate));
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
                    warn!(
                        downstream_id,
                        error = ?e,
                        "Failed to update vardiff for downstream (skipping)"
                    );
                }
            }
            Ok(())
        })?;

        if updates.is_empty() {
            return Ok(());
        }
        let updated_downstreams: Vec<DownstreamId> = updates
            .iter()
            .map(|(downstream_id, ..)| *downstream_id)
            .collect();
        self.send_update_channel_messages(updates).await?;

        for downstream_id in updated_downstreams {
            if let Err(e) = self.update_difficulty(downstream_id) {
                // This loop only fails to shut down. A closed notification channel means the
                // downstream is disconnecting, and its own task cleans it up.
                if !matches!(e.action, Action::Disconnect(_)) {
                    return Err(e);
                }
                debug!(
                    downstream_id,
                    error = ?e.kind,
                    "Failed to send mining.set_difficulty to downstream (downstream disconnecting)"
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
                error!(error = ?e, "Failed to send aggregated UpdateChannel to channel manager");
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
                        downstream_id,
                        error = ?e,
                        "Failed to send UpdateChannel to channel manager"
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
                        data.hashrate
                            .expect("vardiff implies downstream must have a hashrate")
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
                    warn!(
                        downstream_id,
                        hashrate,
                        error = ?e,
                        "Failed to calculate exact target from hashrate (excluding from aggregated UpdateChannel)"
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
    /// Records the new upstream target for the downstreams working on the channel, every
    /// downstream with an open channel in aggregated mode or the channel's own downstream
    /// otherwise, and updates the difficulty advertised to each of them, see
    /// [`Self::update_difficulty`]. A downstream whose notification channel is closed does not
    /// keep the others from getting their new difficulty: the first one is reported once every
    /// downstream was updated.
    pub(super) async fn handle_set_target_message(
        &self,
        set_target: SetTargetOwned,
    ) -> TproxyResult<(), error::Sv1Server> {
        let upstream_target = Target::from_le_bytes(set_target.target.to_array());
        debug!(
            "Received SetTarget for channel {}: upstream_target = {}",
            set_target.channel_id, upstream_target
        );
        // With vardiff disabled the upstream sets the miners' difficulty, so the hashrate reported
        // for them is the one its target implies.
        let hashrate = if self.config.downstream_difficulty_config.enable_vardiff {
            None
        } else {
            self.hashrate_from_upstream_target(upstream_target)
        };
        let record = |data: &mut DownstreamData, downstream_id: DownstreamId| {
            data.set_upstream_target(upstream_target, downstream_id);
            if let Some(hashrate) = hashrate {
                data.hashrate = Some(hashrate);
            }
        };

        let mut downstream_ids = Vec::new();
        if self.mode.is_aggregated() {
            self.downstreams.try_for_each(|downstream_id, downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        if data.channel_id.is_some() {
                            record(data, downstream_id);
                            downstream_ids.push(downstream_id);
                        }
                    })
                    .map_err(TproxyError::shutdown)
            })?;
        } else {
            let Some(downstream_id) = self.channel_owner(set_target.channel_id) else {
                warn!(
                    channel_id = set_target.channel_id,
                    "No downstream found for channel on SetTarget"
                );
                return Ok(());
            };
            if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| record(data, downstream_id))
                    .map_err(TproxyError::shutdown)
            }) {
                if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                    warn!(
                        downstream_id,
                        "No downstream found for downstream ID on SetTarget"
                    );
                    return Ok(());
                }
                return Err(e);
            }
            downstream_ids.push(downstream_id);
        }

        let mut disconnected = None;
        for downstream_id in downstream_ids {
            if let Err(e) = self.update_difficulty(downstream_id) {
                if !matches!(e.action, Action::Disconnect(_)) {
                    return Err(e);
                }
                disconnected.get_or_insert(e);
            }
        }
        disconnected.map_or(Ok(()), Err)
    }

    /// Queues the difficulty a downstream should now be advertised, if it changed.
    ///
    /// The miner is advertised the easier of the target vardiff wants for it and the target of
    /// its upstream channel, or just the upstream target when vardiff is disabled. A larger
    /// target is easier, so the miner never works at a difficulty harder than the upstream
    /// channel's and never discards a share the upstream would credit, and it works at the
    /// difficulty vardiff wants as far as the upstream allows. Shares that miss the upstream
    /// target are not forwarded.
    ///
    /// Called whenever either target changes. Returns an error asking to disconnect the
    /// downstream if its notification channel is closed.
    #[allow(clippy::result_large_err)]
    pub(super) fn update_difficulty(
        &self,
        downstream_id: DownstreamId,
    ) -> TproxyResult<(), error::Sv1Server> {
        let Some(sender) = self
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&downstream_id)
        else {
            return Ok(());
        };
        let recorded = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    let (Some(_), Some(upstream_target)) = (data.channel_id, data.upstream_target)
                    else {
                        return None;
                    };
                    let target = data
                        .desired_target
                        .map_or(upstream_target, |desired| desired.max(upstream_target));
                    let difficulty = match sv1_difficulty(target) {
                        Ok(difficulty) => difficulty,
                        Err(e) => {
                            warn!(
                                downstream_id,
                                target = %target,
                                error = ?e,
                                "Failed to build mining.set_difficulty from target (keeping current difficulty)"
                            );
                            return None;
                        }
                    };
                    if data.advertised_target == Some(difficulty.target) {
                        return None;
                    }
                    trace!(
                        "Queueing mining.set_difficulty for downstream {downstream_id}: target {target}, upstream target {upstream_target}"
                    );
                    data.advertised_target = Some(difficulty.target);
                    Some(difficulty)
                })
                .map_err(TproxyError::shutdown)
        });
        let difficulty = match recorded {
            Ok(Some(difficulty)) => difficulty,
            Ok(None) => return Ok(()),
            Err(e) if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        // Difficulty updates run on the SV1 server's task, and nothing is awaited between recording
        // the target and queueing the difficulty, so the recorded target is always that of the
        // latest difficulty queued for the downstream. The channel is unbounded, so the send only
        // fails once it is closed.
        sender
            .try_send(Sv1ServerEvent::SetDifficulty(difficulty))
            .map_err(|_| {
                TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
            })
    }

    /// Returns the hashrate an upstream target implies at the configured share rate.
    pub(super) fn hashrate_from_upstream_target(&self, target: Target) -> Option<Hashrate> {
        match hash_rate_from_target(target.to_le_bytes().into(), self.shares_per_minute as f64) {
            Ok(hashrate) => Some(hashrate as Hashrate),
            Err(e) => {
                warn!(target = %target, error = ?e, "Failed to derive hashrate from upstream target");
                None
            }
        }
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
                    error = ?e,
                    "Failed to send UpdateChannel after downstream state change"
                );
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })
    }
}
