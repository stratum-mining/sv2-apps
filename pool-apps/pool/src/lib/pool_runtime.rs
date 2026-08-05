//! ## Pool Runtime Module
//!
//! Provides [`PoolRuntime`], a structured state-machine orchestrating the pool's
//! initialization, bootstrap stages, background service loops, and graceful teardown.

use std::{
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
};
use stratum_apps::stratum_core::parsers_sv2::{MiningOwned, TemplateDistributionOwned};

use async_channel::{Receiver, Sender, unbounded};

use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    stratum_core::{
        bitcoin::{TxOut, consensus::Encodable},
        parsers_sv2::Tlv,
    },
    task_manager::TaskManager,
    tp_type::TemplateProviderType,
    utils::types::{DownstreamId, GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS},
};
use tracing::{error, info, warn};

use jd_server_sv2::job_declarator::{
    JobDeclarator,
    job_validation::{JobValidationEngine, bitcoin_core_ipc::BitcoinCoreIPCEngine},
};

use super::PoolSv2;
use crate::{
    channel_manager::ChannelManager,
    error::PoolErrorKind,
    template_receiver::{
        bitcoin_core::{BitcoinCoreSv2TDPConfig, connect_to_bitcoin_core},
        sv2_tp::Sv2Tp,
    },
};

struct Io {
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

pub(super) struct Init;

struct IoReady {
    io: Io,
}

struct JdsReady {
    io: Io,
}

struct TemplateProviderReady {
    io: Io,
}

struct ChannelManagerReady {
    io: Io,
    channel_manager: ChannelManager,
}

pub(super) struct Running;

pub(super) struct Failed;

#[must_use = "bootstrap errors include a partially initialized runtime that must be shut down"]
pub(super) struct BootstrapError {
    kind: PoolErrorKind,
    runtime: PoolRuntime<Failed>,
}

impl BootstrapError {
    pub(super) fn into_parts(self) -> (PoolErrorKind, PoolRuntime<Failed>) {
        (self.kind, self.runtime)
    }
}

impl<State> From<(PoolErrorKind, PoolRuntime<State>)> for BootstrapError {
    fn from((kind, runtime): (PoolErrorKind, PoolRuntime<State>)) -> Self {
        Self {
            kind,
            runtime: runtime.into_failed(),
        }
    }
}

/// The core coordinator of the Pool runtime, parameterized by its current bootstrap `State`.
///
/// It manages the lifecycle of essential sub-services and channels, ensuring resources
/// are correctly initialized, passed to background executors, and cleanly torn down.
pub(super) struct PoolRuntime<State> {
    pool: PoolSv2,
    task_manager: Arc<TaskManager>,
    state: State,
    jd: Option<JobDeclarator>,
    bitcoin_core_sv2: Option<BitcoinCoreSv2Handle>,
    encoded_outputs: Vec<u8>,
    coinbase_outputs: Vec<TxOut>,
}

impl<State> PoolRuntime<State> {
    fn into_failed(self) -> PoolRuntime<Failed> {
        PoolRuntime {
            pool: self.pool,
            task_manager: self.task_manager,
            state: Failed,
            jd: self.jd,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            encoded_outputs: self.encoded_outputs,
            coinbase_outputs: self.coinbase_outputs,
        }
    }

    /// Performs a coordinated, graceful shutdown of the runtime.
    ///
    /// Signals cancellation to all active sub-services and background tasks, awaiting
    /// their clean termination up to a configured graceful timeout.
    pub(super) async fn shutdown(mut self) {
        self.pool.cancellation_token.cancel();

        if let Some(jd) = self.jd.take() {
            jd.shutdown();
        }

        if let Some(bitcoin_core_sv2) = self.bitcoin_core_sv2.take() {
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

        self.pool.shutdown_notify.notify_waiters();
        self.pool.is_alive.store(false, Ordering::Relaxed);
        info!("Pool shutdown complete.");
    }
}

#[allow(clippy::result_large_err)]
impl PoolRuntime<Init> {
    pub(super) fn new(pool: PoolSv2) -> Result<Self, PoolErrorKind> {
        let coinbase_outputs = vec![pool.config.get_txout()];
        let mut encoded_outputs = vec![];

        coinbase_outputs
            .consensus_encode(&mut encoded_outputs)
            .map_err(|err| PoolErrorKind::Io(err.into()))?;

        Ok(PoolRuntime {
            pool,
            task_manager: Arc::new(TaskManager::new()),
            state: Init,
            jd: None,
            bitcoin_core_sv2: None,
            coinbase_outputs,
            encoded_outputs,
        })
    }

    /// Allocates internal channels, transitioning the runtime from
    /// [`Init`] to [`IoReady`].
    fn bootstrap_io(self) -> PoolRuntime<IoReady> {
        let (downstream_to_channel_manager_sender, downstream_to_channel_manager_receiver) =
            unbounded();
        let (channel_manager_to_tp_sender, channel_manager_to_tp_receiver) = unbounded();
        let (tp_to_channel_manager_sender, tp_to_channel_manager_receiver) = unbounded();

        let io = Io {
            downstream_to_channel_manager_sender,
            downstream_to_channel_manager_receiver,
            channel_manager_to_tp_sender,
            channel_manager_to_tp_receiver,
            tp_to_channel_manager_sender,
            tp_to_channel_manager_receiver,
        };

        PoolRuntime {
            pool: self.pool,
            task_manager: self.task_manager,
            jd: self.jd,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            coinbase_outputs: self.coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            state: IoReady { io },
        }
    }

    /// Drives the linear bootstrap sequence of the pool, transitioning the runtime
    /// from [`Init`] to the active [`Running`] state.
    ///
    /// If an intermediate phase fails, the caller receives the partially initialized
    /// runtime and is responsible for shutting down any resources that were already started.
    pub(super) async fn bootstrap(self) -> Result<PoolRuntime<Running>, BootstrapError> {
        let runtime = self.bootstrap_io();

        let runtime: PoolRuntime<JdsReady> = runtime.bootstrap_jds().await?;

        let runtime: PoolRuntime<TemplateProviderReady> =
            runtime.bootstrap_template_provider().await?;

        let runtime: PoolRuntime<ChannelManagerReady> = runtime.bootstrap_channel_manager().await?;

        Ok(runtime.start_services().await?)
    }
}

impl PoolRuntime<IoReady> {
    async fn bootstrap_jds(
        self,
    ) -> Result<PoolRuntime<JdsReady>, (PoolErrorKind, PoolRuntime<IoReady>)> {
        let jds_config = match self.pool.config.build_jds_config() {
            Ok(config) => config,
            Err(err_kind) => return Err((err_kind, self)),
        };

        let cancellation_token = self.pool.cancellation_token.clone();

        let jd = if let Some(jds_config) = jds_config {
            info!("JDS config present — initializing embedded Job Declaration Server");

            let ipc_engine: Arc<dyn JobValidationEngine> =
                match self.pool.config.template_provider_type() {
                    TemplateProviderType::BitcoinCoreIpc {
                        version,
                        network,
                        data_dir,
                        ..
                    } => {
                        let ipc_engine_result = BitcoinCoreIPCEngine::new(
                            *version,
                            network.clone(),
                            data_dir.clone(),
                            self.pool.cancellation_token.clone(),
                        )
                        .await;

                        match ipc_engine_result {
                            Ok(engine) => Arc::new(engine),
                            Err(err) => return Err((PoolErrorKind::Jds(err), self)),
                        }
                    }
                    TemplateProviderType::Sv2Tp { .. } => {
                        return Err((
                            PoolErrorKind::Configuration(
                                "[jds] requires template_provider_type = BitcoinCoreIpc \
                                                     (JDS needs direct IPC access to Bitcoin Core)"
                                    .to_string(),
                            ),
                            self,
                        ));
                    }
                };

            let jd = match JobDeclarator::new(
                ipc_engine,
                cancellation_token.clone(),
                jds_config.coinbase_reward_script().clone(),
                self.task_manager.clone(),
            )
            .await
            {
                Ok(jd) => jd,
                Err(err) => return Err((PoolErrorKind::Jds(err), self)),
            };

            match jd
                .clone()
                .start(
                    self.pool.cancellation_token.clone(),
                    self.task_manager.clone(),
                )
                .await
            {
                Ok(_) => (),
                Err(err) => {
                    cancellation_token.cancel();
                    jd.shutdown();

                    return Err((PoolErrorKind::Jds(err.kind), self));
                }
            };

            match jd
                .clone()
                .start_downstream_server(
                    *jds_config.authority_public_key(),
                    *jds_config.authority_secret_key(),
                    jds_config.cert_validity_sec(),
                    *jds_config.listen_address(),
                    self.task_manager.clone(),
                    cancellation_token.clone(),
                    jds_config.supported_extensions().to_vec(),
                    jds_config.required_extensions().to_vec(),
                )
                .await
            {
                Ok(_) => (),
                Err(err) => {
                    cancellation_token.cancel();
                    jd.shutdown();

                    return Err((PoolErrorKind::Jds(err.kind), self));
                }
            };

            Some(jd)
        } else {
            info!("No [jds] config — Job Declaration not available");

            None
        };

        let new_state = JdsReady { io: self.state.io };

        Ok(PoolRuntime {
            pool: self.pool,
            task_manager: self.task_manager,
            jd,
            bitcoin_core_sv2: None,
            coinbase_outputs: self.coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            state: new_state,
        })
    }
}

impl PoolRuntime<JdsReady> {
    async fn bootstrap_template_provider(
        self,
    ) -> Result<PoolRuntime<TemplateProviderReady>, (PoolErrorKind, PoolRuntime<JdsReady>)> {
        let cancellation_token = self.pool.cancellation_token.clone();
        let mut bitcoin_core_sv2: Option<BitcoinCoreSv2Handle> = None;

        match self.pool.config.template_provider_type().clone() {
            TemplateProviderType::Sv2Tp {
                address,
                public_key,
            } => {
                let sv2_tp = match Sv2Tp::new(
                    address.clone(),
                    public_key,
                    self.state.io.channel_manager_to_tp_receiver.clone(),
                    self.state.io.tp_to_channel_manager_sender.clone(),
                    cancellation_token.clone(),
                    self.task_manager.clone(),
                )
                .await
                {
                    Ok(tp) => tp,
                    Err(err) => return Err((err.kind, self)),
                };

                match sv2_tp
                    .start(
                        address,
                        cancellation_token.clone(),
                        self.task_manager.clone(),
                    )
                    .await
                {
                    Ok(_) => (),
                    Err(err) => return Err((err.kind, self)),
                };

                // Sv2Tp manages its own lifecycle via spawned tasks that run on the `task_manager`.
                // It handles its own shutdown internally when the `cancellation_token` is
                // triggered, so we don't explicitly store and shut it down.
                info!("Sv2 Template Provider setup done");
            }
            TemplateProviderType::BitcoinCoreIpc {
                version,
                network,
                data_dir,
                fee_threshold,
                min_interval,
            } => {
                let unix_socket_path =
                    match stratum_apps::tp_type::resolve_ipc_socket_path(&network, data_dir) {
                        Some(path) => path,
                        None => {
                            return Err((
                                PoolErrorKind::Configuration(
                                                                "Could not determine Bitcoin data directory. Please set data_dir in config."
                                                                    .to_string(),
                                                            ),
                                self,
                            ))
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
                        cancellation_token.clone(),
                        self.task_manager.clone(),
                    )
                    .await,
                    cancellation_token: bitcoin_core_cancellation_token,
                });
            }
        }

        let new_state = TemplateProviderReady { io: self.state.io };

        Ok(PoolRuntime {
            pool: self.pool,
            task_manager: self.task_manager,
            jd: self.jd,
            bitcoin_core_sv2,
            coinbase_outputs: self.coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            state: new_state,
        })
    }
}

impl PoolRuntime<TemplateProviderReady> {
    async fn bootstrap_channel_manager(
        self,
    ) -> Result<PoolRuntime<ChannelManagerReady>, (PoolErrorKind, PoolRuntime<TemplateProviderReady>)>
    {
        let channel_manager = match ChannelManager::new(
            self.pool.config.clone(),
            self.state.io.channel_manager_to_tp_sender.clone(),
            self.state.io.tp_to_channel_manager_receiver.clone(),
            self.state.io.downstream_to_channel_manager_receiver.clone(),
            self.encoded_outputs.clone(),
            self.jd.clone(),
        )
        .await
        {
            Ok(cm) => cm,
            Err(err) => {
                return Err((err.kind, self));
            }
        };

        // Start monitoring server if configured
        #[cfg(feature = "monitoring")]
        if let Some(monitoring_addr) = self.pool.config.monitoring_address() {
            info!(
                "Initializing monitoring server on http://{}",
                monitoring_addr
            );

            let monitoring_server = match stratum_apps::monitoring::MonitoringServer::new(
                monitoring_addr,
                None, // Pool doesn't have channels opened with servers
                Some(Arc::new(channel_manager.clone())), // channels opened with clients
                std::time::Duration::from_secs(
                    self.pool
                        .config
                        .monitoring_cache_refresh_secs()
                        .unwrap_or(15),
                ),
            ) {
                Ok(ms) => ms,
                Err(err) => {
                    return Err((
                        PoolErrorKind::Configuration(format!(
                            "Failed to initialize monitoring server: {err}"
                        )),
                        self,
                    ));
                }
            };

            let cancellation_token_clone = self.pool.cancellation_token.clone();
            let shutdown_signal = async move {
                cancellation_token_clone.cancelled().await;
            };

            self.task_manager.spawn({
                let cancellation_token = self.pool.cancellation_token.clone();
                async move {
                    if let Err(e) = monitoring_server.run(shutdown_signal).await {
                        error!("Monitoring server error: {}", e);
                        cancellation_token.cancel();
                    }
                }
            });
        }

        let new_state = ChannelManagerReady {
            io: self.state.io,
            channel_manager,
        };

        Ok(PoolRuntime {
            pool: self.pool,
            task_manager: self.task_manager,
            jd: self.jd,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            coinbase_outputs: self.coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            state: new_state,
        })
    }
}

impl PoolRuntime<ChannelManagerReady> {
    /// Activates the background execution loop of the [`ChannelManager`], spawns the
    /// downstream TCP listening server, and transitions the runtime to [`Running`].
    async fn start_services(
        self,
    ) -> Result<PoolRuntime<Running>, (PoolErrorKind, PoolRuntime<ChannelManagerReady>)> {
        let cancellation_token = self.pool.cancellation_token.clone();

        match self
            .state
            .channel_manager
            .clone()
            .start(
                cancellation_token.clone(),
                self.task_manager.clone(),
                self.coinbase_outputs.clone(),
            )
            .await
        {
            Ok(_) => (),
            Err(err) => {
                return Err((err.kind, self));
            }
        };

        match self
            .state
            .channel_manager
            .clone()
            .start_downstream_server(
                *self.pool.config.authority_public_key(),
                *self.pool.config.authority_secret_key(),
                self.pool.config.cert_validity_sec(),
                *self.pool.config.listen_address(),
                self.task_manager.clone(),
                cancellation_token.clone(),
                self.state.io.downstream_to_channel_manager_sender.clone(),
            )
            .await
        {
            Ok(_) => (),
            Err(err) => {
                return Err((err.kind, self));
            }
        };

        info!("Spawning status listener task...");

        Ok(PoolRuntime {
            pool: self.pool,
            task_manager: self.task_manager,
            jd: self.jd,
            bitcoin_core_sv2: self.bitcoin_core_sv2,
            coinbase_outputs: self.coinbase_outputs,
            encoded_outputs: self.encoded_outputs,
            state: Running,
        })
    }
}

impl PoolRuntime<Running> {
    pub(super) async fn wait_for_shutdown(&self) {
        let cancellation_token = self.pool.cancellation_token.clone();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received — initiating graceful shutdown...");
                cancellation_token.cancel();
            }
            _ = cancellation_token.cancelled() => {}
        }
    }
}
