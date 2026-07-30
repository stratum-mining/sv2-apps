use clap::Parser;
use jd_client_sv2::{config::JobDeclaratorClientConfig, error::JDCErrorKind};
use stratum_apps::config_helpers::load_config;

use std::path::PathBuf;
#[derive(Debug, Parser)]
#[command(author, version, about = "JD Client", long_about = None)]
pub struct Args {
    #[arg(
        short = 'c',
        long = "config",
        help = "Path to the TOML configuration file",
        default_value = "jdc-config.toml"
    )]
    pub config_path: PathBuf,
    #[arg(
        short = 'f',
        long = "log-file",
        help = "Path to the log file. If not set, logs will only be written to stdout."
    )]
    pub log_file: Option<PathBuf>,
}

/// Comma-separated list fields of [`JobDeclaratorClientConfig`] (see `load_config`).
const LIST_KEYS: &[&str] = &[
    "supported_extensions",
    "required_extensions",
    "miner_telemetry.cidrs",
];

/// Externally tagged enum fields of [`JobDeclaratorClientConfig`] (see `load_config`).
const ENUM_KEYS: &[&str] = &["template_provider_type"];

#[allow(clippy::result_large_err)]
pub fn process_cli_args() -> Result<JobDeclaratorClientConfig, JDCErrorKind> {
    let args = Args::parse();

    // Env vars prefixed `JDC__` override values from the optional TOML file.
    let mut config: JobDeclaratorClientConfig =
        load_config(&args.config_path, "JDC", LIST_KEYS, ENUM_KEYS)?;

    config.set_log_file(args.log_file);

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::tp_type::TemplateProviderType;

    /// Loads a full `JobDeclaratorClientConfig` from `JDC__*` env vars alone,
    /// exercising the list and enum keys registered above and the upstream
    /// array syntax. A structured field that is missing from
    /// `LIST_KEYS`/`ENUM_KEYS` fails this test even when the generic loader
    /// tests pass.
    #[test]
    fn jdc_config_loads_from_env_only() {
        let vars = [
            ("JDC__LISTENING_ADDRESS", "0.0.0.0:34265"),
            ("JDC__MAX_SUPPORTED_VERSION", "2"),
            ("JDC__MIN_SUPPORTED_VERSION", "2"),
            (
                "JDC__AUTHORITY_PUBLIC_KEY",
                "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72",
            ),
            (
                "JDC__AUTHORITY_SECRET_KEY",
                "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n",
            ),
            ("JDC__CERT_VALIDITY_SEC", "3600"),
            (
                "JDC__COINBASE_REWARD_SCRIPT",
                "addr(tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8)",
            ),
            ("JDC__JDC_SIGNATURE", "Sv2MinerSignature"),
            ("JDC__SHARES_PER_MINUTE", "6.0"),
            ("JDC__SHARE_BATCH_SIZE", "10"),
            // Single values must come out as 1-element lists.
            ("JDC__SUPPORTED_EXTENSIONS", "2"),
            ("JDC__REQUIRED_EXTENSIONS", "2"),
            ("JDC__MINER_TELEMETRY__CIDRS", "192.168.1.0/24"),
            (
                "JDC__TEMPLATE_PROVIDER_TYPE__SV2TP__ADDRESS",
                "127.0.0.1:8442",
            ),
            (
                "JDC__UPSTREAM_01__AUTHORITY_PUBKEY",
                "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72",
            ),
            ("JDC__UPSTREAM_01__POOL_ADDRESS", "127.0.0.1"),
            ("JDC__UPSTREAM_01__POOL_PORT", "34254"),
            ("JDC__UPSTREAM_01__JDS_ADDRESS", "127.0.0.1"),
            ("JDC__UPSTREAM_01__JDS_PORT", "34264"),
            ("JDC__UPSTREAM_01__USER_IDENTITY", "user"),
        ];
        for (key, value) in vars {
            unsafe { std::env::set_var(key, value) };
        }

        let config: JobDeclaratorClientConfig = load_config(
            "jdc-config-does-not-exist.toml",
            "JDC",
            LIST_KEYS,
            ENUM_KEYS,
        )
        .expect("load JobDeclaratorClientConfig from env only");

        assert_eq!(config.supported_extensions(), [2]);
        assert_eq!(config.required_extensions(), [2]);
        assert_eq!(config.miner_telemetry_cidrs(), ["192.168.1.0/24"]);
        assert!(matches!(
            config.template_provider_type(),
            TemplateProviderType::Sv2Tp { .. }
        ));
        assert_eq!(config.upstreams().len(), 1);
        assert_eq!(config.upstreams()[0].pool_port, 34254);

        for (key, _) in vars {
            unsafe { std::env::remove_var(key) };
        }
    }
}
