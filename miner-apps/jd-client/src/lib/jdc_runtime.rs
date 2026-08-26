//! ## JDC Runtime Module
//!
//! Provides [`JdcRuntime`], a structured state-machine orchestrating the Job Declarator
//! Client's (JDC) initialization, bootstrap stages, background service loops, and graceful teardown
//! or fallback.

use std::{sync::Arc, thread::JoinHandle, time::Duration};

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    fallback_coordinator::FallbackCoordinator,
    stratum_core::{
        bitcoin::{TxOut, consensus::Encodable},
        parsers_sv2::{JobDeclarationOwned, MiningOwned, TemplateDistributionOwned, Tlv},
    },
    task_manager::TaskManager,
    tp_type::{TemplateProviderType, resolve_ipc_socket_path},
    utils::types::{DownstreamId, GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS, InboundFrame, OutboundFrame},
};
use tracing::{error, info, warn};

use crate::{
    JobDeclaratorClient,
    channel_manager::ChannelManager,
    config::ConfigJDCMode,
    error::{Action, JDCError, JDCErrorKind, JDCResult},
    jd_mode::JDMode,
    job_declarator::JobDeclarator,
    template_receiver::{
        bitcoin_core::{BitcoinCoreSv2TDPConfig, connect_to_bitcoin_core},
        sv2_tp::Sv2Tp,
    },
    upstream::Upstream,
    utils::{UpstreamEntry, UpstreamState},
};
#[cfg(feature = "monitoring")]
use std::net::SocketAddr;

struct Io {
    channel_manager_to_upstream_sender: Sender<OutboundFrame>,
    channel_manager_to_upstream_receiver: Receiver<OutboundFrame>,
    upstream_to_channel_manager_sender: Sender<InboundFrame>,
    upstream_to_channel_manager_receiver: Receiver<InboundFrame>,
    channel_manager_to_jd_sender: Sender<JobDeclarationOwned>,
    channel_manager_to_jd_receiver: Receiver<JobDeclarationOwned>,
    jd_to_channel_manager_sender: Sender<JobDeclarationOwned>,
    jd_to_channel_manager_receiver: Receiver<JobDeclarationOwned>,
    downstream_to_channel_manager_sender: Sender<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
    downstream_to_channel_manager_receiver: Receiver<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
    channel_manager_to_tp_sender: Sender<TemplateDistributionOwned>,
    channel_manager_to_tp_receiver: Receiver<TemplateDistributionOwned>,
    tp_to_channel_manager_sender: Sender<TemplateDistributionOwned>,
    tp_to_channel_manager_receiver: Receiver<TemplateDistributionOwned>,
}

struct BitcoinCoreSv2Handle {
    join_handle: JoinHandle<()>,
    cancellation_token: CancellationToken,
}

/// The core coordinator of the JDC runtime, parameterized by its current bootstrap `State`.
///
/// It manages the lifecycle of essential sub-services and channels, ensuring resources
/// are correctly initialized, passed to background executors, and cleanly torn down.
pub(super) struct JdcRuntime<State> {
    miner_coinbase_outputs: Vec<TxOut>,
    encoded_outputs: Vec<u8>,
    mode: JDMode,
    fallback_coordinator: FallbackCoordinator,
    task_manager: Arc<TaskManager>,
    bitcoin_core_sv2: Option<BitcoinCoreSv2Handle>,
    jd_client: JobDeclaratorClient,
    upstream_addresses: Vec<UpstreamEntry>,
    state: State,
}

pub(super) struct Init;

struct IoReady {
    io: Io,
}

pub(super) struct TemplateProviderReady {
    io: Io,
}

struct ChannelManagerReady {
    io: Io,
    channel_manager: ChannelManager,
}

struct UpstreamReady {
    io: Io,
    channel_manager: ChannelManager,
}

struct SoloMiningReady {
    io: Io,
    channel_manager: ChannelManager,
}

pub(super) struct Running {
    io: Io,
}

pub(super) struct Failed;

#[must_use = "bootstrap errors include a partially initialized runtime that must be shut down"]
pub(super) struct BootstrapError {
    kind: JDCErrorKind,
    runtime: JdcRuntime<Failed>,
}

impl BootstrapError {
    pub(super) fn into_parts(self) -> (JDCErrorKind, JdcRuntime<Failed>) {
        (self.kind, self.runtime)
    }
}

impl<State> From<(JDCErrorKind, JdcRuntime<State>)> for BootstrapError {
    fn from((kind, runtime): (JDCErrorKind, JdcRuntime<State>)) -> Self {
        Self {
            kind,
            runtime: runtime.into_failed(),
        }
    }
}

impl<State> JdcRuntime<State> {
    #[cfg(feature = "monitoring")]
    fn start_monitoring_tasks(
        &self,
        channel_manager: &ChannelManager,
        monitoring_addr: SocketAddr,
    ) -> Result<(), String> {
        let refresh_interval = Duration::from_secs(
            self.jd_client
                .config
                .monitoring_cache_refresh_secs()
                .unwrap_or(15),
        );

        let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
            monitoring_addr,
            Some(Arc::new(channel_manager.clone())),
            Some(Arc::new(channel_manager.clone())),
            refresh_interval,
        )
        .map_err(|e| format!("failed to initialize monitoring server: {e}"))?;

        let cancellation_token_clone = self.jd_client.cancellation_token.clone();
        let fallback_coordinator_token = self.fallback_coordinator.token();
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

        let monitoring_fallback = self.fallback_coordinator.clone();
        self.task_manager.spawn({
            let cancellation_token = self.jd_client.cancellation_token.clone();
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

        let telemetry_fallback = self.fallback_coordinator.clone();
        let telemetry_cm = channel_manager.clone();
        self.task_manager.spawn({
            let cancellation_token = self.jd_client.cancellation_token.clone();
            async move {
                let fallback_token = telemetry_fallback.token();
                let fallback_handler = telemetry_fallback.register();

                telemetry_cm
                    .run_miner_telemetry_loop(refresh_interval, cancellation_token, fallback_token)
                    .await;

                fallback_handler.done();
                info!("JDC miner telemetry task exited and signaled fallback coordinator");
            }
        });

        Ok(())
    }

    async fn start_services_inner(
        &self,
        io: &Io,
        channel_manager: ChannelManager,
    ) -> Result<(), JDCErrorKind> {
        // Start monitoring server if configured
        #[cfg(feature = "monitoring")]
        if let Some(monitoring_addr) = self.jd_client.config.monitoring_address() {
            info!("Initializing monitoring server on http://{monitoring_addr}");
            self.start_monitoring_tasks(&channel_manager, monitoring_addr)
                .map_err(JDCErrorKind::MonitoringServerError)?;
        }

        let config = self.jd_client.config.clone();
        let cancellation_token = self.jd_client.cancellation_token.clone();
        let task_manager = self.task_manager.clone();
        let fallback_coordinator = self.fallback_coordinator.clone();
        let downstream_to_channel_manager_sender = io.downstream_to_channel_manager_sender.clone();

        channel_manager
            .clone()
            .start(
                cancellation_token.clone(),
                fallback_coordinator.clone(),
                task_manager.clone(),
                self.miner_coinbase_outputs.clone(),
            )
            .await
            .map_err(|e| e.kind)?;

        channel_manager
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
            .map_err(|e| e.kind)?;

        Ok(())
    }

    fn into_failed(self) -> JdcRuntime<Failed> {
        JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: Failed,
        }
    }

    /// Performs a coordinated, graceful shutdown of the runtime.
    ///
    /// Signals cancellation to all active sub-services and background tasks, awaiting
    /// their clean termination up to a configured graceful timeout, and finally marks the
    /// owning [`JobDeclaratorClient`] as stopped — callers do not need to do so themselves.
    pub(super) async fn shutdown(self) {
        self.jd_client.cancellation_token.cancel();

        if let Some(bitcoin_core_sv2) = self.bitcoin_core_sv2 {
            bitcoin_core_sv2.cancellation_token.cancel();

            info!("Waiting for BitcoinCoreSv2TDP dedicated thread to shutdown...");
            match bitcoin_core_sv2.join_handle.join() {
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
            self.task_manager.join_all(),
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
                self.task_manager.abort_all().await;
                info!("Joining aborted tasks...");
                self.task_manager.join_all().await;
                warn!("Forced shutdown complete");
            }
        }
        self.jd_client.mark_stopped();
        info!("JD Client shutdown complete.");
    }
}

impl JdcRuntime<Init> {
    pub(super) fn new(jd_client: JobDeclaratorClient) -> Result<Self, JDCErrorKind> {
        let miner_coinbase_outputs = vec![jd_client.config.get_txout()];
        let mut encoded_outputs = vec![];
        let mode = JDMode::new(jd_client.config.mode);
        let upstream_addresses = jd_client
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

        if miner_coinbase_outputs
            .consensus_encode(&mut encoded_outputs)
            .is_err()
        {
            return Err(JDCErrorKind::InvalidCoinbaseOutput);
        }

        Ok(JdcRuntime {
            miner_coinbase_outputs,
            encoded_outputs,
            mode,
            fallback_coordinator: FallbackCoordinator::new(),
            task_manager: Arc::new(TaskManager::new()),
            bitcoin_core_sv2: None,
            jd_client,
            upstream_addresses,
            state: Init,
        })
    }

    /// Drives the linear bootstrap sequence of the JDC, transitioning the runtime
    /// from [`Init`] to the active [`Running`] state.
    ///
    /// If an intermediate phase fails, the caller receives the partially initialized
    /// runtime and is responsible for shutting down any resources that were already started.
    pub(super) async fn bootstrap(self) -> Result<JdcRuntime<Running>, BootstrapError> {
        let runtime = self.bootstrap_io();

        let runtime = runtime.bootstrap_template_provider().await?;

        runtime.bootstrap_mining().await
    }

    fn bootstrap_io(self) -> JdcRuntime<IoReady> {
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

        JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: IoReady {
                io: Io {
                    channel_manager_to_upstream_sender,
                    channel_manager_to_upstream_receiver,
                    upstream_to_channel_manager_sender,
                    upstream_to_channel_manager_receiver,
                    channel_manager_to_jd_sender,
                    channel_manager_to_jd_receiver,
                    jd_to_channel_manager_sender,
                    jd_to_channel_manager_receiver,
                    downstream_to_channel_manager_sender,
                    downstream_to_channel_manager_receiver,
                    channel_manager_to_tp_sender,
                    channel_manager_to_tp_receiver,
                    tp_to_channel_manager_sender,
                    tp_to_channel_manager_receiver,
                },
            },
        }
    }
}

impl JdcRuntime<IoReady> {
    async fn bootstrap_template_provider(
        self,
    ) -> Result<JdcRuntime<TemplateProviderReady>, (JDCErrorKind, JdcRuntime<IoReady>)> {
        let mut bitcoin_core_sv2: Option<BitcoinCoreSv2Handle> = None;

        match self.jd_client.config.template_provider_type().clone() {
            TemplateProviderType::Sv2Tp {
                address,
                public_key,
            } => {
                let template_receiver = match Sv2Tp::new(
                    address.clone(),
                    public_key,
                    self.state.io.channel_manager_to_tp_receiver.clone(),
                    self.state.io.tp_to_channel_manager_sender.clone(),
                    self.jd_client.cancellation_token.clone(),
                    self.task_manager.clone(),
                )
                .await
                {
                    Ok(template_receiver) => template_receiver,
                    Err(e) => {
                        error!(error = ?e, "Failed to initialize SV2 template receiver");
                        return Err((e.kind, self));
                    }
                };

                let cancellation_token_tp = self.jd_client.cancellation_token.clone();
                let task_manager_cl = self.task_manager.clone();

                if let Err(e) = template_receiver
                    .start(address, cancellation_token_tp, task_manager_cl)
                    .await
                {
                    error!(error = ?e, "Failed to start SV2 template receiver");
                    return Err((e.kind, self));
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
                let unix_socket_path = match resolve_ipc_socket_path(&network, data_dir) {
                    Some(unix_socket_path) => unix_socket_path,
                    None => {
                        error!(
                            "Could not determine Bitcoin data directory. Please set data_dir in config."
                        );
                        return Err((JDCErrorKind::InvalidBitcoinDataDir, self));
                    }
                };

                info!(
                    "Using Bitcoin Core IPC socket at: {}",
                    unix_socket_path.display()
                );

                // incoming and outgoing TDP channels from the perspective of BitcoinCoreSv2TDP
                let incoming_tdp_receiver = self.state.io.channel_manager_to_tp_receiver.clone();
                let outgoing_tdp_sender = self.state.io.tp_to_channel_manager_sender.clone();

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

                bitcoin_core_sv2 = Some(BitcoinCoreSv2Handle {
                    join_handle: connect_to_bitcoin_core(
                        bitcoin_core_config,
                        self.jd_client.cancellation_token.clone(),
                        self.task_manager.clone(),
                    )
                    .await,
                    cancellation_token: bitcoin_core_cancellation_token,
                });
            }
        }

        Ok(JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: TemplateProviderReady { io: self.state.io },
        })
    }
}

impl JdcRuntime<TemplateProviderReady> {
    pub(super) async fn bootstrap_mining(self) -> Result<JdcRuntime<Running>, BootstrapError> {
        let runtime = self.bootstrap_channel_manager().await?;

        if runtime.jd_client.config.mode == ConfigJDCMode::SoloMining {
            return Ok(runtime.into_solo_mining().start_services().await?);
        }

        match runtime.try_upstream().await {
            Ok(upstream_ready) => Ok(upstream_ready.start_services().await?),
            Err((JDCErrorKind::NoUpstreamConfig(mode), runtime)) => {
                error!(
                    ?mode,
                    "Invalid configuration: JDC mode is non-solo ({:?}) but no upstream server addresses were configured",
                    mode
                );
                Err((JDCErrorKind::NoUpstreamConfig(mode), runtime).into())
            }
            Err((_, template_provider_ready)) => {
                info!("Upstream initialization failed; falling back to solo mining mode");
                Ok(template_provider_ready
                    .into_solo_mining()
                    .start_services()
                    .await?)
            }
        }
    }

    async fn bootstrap_channel_manager(
        self,
    ) -> Result<JdcRuntime<ChannelManagerReady>, (JDCErrorKind, JdcRuntime<TemplateProviderReady>)>
    {
        let channel_manager = match ChannelManager::new(
            self.jd_client.config.clone(),
            self.state.io.channel_manager_to_upstream_sender.clone(),
            self.state.io.upstream_to_channel_manager_receiver.clone(),
            self.state.io.channel_manager_to_jd_sender.clone(),
            self.state.io.jd_to_channel_manager_receiver.clone(),
            self.state.io.channel_manager_to_tp_sender.clone(),
            self.state.io.tp_to_channel_manager_receiver.clone(),
            self.state.io.downstream_to_channel_manager_receiver.clone(),
            self.encoded_outputs.clone(),
            self.jd_client.config.supported_extensions().to_vec(),
            self.jd_client.config.required_extensions().to_vec(),
            self.mode.clone(),
        )
        .await
        {
            Ok(cm) => cm,
            Err(e) => return Err((e.kind, self)),
        };

        Ok(JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: ChannelManagerReady {
                io: self.state.io,
                channel_manager,
            },
        })
    }
}

impl JdcRuntime<ChannelManagerReady> {
    async fn try_upstream(
        mut self,
    ) -> Result<JdcRuntime<UpstreamReady>, (JDCErrorKind, JdcRuntime<ChannelManagerReady>)> {
        if self.upstream_addresses.is_empty() {
            return Err((
                JDCErrorKind::NoUpstreamConfig(self.jd_client.config.mode),
                self,
            ));
        }

        info!("Attempting to initialize upstream...");

        match self.initialize_jd().await {
            Ok((upstream, job_declarator, user_identity)) => {
                upstream
                    .start(
                        self.jd_client.config.min_supported_version(),
                        self.jd_client.config.max_supported_version(),
                        self.jd_client.cancellation_token.clone(),
                        self.fallback_coordinator.clone(),
                        self.task_manager.clone(),
                    )
                    .await;

                job_declarator
                    .start(
                        self.jd_client.cancellation_token.clone(),
                        self.fallback_coordinator.clone(),
                        self.task_manager.clone(),
                    )
                    .await;

                self.state.channel_manager.set_user_identity(user_identity);
                self.state
                    .channel_manager
                    .upstream_state
                    .set(UpstreamState::NoChannel);
                _ = self.state.channel_manager.allocate_tokens(2).await;

                Ok(JdcRuntime {
                    miner_coinbase_outputs: self.miner_coinbase_outputs,
                    encoded_outputs: self.encoded_outputs,
                    mode: self.mode,
                    fallback_coordinator: self.fallback_coordinator,
                    task_manager: self.task_manager,
                    bitcoin_core_sv2: self.bitcoin_core_sv2,
                    jd_client: self.jd_client,
                    upstream_addresses: self.upstream_addresses,
                    state: UpstreamReady {
                        io: self.state.io,
                        channel_manager: self.state.channel_manager,
                    },
                })
            }
            Err(e) => {
                tracing::error!("Failed to initialize upstream: {:?}", e);
                Err((e, self))
            }
        }
    }

    fn into_solo_mining(self) -> JdcRuntime<SoloMiningReady> {
        info!("Starting in solo mining mode");
        self.mode.set_solo_mining();

        JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: SoloMiningReady {
                io: self.state.io,
                channel_manager: self.state.channel_manager,
            },
        }
    }

    /// Initializes an upstream pool + JD connection pair.
    async fn initialize_jd(&mut self) -> Result<(Upstream, JobDeclarator, String), JDCErrorKind> {
        const MAX_RETRIES: usize = 3;
        let upstream_len = self.upstream_addresses.len();

        for i in 0..upstream_len {
            let upstream_entry = self.upstream_addresses[i].clone();
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
                _ = self.jd_client.cancellation_token.cancelled() => {
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
                if self.jd_client.cancellation_token.is_cancelled() {
                    info!(
                        "Shutdown requested before upstream connection attempt, aborting retries"
                    );
                    return Err(JDCErrorKind::CouldNotInitiateSystem);
                }

                info!("Connection attempt {}/{}...", attempt, MAX_RETRIES);

                match self.try_initialize_single(&upstream_entry).await {
                    Ok((upstream, jd)) => {
                        self.upstream_addresses[i].tried_or_flagged = true;
                        return Ok((upstream, jd, upstream_entry.user_identity.clone()));
                    }
                    Err(e) => {
                        tracing::error!("Upstream and JDS connection terminated");

                        tokio::select! {
                            biased;
                            _ = self.jd_client.cancellation_token.cancelled() => {
                                info!("Shutdown requested after upstream initialization failure, aborting retries");
                                return Err(JDCErrorKind::CouldNotInitiateSystem);
                            }
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }

                        // Stop retrying and fail immediately if a shutdown signal is encountered,
                        // as retries are only intended for fallback scenarios.
                        if e.action.is_shutdown() {
                            info!(
                                "Encountered a shutdown error during upstream initialization, aborting retries"
                            );
                            return Err(e.kind);
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
            self.upstream_addresses[i].tried_or_flagged = true;
        }

        tracing::error!("All upstreams failed after {} retries each", MAX_RETRIES);
        Err(JDCErrorKind::CouldNotInitiateSystem)
    }

    // Attempts to initialize a single upstream (pool + JDS pair).
    #[cfg_attr(not(test), hotpath::measure)]
    async fn try_initialize_single(
        &self,
        upstream_entry: &UpstreamEntry,
    ) -> JDCResult<(Upstream, JobDeclarator), super::error::JobDeclaratorClient> {
        info!("Upstream connection in-progress at initialize single");
        let upstream = Upstream::new(
            upstream_entry,
            self.state.io.upstream_to_channel_manager_sender.clone(),
            self.state.io.channel_manager_to_upstream_receiver.clone(),
            self.jd_client.cancellation_token.clone(),
            self.fallback_coordinator.clone(),
            self.task_manager.clone(),
            self.jd_client.config.required_extensions().to_vec(),
        )
        .await
        .map_err(|err| match err.action {
            Action::Shutdown => JDCError::shutdown(err.kind),
            _ => JDCError::fallback(err.kind),
        })?;

        info!("Upstream connection done at initialize single");

        let job_declarator = JobDeclarator::new(
            upstream_entry,
            self.state.io.jd_to_channel_manager_sender.clone(),
            self.state.io.channel_manager_to_jd_receiver.clone(),
            self.jd_client.cancellation_token.clone(),
            self.fallback_coordinator.clone(),
            self.mode.clone(),
            self.task_manager.clone(),
        )
        .await
        .map_err(|err| match err.action {
            Action::Shutdown => JDCError::shutdown(err.kind),
            _ => JDCError::fallback(err.kind),
        })?;

        Ok((upstream, job_declarator))
    }
}

impl JdcRuntime<UpstreamReady> {
    /// Activates the background execution loops of the [`ChannelManager`], downstream server, and
    /// monitoring server, transitioning the runtime to [`Running`].
    async fn start_services(
        self,
    ) -> Result<JdcRuntime<Running>, (JDCErrorKind, JdcRuntime<UpstreamReady>)> {
        if let Err(e) = self
            .start_services_inner(&self.state.io, self.state.channel_manager.clone())
            .await
        {
            return Err((e, self));
        }

        Ok(JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: Running { io: self.state.io },
        })
    }
}

impl JdcRuntime<SoloMiningReady> {
    /// Activates the background execution loops of the [`ChannelManager`], downstream server, and
    /// monitoring server, transitioning the runtime to [`Running`].
    async fn start_services(
        self,
    ) -> Result<JdcRuntime<Running>, (JDCErrorKind, JdcRuntime<SoloMiningReady>)> {
        if let Err(e) = self
            .start_services_inner(&self.state.io, self.state.channel_manager.clone())
            .await
        {
            return Err((e, self));
        }

        Ok(JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: Running { io: self.state.io },
        })
    }
}

pub(super) enum RuntimeEvent {
    Shutdown,
    Fallback,
}

impl JdcRuntime<Running> {
    pub(super) async fn wait(&self) -> RuntimeEvent {
        info!("Spawning status listener task...");
        let fallback_token = self.fallback_coordinator.token();

        tokio::select! {
            biased;

            _ = self.jd_client.cancellation_token.cancelled() => {
                RuntimeEvent::Shutdown
            }
            _ = fallback_token.cancelled() => {
                warn!("Upstream/Job Declarator connection dropped — attempting reconnection...");
                RuntimeEvent::Fallback
            }
        }
    }

    pub(super) async fn cleanup_for_fallback(self) -> JdcRuntime<TemplateProviderReady> {
        // trigger fallback and wait for all components to finish cleanup
        self.fallback_coordinator.trigger_fallback_and_wait().await;
        info!("All components finished fallback cleanup");

        self.mode.set_solo_mining();
        info!("Existing Upstream or JD instance taken out. Preparing fallback.");

        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();
        let (channel_manager_to_jd_sender, channel_manager_to_jd_receiver) = unbounded();
        let (jd_to_channel_manager_sender, jd_to_channel_manager_receiver) = unbounded();
        let (downstream_to_channel_manager_sender, downstream_to_channel_manager_receiver) =
            unbounded();

        let new_io = Io {
            channel_manager_to_upstream_sender,
            channel_manager_to_upstream_receiver,
            upstream_to_channel_manager_sender,
            upstream_to_channel_manager_receiver,
            channel_manager_to_jd_sender,
            channel_manager_to_jd_receiver,
            jd_to_channel_manager_sender,
            jd_to_channel_manager_receiver,
            downstream_to_channel_manager_sender,
            downstream_to_channel_manager_receiver,
            // Re-use TP channels from running session
            channel_manager_to_tp_sender: self.state.io.channel_manager_to_tp_sender,
            channel_manager_to_tp_receiver: self.state.io.channel_manager_to_tp_receiver,
            tp_to_channel_manager_sender: self.state.io.tp_to_channel_manager_sender,
            tp_to_channel_manager_receiver: self.state.io.tp_to_channel_manager_receiver,
        };

        JdcRuntime {
            miner_coinbase_outputs: self.miner_coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            mode: self.mode,
            fallback_coordinator: FallbackCoordinator::new(),
            task_manager: self.task_manager,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            jd_client: self.jd_client,
            upstream_addresses: self.upstream_addresses,
            state: TemplateProviderReady { io: new_io },
        }
    }
}
