//! ASIC miner monitoring response types.
//!
//! These are plain API schemas. The `asic-rs` integration that populates them
//! lives behind the `asic-monitoring` feature.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AsicMinerCapabilities {
    pub telemetry: bool,
    pub restart: bool,
    pub pause: bool,
    pub resume: bool,
    pub blink_led: bool,
    pub pools_config: bool,
    pub power_limit: bool,
    pub tuning_config: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicHashboardTelemetry {
    pub position: u8,
    pub hashrate_hs: Option<f64>,
    pub expected_hashrate_hs: Option<f64>,
    pub board_temperature_c: Option<f64>,
    pub intake_temperature_c: Option<f64>,
    pub outlet_temperature_c: Option<f64>,
    pub expected_chips: Option<u16>,
    pub working_chips: Option<u16>,
    pub serial_number: Option<String>,
    pub voltage_v: Option<f64>,
    pub frequency_mhz: Option<f64>,
    pub tuned: Option<bool>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicFanTelemetry {
    pub position: i16,
    pub rpm: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicMinerMessage {
    pub timestamp: u32,
    pub code: u64,
    pub message: String,
    pub severity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicPoolData {
    pub position: Option<u16>,
    pub url: Option<String>,
    pub accepted_shares: Option<u64>,
    pub rejected_shares: Option<u64>,
    pub active: Option<bool>,
    pub alive: Option<bool>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicPoolGroupData {
    pub name: String,
    pub quota: u32,
    pub pools: Vec<AsicPoolData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicPoolConfig {
    pub url: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicPoolGroupConfig {
    pub name: String,
    pub quota: u32,
    pub pools: Vec<AsicPoolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicMinerTelemetry {
    pub ip: String,
    pub make: String,
    pub model: String,
    pub firmware: String,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
    pub mac_address: Option<String>,
    pub hostname: Option<String>,
    pub api_version: Option<String>,
    pub control_board_version: Option<String>,
    pub hashrate_hs: Option<f64>,
    pub expected_hashrate_hs: Option<f64>,
    pub power_w: Option<f64>,
    pub efficiency_j_th: Option<f64>,
    pub average_temperature_c: Option<f64>,
    pub fluid_temperature_c: Option<f64>,
    pub uptime_secs: Option<u64>,
    pub is_mining: bool,
    pub light_flashing: Option<bool>,
    pub expected_hashboards: Option<u8>,
    pub expected_chips: Option<u16>,
    pub total_chips: Option<u16>,
    pub expected_fans: Option<u8>,
    pub hashboards: Vec<AsicHashboardTelemetry>,
    pub fans: Vec<AsicFanTelemetry>,
    pub psu_fans: Vec<AsicFanTelemetry>,
    pub pools: Vec<AsicPoolGroupData>,
    pub messages: Vec<AsicMinerMessage>,
    pub capabilities: AsicMinerCapabilities,
    pub last_updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicDiscoveredMiner {
    pub ip: String,
    pub port: Option<u16>,
    pub make: String,
    pub model: String,
    pub firmware: String,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
    pub mac_address: Option<String>,
    pub hostname: Option<String>,
    pub capabilities: AsicMinerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicScanError {
    pub target: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AsicScanResponse {
    pub total_targets: usize,
    pub found: Vec<AsicDiscoveredMiner>,
    pub errors: Vec<AsicScanError>,
}
