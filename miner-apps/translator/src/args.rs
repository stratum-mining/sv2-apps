//! Defines the structure and parsing logic for command-line arguments.
//!
//! It provides the `Args` struct to hold parsed arguments,
//! and the `from_args` function to parse them from the command line.
use clap::Parser;
use std::path::PathBuf;
use stratum_apps::config_helpers::load_config;
use tracing::error;
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

/// Process CLI args, if any.
#[allow(clippy::result_large_err)]
pub fn process_cli_args() -> Result<TranslatorConfig, TproxyErrorKind> {
    // Parse CLI arguments
    let args = Args::parse();

    let config_path = args.config_path.to_str().ok_or_else(|| {
        error!("Invalid configuration path.");
        TproxyErrorKind::BadCliArgs
    })?;

    // Env vars prefixed `TPROXY__` override values from the optional TOML file.
    let mut config: TranslatorConfig = load_config(
        config_path,
        "TPROXY",
        &["supported_extensions", "required_extensions"],
    )?;

    config.set_log_dir(args.log_file);

    Ok(config)
}
