//! Optional `asic-rs` integration for miner discovery, telemetry, and control.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use asic_rs::MinerFactory;
use asic_rs_core::{
    config::pools::{PoolConfig, PoolGroupConfig},
    data::{
        collector::DataField,
        hashrate::{HashRate, HashRateUnit},
        miner::MinerData,
        pool::PoolURL,
    },
    traits::miner::{Miner, MinerAuth},
};
use measurements::Temperature;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use utoipa::ToSchema;

use super::{
    AsicDiscoveredMiner, AsicFanTelemetry, AsicHashboardTelemetry, AsicMinerCapabilities,
    AsicMinerMessage, AsicMinerTelemetry, AsicPoolConfig, AsicPoolData, AsicPoolGroupConfig,
    AsicPoolGroupData, AsicScanError, AsicScanResponse,
};

const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_TELEMETRY_TIMEOUT: Duration = Duration::from_secs(6);
const FALLBACK_FIELD_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_SCAN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_SCAN_CONCURRENCY: usize = 32;
const MAX_SCAN_CONCURRENCY: usize = 64;
const MAX_SCAN_TARGETS: usize = 4096;

type MinerHandle = Arc<Mutex<Box<dyn Miner>>>;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicCredentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicScanRequest {
    /// IPv4 addresses or CIDR blocks to scan. Example: ["192.168.1.0/24"].
    #[serde(default)]
    pub targets: Vec<String>,
    /// Per-host discovery timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Maximum number of concurrent discovery probes.
    pub concurrency: Option<usize>,
    /// Optional miner API credentials used for probes that require auth.
    pub auth: Option<AsicCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicUpdatePoolsRequest {
    pub pool_groups: Vec<AsicPoolGroupConfig>,
    /// Optional miner API credentials, not mining pool credentials.
    pub auth: Option<AsicCredentials>,
}

#[derive(Clone)]
pub struct AsicMonitor {
    factory: Arc<MinerFactory>,
    miners: Arc<Mutex<HashMap<IpAddr, MinerHandle>>>,
}

impl Default for AsicMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl AsicMonitor {
    pub fn new() -> Self {
        Self {
            factory: Arc::new(MinerFactory::new()),
            miners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn telemetry(
        &self,
        ip: IpAddr,
        auth: Option<AsicCredentials>,
    ) -> Result<AsicMinerTelemetry, String> {
        let handle = self.miner_for_ip(ip, auth).await?;
        let guard = handle.lock().await;
        let mut data = tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.get_data())
            .await
            .map_err(|_| format!("Timed out reading telemetry from {ip}"))?;
        let _ = tokio::time::timeout(
            FALLBACK_TELEMETRY_TIMEOUT,
            fill_missing_telemetry_fields(&mut data, guard.as_ref()),
        )
        .await;

        Ok(miner_data_to_telemetry(data, guard.as_ref()))
    }

    pub async fn pools(
        &self,
        ip: IpAddr,
        auth: Option<AsicCredentials>,
    ) -> Result<Vec<AsicPoolGroupConfig>, String> {
        let handle = self.miner_for_ip(ip, auth).await?;
        let guard = handle.lock().await;
        if !guard.supports_pools_config() {
            return Err("Miner does not support pool configuration".to_string());
        }

        let pools = tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.get_pools_config())
            .await
            .map_err(|_| format!("Timed out reading pools from {ip}"))?
            .map_err(|error| format!("Failed reading pools from {ip}: {error}"))?;

        Ok(pools.into_iter().map(pool_group_config_from_asic).collect())
    }

    pub async fn update_pools(
        &self,
        ip: IpAddr,
        request: AsicUpdatePoolsRequest,
    ) -> Result<(), String> {
        let handle = self.miner_for_ip(ip, request.auth).await?;
        let guard = handle.lock().await;
        if !guard.supports_pools_config() {
            return Err("Miner does not support pool configuration".to_string());
        }

        let pools = request
            .pool_groups
            .into_iter()
            .map(pool_group_config_to_asic)
            .collect::<Vec<_>>();

        let ok = tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.set_pools_config(pools))
            .await
            .map_err(|_| format!("Timed out updating pools on {ip}"))?
            .map_err(|error| format!("Failed updating pools on {ip}: {error}"))?;

        if ok {
            Ok(())
        } else {
            Err("Miner rejected pool configuration update".to_string())
        }
    }

    pub async fn action(
        &self,
        ip: IpAddr,
        action: &str,
        auth: Option<AsicCredentials>,
    ) -> Result<(), String> {
        let handle = self.miner_for_ip(ip, auth).await?;
        let guard = handle.lock().await;

        let result = match action {
            "blink" | "blink_led" | "identify" => {
                if !guard.supports_set_fault_light() {
                    return Err("Miner does not support blink LED".to_string());
                }
                tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.set_fault_light(true))
                    .await
                    .map_err(|_| format!("Timed out blinking LED on {ip}"))?
            }
            "reboot" | "restart" => {
                if !guard.supports_restart() {
                    return Err("Miner does not support reboot".to_string());
                }
                tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.restart())
                    .await
                    .map_err(|_| format!("Timed out rebooting {ip}"))?
            }
            "pause" | "stop" | "stop_mining" => {
                if !guard.supports_pause() {
                    return Err("Miner does not support stop mining".to_string());
                }
                tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.pause(None))
                    .await
                    .map_err(|_| format!("Timed out stopping mining on {ip}"))?
            }
            "resume" | "start" | "start_mining" => {
                if !guard.supports_resume() {
                    return Err("Miner does not support start mining".to_string());
                }
                tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, guard.resume(None))
                    .await
                    .map_err(|_| format!("Timed out starting mining on {ip}"))?
            }
            other => return Err(format!("Unsupported miner action: {other}")),
        }
        .map_err(|error| format!("Miner action failed on {ip}: {error}"))?;

        if result {
            Ok(())
        } else {
            Err(format!("Miner action {action} returned false on {ip}"))
        }
    }

    pub async fn scan(&self, request: AsicScanRequest) -> Result<AsicScanResponse, String> {
        let targets = expand_targets(&request.targets)?;
        let total_targets = targets.len();
        let timeout = request
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_SCAN_TIMEOUT)
            .max(Duration::from_millis(250));
        let concurrency = request
            .concurrency
            .unwrap_or(DEFAULT_SCAN_CONCURRENCY)
            .clamp(1, MAX_SCAN_CONCURRENCY);
        let semaphore = Arc::new(Semaphore::new(concurrency));

        let mut handles = Vec::with_capacity(targets.len());
        for ip in targets {
            let permit = semaphore.clone().acquire_owned().await;
            let monitor = self.clone();
            let auth = request.auth.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit.map_err(|error| scan_error(ip, error.to_string()))?;
                monitor.discover(ip, timeout, auth).await
            }));
        }

        let mut found = Vec::new();
        let mut errors = Vec::new();
        for handle in handles {
            match handle.await.map_err(|error| error.to_string())? {
                Ok(miner) => found.push(miner),
                Err(error) => errors.push(error),
            }
        }

        Ok(AsicScanResponse {
            total_targets,
            found,
            errors,
        })
    }

    async fn discover(
        &self,
        ip: IpAddr,
        timeout: Duration,
        auth: Option<AsicCredentials>,
    ) -> Result<AsicDiscoveredMiner, AsicScanError> {
        let miner = tokio::time::timeout(timeout, self.factory.get_miner(ip))
            .await
            .map_err(|_| scan_error(ip, "discovery timed out"))?
            .map_err(|error| scan_error(ip, format!("discovery failed: {error}")))?
            .ok_or_else(|| scan_error(ip, "no asic-rs supported miner found"))?;

        let handle = Arc::new(Mutex::new(miner));
        apply_auth(&handle, auth).await;

        let guard = handle.lock().await;
        let trait_info = guard.get_device_info();
        let mut discovered = AsicDiscoveredMiner {
            ip: ip.to_string(),
            port: canonical_port(&trait_info.make),
            make: trait_info.make,
            model: trait_info.model,
            firmware: trait_info.firmware,
            firmware_version: None,
            serial_number: None,
            mac_address: None,
            hostname: None,
            capabilities: capabilities_from_miner(guard.as_ref()),
        };

        if let Ok(data) = tokio::time::timeout(timeout, guard.get_data()).await {
            discovered.make = data.device_info.make.clone();
            discovered.model = data.device_info.model.clone();
            discovered.firmware = data.device_info.firmware.clone();
            discovered.firmware_version = data.firmware_version.clone();
            discovered.serial_number = data.serial_number.clone();
            discovered.mac_address = data.mac.map(|mac| mac.to_string());
            discovered.hostname = data.hostname.clone();
            discovered.port = canonical_port(&data.device_info.make);
        }
        drop(guard);

        self.miners.lock().await.insert(ip, handle);
        Ok(discovered)
    }

    async fn miner_for_ip(
        &self,
        ip: IpAddr,
        auth: Option<AsicCredentials>,
    ) -> Result<MinerHandle, String> {
        if let Some(handle) = self.miners.lock().await.get(&ip).cloned() {
            apply_auth(&handle, auth).await;
            return Ok(handle);
        }

        let miner = tokio::time::timeout(DEFAULT_OPERATION_TIMEOUT, self.factory.get_miner(ip))
            .await
            .map_err(|_| format!("Timed out connecting to miner at {ip}"))?
            .map_err(|error| format!("Failed connecting to miner at {ip}: {error}"))?
            .ok_or_else(|| format!("No asic-rs supported miner found at {ip}"))?;

        let handle = Arc::new(Mutex::new(miner));
        apply_auth(&handle, auth).await;
        self.miners.lock().await.insert(ip, handle.clone());
        Ok(handle)
    }
}

async fn apply_auth(handle: &MinerHandle, auth: Option<AsicCredentials>) {
    if let Some(auth) = auth {
        let mut guard = handle.lock().await;
        guard.set_auth(MinerAuth::new(&auth.username, &auth.password));
    }
}

fn scan_error(ip: IpAddr, error: impl Into<String>) -> AsicScanError {
    AsicScanError {
        target: ip.to_string(),
        error: error.into(),
    }
}

fn expand_targets(targets: &[String]) -> Result<Vec<IpAddr>, String> {
    let mut expanded = Vec::new();
    for target in targets {
        let target = target.trim();
        if target.is_empty() {
            continue;
        }
        if target.contains('/') {
            expanded.extend(expand_ipv4_cidr(target)?);
        } else {
            expanded.push(
                target
                    .parse::<IpAddr>()
                    .map_err(|_| format!("Invalid scan target: {target}"))?,
            );
        }

        if expanded.len() > MAX_SCAN_TARGETS {
            return Err(format!(
                "Scan target limit exceeded; maximum is {MAX_SCAN_TARGETS} hosts"
            ));
        }
    }
    Ok(expanded)
}

fn expand_ipv4_cidr(target: &str) -> Result<Vec<IpAddr>, String> {
    let (base, prefix) = target
        .split_once('/')
        .ok_or_else(|| format!("Invalid CIDR target: {target}"))?;
    let base = base
        .parse::<Ipv4Addr>()
        .map_err(|_| format!("Invalid IPv4 CIDR address: {target}"))?;
    let prefix = prefix
        .parse::<u32>()
        .map_err(|_| format!("Invalid CIDR prefix: {target}"))?;
    if prefix > 32 {
        return Err(format!("Invalid CIDR prefix: {target}"));
    }

    let host_count = 1usize
        .checked_shl(32 - prefix)
        .ok_or_else(|| format!("Invalid CIDR target: {target}"))?;
    if host_count > MAX_SCAN_TARGETS {
        return Err(format!(
            "CIDR target {target} expands to {host_count} hosts; maximum is {MAX_SCAN_TARGETS}"
        ));
    }

    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = u32::from(base) & mask;
    let first = if prefix <= 30 { network + 1 } else { network };
    let last = if prefix <= 30 {
        network + host_count as u32 - 2
    } else {
        network + host_count as u32 - 1
    };

    Ok((first..=last)
        .map(|addr| IpAddr::V4(Ipv4Addr::from(addr)))
        .collect())
}

async fn fill_missing_telemetry_fields(data: &mut MinerData, miner: &dyn Miner) {
    if !has_missing_telemetry_fields(data) {
        return;
    }

    if data.mac.is_none() {
        data.mac = field_timeout(miner.get_mac()).await.flatten();
    }
    if data.serial_number.is_none() {
        data.serial_number = field_timeout(miner.get_serial_number()).await.flatten();
    }
    if data.hostname.is_none() {
        data.hostname = field_timeout(miner.get_hostname()).await.flatten();
    }
    if data.api_version.is_none() {
        data.api_version = field_timeout(miner.get_api_version()).await.flatten();
    }
    if data.firmware_version.is_none() {
        data.firmware_version = field_timeout(miner.get_firmware_version()).await.flatten();
    }
    if data.control_board_version.is_none() {
        data.control_board_version = field_timeout(miner.get_control_board_version())
            .await
            .flatten();
    }

    if data.hashboards.is_empty() || !hashboards_have_runtime_data(&data.hashboards) {
        let hashboards = field_timeout(miner.get_hashboards())
            .await
            .unwrap_or_default();
        if !hashboards.is_empty()
            && (data.hashboards.is_empty() || hashboards_have_runtime_data(&hashboards))
        {
            data.hashboards = hashboards;
        }
    }

    if data.hashrate.is_none() {
        data.hashrate = field_timeout(miner.get_hashrate()).await.flatten();
    }
    if data.expected_hashrate.is_none() {
        data.expected_hashrate = field_timeout(miner.get_expected_hashrate()).await.flatten();
    }
    if data.fans.is_empty() {
        data.fans = field_timeout(miner.get_fans()).await.unwrap_or_default();
    }
    if data.psu_fans.is_empty() {
        data.psu_fans = field_timeout(miner.get_psu_fans())
            .await
            .unwrap_or_default();
    }
    if data.fluid_temperature.is_none() {
        data.fluid_temperature = field_timeout(miner.get_fluid_temperature()).await.flatten();
    }
    if data.wattage.is_none() {
        data.wattage = field_timeout(miner.get_wattage()).await.flatten();
    }
    if data.tuning_target.is_none() {
        data.tuning_target = field_timeout(miner.get_tuning_target()).await.flatten();
    }
    if data.light_flashing.is_none() {
        data.light_flashing = field_timeout(miner.get_light_flashing()).await.flatten();
    }
    if data.messages.is_empty() {
        data.messages = field_timeout(miner.get_messages())
            .await
            .unwrap_or_default();
    }
    if data.uptime.is_none() {
        data.uptime = field_timeout(miner.get_uptime()).await.flatten();
    }
    if data.pools.is_empty() || pools_are_empty_defaults(&data.pools) {
        data.pools = field_timeout(miner.get_pools()).await.unwrap_or_default();
    }

    if data.average_temperature.is_none() {
        data.average_temperature = match average_temperature_from_hashboards(data) {
            Some(temperature) => Some(temperature),
            None => field_timeout(collect_average_temperature(miner))
                .await
                .flatten(),
        };
    }
    if data.efficiency.is_none() {
        data.efficiency = efficiency_from_miner_data(data);
    }

    if !data.is_mining {
        data.is_mining = field_timeout(miner.get_is_mining()).await.unwrap_or(false)
            || data
                .hashrate
                .as_ref()
                .is_some_and(|hashrate| hashrate_as_hs(hashrate) > 0.0)
            || hashboard_hashrate_hs(data).is_some_and(|hashrate| hashrate > 0.0);
    }
}

async fn field_timeout<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(FALLBACK_FIELD_TIMEOUT, future)
        .await
        .ok()
}

async fn collect_average_temperature(miner: &dyn Miner) -> Option<Temperature> {
    let mut collector = miner.get_collector();
    let values = collector.collect(&[DataField::AverageTemperature]).await;
    values
        .get(&DataField::AverageTemperature)
        .and_then(|value| value.as_f64())
        .map(Temperature::from_celsius)
}

fn has_missing_telemetry_fields(data: &MinerData) -> bool {
    data.hashrate.is_none()
        || data.wattage.is_none()
        || data.average_temperature.is_none()
        || data.uptime.is_none()
        || !hashboards_have_runtime_data(&data.hashboards)
}

fn hashboards_have_runtime_data(hashboards: &[asic_rs_core::data::board::BoardData]) -> bool {
    hashboards.iter().any(|board| {
        board.hashrate.is_some()
            || board.expected_hashrate.is_some()
            || board.board_temperature.is_some()
            || board.intake_temperature.is_some()
            || board.outlet_temperature.is_some()
            || board.working_chips.is_some()
            || board.voltage.is_some()
            || board.frequency.is_some()
            || board.active.is_some()
    })
}

fn pools_are_empty_defaults(pools: &[asic_rs_core::data::pool::PoolGroupData]) -> bool {
    pools.iter().all(|group| {
        group.pools.iter().all(|pool| {
            pool.url
                .as_ref()
                .is_none_or(|url| url.host.is_empty() || url.port == 0)
        })
    })
}

fn average_temperature_from_hashboards(data: &MinerData) -> Option<Temperature> {
    let temperatures = data
        .hashboards
        .iter()
        .filter_map(|board| {
            board
                .board_temperature
                .as_ref()
                .map(|temperature| temperature.as_celsius())
        })
        .collect::<Vec<_>>();

    if temperatures.is_empty() {
        None
    } else {
        Some(Temperature::from_celsius(
            temperatures.iter().sum::<f64>() / temperatures.len() as f64,
        ))
    }
}

fn efficiency_from_miner_data(data: &MinerData) -> Option<f64> {
    let hashrate_th = data
        .hashrate
        .as_ref()
        .map(|hashrate| hashrate.clone().as_unit(HashRateUnit::TeraHash).value)?;
    if hashrate_th <= 0.0 {
        return None;
    }

    data.wattage
        .as_ref()
        .map(|power| power.as_watts() / hashrate_th)
}

fn miner_data_to_telemetry(data: MinerData, miner: &dyn Miner) -> AsicMinerTelemetry {
    let hashrate_hs = data
        .hashrate
        .as_ref()
        .map(hashrate_as_hs)
        .or_else(|| hashboard_hashrate_hs(&data));
    let expected_hashrate_hs = data
        .expected_hashrate
        .as_ref()
        .map(hashrate_as_hs)
        .or_else(|| hashboard_expected_hashrate_hs(&data));
    let average_temperature_c = data
        .average_temperature
        .as_ref()
        .map(|temperature| temperature.as_celsius())
        .or_else(|| hashboard_average_temperature_c(&data));
    let efficiency_j_th = data.efficiency.or_else(|| {
        data.wattage.as_ref().and_then(|power| {
            let hashrate_th = hashrate_hs? / 1_000_000_000_000.0;
            if hashrate_th > 0.0 {
                Some(power.as_watts() / hashrate_th)
            } else {
                None
            }
        })
    });

    AsicMinerTelemetry {
        ip: data.ip.to_string(),
        make: data.device_info.make,
        model: data.device_info.model,
        firmware: data.device_info.firmware,
        firmware_version: data.firmware_version,
        serial_number: data.serial_number,
        mac_address: data.mac.map(|mac| mac.to_string()),
        hostname: data.hostname,
        api_version: data.api_version,
        control_board_version: data
            .control_board_version
            .map(|version| version.to_string()),
        hashrate_hs,
        expected_hashrate_hs,
        power_w: data.wattage.as_ref().map(|power| power.as_watts()),
        efficiency_j_th,
        average_temperature_c,
        fluid_temperature_c: data
            .fluid_temperature
            .as_ref()
            .map(|temperature| temperature.as_celsius()),
        uptime_secs: data.uptime.map(|duration| duration.as_secs()),
        is_mining: data.is_mining,
        light_flashing: data.light_flashing,
        expected_hashboards: data.expected_hashboards,
        expected_chips: data.expected_chips,
        total_chips: data.total_chips,
        expected_fans: data.expected_fans,
        hashboards: data
            .hashboards
            .into_iter()
            .map(|board| AsicHashboardTelemetry {
                position: board.position,
                hashrate_hs: board.hashrate.as_ref().map(hashrate_as_hs),
                expected_hashrate_hs: board.expected_hashrate.as_ref().map(hashrate_as_hs),
                board_temperature_c: board
                    .board_temperature
                    .as_ref()
                    .map(|temperature| temperature.as_celsius()),
                intake_temperature_c: board
                    .intake_temperature
                    .as_ref()
                    .map(|temperature| temperature.as_celsius()),
                outlet_temperature_c: board
                    .outlet_temperature
                    .as_ref()
                    .map(|temperature| temperature.as_celsius()),
                expected_chips: board.expected_chips,
                working_chips: board.working_chips,
                serial_number: board.serial_number,
                voltage_v: board.voltage.as_ref().map(|voltage| voltage.as_volts()),
                frequency_mhz: board
                    .frequency
                    .as_ref()
                    .map(|frequency| frequency.as_megahertz()),
                tuned: board.tuned,
                active: board.active,
            })
            .collect(),
        fans: data
            .fans
            .into_iter()
            .map(|fan| AsicFanTelemetry {
                position: fan.position,
                rpm: fan.rpm.as_ref().map(|rpm| rpm.as_rpm()),
            })
            .collect(),
        psu_fans: data
            .psu_fans
            .into_iter()
            .map(|fan| AsicFanTelemetry {
                position: fan.position,
                rpm: fan.rpm.as_ref().map(|rpm| rpm.as_rpm()),
            })
            .collect(),
        pools: data
            .pools
            .into_iter()
            .map(pool_group_data_from_asic)
            .collect(),
        messages: data
            .messages
            .into_iter()
            .map(|message| AsicMinerMessage {
                timestamp: message.timestamp,
                code: message.code,
                message: message.message,
                severity: message.severity.to_string(),
            })
            .collect(),
        capabilities: capabilities_from_miner(miner),
        last_updated_at: unix_now(),
    }
}

fn capabilities_from_miner(miner: &dyn Miner) -> AsicMinerCapabilities {
    AsicMinerCapabilities {
        telemetry: true,
        restart: miner.supports_restart(),
        pause: miner.supports_pause(),
        resume: miner.supports_resume(),
        blink_led: miner.supports_set_fault_light(),
        pools_config: miner.supports_pools_config(),
        power_limit: miner.supports_set_power_limit(),
        tuning_config: miner.supports_tuning_config(),
    }
}

fn pool_group_data_from_asic(group: asic_rs_core::data::pool::PoolGroupData) -> AsicPoolGroupData {
    AsicPoolGroupData {
        name: group.name,
        quota: group.quota,
        pools: group
            .pools
            .into_iter()
            .map(|pool| AsicPoolData {
                position: pool.position,
                url: pool.url.map(|url| url.to_string()),
                accepted_shares: pool.accepted_shares,
                rejected_shares: pool.rejected_shares,
                active: pool.active,
                alive: pool.alive,
                user: pool.user,
            })
            .collect(),
    }
}

fn pool_group_config_from_asic(group: PoolGroupConfig) -> AsicPoolGroupConfig {
    AsicPoolGroupConfig {
        name: group.name,
        quota: group.quota,
        pools: group
            .pools
            .into_iter()
            .map(|pool| AsicPoolConfig {
                url: pool.url.to_string(),
                username: pool.username,
                password: pool.password,
            })
            .collect(),
    }
}

fn pool_group_config_to_asic(group: AsicPoolGroupConfig) -> PoolGroupConfig {
    PoolGroupConfig {
        name: group.name,
        quota: group.quota,
        pools: group
            .pools
            .into_iter()
            .map(|pool| PoolConfig {
                url: PoolURL::from(pool.url),
                username: pool.username,
                password: pool.password,
            })
            .collect(),
    }
}

fn hashboard_hashrate_hs(data: &MinerData) -> Option<f64> {
    sum_hashboard_hashrates(data, |board| board.hashrate.as_ref())
}

fn hashboard_expected_hashrate_hs(data: &MinerData) -> Option<f64> {
    sum_hashboard_hashrates(data, |board| board.expected_hashrate.as_ref())
}

fn sum_hashboard_hashrates<'a>(
    data: &'a MinerData,
    select: impl Fn(&'a asic_rs_core::data::board::BoardData) -> Option<&'a HashRate>,
) -> Option<f64> {
    let values = data
        .hashboards
        .iter()
        .filter_map(select)
        .map(hashrate_as_hs)
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect::<Vec<_>>();

    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum())
    }
}

fn hashboard_average_temperature_c(data: &MinerData) -> Option<f64> {
    let temperatures = data
        .hashboards
        .iter()
        .filter_map(|board| {
            board
                .board_temperature
                .as_ref()
                .map(|temperature| temperature.as_celsius())
        })
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();

    if temperatures.is_empty() {
        None
    } else {
        Some(temperatures.iter().sum::<f64>() / temperatures.len() as f64)
    }
}

fn hashrate_as_hs(hashrate: &HashRate) -> f64 {
    hashrate.clone().as_unit(HashRateUnit::Hash).value
}

fn canonical_port(make: &str) -> Option<u16> {
    let make = make.to_ascii_lowercase();
    if make.contains("whats") || make.contains("avalon") {
        Some(4028)
    } else {
        Some(80)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
