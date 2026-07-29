//! Defines the structure and parsing logic for command-line arguments.
//!
//! It provides the `Args` struct to hold parsed arguments,
//! and the `from_args` function to parse them from the command line.
use clap::Parser;
use std::path::PathBuf;
use stratum_apps::config_helpers::load_config;
use translator_sv2::{config::TranslatorConfig, error::TproxyErrorKind};

/// Holds the parsed CLI arguments.
#[derive(Parser, Debug)]
#[command(author, version, about = "Translator Proxy", long_about = None)]
pub struct Args {
    #[arg(
        short = 'c',
        long = "config",
        help = "Path to the TOML configuration file",
        default_value = "translator-config.toml"
    )]
    pub config_path: PathBuf,
    #[arg(
        short = 'f',
        long = "log-file",
        help = "Path to the log file. If not set, logs will only be written to stdout."
    )]
    pub log_file: Option<PathBuf>,
}

/// Comma-separated list fields of [`TranslatorConfig`] (see `load_config`).
const LIST_KEYS: &[&str] = &[
    "supported_extensions",
    "required_extensions",
    "miner_telemetry.cidrs",
];

/// Externally tagged enum fields of [`TranslatorConfig`] (see `load_config`).
const ENUM_KEYS: &[&str] = &[];

/// Process CLI args, if any.
#[allow(clippy::result_large_err)]
pub fn process_cli_args() -> Result<TranslatorConfig, TproxyErrorKind> {
    // Parse CLI arguments
    let args = Args::parse();

    // Env vars prefixed `TPROXY__` override values from the optional TOML file.
    let mut config: TranslatorConfig =
        load_config(&args.config_path, "TPROXY", LIST_KEYS, ENUM_KEYS)?;

    config.set_log_dir(args.log_file);

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads a full `TranslatorConfig` from `TPROXY__*` env vars alone,
    /// exercising the list keys registered above and the upstream array
    /// syntax. A structured field that is missing from `LIST_KEYS`/`ENUM_KEYS`
    /// fails this test even when the generic loader tests pass.
    #[test]
    fn translator_config_loads_from_env_only() {
        let vars = [
            ("TPROXY__DOWNSTREAM_ADDRESS", "0.0.0.0"),
            ("TPROXY__DOWNSTREAM_PORT", "34255"),
            ("TPROXY__MAX_SUPPORTED_VERSION", "2"),
            ("TPROXY__MIN_SUPPORTED_VERSION", "2"),
            ("TPROXY__DOWNSTREAM_EXTRANONCE2_SIZE", "4"),
            ("TPROXY__AGGREGATE_CHANNELS", "false"),
            (
                "TPROXY__DOWNSTREAM_DIFFICULTY_CONFIG__MIN_INDIVIDUAL_MINER_HASHRATE",
                "10000000000000.0",
            ),
            (
                "TPROXY__DOWNSTREAM_DIFFICULTY_CONFIG__SHARES_PER_MINUTE",
                "6.0",
            ),
            (
                "TPROXY__DOWNSTREAM_DIFFICULTY_CONFIG__ENABLE_VARDIFF",
                "true",
            ),
            (
                "TPROXY__DOWNSTREAM_DIFFICULTY_CONFIG__JOB_KEEPALIVE_INTERVAL_SECS",
                "60",
            ),
            // Single values must come out as 1-element lists.
            ("TPROXY__SUPPORTED_EXTENSIONS", "2"),
            ("TPROXY__REQUIRED_EXTENSIONS", "2"),
            ("TPROXY__MINER_TELEMETRY__CIDRS", "192.168.1.0/24"),
            ("TPROXY__UPSTREAM_01__ADDRESS", "127.0.0.1"),
            ("TPROXY__UPSTREAM_01__PORT", "34265"),
            (
                "TPROXY__UPSTREAM_01__AUTHORITY_PUBKEY",
                "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72",
            ),
            ("TPROXY__UPSTREAM_01__USER_IDENTITY", "user"),
        ];
        for (key, value) in vars {
            std::env::set_var(key, value);
        }

        let config: TranslatorConfig = load_config(
            "translator-config-does-not-exist.toml",
            "TPROXY",
            LIST_KEYS,
            ENUM_KEYS,
        )
        .expect("load TranslatorConfig from env only");

        assert_eq!(config.supported_extensions, [2]);
        assert_eq!(config.required_extensions, [2]);
        assert_eq!(config.miner_telemetry_cidrs(), ["192.168.1.0/24"]);
        assert_eq!(config.upstreams.len(), 1);
        assert_eq!(config.upstreams[0].port, 34265);
        assert_eq!(config.downstream_port, 34255);

        for (key, _) in vars {
            std::env::remove_var(key);
        }
    }
}
