//! Monitoring integration for JD Client
//!
//! This module implements the ServerMonitoring and Sv2ClientsMonitoring traits on `ChannelManager`.
//! JDC has:
//! - Server channels (upstream to pool)
//! - Client channels (downstream miners connecting to JDC)

use hex;
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    monitoring::{
        DiscoveredMiner, MinerTelemetry, MinerTelemetryCollector, MinerTelemetryStatus,
        client::{
            ExtendedChannelInfo, StandardChannelInfo, Sv2ClientInfo, Sv2ClientKind,
            Sv2ClientsMonitoring,
        },
        match_discovered_miners_to_downstreams_by_worker_and_port,
        server::{ServerExtendedChannelInfo, ServerInfo, ServerMonitoring},
    },
    utils::types::DownstreamId,
};

use crate::{channel_manager::ChannelManager, downstream::Downstream};
use std::{
    collections::HashSet,
    net::IpAddr,
    time::{Duration, Instant},
};
use tracing::{debug, info};

const MINER_TELEMETRY_DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);

impl ServerMonitoring for ChannelManager {
    fn get_server(&self) -> ServerInfo {
        self.upstream_channel
            .with(|upstream_channel| {
                let mut extended_channels = Vec::new();
                let standard_channels = Vec::new(); // JDC only uses extended channels

                if let Some(upstream_channel) = upstream_channel.as_ref() {
                    let channel_id = upstream_channel.get_channel_id();
                    let target = upstream_channel.get_target();
                    let extranonce_prefix = upstream_channel.get_extranonce_prefix();
                    let user_identity = upstream_channel.get_user_identity();
                    let share_accounting = upstream_channel.get_share_accounting();
                    let shares_rejected_by_reason = share_accounting
                        .get_rejected_shares()
                        .map(|(reason, count)| (reason.to_string(), count))
                        .collect();
                    let shares_rejected = share_accounting.get_rejected_shares_count();

                    extended_channels.push(ServerExtendedChannelInfo {
                        channel_id,
                        user_identity: user_identity.to_string(),
                        nominal_hashrate: Some(upstream_channel.get_nominal_hashrate()),
                        target_hex: hex::encode(target.to_be_bytes()),
                        extranonce_prefix_hex: hex::encode(extranonce_prefix),
                        full_extranonce_size: upstream_channel.get_full_extranonce_size(),
                        rollable_extranonce_size: upstream_channel.get_rollable_extranonce_size(),
                        version_rolling: upstream_channel.is_version_rolling(),
                        shares_acknowledged: share_accounting.get_acknowledged_shares(),
                        shares_submitted: share_accounting.get_validated_shares(),
                        shares_rejected,
                        shares_rejected_by_reason,
                        acknowledged_work_sum: share_accounting.get_acknowledged_work_sum(),
                        validated_work_sum: share_accounting.get_validated_work_sum(),
                        best_diff: share_accounting.get_best_diff(),
                        blocks_found: share_accounting.get_blocks_found(),
                    });
                }

                ServerInfo {
                    extended_channels,
                    standard_channels,
                }
            })
            .unwrap_or_else(|_| ServerInfo {
                extended_channels: Vec::new(),
                standard_channels: Vec::new(),
            })
    }
}

/// Helper to convert a Downstream to Sv2ClientInfo.
/// Returns None if the lock cannot be acquired (graceful degradation for monitoring).
fn downstream_to_sv2_client_info(
    client: &Downstream,
    miner_telemetry: Option<MinerTelemetry>,
    management_ip: Option<IpAddr>,
    miner_telemetry_status: Option<MinerTelemetryStatus>,
) -> Option<Sv2ClientInfo> {
    let mut extended_channels = Vec::new();
    let mut standard_channels = Vec::new();
    let client_kind = client.client_kind.get().ok()?;

    client
        .extended_channels
        .for_each(|_channel_id, extended_channel| {
            let channel_id = extended_channel.get_channel_id();
            let target = extended_channel.get_target();
            let requested_max_target = extended_channel.get_requested_max_target();
            let user_identity = extended_channel.get_user_identity();
            let share_accounting = extended_channel.get_share_accounting();

            extended_channels.push(ExtendedChannelInfo {
                channel_id,
                user_identity: user_identity.to_string(),
                nominal_hashrate: extended_channel.get_nominal_hashrate(),
                stable_hashrate: extended_channel.get_stable_hashrate(),
                target_hex: hex::encode(target.to_be_bytes()),
                requested_max_target_hex: hex::encode(requested_max_target.to_be_bytes()),
                extranonce_prefix_hex: hex::encode(extended_channel.get_extranonce_prefix()),
                full_extranonce_size: extended_channel.get_full_extranonce_size(),
                rollable_extranonce_size: extended_channel.get_rollable_extranonce_size(),
                expected_shares_per_minute: extended_channel.get_shares_per_minute(),
                shares_accepted: share_accounting.get_shares_accepted(),
                shares_rejected: share_accounting.get_rejected_shares_count(),
                shares_rejected_by_reason: share_accounting
                    .get_rejected_shares()
                    .map(|(reason, count)| (reason.to_string(), count))
                    .collect(),
                share_work_sum: share_accounting.get_share_work_sum(),
                last_share_sequence_number: share_accounting.get_last_share_sequence_number(),
                best_diff: share_accounting.get_best_diff(),
                last_batch_accepted: share_accounting.get_last_batch_accepted(),
                last_batch_work_sum: share_accounting.get_last_batch_work_sum(),
                share_batch_size: share_accounting.get_share_batch_size(),
                blocks_found: share_accounting.get_blocks_found(),
            });
        });

    client
        .standard_channels
        .for_each(|_channel_id, standard_channel| {
            let channel_id = standard_channel.get_channel_id();
            let target = standard_channel.get_target();
            let requested_max_target = standard_channel.get_requested_max_target();
            let user_identity = standard_channel.get_user_identity();
            let share_accounting = standard_channel.get_share_accounting();

            standard_channels.push(StandardChannelInfo {
                channel_id,
                user_identity: user_identity.to_string(),
                nominal_hashrate: standard_channel.get_nominal_hashrate(),
                stable_hashrate: standard_channel.get_stable_hashrate(),
                target_hex: hex::encode(target.to_be_bytes()),
                requested_max_target_hex: hex::encode(requested_max_target.to_be_bytes()),
                extranonce_prefix_hex: hex::encode(standard_channel.get_extranonce_prefix()),
                expected_shares_per_minute: standard_channel.get_shares_per_minute(),
                shares_accepted: share_accounting.get_shares_accepted(),
                shares_rejected: share_accounting.get_rejected_shares_count(),
                shares_rejected_by_reason: share_accounting
                    .get_rejected_shares()
                    .map(|(reason, count)| (reason.to_string(), count))
                    .collect(),
                share_work_sum: share_accounting.get_share_work_sum(),
                last_share_sequence_number: share_accounting.get_last_share_sequence_number(),
                best_diff: share_accounting.get_best_diff(),
                last_batch_accepted: share_accounting.get_last_batch_accepted(),
                last_batch_work_sum: share_accounting.get_last_batch_work_sum(),
                share_batch_size: share_accounting.get_share_batch_size(),
                blocks_found: share_accounting.get_blocks_found(),
            });
        });

    Some(Sv2ClientInfo {
        client_id: client.downstream_id,
        client_kind,
        extended_channels,
        standard_channels,
        management_ip,
        miner_telemetry,
        miner_telemetry_status,
    })
}

impl Sv2ClientsMonitoring for ChannelManager {
    fn get_sv2_clients(&self) -> Vec<Sv2ClientInfo> {
        let mut downstream_refs = Vec::new();
        self.downstream
            .for_each(|_, downstream| downstream_refs.push(downstream.clone()));

        downstream_refs
            .iter()
            .filter_map(|downstream| {
                downstream_to_sv2_client_info(
                    downstream,
                    self.miner_telemetry.telemetry_for(downstream.downstream_id),
                    self.miner_telemetry
                        .management_ip_for(downstream.downstream_id),
                    self.miner_telemetry.status_for(downstream.downstream_id),
                )
            })
            .collect()
    }

    fn get_sv2_client_by_id(&self, client_id: usize) -> Option<Sv2ClientInfo> {
        let miner_telemetry = self.miner_telemetry.telemetry_for(client_id);
        let management_ip = self.miner_telemetry.management_ip_for(client_id);
        let miner_telemetry_status = self.miner_telemetry.status_for(client_id);
        self.downstream
            .with(&client_id, |downstream| {
                downstream_to_sv2_client_info(
                    downstream,
                    miner_telemetry,
                    management_ip,
                    miner_telemetry_status,
                )
            })
            .unwrap_or(None)
    }
}

impl ChannelManager {
    pub(crate) async fn run_miner_telemetry_loop(
        &self,
        refresh_interval: Duration,
        cancellation_token: CancellationToken,
        fallback_token: CancellationToken,
    ) {
        let discovery_cidrs = self
            .miner_telemetry
            .cidrs
            .iter()
            .map(|cidr| cidr.trim())
            .filter(|cidr| !cidr.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if discovery_cidrs.is_empty() {
            info!("JDC miner telemetry discovery disabled: no miner_telemetry cidrs configured");
            return;
        }

        let refresh_interval = refresh_interval.max(Duration::from_secs(1));
        let collector = MinerTelemetryCollector::new();
        let mut interval = tokio::time::interval(refresh_interval);
        let mut discovered_miners = Vec::new();
        let mut last_discovery = None::<Instant>;

        info!(
            "Starting JDC miner telemetry loop with interval of {} seconds and discovery interval of {} seconds",
            refresh_interval.as_secs(),
            MINER_TELEMETRY_DISCOVERY_INTERVAL.as_secs()
        );

        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    info!("JDC miner telemetry loop received shutdown signal");
                    break;
                }
                _ = fallback_token.cancelled() => {
                    info!("JDC miner telemetry loop received fallback signal");
                    break;
                }
                _ = interval.tick() => {
                    tokio::select! {
                        _ = cancellation_token.cancelled() => {
                            info!("JDC miner telemetry loop received shutdown signal");
                            break;
                        }
                        _ = fallback_token.cancelled() => {
                            info!("JDC miner telemetry loop received fallback signal");
                            break;
                        }
                        _ = async {
                            let should_discover = last_discovery
                                .map(|last| last.elapsed() >= MINER_TELEMETRY_DISCOVERY_INTERVAL)
                                .unwrap_or(true);

                            if should_discover {
                                discovered_miners = collector.discover(&discovery_cidrs).await;
                                debug!(
                                    "JDC miner telemetry discovery found {} miner management interfaces",
                                    discovered_miners.len()
                                );
                                last_discovery = Some(Instant::now());
                            }

                            self.refresh_miner_telemetry(&collector, &discovered_miners).await;
                        } => {}
                    }
                }
            }
        }
    }

    async fn refresh_miner_telemetry(
        &self,
        collector: &MinerTelemetryCollector,
        discovered_miners: &[DiscoveredMiner],
    ) {
        let downstreams = self.current_downstream_user_identities();
        let active_downstream_ids = downstreams
            .iter()
            .map(|(downstream_id, _)| *downstream_id)
            .collect::<HashSet<_>>();
        debug!(
            ?downstreams,
            ?discovered_miners,
            pool_port = self.miner_telemetry.pool_port,
            "JDC miner telemetry matching discovered active pool users to SV2 channel user identities and listening port"
        );
        let match_result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstreams,
            discovered_miners,
            self.miner_telemetry.pool_port,
        );
        let matched_management_ips = match_result.management_ips_by_downstream_id;

        if matched_management_ips.is_empty()
            && !downstreams.is_empty()
            && !discovered_miners.is_empty()
        {
            debug!(
                "JDC miner telemetry discovered {} miner management interfaces but could not match them to {} active downstreams",
                discovered_miners.len(),
                downstreams.len()
            );
        }

        self.miner_telemetry.telemetry.retain(|downstream_id, _| {
            active_downstream_ids.contains(downstream_id)
                && matched_management_ips.contains_key(downstream_id)
        });
        self.miner_telemetry
            .management_ips
            .retain(|downstream_id, _| {
                active_downstream_ids.contains(downstream_id)
                    && matched_management_ips.contains_key(downstream_id)
            });
        self.miner_telemetry
            .statuses
            .retain(|downstream_id, _| active_downstream_ids.contains(downstream_id));

        for (downstream_id, status) in match_result.statuses_by_downstream_id {
            self.miner_telemetry.statuses.insert(downstream_id, status);
        }

        if downstreams.is_empty() {
            return;
        }

        for (downstream_id, ip) in matched_management_ips {
            self.miner_telemetry
                .management_ips
                .insert(downstream_id, ip);

            match collector.fetch(ip).await {
                Some(telemetry) => {
                    let is_active = self.downstream.contains_key(&downstream_id);

                    if is_active {
                        self.miner_telemetry
                            .telemetry
                            .insert(downstream_id, telemetry);
                        self.miner_telemetry
                            .statuses
                            .insert(downstream_id, MinerTelemetryStatus::Matched);
                    }
                }
                None => {
                    self.miner_telemetry.telemetry.remove(&downstream_id);
                    self.miner_telemetry
                        .statuses
                        .insert(downstream_id, MinerTelemetryStatus::FetchFailed);
                }
            }
        }
    }

    fn current_downstream_user_identities(&self) -> Vec<(DownstreamId, String)> {
        let mut downstream_workers = Vec::new();
        self.downstream.for_each(|_, downstream| {
            let Some(client_kind) = downstream.client_kind.get().ok() else {
                return;
            };
            if matches!(client_kind, Sv2ClientKind::TranslatorProxy) {
                return;
            }

            let mut workers = Vec::new();
            downstream
                .extended_channels
                .for_each(|_, channel| workers.push(channel.get_user_identity().to_string()));
            downstream
                .standard_channels
                .for_each(|_, channel| workers.push(channel.get_user_identity().to_string()));
            workers.sort();
            workers.dedup();

            let worker = if workers.len() == 1 {
                workers.remove(0)
            } else {
                String::new()
            };

            downstream_workers.push((downstream.downstream_id, worker));
        });
        downstream_workers
    }
}
