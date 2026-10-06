use pool_sv2::PoolSv2;
use stratum_apps::{config_helpers::logging::init_logging, utils::shutdown::ShutdownSignal};

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
    let mut signals = ShutdownSignal::new().unwrap_or_else(|e| {
        tracing::error!("Pool could not listen for shutdown signals: {e}");
        std::process::exit(1);
    });
    tokio::spawn({
        let pool = pool.clone();
        async move {
            let (signal, _) = signals.wait().await;
            tracing::info!("{signal} received — initiating graceful shutdown...");
            tokio::select! {
                _ = pool.shutdown() => {}
                (signal, exit_code) = signals.wait() => {
                    tracing::warn!("{signal} received again — abandoning graceful shutdown");
                    std::process::exit(exit_code);
                }
            }
        }
    });

    if let Err(e) = pool.start().await {
        tracing::error!("Pool Error'ed out: {e}");
        std::process::exit(1);
    };
}
