use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use error::JDCErrorKind;
use jdc_runtime::{Init, JdcRuntime, RuntimeEvent};
use stratum_apps::bitcoin_core_sv2::CancellationToken;
use tokio::sync::Notify;
use tracing::{error, info};

use crate::config::JobDeclaratorClientConfig;

mod channel_manager;
pub mod config;
mod downstream;
pub mod error;
mod io_task;
pub mod jd_mode;
mod jdc_runtime;
mod job_declarator;
#[cfg(feature = "monitoring")]
pub mod monitoring;
mod template_receiver;
mod upstream;
pub mod utils;

/// Represent Job Declarator Client
#[derive(Clone)]
pub struct JobDeclaratorClient {
    config: JobDeclaratorClientConfig,
    cancellation_token: CancellationToken,
    shutdown_notify: Arc<Notify>,
    is_alive: Arc<AtomicBool>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl JobDeclaratorClient {
    /// Creates a new [`JobDeclaratorClient`] instance.
    pub fn new(config: JobDeclaratorClientConfig) -> Self {
        Self {
            config,
            cancellation_token: CancellationToken::new(),
            shutdown_notify: Arc::new(Notify::new()),
            is_alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Marks the JDC as stopped and releases anyone blocked on [`JobDeclaratorClient::shutdown`].
    ///
    /// Called by [`JdcRuntime::shutdown`] once teardown completes, and directly by
    /// [`JobDeclaratorClient::start`] on the failure path where no runtime exists to tear down.
    fn mark_stopped(&self) {
        self.is_alive.store(false, Ordering::Release);
        self.cancellation_token.cancel();
        self.shutdown_notify.notify_waiters();
    }

    /// Starts the main event loop for the Job Declarator Client (JDC).
    ///
    /// The startup and execution sequence follows:
    /// 1. **Initialize:** Sets up the JDC runtime state machine ([`JdcRuntime`]).
    /// 2. **Bootstrap:** Configures internal channels, connects to the Template Provider,
    ///    initializes the Channel Manager, and establishes upstream SV2 connections or solo mining.
    /// 3. **Run & Loop:** Spawns active background loops/servers and handles fallbacks or graceful
    ///    shutdown.
    /// 4. **Teardown:** Performs a coordinated graceful cleanup of all services and tasks upon
    ///    termination.
    pub async fn start(&self) -> Result<(), JDCErrorKind> {
        let runtime = match JdcRuntime::<Init>::new(self.clone()) {
            Ok(runtime) => runtime,
            Err(e) => {
                self.mark_stopped();
                return Err(e);
            }
        };

        let mut running = match runtime.bootstrap().await {
            Ok(running) => running,
            Err(bootstrap_err) => {
                let (kind, runtime) = bootstrap_err.into_parts();
                error!(?kind, "Failed to bootstrap JDC");
                runtime.shutdown().await;
                return Err(kind);
            }
        };

        loop {
            match running.wait().await {
                RuntimeEvent::Shutdown => {
                    running.shutdown().await;
                    return Ok(());
                }
                RuntimeEvent::Fallback => {
                    let tp_ready_runtime = running.cleanup_for_fallback().await;

                    running = match tp_ready_runtime.bootstrap_mining().await {
                        Ok(new_running) => new_running,
                        Err(bootstrap_err) => {
                            let (kind, runtime) = bootstrap_err.into_parts();
                            error!(?kind, "Failed to reconnect JDC");
                            runtime.shutdown().await;
                            return Err(kind);
                        }
                    };
                }
            }
        }
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

impl Drop for JobDeclaratorClient {
    fn drop(&mut self) {
        info!("JobDeclaratorClient dropped");
        self.cancellation_token.cancel();
    }
}
