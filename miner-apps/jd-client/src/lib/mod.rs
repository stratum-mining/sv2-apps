#[cfg(feature = "monitoring")]
use std::net::SocketAddr;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    fallback_coordinator::FallbackCoordinator,
    stratum_core::{bitcoin::consensus::Encodable, parsers_sv2::JobDeclaration},
    task_manager::TaskManager,
    tp_type::TemplateProviderType,
    utils::types::{GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS, Sv2Frame},
};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::ChannelManager,
    config::JobDeclaratorClientConfig,
    error::JDCErrorKind,
    jd_mode::JDMode,
    job_declarator::JobDeclarator,
    template_receiver::{
        bitcoin_core::{BitcoinCoreSv2TDPConfig, connect_to_bitcoin_core},
        sv2_tp::Sv2Tp,
    },
    upstream::Upstream,
    utils::{UpstreamEntry, UpstreamState},
};

mod channel_manager;
pub mod config;
mod downstream;
pub mod error;
mod io_task;
pub mod jd_mode;
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

    /// Starts the Job Declarator Client (JDC) main loop.
    pub async fn start(&self) {
        info!("Job declarator client starting... setting up subsystems");

        let miner_coinbase_outputs = vec![self.config.get_txout()];
        let mut encoded_outputs = vec![];
        let mode = JDMode::new(self.config.mode);

        if let Err(e) = miner_coinbase_outputs.consensus_encode(&mut encoded_outputs) {
            error!(error = ?e, "Invalid coinbase output in config");
            self.cancellation_token.cancel();
            self.shutdown_notify.notify_waiters();
            self.is_alive.store(false, Ordering::Relaxed);
            return;
        }

        let mut fallback_coordinator = FallbackCoordinator::new();
        let task_manager = Arc::new(TaskManager::new());

        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();

        let (channel_manager_to_jd_sender, channel_manager_to_jd_receiver) = unbounded();
        let (jd_to_channel_manager_sender, jd_to_channel_manager_receiver) = unbounded();

        let (downstream_to_channel_manager_sender, downstream_to_channel_manager_receiver) =
            unbounded();

        let (channel_manager_to_tp_sender, channel_manager_to_tp_receiver) = unbounded();
        let (tp_to_channel_manager_sender, tp_to_channel_manager_receiver) = unbounded();

        debug!("Channels initialized.");

        let channel_manager = match ChannelManager::new(
            self.config.clone(),
            channel_manager_to_upstream_sender.clone(),
            upstream_to_channel_manager_receiver.clone(),
            channel_manager_to_jd_sender.clone(),
            jd_to_channel_manager_receiver.clone(),
            channel_manager_to_tp_sender.clone(),
            tp_to_channel_manager_receiver.clone(),
            downstream_to_channel_manager_receiver,
            encoded_outputs.clone(),
            self.config.supported_extensions().to_vec(),
            self.config.required_extensions().to_vec(),
            mode.clone(),
        )
        .await
        {
            Ok(channel_manager) => channel_manager,
            Err(e) => {
                error!(error = ?e, "Failed to initialize channel manager");
                self.cancellation_token.cancel();
                self.shutdown_notify.notify_waiters();
                self.is_alive.store(false, Ordering::Relaxed);
                return;
            }
        };

        // Start monitoring server if configured
        #[cfg(feature = "monitoring")]
        if let Some(monitoring_addr) = self.config.monitoring_address() {
            info!("Initializing monitoring server on http://{monitoring_addr}");
            if let Err(e) = self.start_monitoring_tasks(
                monitoring_addr,
                channel_manager.clone(),
                self.cancellation_token.clone(),
                fallback_coordinator.clone(),
                task_manager.clone(),
            ) {
                error!("Failed to initialize monitoring tasks: {e}");
                self.cancellation_token.cancel();
            }
        }

        let initial_channel_manager = channel_manager.clone();
        let mut bitcoin_core_sv2_join_handle: Option<JoinHandle<()>> = None;
        let mut bitcoin_core_sv2_cancellation_token: Option<CancellationToken> = None;

        match self.config.template_provider_type().clone() {
            TemplateProviderType::Sv2Tp {
                address,
                public_key,
            } => {
                let template_receiver = match Sv2Tp::new(
                    address.clone(),
                    public_key,
                    channel_manager_to_tp_receiver,
                    tp_to_channel_manager_sender,
                    self.cancellation_token.clone(),
                    task_manager.clone(),
                )
                .await
                {
                    Ok(template_receiver) => template_receiver,
                    Err(e) => {
                        error!(error = ?e, "Failed to initialize SV2 template receiver");
                        self.cancellation_token.cancel();
                        self.shutdown_notify.notify_waiters();
                        self.is_alive.store(false, Ordering::Relaxed);
                        return;
                    }
                };

                let cancellation_token_tp = self.cancellation_token.clone();
                let task_manager_cl = task_manager.clone();

                if let Err(e) = template_receiver
                    .start(address, cancellation_token_tp, task_manager_cl)
                    .await
                {
                    error!(error = ?e, "Failed to start SV2 template receiver");
                    self.cancellation_token.cancel();
                    self.shutdown_notify.notify_waiters();
                    self.is_alive.store(false, Ordering::Relaxed);
                    return;
                }

                info!("Sv2 Template Provider setup done");
            }
            TemplateProviderType::BitcoinCoreIpc {
                version,
                network,
                data_dir,
                fee_threshold,
                min_interval,
            } => {
                let unix_socket_path = match stratum_apps::tp_type::resolve_ipc_socket_path(
                    &network, data_dir,
                ) {
                    Some(unix_socket_path) => unix_socket_path,
                    None => {
                        error!(
                            "Could not determine Bitcoin data directory. Please set data_dir in config."
                        );
                        self.cancellation_token.cancel();
                        self.shutdown_notify.notify_waiters();
                        self.is_alive.store(false, Ordering::Relaxed);
                        return;
                    }
                };

                info!(
                    "Using Bitcoin Core IPC socket at: {}",
                    unix_socket_path.display()
                );

                // incoming and outgoing TDP channels from the perspective of BitcoinCoreSv2TDP
                let incoming_tdp_receiver = channel_manager_to_tp_receiver.clone();
                let outgoing_tdp_sender = tp_to_channel_manager_sender.clone();

                let bitcoin_core_cancellation_token = CancellationToken::new();
                let bitcoin_core_config = BitcoinCoreSv2TDPConfig {
                    version,
                    unix_socket_path,
                    fee_threshold,
                    min_interval,
                    incoming_tdp_receiver,
                    outgoing_tdp_sender,
                    cancellation_token: bitcoin_core_cancellation_token.clone(),
                };

                bitcoin_core_sv2_cancellation_token = Some(bitcoin_core_cancellation_token);
                bitcoin_core_sv2_join_handle = Some(
                    connect_to_bitcoin_core(
                        bitcoin_core_config,
                        self.cancellation_token.clone(),
                        task_manager.clone(),
                    )
                    .await,
                );
            }
        }

        let mut upstream_addresses: Vec<_> = self
            .config
            .upstreams()
            .iter()
            .map(|u| UpstreamEntry {
                pool_host: u.pool_address.clone(),
                pool_port: u.pool_port,
                jds_host: u.jds_address.clone(),
                jds_port: u.jds_port,
                authority_pubkey: u.authority_pubkey,
                tried_or_flagged: false,
                user_identity: u.user_identity.clone(),
            })
            .collect();

        channel_manager
            .clone()
            .start(
                self.cancellation_token.clone(),
                fallback_coordinator.clone(),
                task_manager.clone(),
                miner_coinbase_outputs.clone(),
            )
            .await;

        if self.config.mode == config::ConfigJDCMode::SoloMining {
            if !upstream_addresses.is_empty() {
                warn!(
                    "Solo mining mode configured but upstreams are present - they will be ignored"
                );
            }
            info!("Starting in solo mining mode");
            mode.set_solo_mining();
        } else if upstream_addresses.is_empty() {
            error!(
                "No upstreams configured for {:?} mode - at least one upstream is required",
                self.config.mode
            );
            self.cancellation_token.cancel();
        } else {
            info!("Attempting to initialize upstream...");

            match self
                .initialize_jd(
                    &mut upstream_addresses,
                    channel_manager_to_upstream_receiver.clone(),
                    upstream_to_channel_manager_sender.clone(),
                    channel_manager_to_jd_receiver.clone(),
                    jd_to_channel_manager_sender.clone(),
                    self.cancellation_token.clone(),
                    fallback_coordinator.clone(),
                    mode.clone(),
                    task_manager.clone(),
                )
                .await
            {
                Ok((upstream, job_declarator, user_identity)) => {
                    initial_channel_manager.set_user_identity(user_identity);

                    upstream
                        .start(
                            self.config.min_supported_version(),
                            self.config.max_supported_version(),
                            self.cancellation_token.clone(),
                            fallback_coordinator.clone(),
                            task_manager.clone(),
                        )
                        .await;

                    job_declarator
                        .start(
                            self.cancellation_token.clone(),
                            fallback_coordinator.clone(),
                            task_manager.clone(),
                        )
                        .await;

                    initial_channel_manager
                        .upstream_state
                        .set(UpstreamState::NoChannel);
                    _ = initial_channel_manager.allocate_tokens(2).await;
                }
                Err(e) => {
                    tracing::error!("Failed to initialize upstream: {:?}", e);
                    mode.set_solo_mining();
                }
            };
        }

        task_manager.spawn({
            let config = self.config.clone();
            let cancellation_token = self.cancellation_token.clone();
            let task_manager = task_manager.clone();
            let fallback_coordinator = fallback_coordinator.clone();
            async move {
                if let Err(e) = initial_channel_manager
                    .start_downstream_server(
                        *config.authority_public_key(),
                        *config.authority_secret_key(),
                        config.cert_validity_sec(),
                        *config.listening_address(),
                        task_manager,
                        cancellation_token.clone(),
                        fallback_coordinator,
                        downstream_to_channel_manager_sender,
                        config.supported_extensions().to_vec(),
                        config.required_extensions().to_vec(),
                    )
                    .await
                {
                    tracing::error!(?e, "Downstream server task exited with error");
                    cancellation_token.cancel();
                }
            }
        });

        info!("Spawning status listener task...");
        let mut fallback_token = fallback_coordinator.token();

        loop {
            tokio::select! {
                biased;

                _ = self.cancellation_token.cancelled() => {
                    break;
                }
                _ = fallback_token.cancelled() => {
                    warn!("Upstream/Job Declarator connection dropped — attempting reconnection...");

                    // trigger fallback and wait for all components to finish cleanup
                    fallback_coordinator.trigger_fallback_and_wait().await;
                    info!("All components finished fallback cleanup");

                    mode.set_solo_mining();
                    info!("Existing Upstream or JD instance taken out. Preparing fallback.");

                    // Create a fresh FallbackCoordinator for the reconnection attempt
                    fallback_coordinator = FallbackCoordinator::new();
                    fallback_token = fallback_coordinator.token();

                    // Recreate channels (old ones were closed during fallback)
                    let (channel_manager_to_upstream_sender_new, channel_manager_to_upstream_receiver_new) =
                        unbounded();
                    let (upstream_to_channel_manager_sender_new, upstream_to_channel_manager_receiver_new) =
                        unbounded();
                    let (channel_manager_to_jd_sender_new, channel_manager_to_jd_receiver_new) = unbounded();
                    let (jd_to_channel_manager_sender_new, jd_to_channel_manager_receiver_new) = unbounded();

                    let (downstream_to_channel_manager_sender_new, downstream_to_channel_manager_receiver_new) =
                        unbounded();

                    // Create a fresh channel_manager with new channels
                    let channel_manager = match ChannelManager::new(
                        self.config.clone(),
                        channel_manager_to_upstream_sender_new.clone(),
                        upstream_to_channel_manager_receiver_new.clone(),
                        channel_manager_to_jd_sender_new.clone(),
                        jd_to_channel_manager_receiver_new.clone(),
                        channel_manager_to_tp_sender.clone(),
                        tp_to_channel_manager_receiver.clone(),
                        downstream_to_channel_manager_receiver_new.clone(),
                        encoded_outputs.clone(),
                        self.config.supported_extensions().to_vec(),
                        self.config.required_extensions().to_vec(),
                        mode.clone(),
                    )
                    .await
                    {
                        Ok(channel_manager) => channel_manager,
                        Err(e) => {
                            error!(error = ?e, "Failed to initialize channel manager during fallback");
                            self.cancellation_token.cancel();
                            break;
                        }
                    };

                    channel_manager.clone()
                        .start(
                            self.cancellation_token.clone(),
                            fallback_coordinator.clone(),
                            task_manager.clone(),
                            miner_coinbase_outputs.clone(),
                        )
                        .await;

                    info!("Attempting to initialize Jd and upstream...");

                    match self
                        .initialize_jd(
                            &mut upstream_addresses,
                            channel_manager_to_upstream_receiver_new.clone(),
                            upstream_to_channel_manager_sender_new.clone(),
                            channel_manager_to_jd_receiver_new.clone(),
                            jd_to_channel_manager_sender_new.clone(),
                            self.cancellation_token.clone(),
                            fallback_coordinator.clone(),
                            mode.clone(),
                            task_manager.clone(),
                        )
                        .await
                    {
                        Ok((upstream, job_declarator, user_identity)) => {
                            channel_manager.set_user_identity(user_identity);

                            upstream
                                .start(
                                    self.config.min_supported_version(),
                                    self.config.max_supported_version(),
                                    self.cancellation_token.clone(),
                                    fallback_coordinator.clone(),
                                    task_manager.clone(),
                                )
                                .await;

                            job_declarator
                                .start(
                                    self.cancellation_token.clone(),
                                    fallback_coordinator.clone(),
                                    task_manager.clone(),
                                )
                                .await;

                            channel_manager
                                .upstream_state
                                .set(UpstreamState::NoChannel);

                            _ = channel_manager.allocate_tokens(2).await;
                        }
                        Err(e) => {
                            tracing::error!("Failed to initialize upstream: {:?}", e);
                            channel_manager
                                .upstream_state
                                .set(UpstreamState::SoloMining);
                            mode.set_solo_mining();
                            info!("Fallback to solo mining mode");
                        }
                    };

                    // Reinitialize monitoring server if configured
                    #[cfg(feature = "monitoring")]
                    if let Some(monitoring_addr) = self.config.monitoring_address() {
                        info!("Reinitializing monitoring server on http://{monitoring_addr}");
                        if let Err(e) = self.start_monitoring_tasks(
                            monitoring_addr,
                            channel_manager.clone(),
                            self.cancellation_token.clone(),
                            fallback_coordinator.clone(),
                            task_manager.clone(),
                        ) {
                            error!("Failed to reinitialize monitoring tasks: {e}");
                            self.cancellation_token.cancel();
                            break;
                        }
                    }

                    task_manager.spawn({
                        let config = self.config.clone();
                        let cancellation_token = self.cancellation_token.clone();
                        let task_manager = task_manager.clone();
                        let fallback_coordinator = fallback_coordinator.clone();
                        async move {
                            if let Err(e) = channel_manager
                                .start_downstream_server(
                                    *config.authority_public_key(),
                                    *config.authority_secret_key(),
                                    config.cert_validity_sec(),
                                    *config.listening_address(),
                                    task_manager,
                                    cancellation_token.clone(),
                                    fallback_coordinator,
                                    downstream_to_channel_manager_sender_new,
                                    config.supported_extensions().to_vec(),
                                    config.required_extensions().to_vec(),
                                )
                                .await {
                                    tracing::error!(?e, "Downstream server task exited with error");
                                    cancellation_token.cancel();
                                }
                        }
                    });
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received — initiating graceful shutdown...");
                    self.cancellation_token.cancel();
                    break;
                }
            }
        }

        if let Some(bitcoin_core_sv2_cancellation_token) = bitcoin_core_sv2_cancellation_token {
            bitcoin_core_sv2_cancellation_token.cancel();
        }

        if let Some(bitcoin_core_sv2_join_handle) = bitcoin_core_sv2_join_handle {
            info!("Waiting for BitcoinCoreSv2TDP dedicated thread to shutdown...");
            match bitcoin_core_sv2_join_handle.join() {
                Ok(_) => info!("BitcoinCoreSv2TDP dedicated thread shutdown complete."),
                Err(e) => error!("BitcoinCoreSv2TDP dedicated thread error: {e:?}"),
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
        self.is_alive.store(false, Ordering::Relaxed);
        info!("JD Client shutdown complete.");
    }

    #[cfg(feature = "monitoring")]
    fn start_monitoring_tasks(
        &self,
        monitoring_addr: SocketAddr,
        channel_manager: ChannelManager,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) -> Result<(), String> {
        let refresh_interval =
            Duration::from_secs(self.config.monitoring_cache_refresh_secs().unwrap_or(15));

        let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
            monitoring_addr,
            Some(Arc::new(channel_manager.clone())),
            Some(Arc::new(channel_manager.clone())),
            refresh_interval,
        )
        .map_err(|e| format!("failed to initialize monitoring server: {e}"))?;

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
                let fallback_handler = monitoring_fallback.register();

                if let Err(e) = monitoring_server.run(shutdown_signal).await {
                    error!("Monitoring server error: {:?}", e);
                    cancellation_token.cancel();
                }

                fallback_handler.done();
                info!("Monitoring server task exited and signaled fallback coordinator");
            }
        });

        let telemetry_fallback = fallback_coordinator.clone();
        task_manager.spawn({
            async move {
                let fallback_token = telemetry_fallback.token();
                let fallback_handler = telemetry_fallback.register();

                channel_manager
                    .run_miner_telemetry_loop(refresh_interval, cancellation_token, fallback_token)
                    .await;

                fallback_handler.done();
                info!("JDC miner telemetry task exited and signaled fallback coordinator");
            }
        });

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

    /// Initializes an upstream pool + JD connection pair.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize_jd(
        &self,
        upstreams: &mut [UpstreamEntry],
        channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
        upstream_to_channel_manager_sender: Sender<Sv2Frame>,
        channel_manager_to_jd_receiver: Receiver<JobDeclaration<'static>>,
        jd_to_channel_manager_sender: Sender<JobDeclaration<'static>>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        mode: JDMode,
        task_manager: Arc<TaskManager>,
    ) -> Result<(Upstream, JobDeclarator, String), JDCErrorKind> {
        const MAX_RETRIES: usize = 3;
        let upstream_len = upstreams.len();
        for (i, upstream_entry) in upstreams.iter_mut().enumerate() {
            info!(
                "Trying upstream {} of {}: pool={}:{}, jds={}:{}",
                i + 1,
                upstream_len,
                upstream_entry.pool_host,
                upstream_entry.pool_port,
                upstream_entry.jds_host,
                upstream_entry.jds_port,
            );

            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    info!("Shutdown requested while waiting to initialize upstream, aborting retries");
                    return Err(JDCErrorKind::CouldNotInitiateSystem);
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }

            if upstream_entry.tried_or_flagged {
                info!(
                    "Upstream previously marked as malicious, skipping initial attempt warnings."
                );
                continue;
            }

            for attempt in 1..=MAX_RETRIES {
                if cancellation_token.is_cancelled() {
                    info!(
                        "Shutdown requested before upstream connection attempt, aborting retries"
                    );
                    return Err(JDCErrorKind::CouldNotInitiateSystem);
                }

                info!("Connection attempt {}/{}...", attempt, MAX_RETRIES);

                match try_initialize_single(
                    upstream_entry,
                    upstream_to_channel_manager_sender.clone(),
                    channel_manager_to_upstream_receiver.clone(),
                    jd_to_channel_manager_sender.clone(),
                    channel_manager_to_jd_receiver.clone(),
                    cancellation_token.clone(),
                    fallback_coordinator.clone(),
                    mode.clone(),
                    task_manager.clone(),
                    &self.config,
                )
                .await
                {
                    Ok((upstream, jd)) => {
                        upstream_entry.tried_or_flagged = true;
                        return Ok((upstream, jd, upstream_entry.user_identity.clone()));
                    }
                    Err(e) => {
                        tracing::error!("Upstream and JDS connection terminated");

                        tokio::select! {
                            biased;
                            _ = cancellation_token.cancelled() => {
                                info!("Shutdown requested after upstream initialization failure, aborting retries");
                                return Err(JDCErrorKind::CouldNotInitiateSystem);
                            }
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }

                        warn!(
                            "Attempt {}/{} failed for pool={}:{}, jds={}:{}: {:?}",
                            attempt,
                            MAX_RETRIES,
                            upstream_entry.pool_host,
                            upstream_entry.pool_port,
                            upstream_entry.jds_host,
                            upstream_entry.jds_port,
                            e
                        );
                        if attempt == MAX_RETRIES {
                            warn!(
                                "Max retries reached for pool={}:{}, jds={}:{}, moving to next upstream",
                                upstream_entry.pool_host,
                                upstream_entry.pool_port,
                                upstream_entry.jds_host,
                                upstream_entry.jds_port,
                            );
                        }
                    }
                }
            }
            upstream_entry.tried_or_flagged = true;
        }

        tracing::error!("All upstreams failed after {} retries each", MAX_RETRIES);
        Err(JDCErrorKind::CouldNotInitiateSystem)
    }
}

// Attempts to initialize a single upstream (pool + JDS pair).
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
async fn try_initialize_single(
    upstream_entry: &UpstreamEntry,
    upstream_to_channel_manager_sender: Sender<Sv2Frame>,
    channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
    jd_to_channel_manager_sender: Sender<JobDeclaration<'static>>,
    channel_manager_to_jd_receiver: Receiver<JobDeclaration<'static>>,
    cancellation_token: CancellationToken,
    fallback_coordinator: FallbackCoordinator,
    mode: JDMode,
    task_manager: Arc<TaskManager>,
    config: &JobDeclaratorClientConfig,
) -> Result<(Upstream, JobDeclarator), JDCErrorKind> {
    info!("Upstream connection in-progress at initialize single");
    let upstream = Upstream::new(
        upstream_entry,
        upstream_to_channel_manager_sender,
        channel_manager_to_upstream_receiver,
        cancellation_token.clone(),
        fallback_coordinator.clone(),
        task_manager.clone(),
        config.required_extensions().to_vec(),
    )
    .await
    .map_err(|error| error.kind)?;

    info!("Upstream connection done at initialize single");

    let job_declarator = JobDeclarator::new(
        upstream_entry,
        jd_to_channel_manager_sender,
        channel_manager_to_jd_receiver,
        cancellation_token,
        fallback_coordinator,
        mode,
        task_manager.clone(),
    )
    .await
    .map_err(|error| error.kind)?;

    Ok((upstream, job_declarator))
}

impl Drop for JobDeclaratorClient {
    fn drop(&mut self) {
        info!("JobDeclaratorClient dropped");
        self.cancellation_token.cancel();
    }
}
