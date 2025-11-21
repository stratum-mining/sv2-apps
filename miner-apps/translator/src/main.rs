mod args;
use std::process;

use stratum_apps::config_helpers::logging::init_logging;
pub use translator_sv2::{config, error, status, sv1, sv2, TranslatorSv2};

use crate::args::process_cli_args;

/// Entrypoint for the Translator binary.
///
/// Loads the configuration from TOML and initializes the main runtime
/// defined in `translator_sv2::TranslatorSv2`. Errors during startup are logged.
#[tokio::main]
async fn main() {
    let proxy_config = process_cli_args().unwrap_or_else(|e| {
        eprintln!("Translator proxy config error: {e}");
        std::process::exit(1);
    });

    init_logging(proxy_config.log_dir());

    // Install panic hook to log panics before crash
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        eprintln!("PANIC at {}: {}", location, message);
        eprintln!(
            "Backtrace:\n{:?}",
            std::backtrace::Backtrace::force_capture()
        );
    }));

    TranslatorSv2::new(proxy_config).start().await;

    process::exit(1);
}
