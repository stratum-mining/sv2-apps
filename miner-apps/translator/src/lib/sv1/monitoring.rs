//! SV1 client monitoring integration for Sv1Server
//!
//! This module implements the Sv1ClientsMonitoring trait on `Sv1Server`.
use std::{
    collections::HashSet,
    net::IpAddr,
    time::{Duration, Instant},
};

use stratum_apps::{
    monitoring::{
        DiscoveredMiner, MinerTelemetry, MinerTelemetryCollector, MinerTelemetryStatus,
        match_discovered_miners_to_downstreams_by_worker_and_port,
        sv1::{Sv1ClientInfo, Sv1ClientsMonitoring},
    },
    utils::types::DownstreamId,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::sv1::{Downstream, Sv1Server};

const MINER_TELEMETRY_DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);

/// Helper to convert a Downstream to Sv1ClientInfo
fn downstream_to_sv1_client_info(
    downstream: &Downstream,
    miner_telemetry: Option<MinerTelemetry>,
    management_ip: Option<IpAddr>,
    miner_telemetry_status: Option<MinerTelemetryStatus>,
) -> Option<Sv1ClientInfo> {
    downstream
        .downstream_data
        .with(|dd| Sv1ClientInfo {
            client_id: downstream.downstream_id,
            channel_id: dd.channel_id,
            connection_ip: dd.connection_ip,
            management_ip,
            sv1_username: dd.sv1_username.clone(),
            sv1_worker_name: dd.sv1_worker_name.clone(),
            target_hex: hex::encode(dd.target.to_be_bytes()),
            hashrate: dd.hashrate,
            miner_telemetry,
            miner_telemetry_status,
            stable_hashrate: dd.stable_hashrate,
            extranonce1_hex: hex::encode(&dd.extranonce1),
            extranonce2_len: dd.extranonce2_len,
            version_rolling_mask: dd
                .version_rolling_mask
                .as_ref()
                .map(|mask| format!("{:08x}", mask.0)),
            version_rolling_min_bit: dd
                .version_rolling_min_bit
                .as_ref()
                .map(|bit| format!("{:08x}", bit.0)),
        })
        .ok()
}

impl Sv1ClientsMonitoring for Sv1Server {
    fn get_sv1_clients(&self) -> Vec<Sv1ClientInfo> {
        let mut clients = Vec::new();
        self.downstreams.for_each(|downstream_id, downstream| {
            let miner_telemetry = self.miner_telemetry_for(downstream_id);
            let management_ip = self.miner_telemetry_management_ip_for(downstream_id);
            let miner_telemetry_status = self.miner_telemetry_status_for(downstream_id);
            if let Some(client) = downstream_to_sv1_client_info(
                downstream,
                miner_telemetry,
                management_ip,
                miner_telemetry_status,
            ) {
                clients.push(client);
            }
        });
        clients
    }

    fn get_sv1_client_by_id(&self, client_id: usize) -> Option<Sv1ClientInfo> {
        let miner_telemetry = self.miner_telemetry_for(client_id);
        let management_ip = self.miner_telemetry_management_ip_for(client_id);
        let miner_telemetry_status = self.miner_telemetry_status_for(client_id);
        self.downstreams
            .with(&client_id, |downstream| {
                downstream_to_sv1_client_info(
                    downstream,
                    miner_telemetry,
                    management_ip,
                    miner_telemetry_status,
                )
            })
            .flatten()
    }
}

impl Sv1Server {
    pub(crate) fn miner_telemetry_for(
        &self,
        downstream_id: DownstreamId,
    ) -> Option<MinerTelemetry> {
        self.miner_telemetry.telemetry_for(downstream_id)
    }

    pub(crate) fn miner_telemetry_management_ip_for(
        &self,
        downstream_id: DownstreamId,
    ) -> Option<IpAddr> {
        self.miner_telemetry.management_ip_for(downstream_id)
    }

    pub(crate) fn miner_telemetry_status_for(
        &self,
        downstream_id: DownstreamId,
    ) -> Option<MinerTelemetryStatus> {
        self.miner_telemetry.status_for(downstream_id)
    }

    pub(crate) async fn run_miner_telemetry_loop(
        &self,
        refresh_interval: Duration,
        cancellation_token: CancellationToken,
        fallback_token: CancellationToken,
    ) {
        let discovery_cidrs = self
            .config
            .miner_telemetry_cidrs()
            .iter()
            .map(|cidr| cidr.trim())
            .filter(|cidr| !cidr.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if discovery_cidrs.is_empty() {
            info!("SV1 miner telemetry discovery disabled: no miner_telemetry cidrs configured");
            return;
        }

        let refresh_interval = refresh_interval.max(Duration::from_secs(1));
        let collector = MinerTelemetryCollector::new();
        let mut interval = tokio::time::interval(refresh_interval);
        let mut discovered_miners = Vec::new();
        let mut last_discovery = None::<Instant>;

        info!(
            "Starting SV1 miner telemetry loop with interval of {} seconds and discovery interval of {} seconds",
            refresh_interval.as_secs(),
            MINER_TELEMETRY_DISCOVERY_INTERVAL.as_secs()
        );

        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    info!("SV1 miner telemetry loop received shutdown signal");
                    break;
                }
                _ = fallback_token.cancelled() => {
                    info!("SV1 miner telemetry loop received fallback signal");
                    break;
                }
                _ = interval.tick() => {
                    tokio::select! {
                        _ = cancellation_token.cancelled() => {
                            info!("SV1 miner telemetry loop received shutdown signal");
                            break;
                        }
                        _ = fallback_token.cancelled() => {
                            info!("SV1 miner telemetry loop received fallback signal");
                            break;
                        }
                        _ = async {
                            let should_discover = last_discovery
                                .map(|last| last.elapsed() >= MINER_TELEMETRY_DISCOVERY_INTERVAL)
                                .unwrap_or(true);

                            if should_discover {
                                discovered_miners = collector.discover(&discovery_cidrs).await;
                                debug!(
                                    "SV1 miner telemetry discovery found {} miner management interfaces",
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
        let downstreams = self.current_downstream_sv1_usernames();
        let active_downstream_ids = downstreams
            .iter()
            .map(|(downstream_id, _)| *downstream_id)
            .collect::<HashSet<_>>();
        debug!(
            ?downstreams,
            ?discovered_miners,
            pool_port = self.config.downstream_port,
            "SV1 miner telemetry matching discovered active pool users to SV1 usernames and downstream port"
        );
        let match_result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstreams,
            discovered_miners,
            self.config.downstream_port,
        );
        let matched_management_ips = match_result.management_ips_by_downstream_id;

        if matched_management_ips.is_empty()
            && !downstreams.is_empty()
            && !discovered_miners.is_empty()
        {
            debug!(
                "SV1 miner telemetry discovered {} miner management interfaces but could not match them to {} active downstreams",
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
                    if self.downstreams.contains_key(&downstream_id) {
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

    fn current_downstream_sv1_usernames(&self) -> Vec<(DownstreamId, String)> {
        let mut usernames = Vec::new();
        self.downstreams.for_each(|downstream_id, downstream| {
            if let Ok(worker_name) = downstream
                .downstream_data
                .with(|data| data.sv1_username.clone())
            {
                usernames.push((downstream_id, worker_name));
            }
        });
        usernames
    }
}
