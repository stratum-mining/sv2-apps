use jd_client_sv2::JobDeclaratorClient;
use stratum_apps::{config_helpers::logging::init_logging, utils::shutdown::ShutdownListener};

use crate::args::process_cli_args;

mod args;

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

#[cfg_attr(not(test), hotpath::main(limit = 0))]
async fn inner_main() {
    let jdc_config = process_cli_args().unwrap_or_else(|e| {
        eprintln!("Job Declarator Client config error: {e}");
        std::process::exit(1);
    });

    init_logging(jdc_config.log_file());

    let jdc = JobDeclaratorClient::new(jdc_config);
    let mut shutdown_listener = ShutdownListener::new().unwrap_or_else(|e| {
        tracing::error!("Job Declarator Client could not listen for shutdown signals: {e}");
        std::process::exit(1);
    });
    tokio::spawn({
        let jdc = jdc.clone();
        async move {
            let signal = shutdown_listener.wait().await;
            tracing::info!("{signal} received — initiating graceful shutdown...");
            tokio::select! {
                _ = jdc.shutdown() => {}
                signal = shutdown_listener.wait() => {
                    tracing::warn!("{signal} received again — abandoning graceful shutdown");
                    std::process::exit(signal.exit_code());
                }
            }
        }
    });

    if jdc.start().await.is_err() {
        std::process::exit(1);
    }
}
