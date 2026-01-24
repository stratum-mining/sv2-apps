use pool_sv2::PoolSv2;
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
    let config = process_cli_args();
    init_logging(config.log_dir());
    if let Err(e) = PoolSv2::new(config).start().await {
        tracing::error!("Pool Error'ed out: {e}");
    };
}
