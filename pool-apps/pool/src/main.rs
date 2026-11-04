use pool_sv2::PoolSv2;
use stratum_apps::{config_helpers::logging::init_logging, utils::shutdown::ShutdownListener};
use tracing::{error, info, warn};

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
    let config = process_cli_args().unwrap_or_else(|e| {
        eprintln!("Pool config error: {e}");
        std::process::exit(1);
    });
    init_logging(config.log_dir());

    let pool = PoolSv2::new(config);
    let mut shutdown_listener = ShutdownListener::new().unwrap_or_else(|e| {
        error!(error = %e, "Failed to listen for shutdown signals");
        std::process::exit(1);
    });
    tokio::spawn({
        let pool = pool.clone();
        async move {
            let signal = shutdown_listener.wait().await;
            info!(signal = %signal, "Initiating graceful shutdown");
            tokio::select! {
                _ = pool.shutdown() => {}
                signal = shutdown_listener.wait() => {
                    warn!(signal = %signal, "Abandoning graceful shutdown; exiting immediately");
                    std::process::exit(signal.exit_code());
                }
            }
        }
    });

    if let Err(e) = pool.start().await {
        error!(error = %e, "Pool encountered a fatal error");
        std::process::exit(1);
    };
}
