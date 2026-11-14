//! ## Translator Runtime Module
//!
//! Provides [`TranslatorRuntime`], a structured state-machine orchestrating the translator proxy's
//! initialization, bootstrap stages, background service loops, and graceful teardown or fallback.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    fallback_coordinator::FallbackCoordinator,
    payout::PayoutMode,
    stratum_core::parsers_sv2::MiningOwned,
    sync::SharedLock,
    task_manager::TaskManager,
    upstream_selector::{Attempt, UpstreamSelector, attempts},
    utils::types::{GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS, InboundFrame, OutboundFrame},
};
use tracing::{error, info, warn};

use crate::{
    TranslatorSv2,
    error::{TproxyError, TproxyErrorKind},
    sv1::Sv1Server,
    sv2::{ChannelManager, ProtocolReconnect, Upstream},
    utils::{ReconnectRequest, TproxyMode, UpstreamEntry},
};

struct Io {
    channel_manager_to_upstream_sender: Sender<OutboundFrame>,
    channel_manager_to_upstream_receiver: Receiver<OutboundFrame>,
    upstream_to_channel_manager_sender: Sender<InboundFrame>,
    upstream_to_channel_manager_receiver: Receiver<InboundFrame>,
    channel_manager_to_sv1_server_sender: Sender<MiningOwned>,
    channel_manager_to_sv1_server_receiver: Receiver<MiningOwned>,
    sv1_server_to_channel_manager_sender: Sender<(MiningOwned, Option<String>)>,
    sv1_server_to_channel_manager_receiver: Receiver<(MiningOwned, Option<String>)>,
}

/// The core coordinator of the Translator runtime, parameterized by its current bootstrap `State`.
///
/// It manages the lifecycle of essential sub-services and channels, ensuring resources
/// are correctly initialized, passed to background executors, and cleanly torn down.
pub(super) struct TranslatorRuntime<State> {
    tproxy_mode: TproxyMode,
    fallback_coordinator: FallbackCoordinator,
    task_manager: Arc<TaskManager>,
    translator: TranslatorSv2,
    upstreams: UpstreamSelector<UpstreamEntry>,
    protocol_reconnect: SharedLock<ProtocolReconnect>,
    listen_addr: SocketAddr,
    state: State,
}

pub(super) struct Init;

struct IoReady {
    io: Io,
}

struct ChannelManagerReady {
    io: Io,
    channel_manager: Arc<ChannelManager>,
}

pub(super) struct Sv1ServerReady {
    io: Io,
    channel_manager: Arc<ChannelManager>,
    sv1_server: Arc<Sv1Server>,
}

pub(super) struct UpstreamReady {
    sv1_server: Arc<Sv1Server>,
    channel_manager: Arc<ChannelManager>,
}

pub(super) struct Running;

pub(super) struct Failed;

#[must_use = "bootstrap errors include a partially initialized runtime that must be shut down"]
pub(super) struct BootstrapError {
    kind: TproxyErrorKind,
    runtime: TranslatorRuntime<Failed>,
}

impl BootstrapError {
    pub(super) fn into_parts(self) -> (TproxyErrorKind, TranslatorRuntime<Failed>) {
        (self.kind, self.runtime)
    }
}

impl<State> From<(TproxyErrorKind, TranslatorRuntime<State>)> for BootstrapError {
    fn from((kind, runtime): (TproxyErrorKind, TranslatorRuntime<State>)) -> Self {
        Self {
            kind,
            runtime: runtime.into_failed(),
        }
    }
}

impl<State> TranslatorRuntime<State> {
    fn into_failed(self) -> TranslatorRuntime<Failed> {
        TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: Failed,
        }
    }

    /// Performs a coordinated, graceful shutdown of the runtime.
    ///
    /// Signals cancellation to all active sub-services and background tasks, awaiting
    /// their clean termination up to a configured graceful timeout, and finally marks the
    /// owning [`TranslatorSv2`] as stopped — callers do not need to do so themselves.
    pub(super) async fn shutdown(self) {
        self.translator.cancellation_token.cancel();

        warn!(
            "Graceful shutdown: waiting {} seconds for tasks to finish",
            GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS
        );
        match tokio::time::timeout(
            Duration::from_secs(GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS),
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
        self.translator.mark_stopped();
        info!("TranslatorSv2 shutdown complete.");
    }

    fn payout_mode(&self, user_identity: &str) -> Result<Option<PayoutMode>, TproxyErrorKind> {
        let expected_payout_distribution = self
            .translator
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
}

impl TranslatorRuntime<Init> {
    pub(super) fn new(translator: TranslatorSv2) -> Result<Self, TproxyErrorKind> {
        let tproxy_mode = TproxyMode::from(translator.config.aggregate_channels);

        let downstream_addr_ip = translator
            .config
            .downstream_address
            .parse::<std::net::IpAddr>()
            .map_err(|e| TproxyErrorKind::General(format!("Invalid downstream address: {e}")))?;

        let listen_addr = SocketAddr::new(downstream_addr_ip, translator.config.downstream_port);

        let upstreams =
            UpstreamSelector::new(translator.config.upstreams.iter().map(|u| UpstreamEntry {
                host: u.address.clone(),
                port: u.port,
                authority_pubkey: u.authority_pubkey,
                user_identity: u.user_identity.clone(),
            }));

        Ok(TranslatorRuntime {
            tproxy_mode,
            fallback_coordinator: FallbackCoordinator::new(),
            task_manager: Arc::new(TaskManager::new()),
            translator,
            upstreams,
            protocol_reconnect: SharedLock::new(ProtocolReconnect::default()),
            listen_addr,
            state: Init,
        })
    }

    /// Drives the linear bootstrap sequence of the translator proxy, transitioning the runtime
    /// from [`Init`] to the active [`Running`] state.
    ///
    /// If an intermediate phase fails, the caller receives the partially initialized
    /// runtime and is responsible for shutting down any resources that were already started.
    pub(super) async fn bootstrap(self) -> Result<TranslatorRuntime<Running>, BootstrapError> {
        let runtime = self.bootstrap_io();
        let runtime = runtime.bootstrap_channel_manager();
        let runtime = runtime.bootstrap_sv1_server();
        let runtime = runtime.try_upstream().await?;
        runtime.start_services().await
    }

    fn bootstrap_io(self) -> TranslatorRuntime<IoReady> {
        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();
        let (channel_manager_to_sv1_server_sender, channel_manager_to_sv1_server_receiver) =
            unbounded();
        let (sv1_server_to_channel_manager_sender, sv1_server_to_channel_manager_receiver) =
            unbounded();

        TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: IoReady {
                io: Io {
                    channel_manager_to_upstream_sender,
                    channel_manager_to_upstream_receiver,
                    upstream_to_channel_manager_sender,
                    upstream_to_channel_manager_receiver,
                    channel_manager_to_sv1_server_sender,
                    channel_manager_to_sv1_server_receiver,
                    sv1_server_to_channel_manager_sender,
                    sv1_server_to_channel_manager_receiver,
                },
            },
        }
    }
}

impl TranslatorRuntime<IoReady> {
    fn bootstrap_channel_manager(self) -> TranslatorRuntime<ChannelManagerReady> {
        let channel_manager = Arc::new(ChannelManager::new(
            self.state.io.channel_manager_to_upstream_sender.clone(),
            self.state.io.upstream_to_channel_manager_receiver.clone(),
            self.state.io.channel_manager_to_sv1_server_sender.clone(),
            self.state.io.sv1_server_to_channel_manager_receiver.clone(),
            self.translator.config.supported_extensions.clone(),
            self.translator.config.required_extensions.clone(),
            self.tproxy_mode,
            self.translator.config.max_past_jobs,
            #[cfg(feature = "monitoring")]
            self.translator
                .config
                .downstream_difficulty_config
                .enable_vardiff,
        ));

        TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: ChannelManagerReady {
                io: self.state.io,
                channel_manager,
            },
        }
    }
}

impl TranslatorRuntime<ChannelManagerReady> {
    fn bootstrap_sv1_server(self) -> TranslatorRuntime<Sv1ServerReady> {
        let sv1_server = Arc::new(Sv1Server::new(
            self.listen_addr,
            self.state.io.channel_manager_to_sv1_server_receiver.clone(),
            self.state.io.sv1_server_to_channel_manager_sender.clone(),
            self.translator.config.clone(),
            self.tproxy_mode,
        ));

        TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: Sv1ServerReady {
                io: self.state.io,
                channel_manager: self.state.channel_manager,
                sv1_server,
            },
        }
    }
}

impl TranslatorRuntime<Sv1ServerReady> {
    pub(super) async fn try_upstream(
        self,
    ) -> Result<TranslatorRuntime<UpstreamReady>, BootstrapError> {
        if let Err(kind) = self.initialize_upstream().await {
            return Err(BootstrapError::from((kind, self)));
        }
        Ok(self.into_upstream_ready())
    }

    /// Follows a reconnect the upstream asked for, or moves on to the configured upstreams if its
    /// endpoint cannot be used.
    ///
    /// The endpoint keeps the authority key and user identity of the upstream that requested it,
    /// so it can only be another server of the same pool. It is tried on its own and does not join
    /// the configured list; once connected it stands in for that upstream, at its rank. Leaving the
    /// upstream to follow its request is not a failure of it.
    pub(super) async fn try_reconnect(
        self,
        request: ReconnectRequest,
    ) -> Result<TranslatorRuntime<UpstreamReady>, BootstrapError> {
        let upstream = request.into_upstream();
        info!("Reconnecting to {upstream}");
        let requester_rank = self.upstreams.leave();
        let outcome = self
            .unless_shutdown(attempts(&upstream, &mut |upstream| {
                self.connect_once(upstream)
            }))
            .await;
        match outcome {
            Ok(Attempt::Connected(())) => {
                if let Some(rank) = requester_rank {
                    self.upstreams.connected(rank);
                }
                Ok(self.into_upstream_ready())
            }
            Ok(Attempt::Failed) => {
                warn!("Could not reconnect to {upstream}; trying the configured upstreams");
                self.try_upstream().await
            }
            Ok(Attempt::Abort(kind)) | Err(kind) => Err(BootstrapError::from((kind, self))),
        }
    }

    fn into_upstream_ready(self) -> TranslatorRuntime<UpstreamReady> {
        TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: UpstreamReady {
                sv1_server: self.state.sv1_server,
                channel_manager: self.state.channel_manager,
            },
        }
    }

    /// Connects to the first upstream that answers, in priority order.
    ///
    /// While none does, the SV1 server stays down, so miners cannot connect, and the upstreams are
    /// walked again whenever one of them may be tried again. Only a shutdown, or an error that
    /// calls for one, ends this without a connection.
    async fn initialize_upstream(&self) -> Result<(), TproxyErrorKind> {
        if self.upstreams.is_empty() {
            error!("No upstream is configured");
            return Err(TproxyErrorKind::CouldNotInitiateSystem);
        }
        let connect = async {
            loop {
                match self
                    .upstreams
                    .walk(|upstream| self.connect_once(upstream))
                    .await
                {
                    Attempt::Connected(()) => return Ok(()),
                    Attempt::Failed => {
                        warn!("No upstream is reachable; SV1 miners cannot connect until one is");
                        self.upstreams.next_ready().await;
                    }
                    Attempt::Abort(kind) => return Err(kind),
                }
            }
        };
        self.unless_shutdown(connect).await?
    }

    /// Runs `connect` unless the translator shuts down first, which aborts it.
    async fn unless_shutdown<T>(
        &self,
        connect: impl Future<Output = T>,
    ) -> Result<T, TproxyErrorKind> {
        tokio::select! {
            biased;
            _ = self.translator.cancellation_token.cancelled() => {
                info!("Shutdown requested while connecting to an upstream");
                Err(TproxyErrorKind::CouldNotInitiateSystem)
            }
            outcome = connect => Ok(outcome),
        }
    }

    /// Makes one attempt at `upstream` and, once connected, configures the channel manager and
    /// SV1 server for it.
    async fn connect_once(&self, upstream: UpstreamEntry) -> Attempt<(), TproxyErrorKind> {
        if let Err(e) = self.try_initialize_upstream_single(&upstream).await {
            if e.action.is_shutdown() {
                error!("Fatal shutdown signal during upstream setup: {:?}", e.kind);
                return Attempt::Abort(e.kind);
            }
            warn!("Could not connect to {upstream}: {e:?}");
            return Attempt::Failed;
        }

        let payout_mode = match self.payout_mode(&upstream.user_identity) {
            Ok(payout_mode) => payout_mode,
            Err(kind) => return Attempt::Abort(kind),
        };
        self.state
            .channel_manager
            .set_expected_payout_distribution(payout_mode);
        self.state
            .sv1_server
            .set_user_identity(upstream.user_identity);
        Attempt::Connected(())
    }

    // Attempts to initialize a single upstream.
    #[cfg_attr(not(test), hotpath::measure)]
    async fn try_initialize_upstream_single(
        &self,
        upstream_addr: &UpstreamEntry,
    ) -> Result<(), TproxyError<crate::error::Upstream>> {
        let upstream = Upstream::new(
            upstream_addr,
            self.state.io.upstream_to_channel_manager_sender.clone(),
            self.state.io.channel_manager_to_upstream_receiver.clone(),
            self.translator.cancellation_token.clone(),
            self.fallback_coordinator.clone(),
            self.task_manager.clone(),
            self.translator.config.required_extensions.clone(),
            self.protocol_reconnect.clone(),
        )
        .await?;

        upstream
            .start(
                self.translator.cancellation_token.clone(),
                self.fallback_coordinator.clone(),
                self.task_manager.clone(),
            )
            .await?;
        Ok(())
    }
}

impl TranslatorRuntime<UpstreamReady> {
    /// Activates the background execution loops of the SV1 server, [`ChannelManager`], and
    /// monitoring server, transitioning the runtime to [`Running`].
    pub(super) async fn start_services(self) -> Result<TranslatorRuntime<Running>, BootstrapError> {
        info!("Launching SV1 server...");
        if let Err(e) = self
            .state
            .sv1_server
            .clone()
            .start(
                self.translator.cancellation_token.clone(),
                self.fallback_coordinator.clone(),
                self.task_manager.clone(),
            )
            .await
        {
            error!("SV1 server startup failed: {e:?}");
            return Err(BootstrapError::from((e.kind, self)));
        }

        info!("Launching ChannelManager tasks...");
        ChannelManager::run_channel_manager_tasks(
            self.state.channel_manager.clone(),
            self.translator.cancellation_token.clone(),
            self.fallback_coordinator.clone(),
            self.task_manager.clone(),
        )
        .await;

        #[cfg(feature = "monitoring")]
        if let Some(monitoring_addr) = self.translator.config.monitoring_address() {
            info!("Initializing monitoring server on http://{monitoring_addr}");
            if let Err(e) = self.start_monitoring_tasks(monitoring_addr) {
                error!("Failed to initialize monitoring tasks: {e}");
                return Err(BootstrapError::from((e, self)));
            }
        }

        self.start_upstream_probe();

        Ok(TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: self.fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: Running,
        })
    }

    // Probes the upstreams ranked above the active one and switches to the first that answers
    // through this generation's fallback.
    fn start_upstream_probe(&self) {
        let cancellation_token = self.translator.cancellation_token.clone();
        let task_manager = self.task_manager.clone();
        let required_extensions = self.translator.config.required_extensions.clone();

        self.upstreams.start_probe(
            &self.task_manager,
            self.translator.cancellation_token.clone(),
            &self.fallback_coordinator,
            move |upstream, fallback_coordinator| {
                let cancellation_token = cancellation_token.clone();
                let task_manager = task_manager.clone();
                let required_extensions = required_extensions.clone();
                async move {
                    // Channels connected to nothing: a probed upstream is never started.
                    Upstream::new(
                        &upstream,
                        unbounded().0,
                        unbounded().1,
                        cancellation_token,
                        fallback_coordinator,
                        task_manager,
                        required_extensions,
                        SharedLock::new(ProtocolReconnect::default()),
                    )
                    .await?
                    .setup_connection()
                    .await
                }
            },
        );
    }

    #[cfg(feature = "monitoring")]
    fn start_monitoring_tasks(&self, monitoring_addr: SocketAddr) -> Result<(), TproxyErrorKind> {
        let refresh_interval = Duration::from_secs(
            self.translator
                .config
                .monitoring_cache_refresh_secs()
                .unwrap_or(15),
        );

        let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
            monitoring_addr,
            Some(self.state.channel_manager.clone()),
            None,
            refresh_interval,
        )
        .map_err(|e| {
            TproxyErrorKind::General(format!("failed to initialize monitoring server: {e}"))
        })?
        .with_sv1_monitoring(self.state.sv1_server.clone())
        .map_err(|e| TproxyErrorKind::General(format!("failed to add SV1 monitoring: {e}")))?;

        let cancellation_token_clone = self.translator.cancellation_token.clone();
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

        self.task_manager.spawn(self.fallback_coordinator.track({
            let cancellation_token = self.translator.cancellation_token.clone();
            async move {
                if let Err(e) = monitoring_server.run(shutdown_signal).await {
                    error!("Monitoring server error: {:?}", e);
                    cancellation_token.cancel();
                }

                info!("Monitoring server task exited");
            }
        }));

        self.task_manager.spawn(self.fallback_coordinator.track({
            let cancellation_token = self.translator.cancellation_token.clone();
            let fallback_token = self.fallback_coordinator.token();
            let sv1_server = self.state.sv1_server.clone();
            async move {
                sv1_server
                    .run_miner_telemetry_loop(refresh_interval, cancellation_token, fallback_token)
                    .await;

                info!("SV1 miner telemetry task exited");
            }
        }));

        Ok(())
    }
}

pub(super) enum RuntimeEvent {
    Shutdown,
    Fallback,
    /// The upstream asked tProxy to reconnect.
    Reconnect(ReconnectRequest),
}

impl TranslatorRuntime<Running> {
    pub(super) async fn wait(&self) -> RuntimeEvent {
        let fallback_token = self.fallback_coordinator.token();
        let cancellation_token = self.translator.cancellation_token.clone();

        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => RuntimeEvent::Shutdown,
            _ = fallback_token.cancelled() => {
                // Taken as soon as the teardown starts. A request the upstream recorded after
                // another component started it, before this point, is followed too: it can only
                // point to another server of the same pool. One recorded later is dropped by
                // `cleanup_for_fallback`.
                match self
                    .protocol_reconnect
                    .with(|protocol_reconnect| protocol_reconnect.take(Instant::now()))
                {
                    Ok(Some(request)) => RuntimeEvent::Reconnect(request),
                    _ => {
                        self.upstreams.disconnected();
                        RuntimeEvent::Fallback
                    }
                }
            }
        }
    }

    pub(super) async fn cleanup_for_fallback(self) -> TranslatorRuntime<Sv1ServerReady> {
        info!("Preparing fallback");
        self.fallback_coordinator.trigger_fallback_and_wait().await;
        info!("All components finished fallback cleanup");
        // `wait` already took the request that caused this teardown. One recorded while it was
        // under way, after another component triggered it, is dropped rather than followed later.
        let _ = self
            .protocol_reconnect
            .with(|protocol_reconnect| protocol_reconnect.discard());

        let fresh_fallback_coordinator = FallbackCoordinator::new();

        TranslatorRuntime {
            tproxy_mode: self.tproxy_mode,
            fallback_coordinator: fresh_fallback_coordinator,
            task_manager: self.task_manager,
            translator: self.translator,
            upstreams: self.upstreams,
            protocol_reconnect: self.protocol_reconnect,
            listen_addr: self.listen_addr,
            state: Init,
        }
        .bootstrap_io()
        .bootstrap_channel_manager()
        .bootstrap_sv1_server()
    }
}
