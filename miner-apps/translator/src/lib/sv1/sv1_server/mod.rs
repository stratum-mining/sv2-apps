//! ## SV1 Server Module
//!
//! This module implements the SV1 server component of the translator,
//! responsible for managing connections with SV1 mining clients.
//!
//! It handles the full lifecycle of SV1 miner interactions, including:
//! - Accepting new SV1 miner connections.
//! - Managing difficulty adjustment for connected miners, including variable difficulty (Vardiff)
//!   logic.
//! - Coordinating with the SV2 channel manager for upstream communication, translating SV1 messages
//!   to SV2 and vice-versa.
//! - Tracking mining jobs, share submissions, and managing keepalive mechanisms.
//!
//! The core component is the [`Sv1Server`] struct, which orchestrates these operations,
//! maintaining state for multiple downstream connections and ensuring seamless translation
//! between SV1 and SV2 protocols.

mod difficulty_manager;
pub mod downstream_message_handler;

use crate::{
    config::TranslatorConfig,
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
    sv1::{
        downstream::{Downstream, Sv1Difficulty, Sv1ServerEvent, Sv1SetupRequest},
        job_store::Sv1JobStore,
    },
    utils::{
        AGGREGATED_CHANNEL_ID, KEEPALIVE_JOB_ID_DELIMITER, SubmitShareWithChannelId,
        Sv1ShareMetadata, TproxyMode,
    },
};
use async_channel::{Receiver, Sender, unbounded};
#[cfg(feature = "monitoring")]
use std::net::IpAddr;
use std::{
    net::SocketAddr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
#[cfg(feature = "monitoring")]
use stratum_apps::monitoring::{MinerTelemetry, MinerTelemetryStatus};
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    network_helpers::sv1_connection::ConnectionSV1,
    stratum_core::{
        binary_sv2::Str0255Owned,
        bitcoin::Target,
        channels_sv2::{Vardiff, VardiffState, target::hash_rate_to_target},
        mining_sv2::{CloseChannelOwned, SetNewPrevHashOwned},
        parsers_sv2::MiningOwned,
        stratum_translation::{
            error::StratumTranslationError,
            sv1_to_sv2::{
                build_sv2_open_extended_mining_channel,
                build_sv2_submit_shares_extended_from_sv1_submit,
            },
            sv2_to_sv1::{
                build_sv1_notify_from_sv2,
                build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding,
                sv1_advertised_target_from_sv2_target,
            },
        },
        sv1_api::{
            IsServer,
            client_to_server::{SubmitError, SubmitOutcome},
            json_rpc,
            methods::Client2Server,
            server_to_client,
            utils::{Extranonce, HexU32Be},
        },
    },
    sync::{SharedLock, SharedMap},
    task_manager::TaskManager,
    utils::types::{ChannelId, DownstreamId, Hashrate, RequestId, SharesPerMinute},
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING: f64 = 1.0;

/// Builds the `mining.set_difficulty` advertising `target`, paired with the target shares are
/// validated against once it is sent: the advertised difficulty, after SV1 power-of-two rounding.
fn sv1_difficulty(target: Target) -> Result<Sv1Difficulty, StratumTranslationError> {
    let message = build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding(
        target,
        SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
    )?;
    let target = sv1_advertised_target_from_sv2_target(
        target,
        SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
    )
    .unwrap_or(target);
    Ok(Sv1Difficulty { message, target })
}

// Maximum keepalive nTime offset from the original upstream job.
const MAX_FUTURE_BLOCK_TIME: u32 = 2 * 60 * 60;

#[derive(Clone, Debug)]
struct StoredSv1Job {
    notify: server_to_client::Notify,
    /// Original upstream job time used to cap locally generated keepalive jobs. Keepalives advance
    /// `notify.time`, while this anchor remains fixed so the future-time limit does not move.
    keepalive_base_time: u32,
}

impl StoredSv1Job {
    fn upstream(notify: server_to_client::Notify) -> Self {
        let keepalive_base_time = notify.time.0;
        Self {
            notify,
            keepalive_base_time,
        }
    }
}

/// What the SV1 server keeps for one SV2 channel.
///
/// In aggregated mode, the shared upstream channel, keyed by `AGGREGATED_CHANNEL_ID`, holds the job
/// state and has no owner, while each downstream's own channel only routes to its owner.
#[derive(Default)]
struct ChannelState {
    /// The downstream the channel belongs to.
    owner: Option<DownstreamId>,
    /// The channel's latest `SetNewPrevHash`.
    prevhash: Option<SetNewPrevHashOwned>,
    /// Jobs sent on the channel, kept to validate shares and to build keepalives.
    jobs: Option<Sv1JobStore<StoredSv1Job>>,
}

// A miner can pipeline mining.configure, mining.subscribe, mining.extranonce.subscribe,
// mining.suggest_difficulty, and mining.authorize while its SV2 channel is opening; some BIP 310
// extensions also permit mining.configure to be repeated. Eight messages leave compatibility
// headroom for that setup traffic while preventing an unauthenticated downstream from growing this
// per-connection buffer without bound. The SV1 network reader separately limits each line to 64
// KiB.
const MAX_QUEUED_SV1_HANDSHAKE_MESSAGES: usize = 8;

#[derive(Clone)]
struct Sv1ServerIo {
    sv1_server_to_downstream_sender: SharedMap<DownstreamId, Sender<Sv1ServerEvent>>,
    downstream_to_sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
    downstream_to_sv1_server_receiver: Receiver<(DownstreamId, json_rpc::Message)>,
    channel_manager_receiver: Receiver<MiningOwned>,
    // Sv1ShareMetadata carries optional worker name + share_hash for SubmitSharesExtended.
    channel_manager_sender: Sender<(MiningOwned, Sv1ShareMetadata)>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1ServerIo {
    fn new(
        channel_manager_receiver: Receiver<MiningOwned>,
        channel_manager_sender: Sender<(MiningOwned, Sv1ShareMetadata)>,
    ) -> Self {
        let (downstream_to_sv1_server_sender, downstream_to_sv1_server_receiver) = unbounded();

        Self {
            sv1_server_to_downstream_sender: SharedMap::new(),
            downstream_to_sv1_server_receiver,
            downstream_to_sv1_server_sender,
            channel_manager_receiver,
            channel_manager_sender,
        }
    }

    fn close(&self) {
        self.channel_manager_sender.close();
        self.downstream_to_sv1_server_sender.close();
        self.channel_manager_receiver.close_and_drain();
        self.downstream_to_sv1_server_receiver.close_and_drain();
        self.sv1_server_to_downstream_sender.retain(|_, sender| {
            sender.close();
            false
        });
    }
}

#[cfg(feature = "monitoring")]
#[derive(Clone)]
pub(crate) struct MinerTelemetryState {
    /// Latest telemetry fetched for each matched SV1 downstream connection.
    pub(crate) telemetry: SharedMap<DownstreamId, MinerTelemetry>,
    /// Miner management IP selected for each matched SV1 downstream connection.
    pub(crate) management_ips: SharedMap<DownstreamId, IpAddr>,
    /// Latest telemetry matching status for each active SV1 downstream connection.
    pub(crate) statuses: SharedMap<DownstreamId, MinerTelemetryStatus>,
}

#[cfg(feature = "monitoring")]
impl MinerTelemetryState {
    fn new() -> Self {
        Self {
            telemetry: SharedMap::new(),
            management_ips: SharedMap::new(),
            statuses: SharedMap::new(),
        }
    }

    fn clear(&self) {
        self.telemetry.clear();
        self.management_ips.clear();
        self.statuses.clear();
    }

    fn remove_downstream(&self, downstream_id: DownstreamId) {
        self.telemetry.remove(&downstream_id);
        self.management_ips.remove(&downstream_id);
        self.statuses.remove(&downstream_id);
    }

    pub(crate) fn telemetry_for(&self, downstream_id: DownstreamId) -> Option<MinerTelemetry> {
        self.telemetry
            .with(&downstream_id, |telemetry| telemetry.clone())
    }

    pub(crate) fn management_ip_for(&self, downstream_id: DownstreamId) -> Option<IpAddr> {
        self.management_ips
            .with(&downstream_id, |management_ip| *management_ip)
    }

    pub(crate) fn status_for(&self, downstream_id: DownstreamId) -> Option<MinerTelemetryStatus> {
        self.statuses.with(&downstream_id, |status| *status)
    }
}

/// SV1 server that handles connections from SV1 miners.
///
/// This struct manages the SV1 server component of the translator, which:
/// - Accepts connections from SV1 miners
/// - Manages difficulty adjustment for connected miners
/// - Coordinates with the SV2 channel manager for upstream communication
/// - Tracks mining jobs and share submissions
///
/// The server maintains state for multiple downstream connections and implements
/// variable difficulty adjustment based on share submission rates.
#[derive(Clone)]
pub struct Sv1Server {
    sv1_server_io: Sv1ServerIo,
    pub(crate) shares_per_minute: SharesPerMinute,
    pub(crate) listener_addr: SocketAddr,
    pub(crate) config: TranslatorConfig,
    pub(crate) sequence_counter: Arc<AtomicU32>,
    pub(crate) miner_counter: Arc<AtomicU32>,
    pub(crate) keepalive_job_id_counter: Arc<AtomicU32>,
    /// Wall-clock time of the last job appended to the shared aggregated history. This keeps
    /// phased or unreachable downstreams from advancing the shared nTime more than once per
    /// configured keepalive interval.
    aggregated_job_mutation_time: SharedLock<Option<Instant>>,
    pub(crate) downstream_id_factory: Arc<AtomicUsize>,
    pub(crate) request_id_factory: Arc<AtomicU32>,
    pub(crate) downstreams: SharedMap<DownstreamId, Downstream>,
    #[cfg(feature = "monitoring")]
    pub(crate) miner_telemetry: MinerTelemetryState,
    pub(crate) request_id_to_downstream_id: SharedMap<RequestId, DownstreamId>,
    /// The state kept for each SV2 channel, see [`ChannelState`].
    channels: SharedMap<ChannelId, ChannelState>,
    pub(crate) vardiff: SharedMap<DownstreamId, VardiffState>,
    pub(crate) mode: TproxyMode,
    user_identity: Arc<OnceLock<String>>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1Server {
    async fn handle_error_action(
        &self,
        context: &str,
        e: &TproxyError<error::Sv1Server>,
        cancellation_token: &CancellationToken,
        fallback_token: &CancellationToken,
    ) -> LoopControl {
        if cancellation_token.is_cancelled() {
            debug!(
                error_kind = ?e.kind,
                "{context} returned an error after shutdown was requested"
            );
            return LoopControl::Continue;
        }

        if fallback_token.is_cancelled() {
            debug!(
                error_kind = ?e.kind,
                "{context} returned an error during fallback"
            );
            return LoopControl::Continue;
        }

        match e.action {
            Action::Log => {
                warn!(
                    error_kind = ?e.kind,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
            }
            Action::Disconnect(downstream_id) => {
                warn!(
                    downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested disconnect; cancelling downstream token"
                );
                // Cleanup only ever fails with `Shutdown` (poisoned lock), so honour it.
                match self.handle_downstream_disconnect(downstream_id).await {
                    Ok(()) => LoopControl::Continue,
                    Err(cleanup_error) => {
                        error!(
                            downstream_id,
                            error_kind = ?cleanup_error.kind,
                            "failed to clean up disconnected downstream; cancelling global token"
                        );
                        cancellation_token.cancel();
                        LoopControl::Break
                    }
                }
            }
            // Only the upstream can request a reconnect; anywhere else it means a fallback.
            Action::Fallback | Action::Reconnect => {
                warn!(
                    error_kind = ?e.kind,
                    "{context} requested fallback"
                );
                fallback_token.cancel();
                LoopControl::Break
            }
            Action::Shutdown => {
                warn!(
                    error_kind = ?e.kind,
                    "{context} requested shutdown; cancelling global token"
                );
                cancellation_token.cancel();
                LoopControl::Break
            }
        }
    }
    /// Sends a message to downstream(s) for the given channel_id.
    ///
    /// In aggregated mode the channel manager rewrites the job's channel_id to
    /// `AGGREGATED_CHANNEL_ID` before forwarding, which signals a broadcast: send to every
    /// connected downstream.
    async fn send_to_channel(&self, channel_id: ChannelId, msg: Arc<server_to_client::Notify>) {
        if channel_id == AGGREGATED_CHANNEL_ID {
            let mut downstream_senders = Vec::new();
            self.sv1_server_io
                .sv1_server_to_downstream_sender
                .for_each(|downstream_id, sender| {
                    downstream_senders.push((downstream_id, sender.clone()));
                });
            // Broadcast to every connected downstream.
            for (downstream_id, sender) in downstream_senders {
                if let Err(e) = sender.send(Sv1ServerEvent::Notify(msg.clone())).await {
                    warn!(
                        "Failed to send notify to downstream {}: channel closed: {}",
                        downstream_id, e
                    );
                }
            }
        } else {
            // A concrete channel ID targets the single downstream that owns it.
            let Some(downstream_id) = self.channel_owner(channel_id) else {
                return;
            };

            let sender = self
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .get_cloned(&downstream_id);

            let Some(sender) = sender else { return };

            if let Err(e) = sender.send(Sv1ServerEvent::Notify(msg)).await {
                warn!(
                    "Failed to send notify to downstream {}: channel closed: {}",
                    downstream_id, e
                );
            }
        }
    }

    /// Cleans up server state and closes communication channels.
    fn cleanup(&self) {
        self.channels.clear();
        let _ = self.aggregated_job_mutation_time.set(None);
        if self.config.downstream_difficulty_config.enable_vardiff {
            self.vardiff.clear();
        }
        self.downstreams.clear();
        #[cfg(feature = "monitoring")]
        self.miner_telemetry.clear();
        self.request_id_to_downstream_id.clear();
        self.sv1_server_io.close();
    }

    /// Runs `f` while holding the downstream map entry guard.
    ///
    /// Use this when mutations must only happen if the downstream is still
    /// registered in Sv1Server. Keep `f` short: do not perform blocking work,
    /// send messages, await, or re-enter `self.downstreams` inside it.
    #[allow(clippy::result_large_err)]
    pub(crate) fn with_registered_downstream<R, F>(
        &self,
        downstream_id: DownstreamId,
        f: F,
    ) -> TproxyResult<R, error::Sv1Server>
    where
        F: FnOnce(&Downstream) -> TproxyResult<R, error::Sv1Server>,
    {
        match self
            .downstreams
            .with(&downstream_id, |downstream| f(downstream))
        {
            Some(result) => result,
            None => Err(TproxyError::disconnect(
                TproxyErrorKind::DownstreamNotPresent(downstream_id),
                downstream_id,
            )),
        }
    }

    /// Creates a new SV1 server instance.
    ///
    /// # Arguments
    /// * `listener_addr` - The socket address to bind the server to
    /// * `channel_manager_receiver` - Channel to receive messages from the channel manager
    /// * `channel_manager_sender` - Channel to send messages to the channel manager
    /// * `config` - Configuration settings for the translator
    ///
    /// # Returns
    /// A new Sv1Server instance ready to accept connections
    pub fn new(
        listener_addr: SocketAddr,
        channel_manager_receiver: Receiver<MiningOwned>,
        channel_manager_sender: Sender<(MiningOwned, Sv1ShareMetadata)>,
        config: TranslatorConfig,
        mode: TproxyMode,
    ) -> Self {
        let shares_per_minute = config.downstream_difficulty_config.shares_per_minute;
        let sv1_server_io = Sv1ServerIo::new(channel_manager_receiver, channel_manager_sender);
        Self {
            sv1_server_io,
            config,
            listener_addr,
            shares_per_minute,
            miner_counter: Arc::new(AtomicU32::new(0)),
            sequence_counter: Arc::new(AtomicU32::new(1)),
            keepalive_job_id_counter: Arc::new(AtomicU32::new(0)),
            aggregated_job_mutation_time: SharedLock::new(None),
            downstream_id_factory: Arc::new(AtomicUsize::new(1)),
            request_id_factory: Arc::new(AtomicU32::new(1)),
            downstreams: SharedMap::new(),
            #[cfg(feature = "monitoring")]
            miner_telemetry: MinerTelemetryState::new(),
            request_id_to_downstream_id: SharedMap::new(),
            channels: SharedMap::new(),
            vardiff: SharedMap::new(),
            mode,
            user_identity: Arc::new(OnceLock::new()),
        }
    }

    pub fn set_user_identity(&self, user_identity: String) {
        self.user_identity
            .set(user_identity)
            .expect("user identity already set");
    }

    fn user_identity(&self) -> &String {
        self.user_identity
            .get()
            .expect("user identity should exist")
    }

    /// Starts the SV1 server and begins accepting connections.
    ///
    /// This method:
    /// - Binds to the configured listening address
    /// - Spawns the variable difficulty adjustment loop
    /// - Enters the main event loop to handle:
    ///   - New miner connections
    ///   - Shutdown signals
    ///   - Messages from downstream miners (submit shares)
    ///   - Messages from upstream SV2 channel manager
    ///
    /// The server will continue running until a shutdown signal is received.
    ///
    /// # Arguments
    /// * `cancellation_token` - Global application cancellation token
    /// * `fallback_coordinator` - Fallback coordinator
    /// * `task_manager` - Manager for spawned async tasks
    ///
    /// # Returns
    /// * `Ok(())` - Server shut down gracefully
    /// * `Err(TproxyError)` - Server encountered an error
    pub async fn start(
        self: Arc<Self>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) -> TproxyResult<(), error::Sv1Server> {
        info!("Starting SV1 server on {}", self.listener_addr);

        // get the first target for the first set difficulty message
        let first_target: Target = hash_rate_to_target(
            self.config
                .downstream_difficulty_config
                .min_individual_miner_hashrate as f64,
            self.config.downstream_difficulty_config.shares_per_minute as f64,
        )
        .unwrap();

        let vardiff_future = self.clone().spawn_vardiff_loop();

        let keepalive_future = self.clone().spawn_job_keepalive_loop();

        let listener = TcpListener::bind(self.listener_addr).await.map_err(|e| {
            error!("Failed to bind to {}: {}", self.listener_addr, e);
            TproxyError::shutdown(e)
        })?;

        info!("Translator Proxy: listening on {}", self.listener_addr);

        let task_manager_clone = task_manager.clone();
        let vardiff_enabled = self.config.downstream_difficulty_config.enable_vardiff;
        let keepalive_enabled = self
            .config
            .downstream_difficulty_config
            .job_keepalive_interval_secs
            > 0;
        let fallback_token = fallback_coordinator.token();
        task_manager_clone.spawn(fallback_coordinator.clone().track(async move {
            tokio::pin!(vardiff_future);
            tokio::pin!(keepalive_future);
            // Set after a failed accept() to pause accepting until this instant, while the other
            // arms keep relaying for downstreams that are already connected.
            let mut accept_retry_at: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    biased;
                    // Handle app shutdown signal
                    _ = cancellation_token.cancelled() => {
                        debug!("SV1 Server: received shutdown signal. Exiting.");
                        self.cleanup();
                        break;
                    }

                    // Handle fallback trigger
                    _ = fallback_token.cancelled() => {
                        info!("SV1 Server: fallback triggered, clearing state");
                        self.cleanup();
                        break;
                    }
                    _ = tokio::time::sleep_until(accept_retry_at.unwrap_or_else(tokio::time::Instant::now)), if accept_retry_at.is_some() => {
                        accept_retry_at = None;
                    }
                    result = listener.accept(), if accept_retry_at.is_none() => {
                        match result {
                            Ok((stream, addr)) => {
                                info!("New SV1 downstream connection from {}", addr);
                                let connection_token = cancellation_token.child_token();
                                let connection = ConnectionSV1::new(
                                    stream,
                                    connection_token.clone(),
                                ).await;
                                let downstream_id = self.downstream_id_factory.fetch_add(1, Ordering::Relaxed);
                                let (sv1_server_sender, sv1_server_receiver) = async_channel::unbounded();
                                self.sv1_server_io
                                    .sv1_server_to_downstream_sender
                                    .insert(downstream_id, sv1_server_sender);

                                let downstream = Downstream::new(
                                    downstream_id,
                                    connection.sender().clone(),
                                    connection.receiver().clone(),
                                    self.sv1_server_io.downstream_to_sv1_server_sender.clone(),
                                    sv1_server_receiver,
                                    first_target,
                                    Some(self.config.downstream_difficulty_config.min_individual_miner_hashrate),
                                    self.config.max_past_jobs,
                                    #[cfg(feature = "monitoring")]
                                    addr.ip(),
                                    connection_token,
                                );
                                self.downstreams.insert(downstream_id, downstream.clone());
                                info!("Downstream {} registered successfully (channel will be opened after first message)", downstream_id);

                                let sv1_server = self.clone();
                                let disconnect_cancellation_token = cancellation_token.clone();
                                Downstream::start(
                                    downstream,
                                    cancellation_token.clone(),
                                    fallback_coordinator.clone(),
                                    task_manager.clone(),
                                    move || async move {
                                        // Cleanup only ever fails with `Shutdown` (poisoned
                                        // lock), so honour it.
                                        if let Err(e) = sv1_server
                                            .handle_downstream_disconnect(downstream_id)
                                            .await
                                        {
                                            error!(
                                                downstream_id,
                                                error_kind = ?e.kind,
                                                "failed to clean up disconnected downstream; cancelling global token"
                                            );
                                            disconnect_cancellation_token.cancel();
                                        }
                                    },
                                );
                            }
                            Err(e) => {
                                warn!("Failed to accept new connection: {:?}", e);
                                // accept() fails without consuming the pending connection when descriptors or
                                // socket buffers run out, so the listener stays readable and an immediate retry
                                // fails again. Pause accepting so the loop neither spins nor floods the log,
                                // without sleeping here, which would also stall the arms below.
                                accept_retry_at = Some(tokio::time::Instant::now() + Duration::from_millis(100));
                            }
                        }
                    }
                    res = self.handle_downstream_message() => {
                        if let Err(e) = res {
                            if let LoopControl::Break = self.handle_error_action(
                                "Sv1Server::handle_downstream_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ).await {
                                self.cleanup();
                                break;
                            }
                        }
                    }
                    res = self.handle_upstream_message(
                        first_target,
                    ) => {
                        if let Err(e) = res {
                            if let LoopControl::Break = self.handle_error_action(
                                "Sv1Server::handle_upstream_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ).await {
                                self.cleanup();
                                break;
                            }
                        }
                    }
                    // Safe to poll `&mut` on a future that may complete: this loop only
                    // fails with a shutdown error, which breaks out of the select loop,
                    // so it is never polled again.
                    res = &mut vardiff_future, if vardiff_enabled => {
                        if let Err(e) = res {
                            if let LoopControl::Break = self.handle_error_action(
                                "Sv1Server::spawn_vardiff_loop",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ).await {
                                self.cleanup();
                                break;
                            }
                        }
                    }
                    // Safe to poll `&mut` on a future that may complete: this loop only
                    // fails with a shutdown error, which breaks out of the select loop,
                    // so it is never polled again.
                    res = &mut keepalive_future, if keepalive_enabled => {
                        if let Err(e) = res {
                            if let LoopControl::Break = self.handle_error_action(
                                "Sv1Server::spawn_job_keepalive_loop",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ).await {
                                self.cleanup();
                                break;
                            }
                        }
                    }
                }
            }
            debug!("SV1 Server main listener loop exited.");
        }));

        Ok(())
    }

    /// Handles messages received from downstream SV1 miners.
    ///
    /// This method processes share submissions from miners by:
    /// - Updating variable difficulty counters
    /// - Extracting and validating share data
    /// - Converting SV1 share format to SV2 SubmitSharesExtended
    /// - Forwarding the share to the channel manager for upstream submission
    ///
    /// # Returns
    /// * `Ok(())` - Message processed successfully
    /// * `Err(TproxyError)` - Error processing the message
    async fn handle_downstream_message(&self) -> TproxyResult<(), error::Sv1Server> {
        let (downstream_id, downstream_message) = self
            .sv1_server_io
            .downstream_to_sv1_server_receiver
            .recv()
            .await
            .map_err(TproxyError::shutdown)?;

        let (channel_id, subscribed, opening) =
            match self.with_registered_downstream(downstream_id, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        (
                            data.channel_id,
                            data.session_state.is_subscribed(),
                            !data.queued_sv1_handshake_messages.is_empty(),
                        )
                    })
                    .map_err(TproxyError::shutdown)
            }) {
                Ok(state) => state,
                Err(e) if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
        // sv1_api owns all wire-shape and parameter validation. Keep the decoded request so the
        // normal handler can execute it later without a second parser or duplicated checks.
        let request = Client2Server::try_from(downstream_message).map_err(|error| {
            TproxyError::disconnect(
                stratum_apps::stratum_core::sv1_api::error::Error::from(error),
                downstream_id,
            )
        })?;
        // A well-formed submit before mining.subscribe is a request-level rejection, not a
        // transport violation. Reply without allocating an upstream channel so the miner can
        // subscribe and retry on the same connection.
        if !subscribed {
            if let Client2Server::Submit(submit) = &request {
                let response = submit
                    .clone()
                    .respond(SubmitOutcome::Rejected(SubmitError::NotSubscribed));
                let downstream_sender = self
                    .with_registered_downstream(downstream_id, |downstream| {
                        Ok(downstream.downstream_io.downstream_sv1_sender.clone())
                    })?;
                downstream_sender
                    .send(response.into())
                    .await
                    .map_err(|error| {
                        error!(
                            "Down: Failed to send not-subscribed response to downstream: {error:?}"
                        );
                        TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                    })?;
                return Ok(());
            }
        }
        // Configure is optional; subscribe and authorize may arrive in either order. Ancillary
        // setup requests may follow the opening request. A submit cannot start a session; the
        // not-subscribed case was answered above, while an unauthorized worker is rejected by
        // sv1_api after the channel needed for normal request processing is available.
        let can_start = matches!(
            request,
            Client2Server::Configure(_) | Client2Server::Subscribe(_) | Client2Server::Authorize(_)
        );
        if channel_id.is_none() && !opening && !can_start {
            return Err(TproxyError::disconnect(
                TproxyErrorKind::Sv1RequestBeforeSetup,
                downstream_id,
            ));
        }
        if channel_id.is_none() {
            self.with_registered_downstream(downstream_id, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        if data.queued_sv1_handshake_messages.len()
                            >= MAX_QUEUED_SV1_HANDSHAKE_MESSAGES
                        {
                            return Err(TproxyError::disconnect(
                                TproxyErrorKind::Sv1HandshakeMessageQueueFull,
                                downstream_id,
                            ));
                        }

                        data.queued_sv1_handshake_messages.push(request);
                        Ok(())
                    })
                    .map_err(TproxyError::shutdown)?
            })?;
            if !opening {
                self.handle_open_channel_request(downstream_id).await?;
            }
            debug!("Down: Queuing Sv1 message until channel is established");
            return Ok(());
        }

        self.process_sv1_message(downstream_id, request).await?;

        Ok(())
    }

    /// Processes one SV1 message after its downstream SV2 channel is available.
    ///
    /// Setup progress is recorded immediately after a successful response is queued to the miner.
    /// Pre-subscribe prefix updates are applied in this same server task, so a queued downstream
    /// event cannot change the response prefix during construction. The completion event releases
    /// cached notifications in FIFO order.
    async fn process_sv1_message(
        &self,
        downstream_id: DownstreamId,
        downstream_message: Client2Server,
    ) -> TproxyResult<(), error::Sv1Server> {
        let setup_request = match &downstream_message {
            Client2Server::Subscribe(_) => Some(Sv1SetupRequest::Subscribe),
            Client2Server::Authorize(_) => Some(Sv1SetupRequest::Authorize),
            _ => None,
        };
        let response = self
            .clone()
            .handle_parsed_request(Some(downstream_id), downstream_message)
            .map_err(|e| e.with_sv1_downstream_context(downstream_id));

        match response {
            Ok(Some(response_msg)) => {
                debug!("Down: Sending Sv1 message to downstream: {}", response_msg);
                let downstream_sender = self
                    .with_registered_downstream(downstream_id, |downstream| {
                        Ok(downstream.downstream_io.downstream_sv1_sender.clone())
                    })?;
                downstream_sender
                    .send(response_msg.into())
                    .await
                    .map_err(|error| {
                        error!("Down: Failed to send SV1 response to downstream: {error:?}");
                        TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                    })?;

                if let Some(setup_request) = setup_request {
                    let setup_just_completed =
                        self.with_registered_downstream(downstream_id, |downstream| {
                            downstream
                                .downstream_data
                                .with(|data| data.session_state.record_response(setup_request))
                                .map_err(TproxyError::shutdown)
                        })?;
                    if setup_just_completed {
                        let event_sender = self
                            .sv1_server_io
                            .sv1_server_to_downstream_sender
                            .get_cloned(&downstream_id)
                            .ok_or_else(|| {
                                TproxyError::disconnect(
                                    TproxyErrorKind::DownstreamNotPresent(downstream_id),
                                    downstream_id,
                                )
                            })?;
                        event_sender
                            .send(Sv1ServerEvent::SetupComplete)
                            .await
                            .map_err(|error| {
                                error!(
                                    "Down: Failed to queue setup completion for downstream: {error:?}"
                                );
                                TproxyError::disconnect(
                                    TproxyErrorKind::ChannelErrorSender,
                                    downstream_id,
                                )
                            })?;
                    }
                }
            }
            Ok(None) => {
                // Message was handled but no response needed
            }
            Err(e) => {
                error!("Down: Error handling downstream message: {:?}", e);
                return Err(e);
            }
        }

        // Check if there's a pending share to send to the Sv1Server
        let pending_share = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|d| d.pending_share.take())
                .map_err(TproxyError::shutdown)
        })?;
        if let Some(share) = pending_share {
            self.handle_submit_shares(share).await?;
        }

        Ok(())
    }

    /// Handles share submission messages from downstream.
    async fn handle_submit_shares(
        &self,
        message: SubmitShareWithChannelId,
    ) -> TproxyResult<(), error::Sv1Server> {
        // Increment vardiff counter for this downstream (only if vardiff is enabled)
        if self.config.downstream_difficulty_config.enable_vardiff {
            self.vardiff.with_mut(&message.downstream_id, |state| {
                state.increment_shares_since_last_update();
            });
        }

        let job_version = match message.job_version {
            Some(version) => version,
            None => {
                warn!("Received share submission without valid job version, skipping");
                return Ok(());
            }
        };

        // If this is a keepalive job, extract the original upstream job_id from the job_id string
        let mut share = message.share;
        let job_id_str = share.job_id.clone();
        if Self::is_keepalive_job_id(&job_id_str) {
            if let Some(original_job_id) = Self::extract_original_job_id(&job_id_str) {
                debug!(
                    "Extracting original job_id {} from keepalive job_id {}",
                    original_job_id, job_id_str
                );
                share.job_id = original_job_id;
            } else {
                warn!(
                    "Failed to extract original job_id from keepalive job_id {}, rejecting share",
                    job_id_str
                );
                return Ok(());
            }
        }

        // Increment and return the value for this share
        let sequence_number = self.sequence_counter.fetch_add(1, Ordering::SeqCst);

        let submit_share_extended = build_sv2_submit_shares_extended_from_sv1_submit(
            &share,
            message.channel_id,
            sequence_number,
            job_version,
            message.version_rolling_mask,
        )
        .map_err(TproxyError::shutdown)?;

        let worker_name = match self.with_registered_downstream(
            message.downstream_id,
            |downstream| {
                downstream
                    .downstream_data
                    .with(|data| data.sv1_worker_name.clone())
                    .map_err(TproxyError::shutdown)
            },
        ) {
            Ok(worker_name) => worker_name,
            Err(e) if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) => {
                warn!(
                    "Downstream {} disconnected before its share could be forwarded; dropping share",
                    message.downstream_id
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let sv1_worker_name = (!worker_name.is_empty()).then_some(worker_name);

        self.sv1_server_io
            .channel_manager_sender
            .send((
                MiningOwned::SubmitSharesExtended(submit_share_extended),
                Sv1ShareMetadata::for_share(sv1_worker_name, message.share_hash),
            ))
            .await
            .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;

        Ok(())
    }

    /// Handles channel opening requests from downstream when they send their first message.
    async fn handle_open_channel_request(
        &self,
        downstream_id: DownstreamId,
    ) -> TproxyResult<(), error::Sv1Server> {
        info!(
            "SV1 server: opening extended mining channel for downstream {} after first message",
            downstream_id
        );

        if !self.downstreams.contains_key(&downstream_id) {
            error!(
                "Downstream {} not found when attempting to open channel",
                downstream_id
            );
            return Err(TproxyError::disconnect(
                TproxyErrorKind::DownstreamNotPresent(downstream_id),
                downstream_id,
            ));
        }

        let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
        self.request_id_to_downstream_id
            .insert(request_id, downstream_id);

        self.forward_pending_open_channel_request(request_id, downstream_id)
            .await
    }

    /// Forwards an open request that has already been registered, removing its mapping if the
    /// request cannot be sent.
    async fn forward_pending_open_channel_request(
        &self,
        request_id: RequestId,
        downstream_id: DownstreamId,
    ) -> TproxyResult<(), error::Sv1Server> {
        if let Err(e) = self
            .open_extended_mining_channel(request_id, downstream_id)
            .await
        {
            self.request_id_to_downstream_id.remove(&request_id);
            return Err(e);
        }

        Ok(())
    }

    /// Handles messages received from the upstream SV2 server via the channel manager.
    ///
    /// This method processes various SV2 messages including:
    /// - OpenExtendedMiningChannelSuccess: Sets up downstream connections
    /// - NewExtendedMiningJob: Converts to SV1 notify messages
    /// - SetNewPrevHash: Updates block template information
    /// - Channel error messages (TODO: implement proper handling)
    ///
    /// # Arguments
    /// * `first_target` - Initial difficulty target for new connections
    ///
    /// # Returns
    /// * `Ok(())` - Message processed successfully
    /// * `Err(TproxyError)` - Error processing the message
    async fn handle_upstream_message(
        &self,
        first_target: Target,
    ) -> TproxyResult<(), error::Sv1Server> {
        let message = self
            .sv1_server_io
            .channel_manager_receiver
            .recv()
            .await
            .map_err(TproxyError::shutdown)?;

        match message {
            MiningOwned::OpenExtendedMiningChannelSuccess(m) => {
                debug!(
                    "Received OpenExtendedMiningChannelSuccess for channel id: {}",
                    m.channel_id
                );
                let downstream_id = self.request_id_to_downstream_id.remove(&m.request_id);

                let Some((_, downstream_id)) = downstream_id else {
                    // A setup error may disconnect the miner while its channel is opening. If the
                    // success races with that cleanup, release the resulting unclaimed channel.
                    // Do not close a channel already routed by an earlier success.
                    if self.channel_owner(m.channel_id).is_none() {
                        self.sv1_server_io
                            .channel_manager_sender
                            .send((
                                MiningOwned::CloseChannel(CloseChannelOwned {
                                    channel_id: m.channel_id,
                                    reason_code: Str0255Owned::try_from(
                                        "downstream disconnected".to_string(),
                                    )
                                    .unwrap(),
                                }),
                                Sv1ShareMetadata::none(),
                            ))
                            .await
                            .map_err(|_| {
                                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                            })?;
                    }
                    return Err(TproxyError::log(TproxyErrorKind::RequestIdNotFound(
                        m.request_id,
                    )));
                };
                let initial_target = Target::from_le_bytes(m.target.to_array());
                let vardiff_enabled = self.config.downstream_difficulty_config.enable_vardiff;
                let upstream_hashrate = if vardiff_enabled {
                    None
                } else {
                    self.hashrate_from_upstream_target(initial_target)
                };
                let extranonce1 = m
                    .extranonce_prefix
                    .to_owned_bytes()
                    .try_into()
                    .map_err(TproxyError::fallback)?;
                let downstream_setup =
                    self.with_registered_downstream(downstream_id, |downstream| {
                        downstream
                            .downstream_data
                            .with(|d| {
                                d.extranonce1 = extranonce1;
                                d.extranonce2_len = m.extranonce_size.into();
                                d.channel_id = Some(m.channel_id);
                                // Set the initial upstream target from
                                // OpenExtendedMiningChannelSuccess
                                d.set_upstream_target(initial_target, downstream_id);
                                if vardiff_enabled {
                                    // Vardiff starts from the configured minimum hashrate.
                                    d.desired_target = Some(first_target);
                                } else if let Some(hashrate) = upstream_hashrate {
                                    // Without vardiff the upstream sets the difficulty, so
                                    // the reported hashrate is the one its target implies.
                                    d.hashrate = Some(hashrate);
                                }
                            })
                            .map_err(TproxyError::shutdown)?;

                        let queued_messages = downstream
                            .downstream_data
                            .with(|d| std::mem::take(&mut d.queued_sv1_handshake_messages))
                            .map_err(TproxyError::shutdown)?;
                        self.channels.with_mut_or_default(m.channel_id, |channel| {
                            channel.owner = Some(downstream_id)
                        });

                        Ok(queued_messages)
                    });

                match downstream_setup {
                    Ok(queued_messages) => {
                        // A downstream starts contributing shares only after its SV2 channel is
                        // open. Register vardiff at the same lifecycle boundary so the periodic
                        // update does not include a miner that is still opening its channel.
                        if vardiff_enabled {
                            let vardiff =
                                VardiffState::new().expect("Failed to create vardiffstate");
                            self.vardiff.insert(downstream_id, vardiff);
                        }

                        // Queue the initial difficulty before replaying the setup messages:
                        // completing setup releases any job cached while the channel was opening,
                        // and that job must reach the miner with its difficulty.
                        self.update_difficulty(downstream_id)?;

                        // Process all queued messages now that channel is established
                        if !queued_messages.is_empty() {
                            info!(
                                "Processing {} queued Sv1 messages for downstream {}",
                                queued_messages.len(),
                                downstream_id
                            );

                            for message in queued_messages {
                                self.process_sv1_message(downstream_id, message).await?;
                            }
                        }

                        // Opening a downstream changes the aggregate just like disconnecting one.
                        // Refresh it now so the newly active hashrate is not left out until the
                        // next vardiff update.
                        if self.mode.is_aggregated() && vardiff_enabled {
                            self.send_update_channel_on_downstream_state_change()
                                .await?;
                        }
                    }
                    Err(e) => {
                        if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                            error!("Downstream not found for downstream_id: {}", downstream_id);
                            let reason_code =
                                Str0255Owned::try_from("downstream disconnected".to_string())
                                    .unwrap();
                            self.sv1_server_io
                                .channel_manager_sender
                                .send((
                                    MiningOwned::CloseChannel(CloseChannelOwned {
                                        channel_id: m.channel_id,
                                        reason_code,
                                    }),
                                    Sv1ShareMetadata::none(),
                                ))
                                .await
                                .map_err(|_| {
                                    TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                                })?;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }

            MiningOwned::OpenMiningChannelError(m) => {
                warn!(
                    request_id = m.request_id,
                    error_code = %m.error_code.as_utf8_or_hex(),
                    "Channel manager rejected downstream channel request"
                );
                let downstream_id = self.request_id_to_downstream_id.remove(&m.request_id);
                let Some((_, downstream_id)) = downstream_id else {
                    return Err(TproxyError::log(TproxyErrorKind::RequestIdNotFound(
                        m.request_id,
                    )));
                };
                return Err(TproxyError::disconnect(
                    TproxyErrorKind::OpenMiningChannelError,
                    downstream_id,
                ));
            }

            MiningOwned::NewExtendedMiningJob(m) => {
                debug!(
                    "Received NewExtendedMiningJob for channel id: {}",
                    m.channel_id
                );
                let job_channel_id = if self.mode.is_aggregated() {
                    AGGREGATED_CHANNEL_ID
                } else {
                    m.channel_id
                };
                // Clone the prevhash immediately so shared map access is not held across .await.
                if let Some(prevhash) = self
                    .channels
                    .with(&job_channel_id, |channel| channel.prevhash.clone())
                    .flatten()
                {
                    let clean_jobs = m.job_id == prevhash.job_id;
                    let notify = build_sv1_notify_from_sv2(prevhash, m.clone(), clean_jobs)
                        .map_err(TproxyError::shutdown)?;

                    // A late joiner starts from the current shared work, including any rolled
                    // nTime. Its bootstrap initializes only that miner's view, without mutating
                    // the aggregate history or restarting the shared keepalive interval.
                    if self.mode.is_aggregated() && m.channel_id != AGGREGATED_CHANNEL_ID {
                        if let Some(current) = self.get_last_job(AGGREGATED_CHANNEL_ID) {
                            let mut current = current.notify;
                            let original_job_id = Self::extract_original_job_id(&current.job_id)
                                .unwrap_or_else(|| current.job_id.clone());
                            if original_job_id != m.job_id.to_string() {
                                warn!(
                                    channel_id = m.channel_id,
                                    job_id = m.job_id,
                                    active_job_id = %current.job_id,
                                    "Ignoring bootstrap that does not match the shared active job"
                                );
                                return Ok(());
                            }
                            current.clean_jobs = true;
                            self.send_to_channel(m.channel_id, current.into()).await;
                            return Ok(());
                        }
                    }

                    // Real upstream jobs and generated keepalives share one history. Serialize
                    // their mutations and restart the shared interval only when that history
                    // advances. A bootstrap can also seed an otherwise empty history.
                    let resets_job_history = clean_jobs
                        && (self.mode.is_non_aggregated() || m.channel_id == AGGREGATED_CHANNEL_ID);
                    let store_notify =
                        || self.store_job(job_channel_id, &notify, resets_job_history);
                    let stored =
                        if self.mode.is_aggregated() && m.channel_id == AGGREGATED_CHANNEL_ID {
                            self.aggregated_job_mutation_time
                                .with(|last_mutation| {
                                    let stored = store_notify();
                                    *last_mutation = Some(Instant::now());
                                    stored
                                })
                                .map_err(TproxyError::shutdown)?
                        } else {
                            store_notify()
                        };
                    if !stored {
                        debug!(
                            channel_id = m.channel_id,
                            "Ignoring NewExtendedMiningJob for a closed channel"
                        );
                        return Ok(());
                    }

                    let notify_msg = Arc::new(notify);
                    // Normal aggregated jobs carry AGGREGATED_CHANNEL_ID and are broadcast. A
                    // bootstrap job for a late joiner carries that downstream's channel ID and
                    // must only be delivered to that miner.
                    self.send_to_channel(m.channel_id, notify_msg).await;
                }
            }

            MiningOwned::SetNewPrevHash(m) => {
                debug!("Received SetNewPrevHash for channel id: {}", m.channel_id);
                if !self.store_prevhash(&m) {
                    debug!(
                        channel_id = m.channel_id,
                        "Ignoring SetNewPrevHash for a channel without a downstream"
                    );
                }
            }

            MiningOwned::SetTarget(m) => {
                self.handle_set_target_message(m).await?;
            }
            MiningOwned::CloseChannel(m) => {
                self.disconnect_downstream_for_upstream_close(m.channel_id)?;
            }
            MiningOwned::SetExtranoncePrefix(m) => {
                self.handle_upstream_extranonce_change(m).await?;
            }
            // Guaranteed unreachable: the channel manager only forwards valid,
            // pre-filtered messages, so no other variants can arrive here.
            _ => unreachable!("Invalid message: should have been filtered earlier"),
        }

        Ok(())
    }

    /// Disconnects the SV1 miner whose dedicated SV2 channel was closed by the upstream.
    ///
    /// The channel association is cleared before cancellation so the downstream task's normal
    /// cleanup does not echo a `CloseChannel` back to an upstream that already closed it.
    #[allow(clippy::result_large_err)]
    fn disconnect_downstream_for_upstream_close(
        &self,
        channel_id: ChannelId,
    ) -> TproxyResult<(), error::Sv1Server> {
        let Some(downstream_id) = self
            .channels
            .remove(&channel_id)
            .and_then(|(_, channel)| channel.owner)
        else {
            warn!(
                channel_id,
                "No active SV1 downstream found for upstream-closed channel"
            );
            return Ok(());
        };

        let Some(result) = self.downstreams.with(&downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    if data.channel_id == Some(channel_id) {
                        data.channel_id = None;
                    }
                })
                .map_err(TproxyError::shutdown)?;
            downstream.disconnect();
            Ok(())
        }) else {
            warn!(
                downstream_id,
                channel_id, "SV1 downstream disappeared while processing upstream CloseChannel"
            );
            return Ok(());
        };

        result
    }

    /// Orders a job's prefix against subscribe-response construction in this server task.
    ///
    /// Before subscription, update the response fields synchronously. The queued event must not
    /// apply them again: newer updates may already be reflected in the response. After
    /// subscription, the downstream applies the prefix in FIFO order and checks support when
    /// delivering its job.
    async fn handle_upstream_extranonce_change(
        &self,
        message: stratum_apps::stratum_core::mining_sv2::SetExtranoncePrefixOwned,
    ) -> TproxyResult<(), error::Sv1Server> {
        let channel_id = message.channel_id;
        let Some(downstream_id) = self.channel_owner(channel_id) else {
            warn!(
                channel_id,
                "No active SV1 downstream found for changed extranonce prefix"
            );
            return Ok(());
        };

        let Some(downstream) = self.downstreams.get_cloned(&downstream_id) else {
            warn!(
                downstream_id,
                channel_id, "SV1 downstream disappeared while processing SetExtranoncePrefix"
            );
            return Ok(());
        };

        let extranonce2_len = downstream
            .downstream_data
            .with(|data| data.extranonce2_len)
            .map_err(TproxyError::shutdown)?;

        let extranonce1: Extranonce = message
            .extranonce_prefix
            .to_owned_bytes()
            .try_into()
            .map_err(TproxyError::fallback)?;
        let notify_miner = downstream
            .downstream_data
            .with(|data| {
                data.pending_set_extranonce_notifications =
                    data.pending_set_extranonce_notifications.saturating_add(1);
                data.keepalive_timer_anchor = None;
                let notify_miner = data.session_state.is_subscribed();
                if !notify_miner {
                    data.extranonce1 = extranonce1.clone();
                    data.extranonce2_len = extranonce2_len;
                }
                notify_miner
            })
            .map_err(TproxyError::shutdown)?;

        let notification = server_to_client::SetExtranonce {
            extra_nonce1: extranonce1,
            extra_nonce2_size: extranonce2_len,
        };
        self.sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&downstream_id)
            .ok_or_else(|| {
                TproxyError::disconnect(
                    TproxyErrorKind::DownstreamNotPresent(downstream_id),
                    downstream_id,
                )
            })?
            .send(Sv1ServerEvent::SetExtranonce {
                message: notification,
                notify_miner,
            })
            .await
            .map_err(|error| {
                error!(
                    downstream_id,
                    channel_id, "Failed to queue mining.set_extranonce: {error:?}"
                );
                TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
            })
    }

    /// Opens an extended mining channel for a downstream connection.
    ///
    /// This method initiates the SV2 channel setup process by:
    /// - Calculating the initial target based on configuration
    /// - Generating a unique user identity for the miner
    /// - Creating an OpenExtendedMiningChannel message
    /// - Sending the request to the channel manager
    ///
    /// # Arguments
    /// * `downstream` - The downstream connection to set up a channel for
    ///
    /// # Returns
    /// * `Ok(())` - Channel setup request sent successfully
    /// * `Err(TproxyError)` - Error setting up the channel
    async fn open_extended_mining_channel(
        &self,
        request_id: RequestId,
        downstream_id: DownstreamId,
    ) -> TproxyResult<(), error::Sv1Server> {
        let config = &self.config.downstream_difficulty_config;
        if !self.downstreams.contains_key(&downstream_id) {
            warn!(
                "Downstream {} disconnected before channel could be opened, skipping",
                downstream_id
            );
            return Err(TproxyError::disconnect(
                TproxyErrorKind::DownstreamNotPresent(downstream_id),
                downstream_id,
            ));
        }

        let hashrate = config.min_individual_miner_hashrate as f64;
        let shares_per_min = config.shares_per_minute as f64;
        let min_extranonce_size = self.config.downstream_extranonce2_size;
        let vardiff_enabled = config.enable_vardiff;

        let max_target = if vardiff_enabled {
            hash_rate_to_target(hashrate, shares_per_min).unwrap()
        } else {
            // If translator doesn't manage vardiff, we rely on upstream to do that,
            // so we give it more freedom by setting max_target to maximum possible value
            Target::from_le_bytes([0xff; 32])
        };

        let miner_id = self.miner_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let user_identity = self.user_identity();
        // SRI patterns use `/`-delimited segments for payout mode parsing, so appending
        // a suffix would break pool-side validation.
        // See: https://github.com/stratum-mining/sv2-apps/issues/369
        let user_identity = if user_identity.starts_with("sri/") {
            user_identity.clone()
        } else {
            format!("{user_identity}.miner{miner_id}")
        };

        let open_channel_msg = build_sv2_open_extended_mining_channel(
            request_id,
            user_identity.clone(),
            hashrate as Hashrate,
            max_target,
            min_extranonce_size,
        )
        .map_err(TproxyError::shutdown)?;

        self.sv1_server_io
            .channel_manager_sender
            .send((
                MiningOwned::OpenExtendedMiningChannel(open_channel_msg),
                Sv1ShareMetadata::none(),
            ))
            .await
            .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;

        Ok(())
    }

    /// Handles cleanup when a downstream connection disconnects.
    ///
    /// This method should be called from the main loop when a `State::DownstreamShutdown`
    /// status message is received. It:
    /// - Removes the downstream from the downstreams map
    /// - Removes vardiff state (if enabled)
    /// - Sends UpdateChannel if needed (aggregated mode with vardiff)
    /// - Sends CloseChannel message to ChannelManager (non-aggregated mode)
    ///
    /// # Arguments
    /// * `downstream_id` - The ID of the downstream that disconnected
    pub async fn handle_downstream_disconnect(
        &self,
        downstream_id: DownstreamId,
    ) -> TproxyResult<(), error::Sv1Server> {
        if self.config.downstream_difficulty_config.enable_vardiff {
            self.vardiff.remove(&downstream_id);
        }
        #[cfg(feature = "monitoring")]
        self.miner_telemetry.remove_downstream(downstream_id);
        self.sv1_server_io
            .sv1_server_to_downstream_sender
            .remove(&downstream_id);

        let current_downstream = self.downstreams.remove(&downstream_id);
        self.request_id_to_downstream_id
            .retain(|_, id| *id != downstream_id);

        if let Some((downstream_id, downstream)) = current_downstream {
            info!(
                "🔌 Downstream: {downstream_id} disconnected and removed from sv1 server downstreams"
            );
            // In aggregated mode, send UpdateChannel to reflect the new state (only if vardiff
            // enabled)
            if self.config.downstream_difficulty_config.enable_vardiff {
                if let Err(e) = self.send_update_channel_on_downstream_state_change().await {
                    error!(
                        "Failed to send UpdateChannel after downstream {} disconnect: {:?}",
                        downstream_id, e
                    );
                }
            }

            let channel_id = downstream
                .downstream_data
                .with(|d| d.channel_id)
                .map_err(TproxyError::shutdown)?;
            if let Some(channel_id) = channel_id {
                // The channel is the downstream's own: dropping its entry releases its job
                // state in non-aggregated mode, and never touches the shared aggregated channel.
                self.channels.remove(&channel_id);
                // Send `CloseChannel` to the channel manager in both modes so
                // it can free the per-downstream `ExtendedChannel` (and, in
                // aggregated mode, the allocator-minted `ExtranoncePrefix`
                // bitmap slot owned by that channel). The ChannelManager
                // only forwards the `CloseChannel` upstream in non-aggregated mode;
                // in aggregated mode the upstream channel is shared across
                // all downstreams and must stay open.
                info!("Sending CloseChannel message: {channel_id} for downstream: {downstream_id}");
                let reason_code =
                    Str0255Owned::try_from("downstream disconnected".to_string()).unwrap();
                _ = self
                    .sv1_server_io
                    .channel_manager_sender
                    .send((
                        MiningOwned::CloseChannel(CloseChannelOwned {
                            channel_id,
                            reason_code,
                        }),
                        Sv1ShareMetadata::none(),
                    ))
                    .await;
            }
        }
        Ok(())
    }

    /// Spawns the job keepalive loop that sends periodic mining.notify messages.
    ///
    /// This prevents SV1 miners from timing out when there are no new jobs received from the
    /// upstream for a while.
    async fn spawn_job_keepalive_loop(self: Arc<Self>) -> TproxyResult<(), error::Sv1Server> {
        let keepalive_interval_secs = self
            .config
            .downstream_difficulty_config
            .job_keepalive_interval_secs;

        let check_interval =
            Duration::from_secs(keepalive_interval_secs as u64 / 2).max(Duration::from_secs(5));
        info!(
            "Starting job keepalive loop with interval of {} seconds",
            keepalive_interval_secs
        );

        loop {
            tokio::time::sleep(check_interval).await;
            self.send_keepalive_jobs(keepalive_interval_secs).await?;
        }
    }

    /// Sends work only to miners whose individual keepalive interval has elapsed.
    /// Aggregated miners reuse the latest shared job between wall-clock-bounded mutations.
    async fn send_keepalive_jobs(
        &self,
        keepalive_interval_secs: u16,
    ) -> TproxyResult<(), error::Sv1Server> {
        let interval = Duration::from_secs(keepalive_interval_secs as u64);
        let mut keepalive_targets = Vec::new();
        self.downstreams.try_for_each(|downstream_id, downstream| {
            let keepalive_target = downstream
                .downstream_data
                .with(|d| {
                    // Only send keepalive if:
                    // 1. Handshake is complete
                    // 2. Enough time has passed since last job
                    let handshake_complete = d.session_state.is_ready();

                    if !handshake_complete {
                        return None;
                    }

                    let last_time = d.keepalive_timer_anchor?;
                    if last_time.elapsed() < interval {
                        return None;
                    }

                    // Keepalive eligibility implies an established channel, so the pre-channel
                    // `None` never reaches the keepalive layer.
                    Some((downstream_id, d.channel_id?))
                })
                .map_err(TproxyError::shutdown)?;
            if let Some(keepalive_target) = keepalive_target {
                keepalive_targets.push(keepalive_target);
            }
            Ok::<(), TproxyError<error::Sv1Server>>(())
        })?;

        if keepalive_targets.is_empty() {
            return Ok(());
        }

        if self.mode.is_aggregated() {
            // Every aggregated downstream shares one job history. Gate mutation on aggregate-wide
            // wall time so phase-offset or unreachable miners cannot advance nTime on each poll.
            let notify = self
                .aggregated_job_mutation_time
                .with(|last_mutation| {
                    if last_mutation.is_some_and(|last_mutation| last_mutation.elapsed() < interval)
                    {
                        // A fresh upstream job is already queued to every miner. Only
                        // keepalives need selective replay to miners becoming due later.
                        return self
                            .get_last_job(AGGREGATED_CHANNEL_ID)
                            .map(|job| job.notify)
                            .filter(|job| Self::is_keepalive_job_id(&job.job_id));
                    }
                    let notify =
                        self.create_keepalive_job(AGGREGATED_CHANNEL_ID, keepalive_interval_secs)?;
                    *last_mutation = Some(Instant::now());
                    Some(notify)
                })
                .map_err(TproxyError::shutdown)?;
            let Some(notify) = notify else {
                return Ok(());
            };

            // A shared job need not reach every miner at once. Each recipient's own
            // delivery resets its deadline; reuse does not restart the shared mutation gate.
            for (downstream_id, _) in keepalive_targets {
                self.send_keepalive_to_downstream(downstream_id, notify.clone())
                    .await?;
            }
        } else {
            // Non-aggregated downstreams have independent job histories and therefore need one
            // mutation per channel.
            for (downstream_id, channel_id) in keepalive_targets {
                if let Some(notify) = self.create_keepalive_job(channel_id, keepalive_interval_secs)
                {
                    self.send_keepalive_to_downstream(downstream_id, notify)
                        .await?;
                }
            }
        }

        Ok(())
    }

    fn create_keepalive_job(
        &self,
        channel_id: ChannelId,
        keepalive_interval_secs: u16,
    ) -> Option<server_to_client::Notify> {
        let last_job = self.get_last_job(channel_id)?;
        let original_job_id = Self::extract_original_job_id(&last_job.notify.job_id)
            .unwrap_or_else(|| last_job.notify.job_id.clone());
        let base_time = last_job.keepalive_base_time;

        // Keep nTime within Bitcoin Core's maximum future-time policy relative to the original
        // upstream job.
        let new_time = last_job
            .notify
            .time
            .0
            .saturating_add(keepalive_interval_secs as u32)
            .min(base_time.saturating_add(MAX_FUTURE_BLOCK_TIME));
        if new_time == last_job.notify.time.0 {
            return None;
        }

        let mut keepalive_notify = last_job.notify;
        keepalive_notify.job_id = self.next_keepalive_job_id(&original_job_id);
        keepalive_notify.time = HexU32Be(new_time);
        // A keepalive only rolls nTime on the current work and must not invalidate prior jobs.
        keepalive_notify.clean_jobs = false;

        let job_channel_id = self.resolve_channel_id(channel_id);
        // Do not recreate job history for a channel that was removed after targets were collected.
        self.channels
            .with_mut(&job_channel_id, |channel| {
                channel.jobs.as_mut().map(|jobs| {
                    jobs.activate(
                        keepalive_notify.job_id.clone(),
                        StoredSv1Job {
                            notify: keepalive_notify.clone(),
                            keepalive_base_time: base_time,
                        },
                        false,
                    )
                })
            })
            .flatten()?;

        Some(keepalive_notify)
    }

    async fn send_keepalive_to_downstream(
        &self,
        downstream_id: DownstreamId,
        notify: server_to_client::Notify,
    ) -> TproxyResult<(), error::Sv1Server> {
        debug!(
            "Sending keepalive job to downstream {} with job_id: {}, time: {}",
            downstream_id, notify.job_id, notify.time.0
        );

        let Some(sender) = self
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&downstream_id)
        else {
            warn!("No sender found for downstream {}", downstream_id);
            return Ok(());
        };
        if sender
            .send(Sv1ServerEvent::Notify(Arc::new(notify)))
            .await
            .is_err()
        {
            warn!(
                "Failed to send keepalive job to downstream {}",
                downstream_id
            );
            return Ok(());
        }

        // The downstream resets this again when it processes the queued notify. Setting it here
        // prevents another server keepalive tick before delivery; both writes only move it ahead.
        if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.keepalive_timer_anchor = Some(Instant::now()))
                .map_err(TproxyError::shutdown)
        }) && !matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_))
        {
            return Err(e);
        }

        Ok(())
    }

    /// Generates a keepalive job ID by appending a mutation counter to the original job ID.
    /// Format: `{original_job_id}#{counter}` where `#` is the delimiter.
    /// When receiving a share, split on `#` to extract the original job ID.
    fn next_keepalive_job_id(&self, original_job_id: &str) -> String {
        let counter = self
            .keepalive_job_id_counter
            .fetch_add(1, Ordering::Relaxed);
        format!("{original_job_id}#{counter}")
    }

    /// Extracts the original upstream job ID from a keepalive job ID.
    /// Returns None if the job_id doesn't contain the keepalive delimiter.
    fn extract_original_job_id(job_id: &str) -> Option<String> {
        job_id
            .split_once(KEEPALIVE_JOB_ID_DELIMITER)
            .map(|(original, _)| original.to_string())
    }

    /// Returns true if the job_id is a keepalive job (contains the delimiter).
    #[inline]
    fn is_keepalive_job_id(job_id: &str) -> bool {
        job_id.contains(KEEPALIVE_JOB_ID_DELIMITER)
    }

    /// Resolves the storage key for a channel's job history. Aggregated downstreams share the
    /// single broadcast history keyed by `AGGREGATED_CHANNEL_ID`, while non-aggregated channels
    /// store their jobs under their own channel ID.
    fn resolve_channel_id(&self, channel_id: ChannelId) -> ChannelId {
        if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            channel_id
        }
    }

    /// Gets the last job from the jobs storage.
    /// In aggregated mode, returns the last job from the shared job list.
    /// In non-aggregated mode, returns the last job for the specified channel.
    fn get_last_job(&self, channel_id: ChannelId) -> Option<StoredSv1Job> {
        let channel_id = self.resolve_channel_id(channel_id);

        self.channels
            .with(&channel_id, |channel| {
                channel
                    .jobs
                    .as_ref()
                    .and_then(|jobs| jobs.active().cloned())
            })
            .flatten()
    }

    /// The downstream that owns `channel_id`, if any.
    fn channel_owner(&self, channel_id: ChannelId) -> Option<DownstreamId> {
        self.channels
            .with(&channel_id, |channel| channel.owner)
            .flatten()
    }

    /// Stores a `SetNewPrevHash` in its channel's entry, and returns whether it was stored.
    ///
    /// A downstream's channel is only stored into while its entry exists, which ends when the
    /// channel closes: messages still queued for a closed channel, or for a channel whose
    /// downstream left before it opened, cannot recreate its state. A store and the removal
    /// touch the same entry, so they never interleave. The shared aggregated channel's entry is
    /// created by its first `SetNewPrevHash`.
    fn store_prevhash(&self, prevhash: &SetNewPrevHashOwned) -> bool {
        let store = |channel: &mut ChannelState| channel.prevhash = Some(prevhash.clone());
        if self.mode.is_aggregated() {
            self.channels
                .with_mut_or_default(prevhash.channel_id, store);
            return true;
        }
        self.channels
            .with_mut(&prevhash.channel_id, store)
            .is_some()
    }

    /// Stores a job in its channel's job history, and returns whether it was stored.
    ///
    /// Like [`Self::store_prevhash`], it only stores into an existing entry, so a closed channel's
    /// job history is never recreated.
    fn store_job(
        &self,
        channel_id: ChannelId,
        notify: &server_to_client::Notify,
        resets_job_history: bool,
    ) -> bool {
        self.channels
            .with_mut(&channel_id, |channel| {
                channel
                    .jobs
                    .get_or_insert_with(|| Sv1JobStore::new(self.config.max_past_jobs))
                    .activate(
                        notify.job_id.clone(),
                        StoredSv1Job::upstream(notify.clone()),
                        resets_job_history,
                    );
            })
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_test_channel_owner(
        server: &Sv1Server,
        channel_id: ChannelId,
        downstream_id: DownstreamId,
    ) {
        server
            .channels
            .with_mut_or_default(channel_id, |channel| channel.owner = Some(downstream_id));
    }

    /// Runs `f` on the job history of `channel_id`, creating it if needed.
    fn with_test_jobs<R>(
        server: &Sv1Server,
        channel_id: ChannelId,
        f: impl FnOnce(&mut Sv1JobStore<StoredSv1Job>) -> R,
    ) -> R {
        server.channels.with_mut_or_default(channel_id, |channel| {
            f(channel.jobs.get_or_insert_with(Default::default))
        })
    }

    /// Reads the job history of `channel_id`, if it has one.
    fn read_test_jobs<R>(
        server: &Sv1Server,
        channel_id: ChannelId,
        f: impl FnOnce(&Sv1JobStore<StoredSv1Job>) -> R,
    ) -> Option<R> {
        server
            .channels
            .with(&channel_id, |channel| channel.jobs.as_ref().map(f))
            .flatten()
    }
    use crate::{
        config::{DownstreamDifficultyConfig, TranslatorConfig, Upstream},
        sv1::downstream::{Sv1JobValidationContext, Sv1SessionState},
    };
    use async_channel::unbounded;
    use std::str::FromStr;
    use stratum_apps::{
        key_utils::Secp256k1PublicKey,
        stratum_core::{
            binary_sv2::{Seq0255Owned, Sv2OptionOwned},
            mining_sv2::{
                ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED,
                NewExtendedMiningJobOwned, OpenExtendedMiningChannelSuccessOwned,
                OpenMiningChannelErrorOwned, SetExtranoncePrefixOwned, SetNewPrevHashOwned,
                SetTargetOwned,
            },
            sv1_api::client_to_server,
        },
    };

    fn create_test_config() -> TranslatorConfig {
        let pubkey_str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
        let pubkey = Secp256k1PublicKey::from_str(pubkey_str).unwrap();

        let upstream = Upstream::new(
            "127.0.0.1".to_string(),
            4444,
            pubkey,
            "test_user".to_string(),
        );
        let difficulty_config = DownstreamDifficultyConfig::new(100.0, 5.0, true, 60);

        TranslatorConfig::new(
            vec![upstream],
            "0.0.0.0".to_string(), // downstream_address
            3333,                  // downstream_port
            difficulty_config,     // downstream_difficulty_config
            2,                     // max_supported_version
            1,                     // min_supported_version
            4,                     // downstream_extranonce2_size
            false,                 // verify_payout
            true,                  // aggregate_channels
            vec![],                // supported_extensions
            vec![],                // required_extensions
            None,                  // monitoring_address
            None,                  // monitoring_cache_refresh_secs
        )
    }

    fn create_test_sv1_server() -> Sv1Server {
        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);
        Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode)
    }

    fn register_test_downstream(
        server: &Sv1Server,
        downstream_id: DownstreamId,
        channel_id: Option<ChannelId>,
        hashrate: Hashrate,
        close_server_channel: bool,
    ) -> Receiver<Sv1ServerEvent> {
        register_test_downstream_with_sv1_receiver(
            server,
            downstream_id,
            channel_id,
            hashrate,
            close_server_channel,
        )
        .0
    }

    fn register_test_downstream_with_sv1_receiver(
        server: &Sv1Server,
        downstream_id: DownstreamId,
        channel_id: Option<ChannelId>,
        hashrate: Hashrate,
        close_server_channel: bool,
    ) -> (Receiver<Sv1ServerEvent>, Receiver<json_rpc::Message>) {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_miner_sender, miner_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver) = unbounded();
        if close_server_channel {
            sv1_server_receiver.close();
        }

        let target = hash_rate_to_target(hashrate as f64, 5.0).unwrap();
        let downstream = Downstream::new(
            downstream_id,
            downstream_sv1_sender,
            miner_receiver,
            server.sv1_server_io.downstream_to_sv1_server_sender.clone(),
            sv1_server_receiver.clone(),
            target,
            Some(hashrate),
            server.config.max_past_jobs,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| data.channel_id = channel_id)
            .unwrap();

        server.downstreams.insert(downstream_id, downstream);
        if let Some(channel_id) = channel_id {
            set_test_channel_owner(server, channel_id, downstream_id);
        }
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(downstream_id, sv1_server_sender);

        (sv1_server_receiver, downstream_sv1_receiver)
    }

    fn message_from_server_event(event: Sv1ServerEvent) -> json_rpc::Message {
        match event {
            Sv1ServerEvent::Notify(notify) => (*notify).clone().into(),
            Sv1ServerEvent::SetDifficulty(difficulty) => difficulty.message,
            Sv1ServerEvent::SetExtranonce { message, .. } => message.into(),
            Sv1ServerEvent::SetupComplete => panic!("expected a server notification"),
        }
    }

    fn test_sv1_notify(job_id: &str, clean_jobs: bool) -> server_to_client::Notify {
        let message: json_rpc::Message = serde_json::from_value(serde_json::json!({
            "id": null,
            "method": "mining.notify",
            "params": [
                job_id,
                "00".repeat(32),
                "02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff300344b70e162f5374726174756d2056322053524920506f6f6c2f2f14",
                "feffffff0265adac120000000016001458806074c426bedea9376ac34685eca699f4e90f0000000000000000266a24aa21a9ed50d0d53c0b29a2c630b1c5bd7a133d4758910909abb1a70d37306ed810fa79bc43b70e00",
                [],
                "20000000",
                "1d00ffff",
                "00000001",
                clean_jobs
            ]
        }))
        .unwrap();
        let json_rpc::Message::Notification(notification) = message else {
            unreachable!();
        };
        server_to_client::Notify::try_from(notification).unwrap()
    }

    #[test]
    fn test_sv1_server_creation() {
        let server = create_test_sv1_server();

        assert_eq!(server.shares_per_minute, 5.0);
        assert_eq!(server.listener_addr.ip().to_string(), "127.0.0.1");
        assert_eq!(server.listener_addr.port(), 3333);
    }

    #[test]
    fn late_share_uses_its_job_validation_context() {
        let server = create_test_sv1_server();
        register_test_downstream(&server, 7, Some(9), 100.0, false);
        let old_job = test_sv1_notify("old", false);
        let mut current_job = test_sv1_notify("current", false);
        current_job.version = HexU32Be(0x20000001);
        with_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| {
            jobs.activate(
                old_job.job_id.clone(),
                StoredSv1Job::upstream(old_job.clone()),
                false,
            );
            jobs.activate(
                current_job.job_id.clone(),
                StoredSv1Job::upstream(current_job.clone()),
                false,
            );
        });

        let old_extranonce: Extranonce = vec![1, 2, 3, 4].try_into().unwrap();
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        data.job_validation_contexts.activate(
                            old_job.job_id.clone(),
                            Sv1JobValidationContext {
                                extranonce: old_extranonce.clone(),
                                extranonce2_len: 16,
                                // Any valid hash meets this target. The current target below
                                // accepts none, proving submission uses the job's captured target.
                                target: Target::from_le_bytes([0xff; 32]),
                            },
                            false,
                        );
                        data.extranonce1 = vec![9, 9, 9, 9].try_into().unwrap();
                        data.extranonce2_len = 8;
                        data.target = Target::from_le_bytes([0; 32]);
                    })
                    .unwrap();
            })
            .unwrap();

        let submit = client_to_server::Submit {
            user_name: "worker".to_string(),
            job_id: "old".to_string(),
            extra_nonce2: vec![0; 16].try_into().unwrap(),
            time: HexU32Be(1),
            nonce: HexU32Be(0),
            version_bits: None,
            id: 1,
        };

        assert_eq!(
            server.handle_submit(Some(7), &submit).unwrap(),
            client_to_server::SubmitOutcome::Accepted
        );
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        let pending_share = data.pending_share.as_ref().unwrap();
                        assert_eq!(
                            pending_share.extranonce,
                            Vec::<u8>::from(old_extranonce.clone())
                        );
                        assert_eq!(pending_share.extranonce2_len, 16);
                        assert_eq!(pending_share.job_version, Some(old_job.version.0));
                        assert_ne!(pending_share.job_version, Some(current_job.version.0));
                    })
                    .unwrap();
            })
            .unwrap();
    }

    #[test]
    fn duplicate_sv1_share_is_rejected_before_forwarding() {
        let server = create_test_sv1_server();
        register_test_downstream(&server, 7, Some(9), 100.0, false);
        let job = test_sv1_notify("job", false);
        with_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| {
            jobs.activate(
                job.job_id.clone(),
                StoredSv1Job::upstream(job.clone()),
                false,
            );
        });
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        data.job_validation_contexts.activate(
                            job.job_id.clone(),
                            Sv1JobValidationContext {
                                extranonce: vec![1, 2, 3, 4].try_into().unwrap(),
                                extranonce2_len: 16,
                                target: Target::from_le_bytes([0xff; 32]),
                            },
                            false,
                        );
                    })
                    .unwrap();
            })
            .unwrap();
        let submit = client_to_server::Submit {
            user_name: "worker".to_string(),
            job_id: job.job_id,
            extra_nonce2: vec![0; 16].try_into().unwrap(),
            time: HexU32Be(1),
            nonce: HexU32Be(0),
            version_bits: None,
            id: 1,
        };

        assert_eq!(
            server.handle_submit(Some(7), &submit).unwrap(),
            client_to_server::SubmitOutcome::Accepted
        );
        assert_eq!(
            server.handle_submit(Some(7), &submit).unwrap(),
            client_to_server::SubmitOutcome::Rejected(
                client_to_server::SubmitError::DuplicateShare
            )
        );
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| assert_eq!(data.accepted_share_hashes.len(), 1))
                    .unwrap();
            })
            .unwrap();
    }

    #[test]
    fn sv1_share_rejections_distinguish_missing_jobs_and_low_difficulty() {
        let server = create_test_sv1_server();
        register_test_downstream(&server, 7, Some(9), 100.0, false);
        let mut submit = client_to_server::Submit {
            user_name: "worker".to_string(),
            job_id: "missing".to_string(),
            extra_nonce2: vec![0; 16].try_into().unwrap(),
            time: HexU32Be(1),
            nonce: HexU32Be(0),
            version_bits: None,
            id: 1,
        };

        assert_eq!(
            server.handle_submit(Some(7), &submit).unwrap(),
            client_to_server::SubmitOutcome::Rejected(client_to_server::SubmitError::JobNotFound)
        );

        let job = test_sv1_notify("job", false);
        with_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| {
            jobs.activate(
                job.job_id.clone(),
                StoredSv1Job::upstream(job.clone()),
                false,
            );
        });
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        data.job_validation_contexts.activate(
                            job.job_id.clone(),
                            Sv1JobValidationContext {
                                extranonce: vec![1, 2, 3, 4].try_into().unwrap(),
                                extranonce2_len: 16,
                                // No hash can meet the zero target.
                                target: Target::from_le_bytes([0; 32]),
                            },
                            false,
                        );
                    })
                    .unwrap();
            })
            .unwrap();
        submit.job_id = job.job_id;

        assert_eq!(
            server.handle_submit(Some(7), &submit).unwrap(),
            client_to_server::SubmitOutcome::Rejected(
                client_to_server::SubmitError::LowDifficultyShare
            )
        );
    }

    #[tokio::test]
    async fn authorize_before_subscribe_does_not_enable_mining_notifications_early() {
        let server = create_test_sv1_server();
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_miner_sender, miner_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver) = unbounded();
        let downstream_id = 7;
        let target = hash_rate_to_target(100.0, 5.0).unwrap();
        let downstream = Downstream::new(
            downstream_id,
            downstream_sv1_sender,
            miner_receiver,
            server.sv1_server_io.downstream_to_sv1_server_sender.clone(),
            sv1_server_receiver.clone(),
            target,
            Some(100.0),
            server.config.max_past_jobs,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| data.channel_id = Some(9))
            .unwrap();
        server.downstreams.insert(downstream_id, downstream);
        set_test_channel_owner(&server, 9, downstream_id);
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(downstream_id, sv1_server_sender);

        let cached_set_difficulty: json_rpc::Message =
            serde_json::from_str(r#"{"id":null,"method":"mining.set_difficulty","params":[1.0]}"#)
                .unwrap();
        let cached_notify: json_rpc::Message = serde_json::from_str(
            r#"{"id":null,"method":"mining.notify","params":["job","0000000000000000000000000000000000000000000000000000000000000000","","",[],"20000000","1d00ffff","5f5e1000",true]}"#,
        )
        .unwrap();
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        data.cached_set_difficulty = Some(Sv1Difficulty {
                            message: cached_set_difficulty,
                            target: data.target,
                        });
                        let Sv1ServerEvent::Notify(notify) = Sv1ServerEvent::from(cached_notify)
                        else {
                            panic!("expected notify fixture");
                        };
                        data.cached_notify = Some(notify);
                    })
                    .unwrap();
            })
            .unwrap();

        let authorize: json_rpc::Message = serde_json::from_str(
            r#"{"id":1,"method":"mining.authorize","params":["user.worker","x"]}"#,
        )
        .unwrap();
        server
            .process_sv1_message(7, authorize.try_into().unwrap())
            .await
            .unwrap();

        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            json_rpc::Message::OkResponse(_)
        ));
        assert!(downstream_sv1_receiver.try_recv().is_err());
        assert_eq!(
            server.downstreams.with(&7, |downstream| downstream
                .downstream_data
                .with(|data| data.session_state)
                .unwrap()),
            Some(Sv1SessionState::Starting {
                subscribed: false,
                authorized: true,
            })
        );

        let subscribe: json_rpc::Message =
            serde_json::from_str(r#"{"id":2,"method":"mining.subscribe","params":[]}"#).unwrap();
        server
            .process_sv1_message(7, subscribe.try_into().unwrap())
            .await
            .unwrap();
        server
            .downstreams
            .get_cloned(&7)
            .unwrap()
            .handle_sv1_server_message()
            .await
            .unwrap();

        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            json_rpc::Message::OkResponse(_)
        ));
        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            json_rpc::Message::Notification(notification)
                if notification.method == "mining.set_difficulty"
        ));
        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            json_rpc::Message::Notification(notification)
                if notification.method == "mining.notify"
        ));
        assert_eq!(
            server.downstreams.with(&7, |downstream| downstream
                .downstream_data
                .with(|data| data.session_state)
                .unwrap()),
            Some(Sv1SessionState::Ready)
        );
    }

    #[tokio::test]
    async fn queued_presubscribe_prefix_is_included_in_the_response() {
        let server = create_test_sv1_server();
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_miner_sender, miner_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver) = unbounded();
        let downstream_id = 7;
        let target = hash_rate_to_target(100.0, 5.0).unwrap();
        let downstream = Downstream::new(
            downstream_id,
            downstream_sv1_sender,
            miner_receiver,
            server.sv1_server_io.downstream_to_sv1_server_sender.clone(),
            sv1_server_receiver,
            target,
            Some(100.0),
            server.config.max_past_jobs,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| {
                data.channel_id = Some(42);
                data.supports_set_extranonce = true;
            })
            .unwrap();
        server.downstreams.insert(downstream_id, downstream.clone());
        set_test_channel_owner(&server, 42, downstream_id);
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(downstream_id, sv1_server_sender);

        let authorize: json_rpc::Message = serde_json::from_str(
            r#"{"id":1,"method":"mining.authorize","params":["user.worker","x"]}"#,
        )
        .unwrap();
        server
            .process_sv1_message(7, authorize.try_into().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            json_rpc::Message::OkResponse(_)
        ));

        let new_extranonce_bytes = vec![1, 2, 3, 4];
        let new_extranonce: Extranonce = new_extranonce_bytes.clone().try_into().unwrap();
        server
            .handle_upstream_extranonce_change(SetExtranoncePrefixOwned {
                channel_id: 42,
                extranonce_prefix: new_extranonce_bytes.try_into().unwrap(),
            })
            .await
            .unwrap();

        // Process subscribe before the downstream task consumes the queued prefix change. The
        // server has already applied this prefix, so the response needs no follow-up notification.
        let subscribe: json_rpc::Message =
            serde_json::from_str(r#"{"id":2,"method":"mining.subscribe","params":[]}"#).unwrap();
        server
            .process_sv1_message(7, subscribe.try_into().unwrap())
            .await
            .unwrap();
        let json_rpc::Message::OkResponse(response) = downstream_sv1_receiver.recv().await.unwrap()
        else {
            panic!("expected subscribe response");
        };
        let subscribe_response = server_to_client::Subscribe::try_from(&response).unwrap();
        assert_eq!(subscribe_response.extra_nonce1, new_extranonce);

        downstream.handle_sv1_server_message().await.unwrap();
        assert!(downstream_sv1_receiver.try_recv().is_err());

        downstream.handle_sv1_server_message().await.unwrap();
        assert_eq!(
            downstream
                .downstream_data
                .with(|data| data.session_state)
                .unwrap(),
            Sv1SessionState::Ready
        );
    }

    #[tokio::test]
    async fn queued_prefix_cannot_change_an_in_flight_subscribe_response() {
        let server = create_test_sv1_server();
        // Backpressure is only a test scheduling barrier: pause the real response path
        // after it has captured extranonce1 but before record_response runs.
        let (wire_sender, wire_receiver) = async_channel::bounded(1);
        let (_miner_sender, miner_receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();
        let downstream = Downstream::new(
            7,
            wire_sender.clone(),
            miner_receiver,
            server.sv1_server_io.downstream_to_sv1_server_sender.clone(),
            event_receiver,
            hash_rate_to_target(100.0, 5.0).unwrap(),
            Some(100.0),
            server.config.max_past_jobs,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| {
                data.channel_id = Some(42);
                data.extranonce1 = vec![1, 2, 3, 4].try_into().unwrap();
                data.supports_set_extranonce = true;
            })
            .unwrap();
        server.downstreams.insert(7, downstream.clone());
        set_test_channel_owner(&server, 42, 7);
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(7, event_sender);

        wire_sender
            .send(serde_json::from_str(r#"{"id":99,"result":true,"error":null}"#).unwrap())
            .await
            .unwrap();
        let new_extranonce: Extranonce = vec![9; 4].try_into().unwrap();
        server
            .handle_upstream_extranonce_change(SetExtranoncePrefixOwned {
                channel_id: 42,
                extranonce_prefix: vec![8; 4].try_into().unwrap(),
            })
            .await
            .unwrap();
        server
            .handle_upstream_extranonce_change(SetExtranoncePrefixOwned {
                channel_id: 42,
                extranonce_prefix: vec![9; 4].try_into().unwrap(),
            })
            .await
            .unwrap();
        let subscribe: json_rpc::Message =
            serde_json::from_str(r#"{"id":2,"method":"mining.subscribe","params":[]}"#).unwrap();
        let mut response = Box::pin(server.process_sv1_message(7, subscribe.try_into().unwrap()));
        let pending = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(response.as_mut(), cx).is_pending())
        })
        .await;
        assert!(pending);

        downstream.handle_sv1_server_message().await.unwrap();
        assert_eq!(
            downstream
                .downstream_data
                .with(|data| data.extranonce1.clone())
                .unwrap(),
            new_extranonce
        );
        wire_receiver.recv().await.unwrap();
        response.await.unwrap();
        let json_rpc::Message::OkResponse(response) = wire_receiver.recv().await.unwrap() else {
            panic!("expected subscribe response");
        };
        assert!(
            wire_receiver.is_empty(),
            "no correcting set_extranonce was sent"
        );
        assert_eq!(
            downstream
                .downstream_data
                .with(|data| data.extranonce1.clone())
                .unwrap(),
            new_extranonce
        );
        let subscribe_response = server_to_client::Subscribe::try_from(&response).unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        assert!(wire_receiver.try_recv().is_err());
        assert_eq!(
            subscribe_response.extra_nonce1, new_extranonce,
            "a silently applied prefix must also be advertised in subscribe"
        );
    }

    #[tokio::test]
    async fn prefix_support_is_required_at_job_delivery_not_during_setup() {
        for mode in [TproxyMode::Aggregated, TproxyMode::NonAggregated] {
            for ready_before_change in [false, true] {
                for announce_support in [false, true] {
                    let mut server = create_test_sv1_server();
                    server.mode = mode;
                    let (_, wire) = register_test_downstream_with_sv1_receiver(
                        &server,
                        7,
                        Some(42),
                        100.0,
                        false,
                    );
                    let downstream = server.downstreams.get_cloned(&7).unwrap();
                    let request = |id, method, params| {
                        let message: json_rpc::Message =
                            serde_json::from_value(serde_json::json!({
                                "id": id, "method": method, "params": params,
                            }))
                            .unwrap();
                        Client2Server::try_from(message).unwrap()
                    };
                    server
                        .process_sv1_message(
                            7,
                            request(1, "mining.subscribe", serde_json::json!([])),
                        )
                        .await
                        .unwrap();
                    assert!(matches!(
                        wire.try_recv().unwrap(),
                        json_rpc::Message::OkResponse(_)
                    ));
                    if ready_before_change {
                        server
                            .process_sv1_message(
                                7,
                                request(
                                    2,
                                    "mining.authorize",
                                    serde_json::json!(["user.worker", "x"]),
                                ),
                            )
                            .await
                            .unwrap();
                        wire.try_recv().unwrap();
                        downstream.handle_sv1_server_message().await.unwrap();
                    }
                    server
                        .handle_upstream_extranonce_change(SetExtranoncePrefixOwned {
                            channel_id: 42,
                            extranonce_prefix: vec![9; 4].try_into().unwrap(),
                        })
                        .await
                        .unwrap();
                    downstream.handle_sv1_server_message().await.unwrap();
                    assert!(!downstream.is_disconnected());
                    assert!(wire.try_recv().is_err());
                    let notify: json_rpc::Message = serde_json::from_value(serde_json::json!({
                        "id": null, "method": "mining.notify",
                        "params": ["new", "00".repeat(32), "", "", [], "20000000",
                            "1d00ffff", "5f5e1000", true],
                    }))
                    .unwrap();
                    let sender = server
                        .sv1_server_io
                        .sv1_server_to_downstream_sender
                        .get_cloned(&7)
                        .unwrap();
                    if !ready_before_change {
                        sender
                            .send(Sv1ServerEvent::from(notify.clone()))
                            .await
                            .unwrap();
                        downstream.handle_sv1_server_message().await.unwrap();
                        assert!(!downstream.is_disconnected());
                    }
                    // This announcement is deliberately after the prefix event, and in the
                    // ready case also after authorize. It must still be honored before delivery.
                    if announce_support {
                        server
                            .process_sv1_message(
                                7,
                                request(3, "mining.extranonce.subscribe", serde_json::json!([])),
                            )
                            .await
                            .unwrap();
                        let json_rpc::Message::OkResponse(response) = wire.try_recv().unwrap()
                        else {
                            panic!("expected capability acknowledgement");
                        };
                        assert_eq!(response.result, serde_json::json!(true));
                    }
                    if ready_before_change {
                        sender
                            .send(Sv1ServerEvent::from(notify.clone()))
                            .await
                            .unwrap();
                    } else {
                        server
                            .process_sv1_message(
                                7,
                                request(
                                    2,
                                    "mining.authorize",
                                    serde_json::json!(["user.worker", "x"]),
                                ),
                            )
                            .await
                            .unwrap();
                        wire.try_recv().unwrap();
                    }
                    downstream.handle_sv1_server_message().await.unwrap();
                    assert_eq!(downstream.is_disconnected(), !announce_support);
                    if announce_support {
                        let json_rpc::Message::Notification(update) = wire.try_recv().unwrap()
                        else {
                            panic!("expected prefix update before notify");
                        };
                        let update = server_to_client::SetExtranonce::try_from(update).unwrap();
                        assert_eq!(update.extra_nonce1, vec![9; 4].try_into().unwrap());
                        assert_eq!(
                            serde_json::to_value(wire.try_recv().unwrap()).unwrap(),
                            serde_json::to_value(&notify).unwrap()
                        );
                        assert_eq!(
                            downstream
                                .downstream_data
                                .with(|data| data.job_validation_context("new").unwrap().extranonce)
                                .unwrap(),
                            update.extra_nonce1
                        );
                    } else {
                        assert!(
                            downstream
                                .downstream_data
                                .with(|data| data.job_validation_context("new").is_none())
                                .unwrap()
                        );
                    }
                    assert!(wire.try_recv().is_err());
                }
            }
        }
    }

    #[tokio::test]
    async fn extranonce_subscribe_records_downstream_support() {
        let server = create_test_sv1_server();
        let (_, downstream_sv1_receiver) =
            register_test_downstream_with_sv1_receiver(&server, 7, Some(9), 100.0, false);
        let subscribe: json_rpc::Message =
            serde_json::from_str(r#"{"id":1,"method":"mining.extranonce.subscribe","params":[]}"#)
                .unwrap();

        server
            .process_sv1_message(7, subscribe.try_into().unwrap())
            .await
            .unwrap();

        let json_rpc::Message::OkResponse(response) = downstream_sv1_receiver.recv().await.unwrap()
        else {
            panic!("expected mining.extranonce.subscribe response");
        };
        assert_eq!(response.id, 1);
        assert_eq!(response.result, serde_json::json!(true));

        assert_eq!(
            server.downstreams.with(&7, |downstream| downstream
                .downstream_data
                .with(|data| data.supports_set_extranonce)
                .unwrap()),
            Some(true)
        );
    }

    #[test]
    fn test_sv1_server_config() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = true;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode);

        assert!(server.config.downstream_difficulty_config.enable_vardiff);
    }

    #[tokio::test]
    async fn missing_downstream_requests_disconnect() {
        let server = create_test_sv1_server();

        let error = server.handle_open_channel_request(7).await.unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
    }

    #[tokio::test]
    async fn pre_channel_sv1_message_queue_disconnects_at_limit() {
        let (server_to_channel_manager_sender, _server_to_channel_manager_receiver) = unbounded();
        let (_channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        server.set_user_identity("test_user".to_string());
        let downstream_id = 7;
        let _downstream_receiver =
            register_test_downstream(&server, downstream_id, None, 100.0, false);
        let message = json_rpc::Message::StandardRequest(json_rpc::StandardRequest {
            id: 1,
            method: "mining.subscribe".to_string(),
            params: serde_json::json!([]),
        });

        for _ in 0..MAX_QUEUED_SV1_HANDSHAKE_MESSAGES {
            server
                .sv1_server_io
                .downstream_to_sv1_server_sender
                .send((downstream_id, message.clone()))
                .await
                .unwrap();
            server.handle_downstream_message().await.unwrap();
        }

        server
            .sv1_server_io
            .downstream_to_sv1_server_sender
            .send((downstream_id, message))
            .await
            .unwrap();
        let error = server.handle_downstream_message().await.unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::Sv1HandshakeMessageQueueFull
        ));
        assert_eq!(
            server
                .with_registered_downstream(downstream_id, |downstream| {
                    downstream
                        .downstream_data
                        .with(|data| data.queued_sv1_handshake_messages.len())
                        .map_err(TproxyError::shutdown)
                })
                .unwrap(),
            MAX_QUEUED_SV1_HANDSHAKE_MESSAGES
        );

        let cancellation_token = CancellationToken::new();
        let fallback_token = CancellationToken::new();
        assert_eq!(
            server
                .handle_error_action(
                    "pre_channel_sv1_message_queue_disconnects_at_limit",
                    &error,
                    &cancellation_token,
                    &fallback_token,
                )
                .await,
            LoopControl::Continue
        );
        assert!(!server.downstreams.contains_key(&downstream_id));
    }

    #[tokio::test]
    async fn downstream_removed_before_open_forwarding_clears_pending_request() {
        let server = create_test_sv1_server();
        server.request_id_to_downstream_id.insert(42, 7);

        let error = server
            .forward_pending_open_channel_request(42, 7)
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::DownstreamNotPresent(7)
        ));
        assert!(server.request_id_to_downstream_id.is_empty());
    }

    #[tokio::test]
    async fn rejected_open_request_disconnects_pending_downstream() {
        let (server_to_channel_manager_sender, _server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        register_test_downstream(&server, 7, None, 100.0, false);
        server.request_id_to_downstream_id.insert(42, 7);
        channel_manager_to_server_sender
            .send(MiningOwned::OpenMiningChannelError(
                OpenMiningChannelErrorOwned {
                    request_id: 42,
                    error_code: ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED
                        .try_into()
                        .unwrap(),
                },
            ))
            .await
            .unwrap();

        let error = server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap_err();
        assert!(matches!(error.action, Action::Disconnect(7)));
        assert!(server.request_id_to_downstream_id.is_empty());

        let cancellation_token = CancellationToken::new();
        let fallback_token = CancellationToken::new();
        let control = server
            .handle_error_action(
                "rejected open request",
                &error,
                &cancellation_token,
                &fallback_token,
            )
            .await;
        assert!(matches!(control, LoopControl::Continue));
        assert!(!server.downstreams.contains_key(&7));
        assert!(
            !server
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .contains_key(&7)
        );
    }

    #[tokio::test]
    async fn upstream_close_cancels_the_corresponding_sv1_connection() {
        let (server_to_channel_manager_sender, server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        register_test_downstream(&server, 7, Some(42), 100.0, false);
        let downstream = server.downstreams.get_cloned(&7).unwrap();
        channel_manager_to_server_sender
            .send(MiningOwned::CloseChannel(CloseChannelOwned {
                channel_id: 42,
                reason_code: Str0255Owned::try_from("upstream closed channel".to_string()).unwrap(),
            }))
            .await
            .unwrap();

        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();

        assert!(downstream.is_disconnected());
        assert!(server.channel_owner(42).is_none());
        assert_eq!(
            downstream
                .downstream_data
                .with(|data| data.channel_id)
                .unwrap(),
            None
        );
        assert!(server_to_channel_manager_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn unsupported_prefix_change_waits_for_job_then_keeps_channel_for_cleanup() {
        let (server_to_channel_manager_sender, server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        register_test_downstream(&server, 7, Some(42), 100.0, false);
        let downstream = server.downstreams.get_cloned(&7).unwrap();
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: false,
                };
            })
            .unwrap();
        channel_manager_to_server_sender
            .send(MiningOwned::SetExtranoncePrefix(SetExtranoncePrefixOwned {
                channel_id: 42,
                extranonce_prefix: vec![1, 2, 3, 4].try_into().unwrap(),
            }))
            .await
            .unwrap();

        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();

        assert!(!downstream.is_disconnected());
        downstream.handle_sv1_server_message().await.unwrap();
        assert!(!downstream.is_disconnected());
        let notify = serde_json::from_str::<json_rpc::Message>(
            r#"{"id":null,"method":"mining.notify","params":["new","0000000000000000000000000000000000000000000000000000000000000000","","",[],"20000000","1d00ffff","5f5e1000",true]}"#,
        ).unwrap();
        let sender = server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&7)
            .unwrap();
        sender.send(Sv1ServerEvent::from(notify)).await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        assert!(!downstream.is_disconnected());
        downstream
            .downstream_data
            .with(|data| {
                data.session_state
                    .record_response(Sv1SetupRequest::Authorize);
            })
            .unwrap();
        sender.send(Sv1ServerEvent::SetupComplete).await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        assert!(downstream.is_disconnected());
        assert_eq!(server.channel_owner(42), Some(7));
        assert_eq!(
            downstream
                .downstream_data
                .with(|data| data.channel_id)
                .unwrap(),
            Some(42)
        );
        assert!(server_to_channel_manager_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn extranonce_prefix_change_notifies_supported_miner_without_disconnect() {
        let (server_to_channel_manager_sender, _server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        let downstream_receiver = register_test_downstream(&server, 7, Some(42), 100.0, false);
        let downstream = server.downstreams.get_cloned(&7).unwrap();
        downstream
            .downstream_data
            .with(|data| {
                data.supports_set_extranonce = true;
                data.session_state = Sv1SessionState::Ready;
            })
            .unwrap();
        channel_manager_to_server_sender
            .send(MiningOwned::SetExtranoncePrefix(SetExtranoncePrefixOwned {
                channel_id: 42,
                extranonce_prefix: vec![1, 2, 3, 4].try_into().unwrap(),
            }))
            .await
            .unwrap();

        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();

        assert!(!downstream.is_disconnected());
        assert!(matches!(
            downstream_receiver.recv().await.unwrap(),
            Sv1ServerEvent::SetExtranonce {
                notify_miner: true,
                ..
            }
        ));
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.pending_set_extranonce_notifications, 1);
                assert!(data.keepalive_timer_anchor.is_none());
            })
            .unwrap();
    }

    #[tokio::test]
    async fn late_open_success_closes_channel_for_disconnected_downstream() {
        let (server_to_channel_manager_sender, server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        let target = hash_rate_to_target(200.0, 5.0).unwrap();
        server.request_id_to_downstream_id.insert(42, 7);
        channel_manager_to_server_sender
            .send(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: 42,
                    channel_id: 9,
                    target: target.to_le_bytes().into(),
                    extranonce_size: 4,
                    extranonce_prefix: vec![0; 4].try_into().unwrap(),
                    group_channel_id: 0,
                },
            ))
            .await
            .unwrap();

        server.handle_upstream_message(target).await.unwrap();

        let (message, _) = server_to_channel_manager_receiver
            .try_recv()
            .expect("the orphaned channel should be closed");
        assert!(matches!(message, MiningOwned::CloseChannel(close) if close.channel_id == 9));
        assert!(server.request_id_to_downstream_id.is_empty());
    }

    #[tokio::test]
    async fn aggregated_state_change_ignores_unopened_downstreams() {
        let (channel_manager_sender, channel_manager_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            upstream_receiver,
            channel_manager_sender,
            config,
            mode,
        );
        register_test_downstream(&server, 1, Some(1), 100.0, false);
        register_test_downstream(&server, 2, None, 100.0, false);

        server
            .send_update_channel_on_downstream_state_change()
            .await
            .unwrap();

        let (message, _) = channel_manager_receiver.recv().await.unwrap();
        let MiningOwned::UpdateChannel(update) = message else {
            panic!("expected UpdateChannel");
        };
        assert_eq!(update.nominal_hash_rate, 100.0);
    }

    #[tokio::test]
    async fn aggregated_vardiff_update_ignores_unopened_downstreams() {
        let (channel_manager_sender, channel_manager_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            upstream_receiver,
            channel_manager_sender,
            config,
            mode,
        );
        register_test_downstream(&server, 1, Some(1), 100.0, false);
        register_test_downstream(&server, 2, None, 100.0, false);
        let target = hash_rate_to_target(100.0, 5.0).unwrap();

        server
            .send_aggregated_update_channel(vec![(1, 1, target, 100.0)])
            .await
            .unwrap();

        let (message, _) = channel_manager_receiver.recv().await.unwrap();
        let MiningOwned::UpdateChannel(update) = message else {
            panic!("expected UpdateChannel");
        };
        assert_eq!(update.nominal_hash_rate, 100.0);
    }

    #[tokio::test]
    async fn aggregated_channel_open_registers_vardiff_and_refreshes_hashrate() {
        let (channel_manager_sender, channel_manager_receiver) = unbounded();
        let (upstream_sender, upstream_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            upstream_receiver,
            channel_manager_sender,
            config,
            mode,
        );
        register_test_downstream(&server, 1, Some(1), 100.0, false);
        register_test_downstream(&server, 2, None, 100.0, false);
        assert!(!server.vardiff.contains_key(&2));

        server
            .send_update_channel_on_downstream_state_change()
            .await
            .unwrap();
        let (message, _) = channel_manager_receiver.recv().await.unwrap();
        let MiningOwned::UpdateChannel(update) = message else {
            panic!("expected UpdateChannel");
        };
        assert_eq!(update.nominal_hash_rate, 100.0);

        let target = hash_rate_to_target(100.0, 5.0).unwrap();
        server.request_id_to_downstream_id.insert(42, 2);
        upstream_sender
            .send(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: 42,
                    channel_id: 2,
                    target: target.to_le_bytes().into(),
                    extranonce_size: 4,
                    extranonce_prefix: vec![0; 4].try_into().unwrap(),
                    group_channel_id: 0,
                },
            ))
            .await
            .unwrap();

        server.handle_upstream_message(target).await.unwrap();

        assert!(server.vardiff.contains_key(&2));

        let (message, _) = channel_manager_receiver.recv().await.unwrap();
        let MiningOwned::UpdateChannel(update) = message else {
            panic!("expected UpdateChannel");
        };
        assert_eq!(update.nominal_hash_rate, 200.0);
    }

    #[tokio::test]
    async fn aggregated_bootstrap_reuses_latest_shared_job() {
        let (server_to_channel_manager_sender, _server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        let (first_downstream_sender, first_downstream_receiver) = unbounded();
        let (second_downstream_sender, second_downstream_receiver) = unbounded();
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(1, first_downstream_sender);
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(2, second_downstream_sender);
        set_test_channel_owner(&server, 7, 1);
        set_test_channel_owner(&server, 8, 2);

        channel_manager_to_server_sender
            .send(MiningOwned::SetNewPrevHash(SetNewPrevHashOwned {
                channel_id: AGGREGATED_CHANNEL_ID,
                job_id: 1,
                prev_hash: vec![0; 32].try_into().unwrap(),
                ntime_start: 0,
                nbits: 0x207fffff,
            }))
            .await
            .unwrap();
        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();

        let upstream_job = NewExtendedMiningJobOwned {
            channel_id: AGGREGATED_CHANNEL_ID,
            job_id: 1,
            ntime_start: Sv2OptionOwned::new(None),
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255Owned::new(vec![]).unwrap(),
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08")
                .unwrap()
                .try_into()
                .unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000")
                .unwrap()
                .try_into()
                .unwrap(),
        };
        let target = Target::from_le_bytes([0xff; 32]);
        channel_manager_to_server_sender
            .send(MiningOwned::NewExtendedMiningJob(upstream_job.clone()))
            .await
            .unwrap();
        server.handle_upstream_message(target).await.unwrap();
        first_downstream_receiver.try_recv().unwrap();
        second_downstream_receiver.try_recv().unwrap();
        for _ in 0..3 {
            server
                .create_keepalive_job(AGGREGATED_CHANNEL_ID, 5)
                .unwrap();
        }
        let current = server.get_last_job(AGGREGATED_CHANNEL_ID).unwrap().notify;
        let last_mutation = server.aggregated_job_mutation_time.get().unwrap();

        let mut bootstrap = upstream_job.clone();
        bootstrap.channel_id = 7;
        channel_manager_to_server_sender
            .send(MiningOwned::NewExtendedMiningJob(bootstrap.clone()))
            .await
            .unwrap();
        server.handle_upstream_message(target).await.unwrap();

        let sent = message_from_server_event(first_downstream_receiver.try_recv().unwrap());
        let json_rpc::Message::Notification(sent) = sent else {
            panic!("expected mining.notify");
        };
        let sent = server_to_client::Notify::try_from(sent).unwrap();
        assert_eq!(sent.job_id, current.job_id);
        assert_eq!(sent.time, current.time);
        assert!(
            sent.clean_jobs,
            "only the new miner starts with a clean view"
        );
        assert!(second_downstream_receiver.try_recv().is_err());
        assert_eq!(
            serde_json::to_value(json_rpc::Message::from(
                server.get_last_job(AGGREGATED_CHANNEL_ID).unwrap().notify
            ))
            .unwrap(),
            serde_json::to_value(json_rpc::Message::from(current.clone())).unwrap()
        );
        assert_eq!(
            server.aggregated_job_mutation_time.get().unwrap(),
            last_mutation
        );
        assert_eq!(server.keepalive_job_id_counter.load(Ordering::Relaxed), 3);
        assert_eq!(
            read_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| jobs.len()),
            Some(4)
        );
        assert!(
            read_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| jobs
                .get("1")
                .is_some())
            .unwrap()
        );

        let next = server
            .create_keepalive_job(AGGREGATED_CHANNEL_ID, 5)
            .unwrap();
        assert_eq!(next.time.0, current.time.0 + 5);

        // Never replace current work with an unrelated bootstrap.
        bootstrap.job_id = 2;
        channel_manager_to_server_sender
            .send(MiningOwned::NewExtendedMiningJob(bootstrap))
            .await
            .unwrap();
        server.handle_upstream_message(target).await.unwrap();
        assert!(first_downstream_receiver.try_recv().is_err());
        assert!(second_downstream_receiver.try_recv().is_err());
        assert_eq!(
            server
                .get_last_job(AGGREGATED_CHANNEL_ID)
                .unwrap()
                .notify
                .job_id,
            next.job_id
        );
    }

    #[tokio::test]
    async fn closed_downstream_does_not_abort_aggregated_difficulty_broadcast() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode);
        let receivers = [
            (
                7,
                register_test_downstream(&server, 7, Some(9), 100.0, false),
            ),
            (
                8,
                register_test_downstream(&server, 8, Some(10), 100.0, false),
            ),
        ];

        let mut iteration_order = Vec::new();
        server
            .downstreams
            .for_each(|downstream_id, _| iteration_order.push(downstream_id));
        assert_eq!(iteration_order.len(), 2);
        let closed_id = iteration_order[0];
        let surviving_id = iteration_order[1];
        receivers
            .iter()
            .find(|(downstream_id, _)| *downstream_id == closed_id)
            .unwrap()
            .1
            .close();

        let target = hash_rate_to_target(200.0, 5.0).unwrap();
        let error = server
            .handle_set_target_message(test_set_target(AGGREGATED_CHANNEL_ID, target))
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(id) if id == closed_id));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelErrorSender));
        assert!(
            receivers
                .iter()
                .find(|(downstream_id, _)| *downstream_id == surviving_id)
                .unwrap()
                .1
                .try_recv()
                .is_ok(),
            "a closed peer aborted the broadcast before the surviving miner received the new difficulty"
        );
    }

    #[tokio::test]
    async fn closed_downstream_does_not_shutdown_on_non_aggregated_set_target() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;
        config.aggregate_channels = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode);
        register_test_downstream(&server, 7, Some(9), 100.0, true);

        let target = hash_rate_to_target(200.0, 5.0).unwrap();
        let error = server
            .handle_set_target_message(test_set_target(9, target))
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelErrorSender));
    }

    /// Vardiff server whose channel manager endpoints stay open, so vardiff updates get past their
    /// `UpdateChannel` and reach the downstreams.
    fn vardiff_server(aggregated: bool) -> TestChannels {
        let (to_manager, from_server) = unbounded();
        let (to_server, from_manager) = unbounded();
        let mut config = create_test_config();
        config.aggregate_channels = aggregated;
        let server = Sv1Server::new(
            "127.0.0.1:3333".parse().unwrap(),
            from_manager,
            to_manager,
            config,
            TproxyMode::from(aggregated),
        );
        server.set_user_identity("test_user".to_string());
        (server, to_server, from_server)
    }

    fn test_set_target(channel_id: ChannelId, target: Target) -> SetTargetOwned {
        SetTargetOwned {
            channel_id,
            target: target.to_le_bytes().into(),
        }
    }

    /// Validation target of the difficulty advertised for `target`.
    fn advertised(target: Target) -> Target {
        sv1_difficulty(target).unwrap().target
    }

    /// Returns the validation target of the difficulty queued for a downstream since the last
    /// call, if any, failing if more than one was queued.
    fn queued_target(events: &Receiver<Sv1ServerEvent>) -> Option<Target> {
        let mut queued = None;
        while let Ok(event) = events.try_recv() {
            if let Sv1ServerEvent::SetDifficulty(difficulty) = event {
                assert!(
                    queued.replace(difficulty.target).is_none(),
                    "more than one difficulty queued"
                );
            }
        }
        queued
    }

    fn read_test_data<R>(
        server: &Sv1Server,
        downstream_id: DownstreamId,
        f: impl FnOnce(&mut crate::sv1::downstream::DownstreamData) -> R,
    ) -> R {
        server
            .downstreams
            .with(&downstream_id, |downstream| {
                downstream.downstream_data.with(f).unwrap()
            })
            .unwrap()
    }

    /// Sets the target vardiff wants for a downstream, as a vardiff update would.
    fn set_test_desired_target(server: &Sv1Server, downstream_id: DownstreamId, target: Target) {
        read_test_data(server, downstream_id, |data| {
            data.desired_target = Some(target)
        });
    }

    /// Opens `channel_id` for `downstream_id` as an `OpenExtendedMiningChannelSuccess` assigning
    /// `upstream_target` would.
    async fn open_test_channel(
        server: &Sv1Server,
        to_server: &Sender<MiningOwned>,
        downstream_id: DownstreamId,
        channel_id: ChannelId,
        upstream_target: Target,
        first_target: Target,
    ) {
        server.request_id_to_downstream_id.insert(42, downstream_id);
        to_server
            .send(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: 42,
                    channel_id,
                    target: upstream_target.to_le_bytes().into(),
                    extranonce_size: 4,
                    extranonce_prefix: vec![0; 4].try_into().unwrap(),
                    group_channel_id: 0,
                },
            ))
            .await
            .unwrap();
        server.handle_upstream_message(first_target).await.unwrap();
    }

    fn set_test_upstream_target(server: &Sv1Server, downstream_id: DownstreamId, target: Target) {
        server
            .downstreams
            .with(&downstream_id, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| data.set_upstream_target(target, downstream_id))
                    .unwrap()
            })
            .unwrap();
    }

    /// Makes the next vardiff tick see `shares` submissions over the last two minutes.
    fn set_test_vardiff_shares(server: &Sv1Server, downstream_id: DownstreamId, shares: u32) {
        use stratum_apps::stratum_core::channels_sv2::Vardiff;

        let two_minutes_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 120;
        server.vardiff.with_mut_or_insert_with(
            downstream_id,
            || VardiffState::new().unwrap(),
            |vardiff_state| {
                vardiff_state.set_timestamp_of_last_update(two_minutes_ago);
                vardiff_state.set_shares_since_last_update(shares);
            },
        );
    }

    #[tokio::test]
    async fn easier_vardiff_target_does_not_wait_for_upstream() {
        let (server, _to_server, _channel_manager) = vardiff_server(true);
        let events = register_test_downstream(&server, 7, Some(9), 100.0, false);
        let upstream_target = hash_rate_to_target(100.0, 5.0).unwrap();
        set_test_upstream_target(&server, 7, upstream_target);

        // One share in two minutes against 5 shares/minute expected collapses the hashrate
        // estimate, so vardiff wants an easier target.
        set_test_vardiff_shares(&server, 7, 1);
        server.handle_vardiff_updates().await.unwrap();

        let target =
            queued_target(&events).expect("an easier target must be advertised immediately");
        assert!(
            target > upstream_target,
            "a larger target is easier and must preserve every upstream-valid share"
        );
    }

    #[tokio::test]
    async fn harder_vardiff_target_waits_for_the_upstream_target() {
        let (server, _to_server, _channel_manager) = vardiff_server(false);
        let events = register_test_downstream(&server, 7, Some(9), 100.0, false);
        let upstream_target = hash_rate_to_target(100.0, 5.0).unwrap();
        server
            .handle_set_target_message(test_set_target(9, upstream_target))
            .await
            .unwrap();
        assert_eq!(queued_target(&events), Some(advertised(upstream_target)));

        // Far more shares than expected raise the hashrate estimate, so vardiff wants a harder
        // target. The miner keeps the upstream target, which still credits the shares the harder
        // one would suppress.
        set_test_vardiff_shares(&server, 7, 1_000);
        server.handle_vardiff_updates().await.unwrap();
        let (hashrate, desired_target) =
            read_test_data(&server, 7, |data| (data.hashrate, data.desired_target));
        let desired_target = desired_target.unwrap();
        assert!(desired_target < upstream_target);
        assert!(
            hashrate.unwrap() > 100.0,
            "the new estimate is recorded right away"
        );
        assert_eq!(queued_target(&events), None);

        // The miner moves to it once the upstream applies it.
        server
            .handle_set_target_message(test_set_target(9, desired_target))
            .await
            .unwrap();
        assert_eq!(queued_target(&events), Some(advertised(desired_target)));
    }

    #[tokio::test]
    async fn advertised_difficulty_is_the_easier_of_vardiff_and_upstream() {
        let (server, _to_server, _channel_manager) = vardiff_server(false);
        let events = register_test_downstream(&server, 7, Some(9), 100.0, false);
        let target = |hashrate: f64| hash_rate_to_target(hashrate, 5.0).unwrap();
        // A larger target is easier: vardiff wants the target of 400 H/s, harder than the one of
        // 100 H/s.
        set_test_desired_target(&server, 7, target(400.0));

        for (upstream_target, advertised_target) in [
            // The upstream is easier than vardiff wants: the miner follows it, so it discards no
            // share the upstream would credit.
            (target(100.0), Some(target(100.0))),
            // The upstream tightens, but stays easier than vardiff wants: the miner follows it.
            (target(200.0), Some(target(200.0))),
            // The upstream is harder than vardiff wants: the miner works at the vardiff target.
            (target(800.0), Some(target(400.0))),
            // Another harder upstream target leaves the miner's difficulty unchanged.
            (target(1600.0), None),
            (target(100.0), Some(target(100.0))),
        ] {
            server
                .handle_set_target_message(test_set_target(9, upstream_target))
                .await
                .unwrap();
            assert_eq!(
                queued_target(&events),
                advertised_target.map(advertised),
                "upstream target {upstream_target}"
            );
        }
    }

    #[tokio::test]
    async fn aggregated_set_target_only_moves_miners_it_is_easier_for() {
        let (server, _to_server, _channel_manager) = vardiff_server(true);
        let target = |hashrate: f64| hash_rate_to_target(hashrate, 5.0).unwrap();
        let easy = register_test_downstream(&server, 7, Some(9), 100.0, false);
        let hard = register_test_downstream(&server, 8, Some(10), 800.0, false);
        let opening = register_test_downstream(&server, 11, None, 100.0, false);
        set_test_desired_target(&server, 7, target(100.0));
        set_test_desired_target(&server, 8, target(800.0));

        // The shared channel is harder than both miners want: each works at its vardiff target.
        server
            .handle_set_target_message(test_set_target(1, target(1600.0)))
            .await
            .unwrap();
        assert_eq!(queued_target(&easy), Some(advertised(target(100.0))));
        assert_eq!(queued_target(&hard), Some(advertised(target(800.0))));

        // An easier shared target only moves the miner that wants a harder one.
        server
            .handle_set_target_message(test_set_target(1, target(200.0)))
            .await
            .unwrap();
        assert_eq!(queued_target(&easy), None);
        assert_eq!(queued_target(&hard), Some(advertised(target(200.0))));

        // A miner whose channel is not open yet gets its upstream target when it opens.
        assert_eq!(queued_target(&opening), None);
        assert_eq!(
            read_test_data(&server, 11, |data| data.upstream_target),
            None
        );
    }

    #[tokio::test]
    async fn initial_difficulty_is_the_easier_of_vardiff_and_upstream() {
        // Vardiff starts from the configured target, which above difficulty 1 is advertised
        // rounded down to a power of two.
        let first_target = hash_rate_to_target(1e12, 5.0).unwrap();
        assert_ne!(advertised(first_target), first_target);
        // An upstream target harder than the configured one, and one easier, as a miner joining
        // a shared channel may find.
        let harder = hash_rate_to_target(2e12, 5.0).unwrap();
        let easier = hash_rate_to_target(1e11, 5.0).unwrap();

        for aggregated in [false, true] {
            for (upstream_target, advertised_target) in [(harder, first_target), (easier, easier)] {
                let (server, to_server, _channel_manager) = vardiff_server(aggregated);
                let events = register_test_downstream(&server, 7, None, 100.0, false);
                open_test_channel(&server, &to_server, 7, 9, upstream_target, first_target).await;
                assert_eq!(
                    queued_target(&events),
                    Some(advertised(advertised_target)),
                    "aggregated: {aggregated}, upstream target {upstream_target}"
                );
            }
        }
    }

    #[tokio::test]
    async fn without_vardiff_the_miner_follows_the_upstream_target() {
        let first_target = hash_rate_to_target(1e12, 5.0).unwrap();
        let assert_reported_hashrate = |server: &Sv1Server, expected: f64| {
            let hashrate = read_test_data(server, 7, |data| data.hashrate).unwrap() as f64;
            assert!((hashrate - expected).abs() / expected < 1e-3, "{hashrate}");
        };

        for aggregated in [false, true] {
            let (server, to_server, _channel_manager) = server_with_channels(aggregated);
            let events = register_test_downstream(&server, 7, None, 100.0, false);

            // The upstream assigns a target ten times easier than the configured initial one.
            let initial_target = hash_rate_to_target(1e11, 5.0).unwrap();
            open_test_channel(&server, &to_server, 7, 9, initial_target, first_target).await;
            assert_eq!(queued_target(&events), Some(advertised(initial_target)));
            assert_reported_hashrate(&server, 1e11);

            let next_target = hash_rate_to_target(4e11, 5.0).unwrap();
            server
                .handle_set_target_message(test_set_target(9, next_target))
                .await
                .unwrap();
            assert_eq!(queued_target(&events), Some(advertised(next_target)));
            assert_reported_hashrate(&server, 4e11);
        }
    }

    #[tokio::test]
    async fn job_delivers_the_latest_queued_difficulty_with_its_own_target() {
        let (server, _to_server, _channel_manager) = vardiff_server(false);
        let (_events, responses) =
            register_test_downstream_with_sv1_receiver(&server, 7, Some(9), 100.0, false);
        let downstream = server.downstreams.get_cloned(&7).unwrap();
        downstream
            .downstream_data
            .with(|data| data.session_state = Sv1SessionState::Ready)
            .unwrap();
        let target = |hashrate: f64| hash_rate_to_target(hashrate, 5.0).unwrap();

        // The vardiff difficulty is easier than the upstream one, and is queued until the next
        // job.
        set_test_desired_target(&server, 7, target(10.0));
        server
            .handle_set_target_message(test_set_target(9, target(100.0)))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();

        // Before that job, vardiff wants a difficulty harder than the upstream one, which
        // replaces the queued difficulty.
        set_test_desired_target(&server, 7, target(400.0));
        server.update_difficulty(7).unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        let queued = downstream
            .downstream_data
            .with(|data| data.cached_set_difficulty.clone().unwrap())
            .unwrap();
        assert_eq!(queued.target, advertised(target(100.0)));

        // The next job delivers it, validated against its own target.
        let notify: json_rpc::Message = serde_json::from_str(
            r#"{"id":null,"method":"mining.notify","params":["job","0000000000000000000000000000000000000000000000000000000000000000","","",[],"20000000","1d00ffff","5f5e1000",true]}"#,
        )
        .unwrap();
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&7)
            .unwrap()
            .send(Sv1ServerEvent::from(notify))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();

        let sent_difficulty = responses.try_recv().unwrap();
        assert_eq!(
            serde_json::to_value(sent_difficulty).unwrap(),
            serde_json::to_value(&queued.message).unwrap()
        );
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.target, queued.target);
                assert_eq!(
                    data.job_validation_context("job")
                        .map(|context| context.target),
                    Some(queued.target)
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn aggregated_keepalive_respects_individual_deadlines() {
        let (server_to_channel_manager_sender, _server_to_channel_manager_receiver) = unbounded();
        let (channel_manager_to_server_sender, channel_manager_to_server_receiver) = unbounded();
        let mut config = create_test_config();
        config
            .downstream_difficulty_config
            .job_keepalive_interval_secs = 60;
        let addr = "127.0.0.1:3333".parse().unwrap();
        let mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(
            addr,
            channel_manager_to_server_receiver,
            server_to_channel_manager_sender,
            config,
            mode,
        );
        let first_receiver = register_test_downstream(&server, 1, Some(7), 100.0, false);
        let second_receiver = register_test_downstream(&server, 2, Some(8), 100.0, false);
        for downstream_id in [1, 2] {
            server
                .downstreams
                .with(&downstream_id, |downstream| {
                    downstream
                        .downstream_data
                        .with(|data| {
                            data.session_state = Sv1SessionState::Ready;
                            // This harness observes the server-side queue directly instead of
                            // running the downstream delivery task that normally starts the
                            // keepalive timer when it forwards the original job.
                            data.keepalive_timer_anchor = Some(Instant::now());
                        })
                        .unwrap();
                })
                .unwrap();
        }

        channel_manager_to_server_sender
            .send(MiningOwned::SetNewPrevHash(SetNewPrevHashOwned {
                channel_id: AGGREGATED_CHANNEL_ID,
                job_id: 1,
                prev_hash: vec![0; 32].try_into().unwrap(),
                ntime_start: 0,
                nbits: 0x207fffff,
            }))
            .await
            .unwrap();
        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();
        channel_manager_to_server_sender
            .send(MiningOwned::NewExtendedMiningJob(
                NewExtendedMiningJobOwned {
                    channel_id: AGGREGATED_CHANNEL_ID,
                    job_id: 1,
                    ntime_start: Sv2OptionOwned::new(None),
                    version: 0x20000000,
                    version_rolling_allowed: true,
                    merkle_path: Seq0255Owned::new(vec![]).unwrap(),
                    coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                    coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                },
            ))
            .await
            .unwrap();
        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();

        // Drain the original job broadcast so the next messages are the keepalive under test.
        first_receiver.recv().await.unwrap();
        second_receiver.recv().await.unwrap();

        let set_last_delivery = |downstream_id, time| {
            server
                .downstreams
                .with(&downstream_id, |downstream| {
                    downstream
                        .downstream_data
                        .with(|data| {
                            data.keepalive_timer_anchor = Some(time);
                        })
                        .unwrap();
                })
                .unwrap();
        };
        let recent_delivery = Instant::now();
        for downstream_id in [1, 2] {
            set_last_delivery(downstream_id, recent_delivery);
        }
        let history_len =
            || read_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| jobs.len()).unwrap();
        let original_time = server
            .get_last_job(AGGREGATED_CHANNEL_ID)
            .unwrap()
            .notify
            .time
            .0;
        let interval = Duration::from_secs(60);

        // No miner is due, even if the aggregate could create another job.
        server
            .aggregated_job_mutation_time
            .set(Some(Instant::now() - interval))
            .unwrap();
        server.send_keepalive_jobs(60).await.unwrap();
        assert_eq!(history_len(), 1);
        assert!(first_receiver.try_recv().is_err());
        assert!(second_receiver.try_recv().is_err());

        // A is due; B has just received work. Only A receives the newly created K1.
        set_last_delivery(1, Instant::now() - interval);
        // A fresh upstream job may still be queued while A's old deadline is due.
        // Its normal delivery must not be duplicated by the keepalive path.
        server
            .aggregated_job_mutation_time
            .set(Some(Instant::now()))
            .unwrap();
        server.send_keepalive_jobs(60).await.unwrap();
        assert!(first_receiver.try_recv().is_err());
        assert!(second_receiver.try_recv().is_err());
        assert_eq!(history_len(), 1);
        server
            .aggregated_job_mutation_time
            .set(Some(Instant::now() - interval))
            .unwrap();
        server.send_keepalive_jobs(60).await.unwrap();
        let first_keepalive = message_from_server_event(first_receiver.try_recv().unwrap());
        assert!(second_receiver.try_recv().is_err());
        assert_eq!(history_len(), 2);
        let latest = server.get_last_job(AGGREGATED_CHANNEL_ID).unwrap().notify;
        assert_eq!(latest.time.0, original_time + 60);
        assert!(!latest.clean_jobs);
        assert_eq!(
            server
                .get_last_job(AGGREGATED_CHANNEL_ID)
                .unwrap()
                .keepalive_base_time,
            original_time
        );
        assert_eq!(
            server
                .downstreams
                .with(&2, |downstream| {
                    downstream
                        .downstream_data
                        .with(|data| data.keepalive_timer_anchor)
                        .unwrap()
                })
                .unwrap(),
            Some(recent_delivery)
        );

        // B becomes due halfway through the aggregate interval. Reuse K1 without
        // advancing nTime, adding history, or restarting the aggregate timer.
        let last_mutation = Instant::now() - Duration::from_secs(30);
        server
            .aggregated_job_mutation_time
            .set(Some(last_mutation))
            .unwrap();
        set_last_delivery(2, Instant::now() - interval);
        server.send_keepalive_jobs(60).await.unwrap();
        let second_keepalive = message_from_server_event(second_receiver.try_recv().unwrap());
        assert_eq!(
            serde_json::to_value(&first_keepalive).unwrap(),
            serde_json::to_value(&second_keepalive).unwrap()
        );
        assert!(first_receiver.try_recv().is_err());
        assert_eq!(history_len(), 2);
        assert_eq!(server.keepalive_job_id_counter.load(Ordering::Relaxed), 1);
        assert_eq!(
            server.aggregated_job_mutation_time.get().unwrap(),
            Some(last_mutation)
        );

        // Successful delivery starts only that miner's next interval.
        server.send_keepalive_jobs(60).await.unwrap();
        assert!(first_receiver.try_recv().is_err());
        assert!(second_receiver.try_recv().is_err());

        // When both are due, one new K2 is shared by both.
        for downstream_id in [1, 2] {
            set_last_delivery(downstream_id, Instant::now() - interval);
        }
        server
            .aggregated_job_mutation_time
            .set(Some(Instant::now() - interval))
            .unwrap();
        server.send_keepalive_jobs(60).await.unwrap();
        assert_eq!(
            serde_json::to_value(message_from_server_event(
                first_receiver.try_recv().unwrap()
            ))
            .unwrap(),
            serde_json::to_value(message_from_server_event(
                second_receiver.try_recv().unwrap()
            ))
            .unwrap()
        );
        assert_eq!(history_len(), 3);
        assert_eq!(
            server
                .get_last_job(AGGREGATED_CHANNEL_ID)
                .unwrap()
                .notify
                .time
                .0,
            original_time + 120
        );

        // An unreachable miner remains due, but retries neither mutate shared work
        // again nor send early keepalives to the healthy miner.
        first_receiver.close();
        let failed_delivery = Instant::now() - interval;
        set_last_delivery(1, failed_delivery);
        let last_mutation = server.aggregated_job_mutation_time.get().unwrap();
        for missing_sender in [false, true] {
            if missing_sender {
                server
                    .sv1_server_io
                    .sv1_server_to_downstream_sender
                    .remove(&1);
            }
            for _ in 0..2 {
                server.send_keepalive_jobs(60).await.unwrap();
            }
            assert!(second_receiver.try_recv().is_err());
            assert_eq!(history_len(), 3);
            assert_eq!(server.keepalive_job_id_counter.load(Ordering::Relaxed), 2);
            assert_eq!(
                server.aggregated_job_mutation_time.get().unwrap(),
                last_mutation
            );
            assert_eq!(
                server
                    .downstreams
                    .with(&1, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| data.keepalive_timer_anchor)
                            .unwrap()
                    })
                    .unwrap(),
                Some(failed_delivery)
            );
        }
    }

    #[tokio::test]
    async fn keepalive_skips_miners_without_ready_channels_or_eligible_jobs() {
        for aggregated in [false, true] {
            let mut server = create_test_sv1_server();
            server.mode = TproxyMode::from(aggregated);
            let mut receivers = Vec::new();
            for id in 1..=4 {
                let channel_id = (id != 2).then_some(id as u32);
                receivers.push(register_test_downstream(
                    &server, id, channel_id, 100.0, false,
                ));
                server
                    .downstreams
                    .with(&id, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| {
                                if id != 1 {
                                    data.session_state = Sv1SessionState::Ready;
                                }
                                // Miner 3 has no keepalive-eligible job.
                                if id != 3 {
                                    data.keepalive_timer_anchor =
                                        Some(Instant::now() - Duration::from_secs(60));
                                }
                            })
                            .unwrap();
                    })
                    .unwrap();
            }
            // Only miner 4 is eligible. Non-aggregated mode must use its own channel history.
            let message: json_rpc::Message = serde_json::from_value(serde_json::json!({
                "id": null, "method": "mining.notify",
                "params": ["1", "00".repeat(32), "", "", [], "20000000", "1d00ffff", "00000001", false]
            })).unwrap();
            let json_rpc::Message::Notification(notification) = message else {
                unreachable!()
            };
            let notify = server_to_client::Notify::try_from(notification).unwrap();
            let channel_id = if aggregated { AGGREGATED_CHANNEL_ID } else { 4 };
            with_test_jobs(&server, channel_id, |jobs| {
                jobs.activate(notify.job_id.clone(), StoredSv1Job::upstream(notify), false);
            });
            server.send_keepalive_jobs(60).await.unwrap();
            for (index, receiver) in receivers.iter().enumerate() {
                assert_eq!(receiver.try_recv().is_ok(), index == 3);
            }
            assert_eq!(server.keepalive_job_id_counter.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn reused_keepalive_preserves_each_miners_context_and_prefix_pause() {
        let server = create_test_sv1_server();
        let original = test_sv1_notify("1", false);
        with_test_jobs(&server, AGGREGATED_CHANNEL_ID, |jobs| {
            jobs.activate(
                "1".to_string(),
                StoredSv1Job::upstream(original.clone()),
                false,
            );
        });
        let mut miners = Vec::new();
        for id in [1, 2] {
            let (_events, wire) = register_test_downstream_with_sv1_receiver(
                &server,
                id,
                Some(id as u32),
                100.0,
                false,
            );
            let downstream = server.downstreams.get_cloned(&id).unwrap();
            downstream
                .downstream_data
                .with(|data| {
                    data.session_state = Sv1SessionState::Ready;
                    data.supports_set_extranonce = true;
                    data.extranonce1 = vec![id as u8; 4].try_into().unwrap();
                    data.target = Target::from_le_bytes([id as u8; 32]);
                })
                .unwrap();
            advertise_job(&server, id, &wire, original.clone(), None).await;
            miners.push((downstream, wire));
        }

        let mut received_jobs = Vec::new();
        for (index, (downstream, wire)) in miners.iter().enumerate() {
            downstream
                .downstream_data
                .with(|data| {
                    data.keepalive_timer_anchor = Some(Instant::now() - Duration::from_secs(60));
                })
                .unwrap();
            server.send_keepalive_jobs(60).await.unwrap();
            downstream.handle_sv1_server_message().await.unwrap();
            let json_rpc::Message::Notification(message) = wire.try_recv().unwrap() else {
                panic!("expected mining.notify");
            };
            let notify = server_to_client::Notify::try_from(message).unwrap();
            assert!(!notify.clean_jobs);
            downstream
                .downstream_data
                .with(|data| {
                    let old = data.job_validation_context("1").unwrap();
                    let keepalive = data.job_validation_context(&notify.job_id).unwrap();
                    assert_eq!(keepalive.extranonce, old.extranonce);
                    assert_eq!(keepalive.target, old.target);
                    assert_eq!(keepalive.extranonce2_len, old.extranonce2_len);
                    assert_eq!(
                        keepalive.extranonce,
                        vec![(index + 1) as u8; 4].try_into().unwrap()
                    );
                })
                .unwrap();
            received_jobs.push(serde_json::to_value(json_rpc::Message::from(notify)).unwrap());
        }
        assert_eq!(received_jobs[0], received_jobs[1]);
        assert_eq!(server.keepalive_job_id_counter.load(Ordering::Relaxed), 1);

        // A prefix change pauses keepalives both while the event is queued and after
        // it is consumed, until a matching new-prefix job is delivered.
        server
            .handle_upstream_extranonce_change(
                stratum_apps::stratum_core::mining_sv2::SetExtranoncePrefixOwned {
                    channel_id: 1,
                    extranonce_prefix: vec![9; 4].try_into().unwrap(),
                },
            )
            .await
            .unwrap();
        server.send_keepalive_jobs(60).await.unwrap();
        miners[0].0.handle_sv1_server_message().await.unwrap();
        assert!(miners[0].1.try_recv().is_err());
        server.send_keepalive_jobs(60).await.unwrap();
        for (downstream, wire) in &miners {
            assert!(wire.is_empty());
            assert!(
                downstream
                    .downstream_data
                    .with(|data| { data.job_validation_context("1").is_some() })
                    .unwrap()
            );
        }
        assert!(
            miners[0]
                .0
                .downstream_data
                .with(|data| data.keepalive_timer_anchor.is_none())
                .unwrap()
        );
        assert_eq!(server.keepalive_job_id_counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_sv1_server_counters() {
        let server = create_test_sv1_server();

        // Test initial values
        assert_eq!(server.miner_counter.load(Ordering::SeqCst), 0);
        assert_eq!(server.sequence_counter.load(Ordering::SeqCst), 1);

        // Test incrementing
        let miner_id = server.miner_counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(miner_id, 0);
        assert_eq!(server.miner_counter.load(Ordering::SeqCst), 1);

        // sequence_counter starts at 1, so first share gets sequence 1
        let seq_id = server.sequence_counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(seq_id, 1);
        assert_eq!(server.sequence_counter.load(Ordering::SeqCst), 2);
    }

    type TestChannels = (
        Sv1Server,
        Sender<MiningOwned>,
        Receiver<(MiningOwned, Sv1ShareMetadata)>,
    );

    pub(super) fn server_with_channels(aggregated: bool) -> TestChannels {
        let (to_manager, from_server) = unbounded();
        let (to_server, from_manager) = unbounded();
        let mut config = create_test_config();
        config.aggregate_channels = aggregated;
        config.downstream_difficulty_config.enable_vardiff = false;
        let server = Sv1Server::new(
            "127.0.0.1:3333".parse().unwrap(),
            from_manager,
            to_manager,
            config,
            TproxyMode::from(aggregated),
        );
        server.set_user_identity("test_user".to_string());
        (server, to_server, from_server)
    }

    /// Feeds a `SetNewPrevHash` and a job for `channel_id`, as the channel manager would.
    async fn feed_job_state(
        server: &Sv1Server,
        to_server: &Sender<MiningOwned>,
        channel_id: ChannelId,
    ) {
        let target = Target::from_le_bytes([0xff; 32]);
        to_server
            .send(MiningOwned::SetNewPrevHash(test_prevhash(channel_id)))
            .await
            .unwrap();
        server.handle_upstream_message(target).await.unwrap();
        to_server
            .send(MiningOwned::NewExtendedMiningJob(NewExtendedMiningJobOwned {
                channel_id,
                job_id: 0,
                ntime_start: Sv2OptionOwned::new(Some(1)),
                version: 0x20000000,
                version_rolling_allowed: true,
                merkle_path: Seq0255Owned::new(vec![]).unwrap(),
                coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
                coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
            }))
            .await
            .unwrap();
        server.handle_upstream_message(target).await.unwrap();
    }

    fn test_prevhash(channel_id: ChannelId) -> SetNewPrevHashOwned {
        SetNewPrevHashOwned {
            channel_id,
            job_id: 0,
            prev_hash: vec![0; 32].try_into().unwrap(),
            ntime_start: 1,
            nbits: 0x207fffff,
        }
    }

    fn has_job_state(server: &Sv1Server, channel_id: ChannelId) -> bool {
        server
            .channels
            .with(&channel_id, |channel| {
                channel.prevhash.is_some() && channel.jobs.is_some()
            })
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn non_aggregated_downstream_disconnect_releases_its_channel_job_state() {
        let (server, to_server, _from_server) = server_with_channels(false);
        register_test_downstream(&server, 7, Some(9), 100.0, false);
        register_test_downstream(&server, 8, Some(10), 100.0, false);
        for channel_id in [9, 10] {
            feed_job_state(&server, &to_server, channel_id).await;
        }

        server.handle_downstream_disconnect(7).await.unwrap();

        assert!(!server.channels.contains_key(&9));
        assert!(has_job_state(&server, 10));
    }

    #[tokio::test]
    async fn non_aggregated_upstream_close_releases_the_channel_job_state() {
        let (server, to_server, _from_server) = server_with_channels(false);
        register_test_downstream(&server, 7, Some(9), 100.0, false);
        feed_job_state(&server, &to_server, 9).await;

        to_server
            .send(MiningOwned::CloseChannel(CloseChannelOwned {
                channel_id: 9,
                reason_code: Str0255Owned::try_from("upstream closed channel".to_string()).unwrap(),
            }))
            .await
            .unwrap();
        server
            .handle_upstream_message(Target::from_le_bytes([0xff; 32]))
            .await
            .unwrap();

        assert!(!server.channels.contains_key(&9));
    }

    #[tokio::test]
    async fn non_aggregated_job_state_is_not_stored_for_a_channel_without_a_downstream() {
        let (server, to_server, _from_server) = server_with_channels(false);
        // Messages can still be queued for a channel that has just closed.
        feed_job_state(&server, &to_server, 9).await;

        assert!(!server.channels.contains_key(&9));
    }

    #[test]
    fn job_state_is_only_stored_while_its_channel_exists() {
        let (server, _to_server, _from_server) = server_with_channels(false);
        let notify = test_sv1_notify("1", true);
        assert!(!server.store_prevhash(&test_prevhash(9)));
        assert!(!server.store_job(9, &notify, true));
        assert!(!server.channels.contains_key(&9));

        set_test_channel_owner(&server, 9, 7);
        assert!(server.store_prevhash(&test_prevhash(9)));
        assert!(server.store_job(9, &notify, true));

        // The channel closes after the job was read, before it is stored.
        server.channels.remove(&9);
        assert!(!server.store_job(9, &notify, false));
        assert!(!server.channels.contains_key(&9));
    }

    #[tokio::test]
    async fn aggregated_downstream_disconnect_keeps_the_shared_job_state() {
        let (server, to_server, _from_server) = server_with_channels(true);
        register_test_downstream(&server, 7, Some(9), 100.0, false);
        feed_job_state(&server, &to_server, AGGREGATED_CHANNEL_ID).await;

        server.handle_downstream_disconnect(7).await.unwrap();

        assert!(has_job_state(&server, AGGREGATED_CHANNEL_ID));
        assert!(!server.channels.contains_key(&9));
    }

    #[tokio::test]
    async fn downstream_churn_leaves_no_downstream_state_behind() {
        for aggregated in [false, true] {
            let (server, to_server, _from_server) = server_with_channels(aggregated);
            for cycle in 0..32 {
                let downstream_id = 7 + cycle;
                // Like SRI Pool, this upstream never reuses channel IDs.
                let channel_id = 1_000 + cycle as ChannelId;
                register_test_downstream(&server, downstream_id, Some(channel_id), 100.0, false);
                let job_channel_id = if aggregated {
                    AGGREGATED_CHANNEL_ID
                } else {
                    channel_id
                };
                feed_job_state(&server, &to_server, job_channel_id).await;
                server
                    .handle_downstream_disconnect(downstream_id)
                    .await
                    .unwrap();
            }

            assert!(server.downstreams.is_empty());
            assert!(server.request_id_to_downstream_id.is_empty());
            assert!(
                server
                    .sv1_server_io
                    .sv1_server_to_downstream_sender
                    .is_empty()
            );
            // Aggregated mode keeps one shared entry for the upstream channel.
            assert_eq!(server.channels.len(), usize::from(aggregated));
        }
    }

    #[tokio::test]
    async fn configured_history_cap_reaches_shared_jobs_and_downstream_validation() {
        use stratum_apps::stratum_core::channels_sv2::client::MAX_PAST_JOBS;

        for aggregated in [false, true] {
            for cap in [2, MAX_PAST_JOBS + 10] {
                let (mut server, to_server, _from_server) = server_with_channels(aggregated);
                server.config.max_past_jobs = Some(cap);
                let (_events, responses) =
                    register_test_downstream_with_sv1_receiver(&server, 7, Some(9), 100.0, false);
                let downstream = server.downstreams.get_cloned(&7).unwrap();
                downstream
                    .downstream_data
                    .with(|data| {
                        data.session_state = Sv1SessionState::Ready;
                    })
                    .unwrap();
                let job_channel = if aggregated { AGGREGATED_CHANNEL_ID } else { 9 };
                let target = Target::from_le_bytes([0xff; 32]);
                to_server
                    .send(MiningOwned::SetNewPrevHash(SetNewPrevHashOwned {
                        channel_id: job_channel,
                        job_id: 0,
                        prev_hash: vec![0; 32].try_into().unwrap(),
                        ntime_start: 1,
                        nbits: 0x207fffff,
                    }))
                    .await
                    .unwrap();
                server.handle_upstream_message(target).await.unwrap();

                for job_id in 0..cap as u32 + 2 {
                    to_server.send(MiningOwned::NewExtendedMiningJob(NewExtendedMiningJobOwned {
                        channel_id: job_channel,
                        job_id,
                        ntime_start: Sv2OptionOwned::new(Some(1)),
                        version: 0x20000000,
                        version_rolling_allowed: true,
                        merkle_path: Seq0255Owned::new(vec![]).unwrap(),
                        coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
                        coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
                    })).await.unwrap();
                    server.handle_upstream_message(target).await.unwrap();
                    downstream.handle_sv1_server_message().await.unwrap();
                    let json_rpc::Message::Notification(sent) = responses.try_recv().unwrap()
                    else {
                        panic!("expected mining.notify");
                    };
                    let sent = server_to_client::Notify::try_from(sent).unwrap();
                    assert_eq!(sent.job_id, job_id.to_string());
                }

                read_test_jobs(&server, job_channel, |jobs| {
                    assert_eq!(jobs.len(), cap + 1);
                    assert!(jobs.get("0").is_none());
                    assert!(jobs.get("1").is_some());
                })
                .unwrap();
                downstream
                    .downstream_data
                    .with(|data| {
                        assert_eq!(data.job_validation_contexts.len(), cap + 1);
                        assert!(data.job_validation_contexts.get("0").is_none());
                        assert!(data.job_validation_contexts.get("1").is_some());
                    })
                    .unwrap();
            }
        }
    }

    async fn receive_request(
        server: &Sv1Server,
        message: json_rpc::Message,
    ) -> TproxyResult<(), error::Sv1Server> {
        server
            .sv1_server_io
            .downstream_to_sv1_server_sender
            .send((7, message))
            .await
            .unwrap();
        server.handle_downstream_message().await
    }

    #[tokio::test]
    async fn invalid_initial_requests_allocate_no_channel_or_pending_state() {
        for aggregated in [false, true] {
            for wire in [
                r#"{"id":1,"method":"mining.foobar","params":[]}"#,
                r#"{"id":1,"method":"mining.subscribe","params":[123]}"#,
                r#"{"id":1,"method":"mining.authorize","params":[123, ""]}"#,
                r#"{"id":1,"method":"mining.configure","params":null}"#,
                r#"{"id":1,"method":"mining.extranonce.subscribe","params":[]}"#,
                r#"{"id":1,"method":"mining.suggest_difficulty","params":[1]}"#,
                r#"{"id":1,"result":true,"error":null}"#,
            ] {
                let (server, _to_server, from_server) = server_with_channels(aggregated);
                let _events = register_test_downstream(&server, 7, None, 100.0, false);
                let error = receive_request(&server, serde_json::from_str(wire).unwrap())
                    .await
                    .unwrap_err();
                assert!(matches!(error.action, Action::Disconnect(7)), "{wire}");
                assert!(
                    from_server.is_empty(),
                    "invalid request opened a channel: {wire}"
                );
                assert!(server.request_id_to_downstream_id.is_empty());
                server
                    .downstreams
                    .with(&7, |downstream| {
                        downstream
                            .downstream_data
                            .with(|data| {
                                assert!(data.queued_sv1_handshake_messages.is_empty());
                                assert!(data.channel_id.is_none());
                            })
                            .unwrap();
                    })
                    .unwrap();
                server
                    .handle_error_action(
                        "admission test",
                        &error,
                        &CancellationToken::new(),
                        &CancellationToken::new(),
                    )
                    .await;
                assert!(server.downstreams.is_empty());
                assert!(
                    server
                        .sv1_server_io
                        .sv1_server_to_downstream_sender
                        .is_empty()
                );
                assert!(server.vardiff.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn submit_before_subscribe_returns_code_25_without_opening_channel() {
        let (server, _to_server, from_server) = server_with_channels(false);
        let (_events, responses) =
            register_test_downstream_with_sv1_receiver(&server, 7, None, 100.0, false);

        receive_request(&server, serde_json::from_str(r#"{"id":1,"method":"mining.submit","params":["worker","job","00000000","00000001","00000000"]}"#).unwrap()).await.unwrap();

        let response = responses.recv().await.unwrap();
        let json_rpc::Message::ErrorResponse(response) = response else {
            panic!("expected mining.submit error response");
        };
        assert_eq!(response.id, 1);
        assert_eq!(response.error.unwrap().code, 25);
        assert!(from_server.is_empty());
        assert!(server.request_id_to_downstream_id.is_empty());
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        assert!(data.queued_sv1_handshake_messages.is_empty());
                        assert!(data.channel_id.is_none());
                    })
                    .unwrap();
            })
            .unwrap();
    }

    #[tokio::test]
    async fn unauthorized_worker_returns_code_24_without_disconnect() {
        let (server, _to_server, _from_server) = server_with_channels(false);
        let (_events, responses) =
            register_test_downstream_with_sv1_receiver(&server, 7, Some(42), 100.0, false);
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        data.session_state = Sv1SessionState::Starting {
                            subscribed: true,
                            authorized: false,
                        };
                    })
                    .unwrap();
            })
            .unwrap();

        receive_request(&server, serde_json::from_str(r#"{"id":1,"method":"mining.submit","params":["worker","job","00000000","00000001","00000000"]}"#).unwrap()).await.unwrap();

        let response = responses.recv().await.unwrap();
        let json_rpc::Message::ErrorResponse(response) = response else {
            panic!("expected mining.submit error response");
        };
        assert_eq!(response.id, 1);
        assert_eq!(response.error.unwrap().code, 24);
        assert!(server.downstreams.contains_key(&7));
    }

    #[tokio::test]
    async fn accepted_setup_is_parsed_before_open_and_dispatched_after_success() {
        let configure = r#"{"id":1,"method":"mining.configure","params":[["version-rolling"],{"version-rolling.mask":"1fffe000","version-rolling.min-bit-count":2}]}"#;
        let authorize = r#"{"id":3,"method":"mining.authorize","params":["worker",""]}"#;
        for aggregated in [false, true] {
            // Preserve the subscribe forms supported by sv1_api, including BOSminer variants.
            for params in [
                "[]",
                r#"["miner/1.0"]"#,
                r#"["miner/1.0","01020304"]"#,
                r#"["bosminer",null]"#,
                r#"["bosminer",null,"url",null]"#,
            ] {
                let subscribe =
                    format!(r#"{{"id":2,"method":"mining.subscribe","params":{params}}}"#);
                for sequence in [
                    vec![configure, subscribe.as_str(), authorize],
                    vec![subscribe.as_str(), authorize],
                    vec![authorize, subscribe.as_str()],
                ] {
                    let (server, to_server, from_server) = server_with_channels(aggregated);
                    let (_events, responses) =
                        register_test_downstream_with_sv1_receiver(&server, 7, None, 100.0, false);
                    for wire in &sequence {
                        receive_request(&server, serde_json::from_str(wire).unwrap())
                            .await
                            .unwrap();
                    }
                    let (MiningOwned::OpenExtendedMiningChannel(open), _) =
                        from_server.try_recv().unwrap()
                    else {
                        panic!("expected channel open");
                    };
                    assert!(from_server.is_empty(), "only one open per miner");
                    assert!(
                        responses.is_empty(),
                        "subscribe must wait for the real extranonce"
                    );
                    server
                        .downstreams
                        .with(&7, |downstream| {
                            downstream
                                .downstream_data
                                .with(|data| {
                                    assert!(!data.session_state.setup_complete());
                                    assert!(
                                        data.version_rolling_mask.is_none(),
                                        "parsing must not execute configure"
                                    );
                                })
                                .unwrap()
                        })
                        .unwrap();
                    let target = hash_rate_to_target(100.0, 5.0).unwrap();
                    to_server
                        .send(MiningOwned::OpenExtendedMiningChannelSuccess(
                            OpenExtendedMiningChannelSuccessOwned {
                                request_id: open.request_id,
                                channel_id: 9,
                                target: target.to_le_bytes().into(),
                                extranonce_size: 4,
                                extranonce_prefix: vec![1, 2, 3, 4].try_into().unwrap(),
                                group_channel_id: 0,
                            },
                        ))
                        .await
                        .unwrap();
                    server.handle_upstream_message(target).await.unwrap();
                    for wire in sequence {
                        let input: serde_json::Value = serde_json::from_str(wire).unwrap();
                        let json_rpc::Message::OkResponse(response) = responses.try_recv().unwrap()
                        else {
                            panic!("expected setup response");
                        };
                        assert_eq!(serde_json::json!(response.id), input["id"]);
                        if response.id == 2 {
                            assert_eq!(response.result[1], "01020304");
                            assert_eq!(response.result[2], 4);
                        }
                    }
                    assert!(responses.is_empty());
                    assert!(server.request_id_to_downstream_id.is_empty());
                    server
                        .downstreams
                        .with(&7, |downstream| {
                            downstream
                                .downstream_data
                                .with(|data| {
                                    assert!(data.session_state.setup_complete());
                                    assert!(data.queued_sv1_handshake_messages.is_empty());
                                })
                                .unwrap()
                        })
                        .unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn job_cached_during_setup_is_delivered_with_the_initial_difficulty() {
        let (server, to_server, from_server) = server_with_channels(true);
        let (events, responses) =
            register_test_downstream_with_sv1_receiver(&server, 7, None, 100.0, false);
        let downstream = server.downstreams.get_cloned(&7).unwrap();
        for wire in [
            r#"{"id":1,"method":"mining.subscribe","params":[]}"#,
            r#"{"id":2,"method":"mining.authorize","params":["worker",""]}"#,
        ] {
            receive_request(&server, serde_json::from_str(wire).unwrap())
                .await
                .unwrap();
        }
        let (MiningOwned::OpenExtendedMiningChannel(open), _) = from_server.try_recv().unwrap()
        else {
            panic!("expected channel open");
        };

        // Shared jobs reach every connected miner, so one can arrive while the miner's channel is
        // still opening. The downstream caches it until setup completes.
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&7)
            .unwrap()
            .send(Sv1ServerEvent::Notify(Arc::new(test_sv1_notify(
                "shared", true,
            ))))
            .await
            .unwrap();
        let target = hash_rate_to_target(1e12, 5.0).unwrap();
        to_server
            .send(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: open.request_id,
                    channel_id: 9,
                    target: target.to_le_bytes().into(),
                    extranonce_size: 4,
                    extranonce_prefix: vec![1, 2, 3, 4].try_into().unwrap(),
                    group_channel_id: 0,
                },
            ))
            .await
            .unwrap();
        server.handle_upstream_message(target).await.unwrap();
        while !events.is_empty() {
            downstream.handle_sv1_server_message().await.unwrap();
        }

        // The miner gets both setup responses, then the cached job with its difficulty.
        let delivered: Vec<String> = std::iter::from_fn(|| responses.try_recv().ok())
            .map(|message| match message {
                json_rpc::Message::Notification(notification) => notification.method,
                json_rpc::Message::OkResponse(_) => "response".to_string(),
                other => panic!("unexpected message {other:?}"),
            })
            .collect();
        assert_eq!(
            delivered,
            [
                "response",
                "response",
                "mining.set_difficulty",
                "mining.notify"
            ]
        );
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(
                    data.job_validation_context("shared")
                        .map(|context| context.target),
                    Some(advertised(target))
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn repeated_setup_and_premature_submit_never_open_a_second_channel() {
        let (server, _to_server, from_server) = server_with_channels(false);
        let (_events, responses) =
            register_test_downstream_with_sv1_receiver(&server, 7, None, 100.0, false);
        for _ in 0..3 {
            receive_request(
                &server,
                serde_json::from_str(r#"{"id":1,"method":"mining.subscribe","params":[]}"#)
                    .unwrap(),
            )
            .await
            .unwrap();
        }
        assert_eq!(from_server.len(), 1);
        receive_request(&server, serde_json::from_str(r#"{"id":2,"method":"mining.submit","params":["worker","job","00000000","00000001","00000000"]}"#).unwrap()).await.unwrap();
        let response = responses.recv().await.unwrap();
        let json_rpc::Message::ErrorResponse(response) = response else {
            panic!("expected mining.submit error response");
        };
        assert_eq!(response.id, 2);
        assert_eq!(response.error.unwrap().code, 25);
        assert_eq!(from_server.len(), 1);
        assert_eq!(server.request_id_to_downstream_id.len(), 1);
    }

    async fn advertise_job(
        server: &Sv1Server,
        downstream_id: usize,
        responses: &Receiver<json_rpc::Message>,
        notify: server_to_client::Notify,
        next_target: Option<Target>,
    ) {
        let downstream = server.downstreams.get_cloned(&downstream_id).unwrap();
        let events = server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&downstream_id)
            .unwrap();
        if let Some(target) = next_target {
            // Synthetic targets make share acceptance/rejection deterministic. This test exercises
            // the difficulty/notify boundary; target-to-difficulty conversion is tested separately.
            events
                .send(Sv1ServerEvent::SetDifficulty(Sv1Difficulty {
                    message: server_to_client::SetDifficulty { value: 1.0 }.into(),
                    target,
                }))
                .await
                .unwrap();
            downstream.handle_sv1_server_message().await.unwrap();
            assert!(responses.is_empty(), "difficulty waits for its notify");
        }
        events
            .send(Sv1ServerEvent::Notify(Arc::new(notify.clone())))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        if next_target.is_some() {
            let json_rpc::Message::Notification(difficulty) = responses.try_recv().unwrap() else {
                panic!("expected difficulty");
            };
            assert_eq!(difficulty.method, "mining.set_difficulty");
        }
        let json_rpc::Message::Notification(sent) = responses.try_recv().unwrap() else {
            panic!("expected job");
        };
        let sent = server_to_client::Notify::try_from(sent).unwrap();
        assert_eq!(sent.job_id, notify.job_id);
        assert_eq!(sent.clean_jobs, notify.clean_jobs);
    }

    async fn submit_old_job(
        server: &Sv1Server,
        downstream_id: usize,
        responses: &Receiver<json_rpc::Message>,
        forwarded: &Receiver<(MiningOwned, Sv1ShareMetadata)>,
        nonce: u32,
        expected_error: Option<i32>,
    ) {
        let request = client_to_server::Submit {
            user_name: "worker".to_string(),
            job_id: "1".to_string(),
            extra_nonce2: vec![0; 16].try_into().unwrap(),
            time: HexU32Be(1),
            nonce: HexU32Be(nonce),
            version_bits: None,
            id: 42,
        };
        server
            .sv1_server_io
            .downstream_to_sv1_server_sender
            .send((downstream_id, request.into()))
            .await
            .unwrap();
        server.handle_downstream_message().await.unwrap();
        let response = serde_json::to_value(responses.try_recv().unwrap()).unwrap();
        assert_eq!(response["id"], 42);
        match expected_error {
            Some(code) => {
                assert_eq!(response["error"][0], code);
                assert!(response["result"].is_null());
                assert!(
                    forwarded.is_empty(),
                    "rejected work must not reach ChannelManager"
                );
            }
            None => {
                assert_eq!(response["result"], true);
                assert!(response["error"].is_null());
                assert!(matches!(
                    forwarded.try_recv().unwrap().0,
                    MiningOwned::SubmitSharesExtended(_)
                ));
                assert!(forwarded.is_empty());
            }
        }
    }

    // Loupe #228 / sv2-apps #832: exercise the actual per-miner notification FIFO, the SV1
    // response path, and the ChannelManager boundary, even while the shared job still exists.
    #[tokio::test]
    async fn advertised_clean_jobs_invalidate_only_the_recipient_and_cannot_be_revived_by_difficulty()
     {
        for aggregated in [false, true] {
            for easier in [false, true] {
                let (server, _to_server, forwarded) = server_with_channels(aggregated);
                let max_target = Target::from_le_bytes([0xff; 32]);
                let zero_target = Target::from_le_bytes([0; 32]);
                let (old_target, new_target) = if easier {
                    (zero_target, max_target)
                } else {
                    (max_target, zero_target)
                };
                let mut miners = Vec::new();
                for (id, channel_id) in [(7, 9), (8, 10)] {
                    let (_events, responses) = register_test_downstream_with_sv1_receiver(
                        &server,
                        id,
                        Some(channel_id),
                        100.0,
                        false,
                    );
                    server
                        .downstreams
                        .with(&id, |downstream| {
                            downstream
                                .downstream_data
                                .with(|data| {
                                    data.session_state = Sv1SessionState::Ready;
                                    data.sv1_username = "worker".to_string();
                                    data.extranonce1 = vec![1, 2, 3, 4].try_into().unwrap();
                                    data.extranonce2_len = 16;
                                    data.target = if id == 7 { old_target } else { max_target };
                                    data.version_rolling_mask = None;
                                })
                                .unwrap()
                        })
                        .unwrap();
                    let job_channel = if aggregated {
                        AGGREGATED_CHANNEL_ID
                    } else {
                        channel_id
                    };
                    with_test_jobs(&server, job_channel, |jobs| {
                        for name in ["1", "2", "3", "4"] {
                            let job = test_sv1_notify(name, false);
                            jobs.activate(name.to_string(), StoredSv1Job::upstream(job), false);
                        }
                    });
                    advertise_job(&server, id, &responses, test_sv1_notify("1", false), None).await;
                    miners.push(responses);
                }
                let before_clean_error = if easier { Some(23) } else { None };
                submit_old_job(&server, 7, &miners[0], &forwarded, 0, before_clean_error).await;

                // An ordinary difficulty transition is not a clean job. Old work still uses its
                // original target, whether the new target is easier or harder.
                advertise_job(
                    &server,
                    7,
                    &miners[0],
                    test_sv1_notify("2", false),
                    Some(new_target),
                )
                .await;
                submit_old_job(&server, 7, &miners[0], &forwarded, 1, before_clean_error).await;

                // Only miner 7 receives a clean transition. Its old shares must now be stale,
                // including ones that would satisfy the easiest possible current target.
                advertise_job(
                    &server,
                    7,
                    &miners[0],
                    test_sv1_notify("3", true),
                    Some(new_target),
                )
                .await;
                submit_old_job(&server, 7, &miners[0], &forwarded, 2, Some(21)).await;
                advertise_job(
                    &server,
                    7,
                    &miners[0],
                    test_sv1_notify("4", false),
                    Some(max_target),
                )
                .await;
                submit_old_job(&server, 7, &miners[0], &forwarded, 3, Some(21)).await;

                // The other miner never received clean_jobs=true and can still submit the old job.
                submit_old_job(&server, 8, &miners[1], &forwarded, 4, None).await;
            }
        }
    }
}
