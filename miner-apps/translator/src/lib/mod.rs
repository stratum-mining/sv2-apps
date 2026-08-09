//! ## Translator Sv2
//!
//! Provides the core logic and main struct (`TranslatorSv2`) for running a
//! Stratum V1 to Stratum V2 translation proxy.
//!
//! This module orchestrates the interaction between downstream SV1 miners and upstream SV2
//! applications (proxies or pool servers).
//!
//! The central component is the `TranslatorSv2` struct, which encapsulates the state and
//! provides the `start` method as the main entry point for running the translator service.
//! It relies on several sub-modules (`config`, `downstream_sv1`, `upstream_sv2`, `proxy`, `status`,
//! etc.) for specialized functionalities.
#![allow(clippy::module_inception)]
use async_channel::{Receiver, Sender, unbounded};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use stratum_apps::{
    fallback_coordinator::FallbackCoordinator,
    payout::PayoutMode,
    task_manager::TaskManager,
    utils::types::{GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS, Sv2Frame},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub use stratum_apps::stratum_core::sv1_api::server_to_client;

use config::TranslatorConfig;

use crate::{
    error::TproxyErrorKind,
    sv1::Sv1Server,
    sv2::{ChannelManager, Upstream},
    utils::{TproxyMode, UpstreamEntry},
};

pub mod config;
pub mod error;
mod io_task;
#[cfg(feature = "monitoring")]
mod monitoring;
pub mod sv1;
pub mod sv2;
pub mod utils;

/// The main struct that manages the SV1/SV2 translator.
#[derive(Clone)]
pub struct TranslatorSv2 {
    config: TranslatorConfig,
    cancellation_token: CancellationToken,
    shutdown_notify: Arc<Notify>,
    is_alive: Arc<AtomicBool>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl TranslatorSv2 {
    /// Creates a new `TranslatorSv2`.
    ///
    /// Initializes the translator with the given configuration and sets up
    /// the reconnect wait time.
    pub fn new(config: TranslatorConfig) -> Self {
        Self {
            config,
            cancellation_token: CancellationToken::new(),
            shutdown_notify: Arc::new(Notify::new()),
            is_alive: Arc::new(AtomicBool::new(true)),
        }
    }

    fn payout_mode(&self, user_identity: &str) -> Result<Option<PayoutMode>, TproxyErrorKind> {
        let expected_payout_distribution = self
            .config
            .expected_payout_distribution(user_identity)
            .map_err(|e| {
                TproxyErrorKind::InvalidUserIdentity(format!(
                    "invalid payout user_identity `{user_identity}`: {e}"
                ))
            })?;
        if let Some(distribution) = &expected_payout_distribution {
            info!(
                "Payout verification enabled for configured user_identity: {}",
                distribution
            );
        }
        Ok(expected_payout_distribution)
    }

    /// Starts the translator.
    ///
    /// This method starts the main event loop, which handles connections,
    /// protocol translation, job management, and status reporting.
    pub async fn start(self) {
        info!("Starting Translator Proxy...");

        let cancellation_token = self.cancellation_token.clone();
        let mut fallback_coordinator = FallbackCoordinator::new();
        let tproxy_mode = TproxyMode::from(self.config.aggregate_channels);

        let task_manager = Arc::new(TaskManager::new());

        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();
        let (channel_manager_to_sv1_server_sender, channel_manager_to_sv1_server_receiver) =
            unbounded();
        let (sv1_server_to_channel_manager_sender, sv1_server_to_channel_manager_receiver) =
            unbounded();

        debug!("All inter-subsystem channels initialized");

        let mut upstream_addresses = self
            .config
            .upstreams
            .iter()
            .map(|u| UpstreamEntry {
                host: u.address.clone(),
                port: u.port,
                authority_pubkey: u.authority_pubkey,
                tried_or_flagged: false,
                user_identity: u.user_identity.clone(),
            })
            .collect::<Vec<_>>();

        let downstream_addr: SocketAddr = SocketAddr::new(
            self.config.downstream_address.parse().unwrap(),
            self.config.downstream_port,
        );

        let mut sv1_server = Arc::new(Sv1Server::new(
            downstream_addr,
            channel_manager_to_sv1_server_receiver,
            sv1_server_to_channel_manager_sender,
            self.config.clone(),
            tproxy_mode,
        ));

        info!("Initializing upstream connection...");

        let mut channel_manager: Arc<ChannelManager> = Arc::new(ChannelManager::new(
            channel_manager_to_upstream_sender,
            upstream_to_channel_manager_receiver,
            channel_manager_to_sv1_server_sender.clone(),
            sv1_server_to_channel_manager_receiver,
            self.config.supported_extensions.clone(),
            self.config.required_extensions.clone(),
            tproxy_mode,
            #[cfg(feature = "monitoring")]
            self.config.downstream_difficulty_config.enable_vardiff,
        ));

        if let Err(e) = self
            .initialize_upstream(
                &mut upstream_addresses,
                channel_manager_to_upstream_receiver.clone(),
                upstream_to_channel_manager_sender.clone(),
                cancellation_token.clone(),
                fallback_coordinator.clone(),
                task_manager.clone(),
                sv1_server.clone(),
                self.config.required_extensions.clone(),
                channel_manager.clone(),
            )
            .await
        {
            error!("Failed to initialize any upstream connection: {e:?}");
            self.shutdown_notify.notify_waiters();
            self.is_alive.store(false, Ordering::Release);
            return;
        }

        info!("Launching ChannelManager tasks...");
        ChannelManager::run_channel_manager_tasks(
            channel_manager.clone(),
            cancellation_token.clone(),
            fallback_coordinator.clone(),
            task_manager.clone(),
        )
        .await;

        // Start monitoring tasks if configured
        #[cfg(feature = "monitoring")]
        if let Some(monitoring_addr) = self.config.monitoring_address() {
            info!("Initializing monitoring server on http://{monitoring_addr}");
            if let Err(e) = self.start_monitoring_tasks(
                monitoring_addr,
                channel_manager.clone(),
                sv1_server.clone(),
                cancellation_token.clone(),
                fallback_coordinator.clone(),
                task_manager.clone(),
            ) {
                error!("Failed to initialize monitoring tasks: {e}");
                cancellation_token.cancel();
            }
        }

        let mut fallback_token = fallback_coordinator.token();

        loop {
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    break;
                }
                _ = fallback_token.cancelled() => {
                    info!("Preparing fallback");
                                // Trigger fallback and wait for all components to finish cleanup
                                fallback_coordinator.trigger_fallback_and_wait().await;
                                info!("All components finished fallback cleanup");

                                // Create a fresh FallbackCoordinator for the reconnection attempt
                                fallback_coordinator = FallbackCoordinator::new();
                                fallback_token = fallback_coordinator.token();

                                // Recreate channels and components (old ones were closed during fallback)
                                let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
                                    unbounded();
                                let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
                                    unbounded();
                                let (channel_manager_to_sv1_server_sender, channel_manager_to_sv1_server_receiver) =
                                    unbounded();
                                let (sv1_server_to_channel_manager_sender, sv1_server_to_channel_manager_receiver) =
                                    unbounded();

                                sv1_server = Arc::new(Sv1Server::new(
                                    downstream_addr,
                                    channel_manager_to_sv1_server_receiver,
                                    sv1_server_to_channel_manager_sender,
                                    self.config.clone(),
                                    tproxy_mode
                                ));

                                channel_manager = Arc::new(ChannelManager::new(
                                    channel_manager_to_upstream_sender,
                                    upstream_to_channel_manager_receiver,
                                    channel_manager_to_sv1_server_sender,
                                    sv1_server_to_channel_manager_receiver,
                                    self.config.supported_extensions.clone(),
                                    self.config.required_extensions.clone(),
                                    tproxy_mode,
                                    #[cfg(feature = "monitoring")]
                                    self.config.downstream_difficulty_config.enable_vardiff,
                                ));

                                if let Err(e) = self.initialize_upstream(
                                    &mut upstream_addresses,
                                    channel_manager_to_upstream_receiver,
                                    upstream_to_channel_manager_sender,
                                    cancellation_token.clone(),
                                    fallback_coordinator.clone(),
                                    task_manager.clone(),
                                    sv1_server.clone(),
                                    self.config.required_extensions.clone(),
                                    channel_manager.clone(),
                                )
                                .await
                                {
                                    error!("Couldn't perform fallback, shutting system down: {e:?}");
                                    cancellation_token.cancel();
                                    break;
                                }

                                info!("Launching ChannelManager tasks...");
                                ChannelManager::run_channel_manager_tasks(
                                    channel_manager.clone(),
                                    cancellation_token.clone(),
                                    fallback_coordinator.clone(),
                                    task_manager.clone(),
                                )
                                .await;

                                // Recreate monitoring tasks with new components
                                #[cfg(feature = "monitoring")]
                                if let Some(monitoring_addr) = self.config.monitoring_address() {
                                    info!("Reinitializing monitoring server on http://{monitoring_addr}");
                                    if let Err(e) = self.start_monitoring_tasks(
                                        monitoring_addr,
                                        channel_manager.clone(),
                                        sv1_server.clone(),
                                        cancellation_token.clone(),
                                        fallback_coordinator.clone(),
                                        task_manager.clone(),
                                    ) {
                                        error!("Failed to reinitialize monitoring tasks: {e}");
                                        cancellation_token.cancel();
                                        break;
                                    }
                                }

                                info!("Upstream and ChannelManager restarted successfully.");
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received — initiating graceful shutdown...");
                    cancellation_token.cancel();
                    break;
                }
            }
        }

        warn!(
            "Graceful shutdown: waiting {} seconds for tasks to finish",
            GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS),
            task_manager.join_all(),
        )
        .await
        {
            Ok(_) => {
                info!("All tasks joined cleanly");
            }
            Err(_) => {
                warn!(
                    "Tasks did not finish within {} seconds, aborting",
                    GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS
                );
                task_manager.abort_all().await;
                info!("Joining aborted tasks...");
                task_manager.join_all().await;
                warn!("Forced shutdown complete");
            }
        }
        self.shutdown_notify.notify_waiters();
        self.is_alive.store(false, Ordering::Release);
        info!("TranslatorSv2 shutdown complete.");
    }

    #[cfg(feature = "monitoring")]
    fn start_monitoring_tasks(
        &self,
        monitoring_addr: SocketAddr,
        channel_manager: Arc<ChannelManager>,
        sv1_server: Arc<Sv1Server>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) -> Result<(), TproxyErrorKind> {
        let refresh_interval =
            Duration::from_secs(self.config.monitoring_cache_refresh_secs().unwrap_or(15));

        let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
            monitoring_addr,
            Some(channel_manager.clone()), // SV2 channels opened with servers
            None,                          /* no SV2 channels opened with clients (SV1
                                            * handled separately) */
            refresh_interval,
        )
        .map_err(|e| {
            TproxyErrorKind::General(format!("failed to initialize monitoring server: {e}"))
        })?
        .with_sv1_monitoring(sv1_server.clone()) // SV1 client connections
        .map_err(|e| TproxyErrorKind::General(format!("failed to add SV1 monitoring: {e}")))?;

        // Create shutdown signal using cancellation token
        let cancellation_token_clone = cancellation_token.clone();
        let fallback_coordinator_token = fallback_coordinator.token();
        let shutdown_signal = async move {
            tokio::select! {
                _ = cancellation_token_clone.cancelled() => {
                    info!("Monitoring server: received shutdown signal.");
                }
                _ = fallback_coordinator_token.cancelled() => {
                    info!("Monitoring server: fallback triggered.");
                }
            }
        };

        let monitoring_fallback = fallback_coordinator.clone();
        task_manager.spawn({
            let cancellation_token = cancellation_token.clone();
            async move {
                // we just spawned a new task that's relevant to fallback coordination
                // so register it with the fallback coordinator
                let fallback_handler = monitoring_fallback.register();

                if let Err(e) = monitoring_server.run(shutdown_signal).await {
                    error!("Monitoring server error: {:?}", e);
                    cancellation_token.cancel();
                }

                // signal fallback coordinator that this task has completed its cleanup
                fallback_handler.done();
                info!("Monitoring server task exited and signaled fallback coordinator");
            }
        });

        let telemetry_fallback = fallback_coordinator.clone();
        task_manager.spawn({
            let cancellation_token = cancellation_token.clone();
            async move {
                let fallback_token = telemetry_fallback.token();
                let fallback_handler = telemetry_fallback.register();

                sv1_server
                    .run_miner_telemetry_loop(refresh_interval, cancellation_token, fallback_token)
                    .await;

                fallback_handler.done();
                info!("SV1 miner telemetry task exited and signaled fallback coordinator");
            }
        });

        Ok(())
    }

    pub async fn shutdown(&self) {
        if !self.is_alive.load(Ordering::Acquire) {
            return;
        }
        // The Notified future is guaranteed to receive wakeups from notify_waiters()
        // as soon as it has been created, even if it has not yet been polled.
        let notified = self.shutdown_notify.notified();
        self.cancellation_token.cancel();
        notified.await;
    }

    /// Initializes the upstream connection list, handling retries, fallbacks, and flagging.
    ///
    /// Upstreams are tried sequentially, each receiving a fixed number of retries before we
    /// advance to the next entry. This ensures we exhaust every healthy upstream before shutting
    /// the translator down.
    ///
    /// The `tried_or_flagged` flag in the `UpstreamEntry` acts as the upstream's state machine:
    ///  `false` means "never tried", while `true` means "already connected or marked as
    /// malicious". Once an upstream is flagged we skip it on future loops
    /// to avoid hammering known-bad endpoints during failover.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize_upstream(
        &self,
        upstreams: &mut [UpstreamEntry],
        channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
        upstream_to_channel_manager_sender: Sender<Sv2Frame>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        sv1_server_instance: Arc<Sv1Server>,
        required_extensions: Vec<u16>,
        channel_manager_instance: Arc<ChannelManager>,
    ) -> Result<(), TproxyErrorKind> {
        const MAX_RETRIES: usize = 3;
        let upstream_len = upstreams.len();
        for (i, upstream_entry) in upstreams.iter_mut().enumerate() {
            // Skip upstreams already marked as malicious. We’ve previously failed or
            // blacklisted them, so no need to warn or attempt reconnecting again.
            if upstream_entry.tried_or_flagged {
                debug!(
                    "Upstream previously marked as malicious, skipping initial attempt warnings."
                );
                continue;
            }

            info!(
                "Trying upstream {} of {}: {}:{}",
                i + 1,
                upstream_len,
                upstream_entry.host,
                upstream_entry.port
            );
            for attempt in 1..=MAX_RETRIES {
                info!("Connection attempt {}/{}...", attempt, MAX_RETRIES);
                tokio::time::sleep(Duration::from_secs(1)).await;

                match try_initialize_upstream(
                    upstream_entry,
                    upstream_to_channel_manager_sender.clone(),
                    channel_manager_to_upstream_receiver.clone(),
                    cancellation_token.clone(),
                    fallback_coordinator.clone(),
                    task_manager.clone(),
                    required_extensions.clone(),
                )
                .await
                {
                    Ok(()) => {
                        let user_identity = upstream_entry.user_identity.to_string();
                        let payout_mode = self.payout_mode(&user_identity)?;

                        channel_manager_instance.set_expected_payout_distribution(payout_mode);
                        sv1_server_instance.set_user_identity(user_identity);

                        // starting sv1 server instance
                        if let Err(e) = sv1_server_instance
                            .clone()
                            .start(
                                cancellation_token.clone(),
                                fallback_coordinator.clone(),
                                task_manager.clone(),
                            )
                            .await
                        {
                            error!("SV1 server startup failed: {e:?}");
                            return Err(e.kind);
                        }

                        upstream_entry.tried_or_flagged = true;
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            "Attempt {}/{} failed for {}:{}: {:?}",
                            attempt, MAX_RETRIES, upstream_entry.host, upstream_entry.port, e
                        );
                        if attempt == MAX_RETRIES {
                            warn!(
                                "Max retries reached for {}:{}, moving to next upstream",
                                upstream_entry.host, upstream_entry.port
                            );
                        }
                    }
                }
            }
            upstream_entry.tried_or_flagged = true;
        }

        tracing::error!("All upstreams failed after {} retries each", MAX_RETRIES);
        Err(TproxyErrorKind::CouldNotInitiateSystem)
    }
}

// Attempts to initialize a single upstream.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
async fn try_initialize_upstream(
    upstream_addr: &UpstreamEntry,
    upstream_to_channel_manager_sender: Sender<Sv2Frame>,
    channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
    cancellation_token: CancellationToken,
    fallback_coordinator: FallbackCoordinator,
    task_manager: Arc<TaskManager>,
    required_extensions: Vec<u16>,
) -> Result<(), TproxyErrorKind> {
    let upstream = Upstream::new(
        upstream_addr,
        upstream_to_channel_manager_sender,
        channel_manager_to_upstream_receiver,
        cancellation_token.clone(),
        fallback_coordinator.clone(),
        task_manager.clone(),
        required_extensions,
    )
    .await?;

    upstream
        .start(cancellation_token, fallback_coordinator, task_manager)
        .await?;
    Ok(())
}

impl Drop for TranslatorSv2 {
    fn drop(&mut self) {
        info!("TranslatorSv2 dropped");
        self.cancellation_token.cancel();
    }
}
