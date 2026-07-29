//! CLI argument parsing for the Pool binary.
//!
//! Defines the `Args` struct and a function to process CLI arguments into a PoolConfig.

use clap::Parser;
use pool_sv2::{config::PoolConfig, error::PoolErrorKind};
use std::path::PathBuf;
use stratum_apps::config_helpers::load_config;

/// Holds the parsed CLI arguments for the Pool binary.
#[derive(Parser, Debug)]
#[command(author, version, about = "Pool CLI", long_about = None)]
pub struct Args {
    #[arg(
        short = 'c',
        long = "config",
        help = "Path to the TOML configuration file",
        default_value = "pool-config.toml"
    )]
    pub config_path: PathBuf,
    #[arg(
        short = 'f',
        long = "log-file",
        help = "Path to the log file. If not set, logs will only be written to stdout."
    )]
    pub log_file: Option<PathBuf>,
}

/// Comma-separated list fields of [`PoolConfig`] (see `load_config`).
const LIST_KEYS: &[&str] = &[
    "supported_extensions",
    "required_extensions",
    "jds.supported_extensions",
    "jds.required_extensions",
];

/// Externally tagged enum fields of [`PoolConfig`] (see `load_config`).
const ENUM_KEYS: &[&str] = &["template_provider_type"];

#[cfg_attr(not(test), hotpath::measure)]
/// Parses CLI arguments and loads the PoolConfig from the specified file.
#[allow(clippy::result_large_err)]
pub fn process_cli_args() -> Result<PoolConfig, PoolErrorKind> {
    let args = Args::parse();

    // Env vars prefixed `POOL__` override values from the optional TOML file.
    let mut config: PoolConfig = load_config(&args.config_path, "POOL", LIST_KEYS, ENUM_KEYS)?;

    config.set_log_dir(args.log_file);

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::tp_type::TemplateProviderType;

    /// Loads a full `PoolConfig` from `POOL__*` env vars alone, exercising the
    /// list and enum keys registered above. A structured field that is missing
    /// from `LIST_KEYS`/`ENUM_KEYS` fails this test even when the generic
    /// loader tests pass.
    #[test]
    fn pool_config_loads_from_env_only() {
        let vars = [
            ("POOL__LISTEN_ADDRESS", "0.0.0.0:3333"),
            (
                "POOL__AUTHORITY_PUBLIC_KEY",
                "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72",
            ),
            (
                "POOL__AUTHORITY_SECRET_KEY",
                "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n",
            ),
            ("POOL__CERT_VALIDITY_SEC", "3600"),
            (
                "POOL__COINBASE_REWARD_SCRIPT",
                "addr(tb1qa0sm0hxzj0x25rh8gw5xlzwlsfvvyz8u96w3p8)",
            ),
            ("POOL__POOL_SIGNATURE", "Stratum V2 SRI Pool"),
            ("POOL__SHARES_PER_MINUTE", "6.0"),
            ("POOL__SHARE_BATCH_SIZE", "10"),
            // Single values must come out as 1-element lists.
            ("POOL__SUPPORTED_EXTENSIONS", "2"),
            ("POOL__REQUIRED_EXTENSIONS", "2"),
            ("POOL__JDS__LISTEN_ADDRESS", "0.0.0.0:3334"),
            ("POOL__JDS__SUPPORTED_EXTENSIONS", "2,3"),
            ("POOL__JDS__REQUIRED_EXTENSIONS", "2"),
            (
                "POOL__TEMPLATE_PROVIDER_TYPE__SV2TP__ADDRESS",
                "127.0.0.1:8442",
            ),
        ];
        for (key, value) in vars {
            std::env::set_var(key, value);
        }

        let config: PoolConfig = load_config(
            "pool-config-does-not-exist.toml",
            "POOL",
            LIST_KEYS,
            ENUM_KEYS,
        )
        .expect("load PoolConfig from env only");

        assert_eq!(config.supported_extensions(), [2]);
        assert_eq!(config.required_extensions(), [2]);
        assert!(matches!(
            config.template_provider_type(),
            TemplateProviderType::Sv2Tp { .. }
        ));
        let jds = config
            .build_jds_config()
            .expect("build JDS config")
            .expect("jds section present");
        assert_eq!(jds.supported_extensions(), [2, 3]);
        assert_eq!(jds.required_extensions(), [2]);

        for (key, _) in vars {
            std::env::remove_var(key);
        }
    }
}
