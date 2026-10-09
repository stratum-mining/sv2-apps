mod args;
use stratum_apps::{config_helpers::logging::init_logging, utils::shutdown::ShutdownSignal};
pub use translator_sv2::{TranslatorSv2, config, error, sv1, sv2};

use crate::args::process_cli_args;

#[cfg(all(feature = "hotpath-alloc", not(test)))]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    inner_main().await;
}

#[cfg(not(all(feature = "hotpath-alloc", not(test))))]
#[tokio::main]
async fn main() {
    inner_main().await;
}

/// Entrypoint for the Translator binary.
///
/// Loads the configuration from TOML and initializes the main runtime
/// defined in `translator_sv2::TranslatorSv2`. Errors during startup are logged.
#[cfg_attr(not(test), hotpath::main(limit = 0))]
async fn inner_main() {
    let proxy_config = process_cli_args().unwrap_or_else(|e| {
        eprintln!("Translator proxy config error: {e}");
        std::process::exit(1);
    });

    init_logging(proxy_config.log_dir());

    let translator = TranslatorSv2::new(proxy_config);
    let mut signals = ShutdownSignal::new().unwrap_or_else(|e| {
        tracing::error!("Translator proxy could not listen for shutdown signals: {e}");
        std::process::exit(1);
    });
    tokio::spawn({
        let translator = translator.clone();
        async move {
            let (signal, _) = signals.wait().await;
            tracing::info!("{signal} received — initiating graceful shutdown...");
            tokio::select! {
                _ = translator.shutdown() => {}
                (signal, exit_code) = signals.wait() => {
                    tracing::warn!("{signal} received again — abandoning graceful shutdown");
                    std::process::exit(exit_code);
                }
            }
        }
    });

    if translator.start().await.is_err() {
        std::process::exit(1);
    };
}
