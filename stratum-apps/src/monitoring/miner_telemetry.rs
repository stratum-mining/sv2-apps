//! Miner telemetry monitoring types and helpers.

use crate::utils::types::DownstreamId;
use asic_rs::{
    MinerFactory,
    core::data::{collector::DataField, hashrate::HashRateUnit, miner::MinerData, pool::PoolData},
};
use futures::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};
use tokio::time::timeout;
use tracing::{debug, warn};
use utoipa::ToSchema;

const MINER_DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MINER_DISCOVERY_MAX_CONCURRENCY: usize = 64;
const MINER_DISCOVERY_MIN_IPV4_PREFIX: u8 = 24;

/// Telemetry reported by the miner's management interface.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MinerTelemetry {
    /// Miner manufacturer or brand reported by the management interface.
    pub make: Option<String>,
    /// Miner model reported by the management interface.
    pub model: Option<String>,
    /// Firmware version reported by the miner, when exposed.
    pub firmware_version: Option<String>,
    /// Miner-reported hashrate in hashes per second.
    pub reported_hashrate_hs: Option<f64>,
    /// Current miner power consumption in watts.
    pub power_consumption_w: Option<f64>,
    /// Miner efficiency in joules per terahash.
    pub efficiency_j_per_th: Option<f64>,
    /// Average miner temperature in degrees Celsius.
    pub average_temperature_c: Option<f64>,
    /// Total miner system uptime in seconds.
    pub uptime_secs: Option<u64>,
    /// Whether the miner reports that hashing is currently running.
    pub is_mining: Option<bool>,
}

/// Status of matching a connected miner to discovered management telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MinerTelemetryStatus {
    /// Telemetry was matched to this connected miner.
    Matched,
    /// No discovered miner management interface matched this connection.
    Unmatched,
    /// More than one connected miner used the same worker name, so telemetry was not assigned.
    DuplicateWorkerName,
    /// A matching miner was found, but fetching telemetry from its management interface failed.
    FetchFailed,
}

/// Matches between active downstream connections and discovered miner management interfaces.
#[derive(Debug, Clone, Default)]
pub struct MinerTelemetryDownstreamMatches {
    /// Matched miner management IP for each downstream connection id.
    pub management_ips_by_downstream_id: HashMap<DownstreamId, IpAddr>,
    /// Telemetry matching status for each active downstream connection id.
    pub statuses_by_downstream_id: HashMap<DownstreamId, MinerTelemetryStatus>,
}

impl From<MinerData> for MinerTelemetry {
    fn from(data: MinerData) -> Self {
        Self {
            make: Some(data.device_info.make),
            model: Some(data.device_info.model),
            firmware_version: data.firmware_version,
            reported_hashrate_hs: data
                .hashrate
                .map(|hashrate| hashrate.as_unit(HashRateUnit::Hash).value),
            power_consumption_w: data.wattage.map(|power| power.as_watts()),
            efficiency_j_per_th: data.efficiency,
            average_temperature_c: data
                .average_temperature
                .map(|temperature| temperature.as_celsius()),
            uptime_secs: data.uptime.map(|uptime| uptime.as_secs()),
            is_mining: Some(data.is_mining),
        }
    }
}

const MINER_TELEMETRY_EXCLUDED_FIELDS: &[DataField] = &[
    DataField::Mac,
    DataField::SerialNumber,
    DataField::Hostname,
    DataField::ApiVersion,
    DataField::ControlBoardVersion,
    DataField::Chips,
    DataField::ExpectedHashrate,
    DataField::Fans,
    DataField::PsuFans,
    DataField::FluidTemperature,
    DataField::TuningTarget,
    DataField::LightFlashing,
    DataField::Messages,
    DataField::Pools,
];

/// Collects miner telemetry and discovers miner management interfaces on configured LAN ranges.
pub struct MinerTelemetryCollector {
    factory: MinerFactory,
}

/// Miner management interface discovered on the LAN.
#[derive(Debug, Clone)]
pub struct DiscoveredMiner {
    /// Management IP address of the discovered miner.
    pub ip: IpAddr,
    /// Pools configured on the discovered miner.
    pub pools: Vec<DiscoveredMinerPool>,
}

/// Pool configuration reported by a discovered miner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredMinerPool {
    /// Worker name or user configured for the pool.
    pub user: String,
    /// Pool host configured on the miner.
    pub host: String,
    /// Pool port configured on the miner.
    pub port: u16,
}

#[derive(Debug, Clone, Copy)]
struct Ipv4Cidr {
    network: Ipv4Addr,
    prefix: u8,
}

impl MinerTelemetryCollector {
    pub fn new() -> Self {
        Self {
            factory: MinerFactory::new(),
        }
    }

    pub async fn fetch(&self, ip: IpAddr) -> Option<MinerTelemetry> {
        let miner = match self.factory.get_miner(ip).await {
            Ok(Some(miner)) => miner,
            Ok(None) => {
                debug!("No miner management interface found at {ip}");
                return None;
            }
            Err(error) => {
                debug!("Failed to get miner management interface at {ip}: {error}");
                return None;
            }
        };

        Some(MinerTelemetry::from(
            miner
                .get_data_filtered(MINER_TELEMETRY_EXCLUDED_FIELDS.to_vec())
                .await,
        ))
    }

    pub async fn discover(&self, cidrs: &[String]) -> Vec<DiscoveredMiner> {
        let ips = cidrs
            .iter()
            .filter_map(|cidr| parse_private_ipv4_cidr(cidr))
            .flat_map(|cidr| cidr.host_ips())
            .collect::<Vec<_>>();

        debug!(
            cidrs = ?cidrs,
            hosts = ips.len(),
            "Starting miner telemetry discovery scan"
        );

        let mut discovered = Vec::new();

        for chunk in ips.chunks(MINER_DISCOVERY_MAX_CONCURRENCY) {
            let mut probes = FuturesUnordered::new();
            for ip in chunk {
                probes.push(self.discover_ip(*ip));
            }

            while let Some(result) = probes.next().await {
                if let Some(miner) = result {
                    discovered.push(miner);
                }
            }
        }

        discovered
    }

    async fn discover_ip(&self, ip: IpAddr) -> Option<DiscoveredMiner> {
        let miner = match timeout(MINER_DISCOVERY_PROBE_TIMEOUT, self.factory.get_miner(ip)).await {
            Ok(Ok(Some(miner))) => miner,
            Ok(Ok(None)) => return None,
            Ok(Err(error)) => {
                debug!("Failed to discover miner management interface at {ip}: {error}");
                return None;
            }
            Err(_) => return None,
        };

        let pools = match timeout(MINER_DISCOVERY_PROBE_TIMEOUT, miner.get_pools()).await {
            Ok(pools) => pools,
            Err(_) => {
                debug!("Timed out reading pool users from miner management interface at {ip}");
                Vec::new()
            }
        };

        let mut miner_pools = pools
            .into_iter()
            .flat_map(|group| group.pools)
            .filter_map(discovered_miner_pool)
            .collect::<Vec<_>>();
        miner_pools.sort_unstable_by(|a, b| {
            a.user
                .cmp(&b.user)
                .then_with(|| a.host.cmp(&b.host))
                .then_with(|| a.port.cmp(&b.port))
        });
        miner_pools.dedup();

        debug!(
            pools = ?miner_pools,
            "Discovered miner management interface at {ip}"
        );

        Some(DiscoveredMiner {
            ip,
            pools: miner_pools,
        })
    }
}

fn discovered_miner_pool(pool: PoolData) -> Option<DiscoveredMinerPool> {
    if pool.active == Some(false) {
        return None;
    }

    let user = pool.user?.trim().to_owned();
    if user.is_empty() {
        return None;
    }

    let url = pool.url?;
    Some(DiscoveredMinerPool {
        user,
        host: url.host,
        port: url.port,
    })
}

pub fn match_discovered_miners_to_downstreams_by_worker_and_port(
    downstream_workers: &[(DownstreamId, String)],
    discovered_miners: &[DiscoveredMiner],
    expected_pool_port: u16,
) -> MinerTelemetryDownstreamMatches {
    let mut result = MinerTelemetryDownstreamMatches::default();
    let mut downstream_worker_counts = HashMap::new();

    for (_, worker_name) in downstream_workers {
        if !worker_name.is_empty() {
            *downstream_worker_counts
                .entry(worker_name.as_str())
                .or_insert(0usize) += 1;
        }
    }

    for (downstream_id, worker_name) in downstream_workers {
        if worker_name.is_empty() {
            result
                .statuses_by_downstream_id
                .insert(*downstream_id, MinerTelemetryStatus::Unmatched);
            continue;
        }

        let matches = discovered_miners
            .iter()
            .filter(|miner| {
                miner.pools.iter().any(|pool| {
                    pool.user == worker_name.as_str() && pool.port == expected_pool_port
                })
            })
            .collect::<Vec<_>>();

        if downstream_worker_counts
            .get(worker_name.as_str())
            .copied()
            .unwrap_or_default()
            > 1
        {
            result
                .statuses_by_downstream_id
                .insert(*downstream_id, MinerTelemetryStatus::DuplicateWorkerName);
            debug!(
                "Miner telemetry discovery found multiple active downstreams for worker {worker_name}; leaving downstream {downstream_id} unmatched"
            );
            continue;
        }

        if matches.len() == 1 {
            result
                .management_ips_by_downstream_id
                .insert(*downstream_id, matches[0].ip);
            result
                .statuses_by_downstream_id
                .insert(*downstream_id, MinerTelemetryStatus::Matched);
        } else if matches.len() > 1 {
            result
                .statuses_by_downstream_id
                .insert(*downstream_id, MinerTelemetryStatus::DuplicateWorkerName);
            debug!(
                "Miner telemetry discovery found multiple miners for worker {worker_name}; leaving downstream {downstream_id} unmatched"
            );
        } else {
            result
                .statuses_by_downstream_id
                .insert(*downstream_id, MinerTelemetryStatus::Unmatched);
        }
    }

    result
}

fn parse_private_ipv4_cidr(value: &str) -> Option<Ipv4Cidr> {
    let (addr, prefix) = match value.split_once('/') {
        Some(parts) => parts,
        None => {
            warn!("Ignoring miner telemetry CIDR {value}: expected IPv4 CIDR notation");
            return None;
        }
    };

    let addr = match addr.parse::<Ipv4Addr>() {
        Ok(addr) => addr,
        Err(error) => {
            warn!("Ignoring miner telemetry CIDR {value}: invalid IPv4 address: {error}");
            return None;
        }
    };

    let prefix = match prefix.parse::<u8>() {
        Ok(prefix) => prefix,
        Err(error) => {
            warn!("Ignoring miner telemetry CIDR {value}: invalid prefix: {error}");
            return None;
        }
    };

    if !(MINER_DISCOVERY_MIN_IPV4_PREFIX..=32).contains(&prefix) {
        warn!(
            "Ignoring miner telemetry CIDR {value}: prefix must be /{MINER_DISCOVERY_MIN_IPV4_PREFIX} or narrower"
        );
        return None;
    }

    if !addr.is_private() {
        warn!("Ignoring miner telemetry CIDR {value}: only private IPv4 ranges are supported");
        return None;
    }

    let network = Ipv4Addr::from(u32::from(addr) & ipv4_mask(prefix));
    Some(Ipv4Cidr { network, prefix })
}

fn ipv4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

impl Ipv4Cidr {
    fn host_ips(self) -> Vec<IpAddr> {
        let network = u32::from(self.network);
        let host_count = 1u64 << (32 - self.prefix);
        let broadcast = network + host_count as u32 - 1;

        let (first, last) = if self.prefix <= 30 {
            (network + 1, broadcast - 1)
        } else {
            (network, broadcast)
        };

        (first..=last)
            .map(|ip| IpAddr::V4(Ipv4Addr::from(ip)))
            .collect()
    }
}

impl Default for MinerTelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asic_rs::core::data::pool::{PoolScheme, PoolURL};

    fn discovered_miner(ip: [u8; 4], user: &str, port: u16) -> DiscoveredMiner {
        DiscoveredMiner {
            ip: IpAddr::V4(Ipv4Addr::from(ip)),
            pools: vec![DiscoveredMinerPool {
                user: user.to_string(),
                host: "192.168.1.10".to_string(),
                port,
            }],
        }
    }

    fn pool_data(user: &str, port: u16, active: Option<bool>) -> PoolData {
        PoolData {
            position: Some(0),
            url: Some(PoolURL {
                scheme: PoolScheme::StratumV1,
                host: "192.168.1.10".to_string(),
                port,
                pubkey: None,
            }),
            accepted_shares: None,
            rejected_shares: None,
            active,
            alive: None,
            user: Some(user.to_string()),
        }
    }

    #[test]
    fn parses_private_ipv4_cidr_hosts() {
        let cidr = parse_private_ipv4_cidr("192.168.1.0/30").unwrap();
        let hosts = cidr.host_ips();

        assert_eq!(
            hosts,
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2))
            ]
        );
    }

    #[test]
    fn rejects_non_private_or_broad_cidrs() {
        assert!(parse_private_ipv4_cidr("8.8.8.0/24").is_none());
        assert!(parse_private_ipv4_cidr("192.168.0.0/16").is_none());
        assert!(parse_private_ipv4_cidr("192.168.1.0").is_none());
    }

    #[test]
    fn ignores_inactive_discovered_pool_entries() {
        assert_eq!(
            discovered_miner_pool(pool_data("worker-a", 34255, Some(false))),
            None
        );
        assert_eq!(
            discovered_miner_pool(pool_data("worker-a", 34255, Some(true))),
            Some(DiscoveredMinerPool {
                user: "worker-a".to_string(),
                host: "192.168.1.10".to_string(),
                port: 34255
            })
        );
    }

    #[test]
    fn matches_unique_worker_names() {
        let downstream_workers = vec![(10, "worker-a".to_string()), (11, "worker-b".to_string())];
        let discovered_miners = vec![
            discovered_miner([192, 168, 1, 20], "worker-a", 34255),
            discovered_miner([192, 168, 1, 21], "worker-b", 34255),
        ];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert_eq!(
            result.management_ips_by_downstream_id.get(&10),
            Some(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)))
        );
        assert_eq!(
            result.management_ips_by_downstream_id.get(&11),
            Some(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 21)))
        );
    }

    #[test]
    fn reports_duplicate_worker_status_for_discovered_duplicates() {
        let downstream_workers = vec![(10, "worker-a".to_string())];
        let discovered_miners = vec![
            discovered_miner([192, 168, 1, 20], "worker-a", 34255),
            discovered_miner([192, 168, 1, 21], "worker-a", 34255),
        ];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert!(result.management_ips_by_downstream_id.is_empty());
        assert_eq!(
            result.statuses_by_downstream_id.get(&10),
            Some(&MinerTelemetryStatus::DuplicateWorkerName)
        );
    }

    #[test]
    fn leaves_duplicate_downstream_worker_unmatched() {
        let downstream_workers = vec![(10, "worker-a".to_string()), (11, "worker-a".to_string())];
        let discovered_miners = vec![discovered_miner([192, 168, 1, 20], "worker-a", 34255)];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert!(result.management_ips_by_downstream_id.is_empty());
    }

    #[test]
    fn reports_unmatched_worker_status() {
        let downstream_workers = vec![(10, "worker-a".to_string()), (11, String::new())];
        let discovered_miners = vec![discovered_miner([192, 168, 1, 20], "worker-b", 34255)];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert!(result.management_ips_by_downstream_id.is_empty());
        assert_eq!(
            result.statuses_by_downstream_id.get(&10),
            Some(&MinerTelemetryStatus::Unmatched)
        );
        assert_eq!(
            result.statuses_by_downstream_id.get(&11),
            Some(&MinerTelemetryStatus::Unmatched)
        );
    }

    #[test]
    fn reports_duplicate_downstream_worker_status() {
        let downstream_workers = vec![(10, "worker-a".to_string()), (11, "worker-a".to_string())];
        let discovered_miners = vec![discovered_miner([192, 168, 1, 63], "worker-a", 34255)];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert!(result.management_ips_by_downstream_id.is_empty());
        assert_eq!(
            result.statuses_by_downstream_id.get(&10),
            Some(&MinerTelemetryStatus::DuplicateWorkerName)
        );
        assert_eq!(
            result.statuses_by_downstream_id.get(&11),
            Some(&MinerTelemetryStatus::DuplicateWorkerName)
        );
    }

    #[test]
    fn ignores_miner_with_matching_worker_on_different_pool_port() {
        let downstream_workers = vec![(10, "worker-a".to_string())];
        let discovered_miners = vec![discovered_miner([192, 168, 1, 63], "worker-a", 34265)];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert!(result.management_ips_by_downstream_id.is_empty());
        assert_eq!(
            result.statuses_by_downstream_id.get(&10),
            Some(&MinerTelemetryStatus::Unmatched)
        );
    }

    #[test]
    fn matches_worker_on_expected_pool_port() {
        let downstream_workers = vec![(10, "worker-a".to_string())];
        let discovered_miners = vec![discovered_miner([192, 168, 1, 63], "worker-a", 34255)];

        let result = match_discovered_miners_to_downstreams_by_worker_and_port(
            &downstream_workers,
            &discovered_miners,
            34255,
        );

        assert_eq!(
            result.management_ips_by_downstream_id.get(&10),
            Some(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 63)))
        );
        assert_eq!(
            result.statuses_by_downstream_id.get(&10),
            Some(&MinerTelemetryStatus::Matched)
        );
    }
}
