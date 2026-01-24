use jd_client_sv2::JobDeclaratorClient;
use stratum_apps::config_helpers::logging::init_logging;

use crate::args::process_cli_args;
use mimalloc::MiMalloc;

// Add mimalloc
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

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

#[cfg_attr(not(test), hotpath::main)]
async fn inner_main() {
    let jdc_config = process_cli_args().unwrap_or_else(|e| {
        eprintln!("Job Declarator Client config error: {e}");
        std::process::exit(1);
    });

    init_logging(jdc_config.log_file());
    JobDeclaratorClient::new(jdc_config).start().await;
}
