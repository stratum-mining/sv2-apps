use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::CancellationToken;
use stratum_apps::{
    fallback_coordinator::FallbackCoordinator,
    stratum_core::{bitcoin::consensus::Encodable, parsers_sv2::JobDeclaration},
    task_manager::TaskManager,
    tp_type::TemplateProviderType,
    utils::types::{Sv2Frame, GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS},
};
use tokio::sync::{broadcast, Notify};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::ChannelManager,
    config::{ConfigJDCMode, JobDeclaratorClientConfig},
    error::JDCErrorKind,
    jd_mode::{set_jd_mode, JdMode},
    job_declarator::JobDeclarator,
    status::{State, Status},
    template_receiver::{
        bitcoin_core::{connect_to_bitcoin_core, BitcoinCoreSv2Config},
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
mod status;
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
        info!(
            "Job declarator client starting... setting up subsystems, User Identity: {}",
            self.config.user_identity()
        );

        let miner_coinbase_outputs = vec![self.config.get_txout()];
        let mut encoded_outputs = vec![];

        miner_coinbase_outputs
            .consensus_encode(&mut encoded_outputs)
            .expect("Invalid coinbase output in config");

        let mut fallback_coordinator = FallbackCoordinator::new();
        let task_manager = Arc::new(TaskManager::new());

        let (status_sender, status_receiver) = async_channel::unbounded::<Status>();

        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();

        let (channel_manager_to_jd_sender, channel_manager_to_jd_receiver) = unbounded();
        let (jd_to_channel_manager_sender, jd_to_channel_manager_receiver) = unbounded();

        let (channel_manager_to_downstream_sender, _channel_manager_to_downstream_receiver) =
            broadcast::channel(10);
        let (downstream_to_channel_manager_sender, downstream_to_channel_manager_receiver) =
            unbounded();

        let (channel_manager_to_tp_sender, channel_manager_to_tp_receiver) = unbounded();
        let (tp_to_channel_manager_sender, tp_to_channel_manager_receiver) = unbounded();

        debug!("Channels initialized.");

        let mut channel_manager = ChannelManager::new(
            self.config.clone(),
            channel_manager_to_upstream_sender.clone(),
            upstream_to_channel_manager_receiver.clone(),
            channel_manager_to_jd_sender.clone(),
            jd_to_channel_manager_receiver.clone(),
            channel_manager_to_tp_sender.clone(),
            tp_to_channel_manager_receiver.clone(),
            channel_manager_to_downstream_sender.clone(),
            downstream_to_channel_manager_receiver,
            encoded_outputs.clone(),
            self.config.supported_extensions().to_vec(),
            self.config.required_extensions().to_vec(),
        )
        .await
        .unwrap();

        // Start monitoring server if configured
        #[cfg(feature = "monitoring")]
        if let Some(monitoring_addr) = self.config.monitoring_address() {
            info!(
                "Initializing monitoring server on http://{}",
                monitoring_addr
            );

            let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
                monitoring_addr,
                Some(Arc::new(channel_manager.clone())), // SV2 channels opened with servers
                Some(Arc::new(channel_manager.clone())), // SV2 channels opened with clients
                std::time::Duration::from_secs(
                    self.config.monitoring_cache_refresh_secs().unwrap_or(15),
                ),
            )
            .expect("Failed to initialize monitoring server");

            // Create shutdown signal using cancellation token
            let cancellation_token_clone = self.cancellation_token.clone();
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

            let fallback_coordinator_clone = fallback_coordinator.clone();
            task_manager.spawn(async move {
                // we just spawned a new task that's relevant to fallback coordination
                // so register it with the fallback coordinator
                let fallback_handler = fallback_coordinator_clone.register();

                if let Err(e) = monitoring_server.run(shutdown_signal).await {
                    error!("Monitoring server error: {:?}", e);
                }

                // signal fallback coordinator that this task has completed its cleanup
                fallback_handler.done();
                info!("Monitoring server task exited and signaled fallback coordinator");
            });
        }

        let channel_manager_clone = channel_manager.clone();
        let mut bitcoin_core_sv2_join_handle: Option<JoinHandle<()>> = None;

        match self.config.template_provider_type().clone() {
            TemplateProviderType::Sv2Tp {
                address,
                public_key,
            } => {
                let template_receiver = Sv2Tp::new(
                    address.clone(),
                    public_key,
                    channel_manager_to_tp_receiver,
                    tp_to_channel_manager_sender,
                    self.cancellation_token.clone(),
                    fallback_coordinator.clone(),
                    task_manager.clone(),
                )
                .await
                .unwrap();

                let cancellation_token_tp = self.cancellation_token.clone();
                let status_sender_cl = status_sender.clone();
                let task_manager_cl = task_manager.clone();

                template_receiver
                    .start(
                        address,
                        cancellation_token_tp,
                        status_sender_cl,
                        task_manager_cl,
                    )
                    .await;

                info!("Sv2 Template Provider setup done");
            }
            TemplateProviderType::BitcoinCoreIpc {
                network,
                data_dir,
                fee_threshold,
                min_interval,
            } => {
                let unix_socket_path = stratum_apps::tp_type::resolve_ipc_socket_path(
                    &network, data_dir,
                )
                .expect(
                    "Could not determine Bitcoin data directory. Please set data_dir in config.",
                );

                info!(
                    "Using Bitcoin Core IPC socket at: {}",
                    unix_socket_path.display()
                );

                // incoming and outgoing TDP channels from the perspective of BitcoinCoreSv2
                let incoming_tdp_receiver = channel_manager_to_tp_receiver.clone();
                let outgoing_tdp_sender = tp_to_channel_manager_sender.clone();

                let bitcoin_core_config = BitcoinCoreSv2Config {
                    unix_socket_path,
                    fee_threshold,
                    min_interval,
                    incoming_tdp_receiver,
                    outgoing_tdp_sender,
                    cancellation_token: CancellationToken::new(),
                };

                bitcoin_core_sv2_join_handle = Some(
                    connect_to_bitcoin_core(
                        bitcoin_core_config,
                        self.cancellation_token.clone(),
                        task_manager.clone(),
                        status_sender.clone(),
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
            })
            .collect();

        channel_manager
            .clone()
            .start(
                self.cancellation_token.clone(),
                fallback_coordinator.clone(),
                status_sender.clone(),
                task_manager.clone(),
                miner_coinbase_outputs.clone(),
            )
            .await;

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
                self.config.mode.clone(),
                task_manager.clone(),
            )
            .await
        {
            Ok((upstream, job_declarator)) => {
                upstream
                    .start(
                        self.config.min_supported_version(),
                        self.config.max_supported_version(),
                        self.cancellation_token.clone(),
                        fallback_coordinator.clone(),
                        status_sender.clone(),
                        task_manager.clone(),
                    )
                    .await;

                job_declarator
                    .start(
                        self.cancellation_token.clone(),
                        fallback_coordinator.clone(),
                        status_sender.clone(),
                        task_manager.clone(),
                    )
                    .await;

                channel_manager_clone
                    .upstream_state
                    .set(UpstreamState::NoChannel);
                _ = channel_manager_clone.allocate_tokens(2).await;
            }
            Err(e) => {
                tracing::error!("Failed to initialize upstream: {:?}", e);
                set_jd_mode(jd_mode::JdMode::SoloMining);
            }
        };

        _ = channel_manager_clone
            .clone()
            .start_downstream_server(
                *self.config.authority_public_key(),
                *self.config.authority_secret_key(),
                self.config.cert_validity_sec(),
                *self.config.listening_address(),
                task_manager.clone(),
                self.cancellation_token.clone(),
                fallback_coordinator.clone(),
                status_sender.clone(),
                downstream_to_channel_manager_sender.clone(),
                channel_manager_to_downstream_sender.clone(),
                self.config.supported_extensions().to_vec(),
                self.config.required_extensions().to_vec(),
            )
            .await;

        info!("Spawning status listener task...");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received — initiating graceful shutdown...");
                    self.cancellation_token.cancel();
                    break;
                }
                _ = self.cancellation_token.cancelled() => {
                    break;
                }
                message = status_receiver.recv() => {
                    if let Ok(status) = message {
                        match status.state {
                            State::DownstreamShutdown{downstream_id,..} => {
                                warn!("Downstream {downstream_id:?} disconnected — cleaning up channel manager.");
                                // Clean up channel manager state
                                if let Err(e) = channel_manager.remove_downstream(downstream_id) {
                                    error!("Failed to remove downstream {downstream_id:?}: {e:?}... initiating full shutdown.");
                                    self.cancellation_token.cancel();
                                    break;
                                }
                            }
                            State::TemplateReceiverShutdown(_) => {
                                warn!("Template Receiver shutdown requested — initiating full shutdown.");
                                self.cancellation_token.cancel();
                                break;
                            }
                            State::ChannelManagerShutdown(_) => {
                                warn!("Channel Manager shutdown requested — initiating full shutdown.");
                                self.cancellation_token.cancel();
                                break;
                            }
                            State::UpstreamShutdownFallback(_) | State::JobDeclaratorShutdownFallback(_) => {
                                warn!("Upstream/Job Declarator connection dropped — attempting reconnection...");

                                // trigger fallback and wait for all components to finish cleanup
                                fallback_coordinator.trigger_fallback_and_wait().await;
                                info!("All components finished fallback cleanup");

                                // Drain any buffered status messages from old components
                                while let Ok(old_status) = status_receiver.try_recv() {
                                    debug!("Draining buffered status message: {:?}", old_status.state);
                                }

                                set_jd_mode(JdMode::SoloMining);
                                info!("Existing Upstream or JD instance taken out. Preparing fallback.");

                                // Create a fresh FallbackCoordinator for the reconnection attempt
                                fallback_coordinator = FallbackCoordinator::new();

                                // Recreate channels (old ones were closed during fallback)
                                let (channel_manager_to_upstream_sender_new, channel_manager_to_upstream_receiver_new) =
                                    unbounded();
                                let (upstream_to_channel_manager_sender_new, upstream_to_channel_manager_receiver_new) =
                                    unbounded();
                                let (channel_manager_to_jd_sender_new, channel_manager_to_jd_receiver_new) = unbounded();
                                let (jd_to_channel_manager_sender_new, jd_to_channel_manager_receiver_new) = unbounded();

                                let (channel_manager_to_downstream_sender_new, _channel_manager_to_downstream_receiver_new) =
                                    broadcast::channel(10);
                                let (downstream_to_channel_manager_sender_new, downstream_to_channel_manager_receiver_new) =
                                    unbounded();

                                // Create a fresh channel_manager with new channels
                                channel_manager = ChannelManager::new(
                                    self.config.clone(),
                                    channel_manager_to_upstream_sender_new.clone(),
                                    upstream_to_channel_manager_receiver_new.clone(),
                                    channel_manager_to_jd_sender_new.clone(),
                                    jd_to_channel_manager_receiver_new.clone(),
                                    channel_manager_to_tp_sender.clone(),
                                    tp_to_channel_manager_receiver.clone(),
                                    channel_manager_to_downstream_sender_new.clone(),
                                    downstream_to_channel_manager_receiver_new.clone(),
                                    encoded_outputs.clone(),
                                    self.config.supported_extensions().to_vec(),
                                    self.config.required_extensions().to_vec(),
                                )
                                .await
                                .unwrap();

                                channel_manager
                                    .clone()
                                    .start(
                                        self.cancellation_token.clone(),
                                        fallback_coordinator.clone(),
                                        status_sender.clone(),
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
                                        self.config.mode.clone(),
                                        task_manager.clone(),
                                    )
                                    .await
                                {
                                    Ok((upstream, job_declarator)) => {
                                        upstream
                                            .start(
                                                self.config.min_supported_version(),
                                                self.config.max_supported_version(),
                                                self.cancellation_token.clone(),
                                                fallback_coordinator.clone(),
                                                status_sender.clone(),
                                                task_manager.clone(),
                                            )
                                            .await;

                                        job_declarator
                                            .start(
                                                self.cancellation_token.clone(),
                                                fallback_coordinator.clone(),
                                                status_sender.clone(),
                                                task_manager.clone(),
                                            )
                                            .await;

                                        channel_manager_clone.upstream_state.set(UpstreamState::NoChannel);

                                        _ = channel_manager_clone.allocate_tokens(2).await;
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to initialize upstream: {:?}", e);
                                        channel_manager_clone.upstream_state.set(UpstreamState::SoloMining);
                                        set_jd_mode(jd_mode::JdMode::SoloMining);
                                        info!("Fallback to solo mining mode");
                                    }
                                };

                                // Reinitialize monitoring server if configured
                                #[cfg(feature = "monitoring")]
                                if let Some(monitoring_addr) = self.config.monitoring_address() {
                                    info!(
                                        "Reinitializing monitoring server on http://{}",
                                        monitoring_addr
                                    );

                                    let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
                                        monitoring_addr,
                                        Some(Arc::new(channel_manager_clone.clone())),
                                        Some(Arc::new(channel_manager_clone.clone())),
                                        std::time::Duration::from_secs(self.config.monitoring_cache_refresh_secs().unwrap_or(15)),
                                    )
                                    .expect("Failed to initialize monitoring server");

                                    let cancellation_token_clone = self.cancellation_token.clone();
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

                                    let fallback_coordinator_clone = fallback_coordinator.clone();
                                    task_manager.spawn(async move {
                                        // we just spawned a new task that's relevant to fallback coordination
                                        // so register it with the fallback coordinator
                                        let fallback_handler = fallback_coordinator_clone.register();

                                        if let Err(e) = monitoring_server.run(shutdown_signal).await {
                                            error!("Monitoring server error: {:?}", e);
                                        }

                                        // signal that this task has completed its cleanup
                                        // (no-op during normal shutdown, only matters during fallback)
                                        fallback_handler.done();
                                        info!("Monitoring server task exited and signaled fallback coordinator");
                                    });
                                }

                                _ = channel_manager_clone.clone()
                                    .start_downstream_server(
                                        *self.config.authority_public_key(),
                                        *self.config.authority_secret_key(),
                                        self.config.cert_validity_sec(),
                                        *self.config.listening_address(),
                                        task_manager.clone(),
                                        self.cancellation_token.clone(),
                                        fallback_coordinator.clone(),
                                        status_sender.clone(),
                                        downstream_to_channel_manager_sender_new.clone(),
                                        channel_manager_to_downstream_sender_new.clone(),
                                        self.config.supported_extensions().to_vec(),
                                        self.config.required_extensions().to_vec(),
                                    )
                                    .await;
                                }
                        }
                    }
                }
            }
        }

        if let Some(bitcoin_core_sv2_join_handle) = bitcoin_core_sv2_join_handle {
            info!("Waiting for BitcoinCoreSv2 dedicated thread to shutdown...");
            match bitcoin_core_sv2_join_handle.join() {
                Ok(_) => info!("BitcoinCoreSv2 dedicated thread shutdown complete."),
                Err(e) => error!("BitcoinCoreSv2 dedicated thread error: {e:?}"),
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
        mode: ConfigJDCMode,
        task_manager: Arc<TaskManager>,
    ) -> Result<(Upstream, JobDeclarator), JDCErrorKind> {
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

            tokio::time::sleep(Duration::from_secs(1)).await;

            if upstream_entry.tried_or_flagged {
                info!(
                    "Upstream previously marked as malicious, skipping initial attempt warnings."
                );
                continue;
            }

            for attempt in 1..=MAX_RETRIES {
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
                    Ok(pair) => {
                        upstream_entry.tried_or_flagged = true;
                        return Ok(pair);
                    }
                    Err(e) => {
                        tracing::error!("Upstream and JDS connection terminated");
                        tokio::time::sleep(Duration::from_secs(1)).await;
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
    mode: ConfigJDCMode,
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
