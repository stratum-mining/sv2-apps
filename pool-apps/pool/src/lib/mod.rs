use error::PoolErrorKind;
use pool_runtime::{Init, PoolRuntime};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use stratum_apps::bitcoin_core_sv2::CancellationToken;
use tokio::sync::Notify;
use tracing::info;

use crate::config::PoolConfig;

pub mod channel_manager;
pub mod config;
pub mod downstream;
pub mod error;
mod io_task;
#[cfg(feature = "monitoring")]
mod monitoring;
mod pool_runtime;
pub mod template_receiver;
pub mod utils;

#[derive(Debug, Clone)]
pub struct PoolSv2 {
    config: PoolConfig,
    cancellation_token: CancellationToken,
    shutdown_notify: Arc<Notify>,
    is_alive: Arc<AtomicBool>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl PoolSv2 {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            cancellation_token: CancellationToken::new(),
            shutdown_notify: Arc::new(Notify::new()),
            is_alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Starts the Pool server and blocks asynchronously on the `PoolRuntime`.
    ///
    /// The startup and execution sequence follows:
    /// 1. **Initialize:** Sets up the pool runtime state machine.
    /// 2. **Bootstrap:** Configures internal channels, starts the Job Declarator Server (JDS),
    ///    connects to the Template Provider, and initializes the Channel Manager.
    /// 3. **Run & Block:** Spawns active background loops/servers and blocks the caller while
    ///    awaiting the runtime's shutdown signal (e.g., Ctrl+C or program cancellation).
    /// 4. **Teardown:** Performs a coordinated graceful cleanup of all services and tasks,
    ///    remaining blocked until all sub-services have exited.
    ///
    /// If any error occurs during bootstrapping, `start` receives the partially initialized
    /// runtime, gracefully shuts it down, and then returns the error.
    pub async fn start(&self) -> Result<(), PoolErrorKind> {
        let runtime = PoolRuntime::<Init>::new(self.clone())?;

        let runtime = match runtime.bootstrap().await {
            Ok(runtime) => runtime,
            Err(err) => {
                let (err, runtime) = err.into_parts();
                runtime.shutdown().await;
                return Err(err);
            }
        };

        runtime.wait_for_shutdown().await;
        runtime.shutdown().await;

        Ok(())
    }

    pub async fn shutdown(&self) {
        if !self.is_alive.load(Ordering::Relaxed) {
            return;
        }
        // The Notified future is guaranteed to receive wakeups from notify_waiters()
        // as soon as it has been created, even if it has not yet been polled.
        let notified = self.shutdown_notify.notified();
        self.cancellation_token.cancel();
        notified.await;
    }
}

impl Drop for PoolSv2 {
    fn drop(&mut self) {
        info!("PoolSv2 dropped");
        self.cancellation_token.cancel();
    }
}
