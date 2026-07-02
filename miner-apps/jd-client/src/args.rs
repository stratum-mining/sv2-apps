use clap::Parser;
use jd_client_sv2::{config::JobDeclaratorClientConfig, error::JDCErrorKind};
use stratum_apps::config_helpers::load_config;

use std::path::PathBuf;
use tracing::error;
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

#[allow(clippy::result_large_err)]
pub fn process_cli_args() -> Result<JobDeclaratorClientConfig, JDCErrorKind> {
    let args = Args::parse();

    let config_path = args.config_path.to_str().ok_or_else(|| {
        error!("Invalid configuration path.");
        JDCErrorKind::BadCliArgs
    })?;

    // Env vars prefixed `JDC__` override values from the optional TOML file.
    let mut config: JobDeclaratorClientConfig = load_config(
        config_path,
        "JDC",
        &["supported_extensions", "required_extensions"],
    )?;

    config.set_log_file(args.log_file);

    Ok(config)
}
