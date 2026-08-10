use jd_client_sv2::JobDeclaratorClient;
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
    let jdc_config = process_cli_args().unwrap_or_else(|e| {
        eprintln!("Job Declarator Client config error: {e}");
        std::process::exit(1);
    });

    init_logging(jdc_config.log_file());

    let jdc = JobDeclaratorClient::new(jdc_config);
    tokio::spawn({
        let jdc = jdc.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl+C received — initiating graceful shutdown...");
                jdc.shutdown().await;
            }
        }
    });

    if jdc.start().await.is_err() {
        std::process::exit(1);
    }
}
