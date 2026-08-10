use pool_sv2::PoolSv2;
use stratum_apps::config_helpers::logging::init_logging;

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
    tokio::spawn({
        let pool = pool.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl+C received — initiating graceful shutdown...");
                pool.shutdown().await;
            }
        }
    });

    if let Err(e) = pool.start().await {
        tracing::error!("Pool Error'ed out: {e}");
        std::process::exit(1);
    };
}
