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
    sv1::downstream::Downstream,
    utils::{
        AGGREGATED_CHANNEL_ID, KEEPALIVE_JOB_ID_DELIMITER, SubmitShareWithChannelId, TproxyMode,
        is_mining_authorize,
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
        channels_sv2::{
            Vardiff, VardiffState,
            target::{hash_rate_from_target, hash_rate_to_target},
        },
        mining_sv2::{CloseChannelOwned, SetNewPrevHashOwned, SetTargetOwned},
        parsers_sv2::MiningOwned,
        stratum_translation::{
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
        sv1_api::{IsServer, json_rpc, server_to_client, utils::HexU32Be},
    },
    sync::SharedMap,
    task_manager::TaskManager,
    utils::types::{ChannelId, DownstreamId, Hashrate, RequestId, SharesPerMinute},
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

const SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING: f64 = 1.0;

#[derive(Clone)]
struct Sv1ServerIo {
    sv1_server_to_downstream_sender: SharedMap<DownstreamId, Sender<json_rpc::Message>>,
    downstream_to_sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
    downstream_to_sv1_server_receiver: Receiver<(DownstreamId, json_rpc::Message)>,
    channel_manager_receiver: Receiver<MiningOwned>,
    // Option<String> carries non-empty sv1_worker_name metadata for SubmitSharesExtended.
    channel_manager_sender: Sender<(MiningOwned, Option<String>)>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1ServerIo {
    fn new(
        channel_manager_receiver: Receiver<MiningOwned>,
        channel_manager_sender: Sender<(MiningOwned, Option<String>)>,
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
    pub(crate) downstream_id_factory: Arc<AtomicUsize>,
    pub(crate) request_id_factory: Arc<AtomicU32>,
    pub(crate) downstreams: SharedMap<DownstreamId, Downstream>,
    #[cfg(feature = "monitoring")]
    pub(crate) miner_telemetry: MinerTelemetryState,
    pub(crate) request_id_to_downstream_id: SharedMap<RequestId, DownstreamId>,
    pub(crate) channel_id_to_downstream_id: SharedMap<ChannelId, DownstreamId>,
    pub(crate) vardiff: SharedMap<DownstreamId, VardiffState>,
    /// HashMap to store the SetNewPrevHash for each channel
    /// Used in both aggregated and non-aggregated mode
    pub(crate) prevhashes: SharedMap<ChannelId, SetNewPrevHashOwned>,
    /// Tracks the latest target update per downstream that is waiting for a SetTarget response
    /// from upstream.
    pub(crate) pending_target_updates: SharedMap<DownstreamId, Target>,
    /// Valid Sv1 jobs storage, containing only a single shared entry (AGGREGATED_CHANNEL_ID) in
    /// case of channels aggregation (aggregated mode)
    pub(crate) valid_sv1_jobs: SharedMap<ChannelId, Vec<server_to_client::Notify>>,
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
            Action::Fallback => {
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
    async fn send_to_channel(
        &self,
        channel_id: ChannelId,
        msg: stratum_apps::stratum_core::sv1_api::json_rpc::Message,
    ) {
        if channel_id == AGGREGATED_CHANNEL_ID {
            let mut downstream_senders = Vec::new();
            self.sv1_server_io
                .sv1_server_to_downstream_sender
                .for_each(|downstream_id, sender| {
                    downstream_senders.push((downstream_id, sender.clone()));
                });
            // Broadcast to every connected downstream.
            for (downstream_id, sender) in downstream_senders {
                if let Err(e) = sender.send(msg.clone()).await {
                    warn!(
                        "Failed to send notify to downstream {}: channel closed: {}",
                        downstream_id, e
                    );
                }
            }
        } else {
            // Non-aggregated: send to the single downstream that owns this channel_id.
            let downstream_id = match self
                .channel_id_to_downstream_id
                .with(&channel_id, |downstream_id| *downstream_id)
            {
                Some(id) => id,
                None => return,
            };

            let sender = self
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .get_cloned(&downstream_id);

            let Some(sender) = sender else { return };

            if let Err(e) = sender.send(msg).await {
                warn!(
                    "Failed to send notify to downstream {}: channel closed: {}",
                    downstream_id, e
                );
            }
        }
    }

    /// Cleans up server state and closes communication channels.
    fn cleanup(&self) {
        self.prevhashes.clear();
        self.valid_sv1_jobs.clear();
        if self.config.downstream_difficulty_config.enable_vardiff {
            self.vardiff.clear();
        }
        self.downstreams.clear();
        #[cfg(feature = "monitoring")]
        self.miner_telemetry.clear();
        self.channel_id_to_downstream_id.clear();
        self.request_id_to_downstream_id.clear();
        self.pending_target_updates.clear();
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
        channel_manager_sender: Sender<(MiningOwned, Option<String>)>,
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
            downstream_id_factory: Arc::new(AtomicUsize::new(1)),
            request_id_factory: Arc::new(AtomicU32::new(1)),
            downstreams: SharedMap::new(),
            #[cfg(feature = "monitoring")]
            miner_telemetry: MinerTelemetryState::new(),
            request_id_to_downstream_id: SharedMap::new(),
            channel_id_to_downstream_id: SharedMap::new(),
            vardiff: SharedMap::new(),
            prevhashes: SharedMap::new(),
            pending_target_updates: SharedMap::new(),
            valid_sv1_jobs: SharedMap::new(),
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
        task_manager_clone.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();

            tokio::pin!(vardiff_future);
            tokio::pin!(keepalive_future);
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
                    result = listener.accept() => {
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
                                    #[cfg(feature = "monitoring")]
                                    addr.ip(),
                                    connection_token,
                                );
                                // vardiff initialization (only if enabled)
                                self.downstreams.insert(downstream_id, downstream.clone());
                                // Insert vardiff state for this downstream only if vardiff is enabled
                                if self.config.downstream_difficulty_config.enable_vardiff {
                                    let vardiff = VardiffState::new().expect("Failed to create vardiffstate");
                                    self.vardiff.insert(downstream_id, vardiff);
                                }
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

            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });

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

        let channel_id = match self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.channel_id)
                .map_err(TproxyError::shutdown)
        }) {
            Ok(channel_id) => channel_id,
            Err(e) if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        if channel_id.is_none() {
            let is_first_message =
                self.with_registered_downstream(downstream_id, |downstream| {
                    downstream
                        .downstream_data
                        .with(|d| d.queued_sv1_handshake_messages.is_empty())
                        .map_err(TproxyError::shutdown)
                })?;
            if is_first_message {
                self.handle_open_channel_request(downstream_id).await?;
                debug!(
                    "Down: Sent OpenChannel request for downstream {}",
                    downstream_id
                );
            }
            debug!("Down: Queuing Sv1 message until channel is established");
            self.with_registered_downstream(downstream_id, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| {
                        data.queued_sv1_handshake_messages
                            .push(downstream_message.clone())
                    })
                    .map_err(TproxyError::shutdown)
            })?;
            return Ok(());
        }

        let is_authorize = is_mining_authorize(&downstream_message);

        let response = self
            .clone()
            .handle_message(Some(downstream_id), downstream_message)
            .map_err(|e| e.with_sv1_downstream_context(downstream_id));

        match response {
            Ok(Some(response_msg)) => {
                debug!("Down: Sending Sv1 message to downstream: {}", response_msg);
                let (downstream_sv1_sender, downstream) =
                    self.with_registered_downstream(downstream_id, |downstream| {
                        Ok((
                            downstream.downstream_io.downstream_sv1_sender.clone(),
                            downstream.clone(),
                        ))
                    })?;
                downstream_sv1_sender
                    .send(response_msg.into())
                    .await
                    .map_err(|error| {
                        error!("Down: Failed to send message to downstream: {error:?}");
                        TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                    })?;

                // Check if this was an authorize message and handle sv1 handshake completion
                if is_authorize {
                    info!("Down: Handling mining.authorize after handshake completion");
                    if let Err(e) = downstream.handle_sv1_handshake_completion().await {
                        error!("Down: Failed to handle handshake completion: {:?}", e);
                        return Err(TproxyError::disconnect(e, downstream_id));
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
                sv1_worker_name,
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
                    return Err(TproxyError::log(TproxyErrorKind::RequestIdNotFound(
                        m.request_id,
                    )));
                };
                let initial_target = Target::from_le_bytes(m.target.to_array());
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
                            })
                            .map_err(TproxyError::shutdown)?;

                        let queued_messages = downstream
                            .downstream_data
                            .with(|d| {
                                let messages = d.queued_sv1_handshake_messages.clone();
                                d.queued_sv1_handshake_messages.clear();
                                messages
                            })
                            .map_err(TproxyError::shutdown)?;
                        self.channel_id_to_downstream_id
                            .insert(m.channel_id, downstream_id);

                        Ok((
                            queued_messages,
                            downstream.downstream_io.downstream_sv1_sender.clone(),
                            downstream.clone(),
                        ))
                    });

                match downstream_setup {
                    Ok((queued_messages, downstream_sv1_sender, downstream)) => {
                        // Process all queued messages now that channel is established
                        {
                            if !queued_messages.is_empty() {
                                info!(
                                    "Processing {} queued Sv1 messages for downstream {}",
                                    queued_messages.len(),
                                    downstream_id
                                );

                                for message in queued_messages {
                                    let is_authorize = is_mining_authorize(&message);
                                    let response = self
                                        .clone()
                                        .handle_message(Some(downstream_id), message)
                                        .map_err(|e| e.with_sv1_downstream_context(downstream_id));
                                    match response {
                                        Ok(Some(response_msg)) => {
                                            downstream_sv1_sender.send(response_msg.into()).await
                                            .map_err(|e| {
                                                error!(
                                                    "Down: Failed to send message to downstream: {e:?}"
                                                );
                                                TproxyError::disconnect(
                                                    TproxyErrorKind::ChannelErrorSender, downstream_id
                                                )
                                            })?;

                                            if is_authorize {
                                                info!(
                                                    "Down: Handling mining.authorize after upstream channel is open"
                                                );
                                                if let Err(e) = downstream
                                                    .handle_sv1_handshake_completion()
                                                    .await
                                                {
                                                    error!(
                                                        "Down: Failed to handle handshake completion: {:?}",
                                                        e
                                                    );
                                                    return Err(TproxyError::disconnect(
                                                        e,
                                                        downstream_id,
                                                    ));
                                                }
                                            }
                                        }
                                        Ok(None) => {
                                            // Message was handled but no response needed
                                        }
                                        Err(e) => {
                                            error!(
                                                "Down: Error handling downstream message: {:?}",
                                                e
                                            );
                                            return Err(e);
                                        }
                                    }
                                }
                            }
                        }

                        let set_difficulty =
                        build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding(
                            first_target,
                            SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                        )
                        .map_err(TproxyError::shutdown)?;
                        // send the set_difficulty message to the downstream
                        if let Some(sender) = self
                            .sv1_server_io
                            .sv1_server_to_downstream_sender
                            .get_cloned(&downstream_id)
                        {
                            sender.send(set_difficulty).await.map_err(|_| {
                                TproxyError::disconnect(
                                    TproxyErrorKind::ChannelErrorSender,
                                    downstream_id,
                                )
                            })?;
                        }

                        // Opening a downstream changes the aggregate just like disconnecting one.
                        // Refresh it now so the newly active hashrate is not left out until the
                        // next vardiff update.
                        if self.mode.is_aggregated()
                            && self.config.downstream_difficulty_config.enable_vardiff
                        {
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
                                    None,
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
                // Clone the prevhash immediately so shared map access is not held across .await.
                if let Some(prevhash) = self
                    .prevhashes
                    .with(&m.channel_id, |prevhash| prevhash.clone())
                {
                    let clean_jobs = m.job_id == prevhash.job_id;
                    let notify = build_sv1_notify_from_sv2(prevhash, m.clone(), clean_jobs)
                        .map_err(TproxyError::shutdown)?;

                    // Update job storage based on the configured mode
                    let notify_parsed = notify.clone();
                    let job_channel_id = if self.mode.is_non_aggregated() {
                        m.channel_id
                    } else {
                        AGGREGATED_CHANNEL_ID
                    };

                    self.valid_sv1_jobs
                        .with_mut_or_default(job_channel_id, |channel_jobs| {
                            if clean_jobs {
                                channel_jobs.clear();
                            }
                            channel_jobs.push(notify_parsed);
                        });

                    let notify_msg: stratum_apps::stratum_core::sv1_api::json_rpc::Message =
                        notify.into();
                    self.send_to_channel(job_channel_id, notify_msg).await;
                }
            }

            MiningOwned::SetNewPrevHash(m) => {
                debug!("Received SetNewPrevHash for channel id: {}", m.channel_id);
                self.prevhashes.insert(m.channel_id, m.clone());
            }

            MiningOwned::SetTarget(m) => {
                debug!("Received SetTarget for channel id: {}", m.channel_id);
                if self.config.downstream_difficulty_config.enable_vardiff {
                    // Vardiff enabled - use full difficulty management
                    self.handle_set_target_message(m).await?;
                } else {
                    // Vardiff disabled - just forward the difficulty to downstreams
                    debug!("Vardiff disabled - forwarding SetTarget to downstreams");
                    self.handle_set_target_without_vardiff(m).await?;
                }
            }
            // Guaranteed unreachable: the channel manager only forwards valid,
            // pre-filtered messages, so no other variants can arrive here.
            _ => unreachable!("Invalid message: should have been filtered earlier"),
        }

        Ok(())
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
                None,
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
            // Pending target updates are vardiff state too and must not outlive the miner.
            self.vardiff.remove(&downstream_id);
            self.pending_target_updates.remove(&downstream_id);
        }
        #[cfg(feature = "monitoring")]
        self.miner_telemetry.remove_downstream(downstream_id);
        self.sv1_server_io
            .sv1_server_to_downstream_sender
            .remove(&downstream_id);

        let current_downstream = self.downstreams.remove(&downstream_id);

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
                self.channel_id_to_downstream_id.remove(&channel_id);
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
                        None,
                    ))
                    .await;
            }
        }
        Ok(())
    }

    /// Handles SetTarget messages when vardiff is disabled.
    ///
    /// This method forwards difficulty changes from upstream directly to downstream miners
    /// without any variable difficulty logic. It respects the aggregated/non-aggregated
    /// channel configuration.
    ///
    /// When vardiff is disabled, the upstream (Pool or JDC) controls difficulty via SetTarget
    /// messages. We derive the hashrate from the received target so that monitoring can report
    /// meaningful SV1 downstream hashrate values.
    async fn handle_set_target_without_vardiff(
        &self,
        set_target: SetTargetOwned,
    ) -> TproxyResult<(), error::Sv1Server> {
        let new_target = Target::from_le_bytes(set_target.maximum_target.to_array());
        debug!(
            "Forwarding SetTarget to downstreams: channel_id={}, target={}",
            set_target.channel_id, new_target
        );

        // Derive hashrate from the upstream target so monitoring can report it
        let derived_hashrate = match hash_rate_from_target(
            set_target.maximum_target.clone(),
            self.shares_per_minute as f64,
        ) {
            Ok(hr) => {
                debug!(
                    "Derived hashrate from SetTarget: {} H/s (channel_id={})",
                    hr, set_target.channel_id
                );
                Some(hr)
            }
            Err(e) => {
                warn!(
                    "Failed to derive hashrate from SetTarget target: {:?} (channel_id={})",
                    e, set_target.channel_id
                );
                None
            }
        };

        if self.mode.is_aggregated() {
            // Aggregated mode: send set_difficulty to ALL downstreams and update hashrate
            return self
                .send_set_difficulty_to_all_downstreams(new_target, derived_hashrate)
                .await;
        }

        // Non-aggregated mode: send set_difficulty to specific downstream for this channel
        self.send_set_difficulty_to_specific_downstream(
            set_target.channel_id,
            new_target,
            derived_hashrate,
        )
        .await
    }

    /// Sends set_difficulty to all downstreams (aggregated mode).
    /// Used only when vardiff is disabled.
    async fn send_set_difficulty_to_all_downstreams(
        &self,
        target: Target,
        derived_hashrate: Option<f64>,
    ) -> TproxyResult<(), error::Sv1Server> {
        let mut tasks = Vec::new();
        self.downstreams.try_for_each(|downstream_id, downstream| {
            let has_channel = downstream
                .downstream_data
                .with(|d| {
                    let channel_id = d.channel_id?;
                    d.set_upstream_target(target, downstream_id);
                    // Downstream validation must use the advertised (pow2
                    // rounded) difficulty; upstream_target keeps the exact
                    // pool target for vardiff comparisons.
                    d.set_pending_target(
                        sv1_advertised_target_from_sv2_target(
                            target,
                            SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                        )
                        .unwrap_or(target),
                        downstream_id,
                    );
                    if let Some(hr) = derived_hashrate {
                        d.set_pending_hashrate(Some(hr as f32), downstream_id);
                    }
                    Some(channel_id)
                })
                .map_err(TproxyError::shutdown)?;
            if has_channel.is_none() {
                trace!(
                    "Skipping downstream {}: no channel_id set (vardiff disabled)",
                    downstream_id
                );
                return Ok(());
            }
            if let Some(sender) = self
                .sv1_server_io
                .sv1_server_to_downstream_sender
                .get_cloned(&downstream_id)
            {
                tasks.push((downstream_id, sender));
            }
            Ok::<(), TproxyError<error::Sv1Server>>(())
        })?;

        for (downstream_id, sender) in tasks {
            let set_difficulty_msg =
                match build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding(
                    target,
                    SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                ) {
                    Ok(msg) => msg,
                    Err(e) => {
                        error!(
                            "Failed to build mining.set_difficulty for downstream {}: {:?}",
                            downstream_id, e
                        );
                        return Err(TproxyError::shutdown(e));
                    }
                };
            if let Err(e) = sender.send(set_difficulty_msg).await {
                error!(
                    "Failed to send mining.set_difficulty to downstream {}: {:?}",
                    downstream_id, e
                );
                return Err(TproxyError::disconnect(
                    TproxyErrorKind::ChannelErrorSender,
                    downstream_id,
                ));
            } else {
                debug!(
                    "Sent mining.set_difficulty to downstream {} (vardiff disabled)",
                    downstream_id
                );
            }
        }
        Ok(())
    }

    /// Sends set_difficulty to the specific downstream associated with a channel (non-aggregated
    /// mode).
    /// Used only when vardiff is disabled.
    async fn send_set_difficulty_to_specific_downstream(
        &self,
        channel_id: ChannelId,
        target: Target,
        derived_hashrate: Option<f64>,
    ) -> TproxyResult<(), error::Sv1Server> {
        let Some(downstream_id) = self
            .channel_id_to_downstream_id
            .with(&channel_id, |downstream_id| *downstream_id)
        else {
            warn!(
                "No downstream found for channel {} when vardiff is disabled",
                channel_id
            );
            info!("Sending CloseChannel message: Channel id {channel_id}");
            let reason_code =
                Str0255Owned::try_from("downstream disconnected".to_string()).unwrap();
            self.sv1_server_io
                .channel_manager_sender
                .send((
                    MiningOwned::CloseChannel(CloseChannelOwned {
                        channel_id,
                        reason_code,
                    }),
                    None,
                ))
                .await
                .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;
            return Err(TproxyError::log(
                TproxyErrorKind::DownstreamNotFoundWithChannelId(channel_id),
            ));
        };

        if let Err(e) = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|d| {
                    d.set_upstream_target(target, downstream_id);
                    // See send_set_difficulty_to_all_downstreams: downstream validation
                    // uses the advertised pow2 difficulty.
                    d.set_pending_target(
                        sv1_advertised_target_from_sv2_target(
                            target,
                            SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
                        )
                        .unwrap_or(target),
                        downstream_id,
                    );
                    // Update pending hashrate derived from the upstream target
                    if let Some(hr) = derived_hashrate {
                        d.set_pending_hashrate(Some(hr as f32), downstream_id);
                    }
                })
                .map_err(TproxyError::shutdown)
        }) {
            if matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                return Ok(());
            }
            return Err(e);
        }

        let set_difficulty_msg =
            match build_sv1_set_difficulty_from_sv2_target_with_integer_power_of_two_rounding(
                target,
                SV1_MIN_DIFFICULTY_FOR_INTEGER_POWER_OF_TWO_ROUNDING,
            ) {
                Ok(msg) => msg,
                Err(e) => {
                    error!(
                        "Failed to build SetDifficulty for downstream {}: {:?}",
                        downstream_id, e
                    );
                    return Err(TproxyError::shutdown(e));
                }
            };

        let sender = self
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .get_cloned(&downstream_id);

        if let Some(sender) = sender {
            if let Err(e) = sender.send(set_difficulty_msg).await {
                error!(
                    "Failed to send SetDifficulty to downstream {}: {:?}",
                    downstream_id, e
                );
                return Err(TproxyError::disconnect(
                    TproxyErrorKind::ChannelErrorSender,
                    downstream_id,
                ));
            } else {
                debug!(
                    "Sent SetDifficulty to downstream {} for channel {} (vardiff disabled)",
                    downstream_id, channel_id
                );
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

        let interval = Duration::from_secs(keepalive_interval_secs as u64);
        let check_interval =
            Duration::from_secs(keepalive_interval_secs as u64 / 2).max(Duration::from_secs(5));
        info!(
            "Starting job keepalive loop with interval of {} seconds",
            keepalive_interval_secs
        );

        loop {
            tokio::time::sleep(check_interval).await;
            let mut keepalive_targets = Vec::new();
            self.downstreams.try_for_each(|downstream_id, downstream| {
                let keepalive_target = downstream
                    .downstream_data
                    .with(|d| {
                        // Only send keepalive if:
                        // 1. Handshake is complete
                        // 2. Enough time has passed since last job
                        let handshake_complete =
                            downstream.sv1_handshake_complete.load(Ordering::SeqCst);

                        if !handshake_complete {
                            return None;
                        }

                        let needs_keepalive = match d.last_job_received_time {
                            Some(last_time) => last_time.elapsed() >= interval,
                            None => false, // No job received yet, don't send keepalive
                        };

                        if needs_keepalive {
                            Some((downstream_id, d.channel_id))
                        } else {
                            None
                        }
                    })
                    .map_err(TproxyError::shutdown)?;
                if let Some(keepalive_target) = keepalive_target {
                    keepalive_targets.push(keepalive_target);
                }
                Ok::<(), TproxyError<error::Sv1Server>>(())
            })?;

            // Send keepalive to each downstream that needs one
            for (downstream_id, channel_id) in keepalive_targets {
                // Get the appropriate job for this downstream's channel and create keepalive
                let keepalive_job = self.get_last_job(channel_id).and_then(|last_job| {
                    // Extract the original upstream job_id from the last job
                    // If it's already a keepalive job, extract its original; otherwise use
                    // as-is
                    let original_job_id = Self::extract_original_job_id(&last_job.job_id)
                        .unwrap_or_else(|| last_job.job_id.clone());

                    // Find the original upstream job to get its base time
                    let original_job = self.get_original_job(&original_job_id, channel_id);
                    let base_time = original_job
                        .as_ref()
                        .map(|j| j.time.0)
                        .unwrap_or(last_job.time.0);

                    // Increment the time by the keepalive interval, but cap at
                    // MAX_FUTURE_BLOCK_TIME from the original job's time to maintain consensus
                    // validity (see https://github.com/bitcoin/bitcoin/blob/cd6e4c9235f763b8077cece69c2e3b2025cc8d0f/src/chain.h#L29)
                    const MAX_FUTURE_BLOCK_TIME: u32 = 2 * 60 * 60;
                    let new_time = last_job
                        .time
                        .0
                        .saturating_add(keepalive_interval_secs as u32)
                        .min(base_time.saturating_add(MAX_FUTURE_BLOCK_TIME));

                    // If we've hit the cap, don't send another keepalive for this job
                    if new_time == last_job.time.0 {
                        return None;
                    }

                    // Generate new keepalive job_id: {original_job_id}#{counter}
                    let new_job_id = self.next_keepalive_job_id(&original_job_id);

                    let mut keepalive_notify = last_job;
                    keepalive_notify.job_id = new_job_id.clone();
                    keepalive_notify.time = HexU32Be(new_time);

                    // Add the keepalive job to valid jobs so shares can be validated
                    let job_channel_id = if self.mode.is_aggregated() {
                        Some(AGGREGATED_CHANNEL_ID)
                    } else {
                        channel_id
                    };

                    if let Some(ch_id) = job_channel_id {
                        // Use with_mut (not with_mut_or_default) so we never
                        // re-create a valid_sv1_jobs entry for a channel that was
                        // already cleaned up.
                        self.valid_sv1_jobs
                            .with_mut(&ch_id, |jobs| jobs.push(keepalive_notify.clone()));
                    }

                    Some(keepalive_notify)
                });

                if let Some(notify) = keepalive_job {
                    debug!(
                        "Sending keepalive job to downstream {} with job_id: {}, time: {}",
                        downstream_id, notify.job_id, notify.time.0
                    );

                    let sent = match self
                        .sv1_server_io
                        .sv1_server_to_downstream_sender
                        .get_cloned(&downstream_id)
                    {
                        Some(sender) => sender.send(notify.into()).await.is_ok(),
                        None => false,
                    };
                    if !sent {
                        warn!(
                            "Failed to send keepalive job to downstream {}",
                            downstream_id
                        );
                    } else if let Err(e) =
                        self.with_registered_downstream(downstream_id, |downstream| {
                            downstream
                                .downstream_data
                                .with(|d| {
                                    d.last_job_received_time = Some(Instant::now());
                                })
                                .map_err(TproxyError::shutdown)
                        })
                    {
                        if !matches!(e.kind, TproxyErrorKind::DownstreamNotPresent(_)) {
                            return Err(e);
                        }
                    }
                }
            }
        }
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

    /// Gets the last job from the jobs storage.
    /// In aggregated mode, returns the last job from the shared job list.
    /// In non-aggregated mode, returns the last job for the specified channel.
    fn get_last_job(&self, channel_id: Option<u32>) -> Option<server_to_client::Notify> {
        let channel_id = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            channel_id?
        };

        self.valid_sv1_jobs
            .with(&channel_id, |jobs| jobs.last().cloned())
            .flatten()
    }

    /// Gets the original upstream job by its job_id.
    /// This is used to find the base time for keepalive time capping.
    fn get_original_job(
        &self,
        job_id: &str,
        channel_id: Option<u32>,
    ) -> Option<server_to_client::Notify> {
        let channel_id = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            channel_id?
        };

        self.valid_sv1_jobs
            .with(&channel_id, |jobs| {
                jobs.iter().find(|j| j.job_id == job_id).cloned()
            })
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DownstreamDifficultyConfig, TranslatorConfig, Upstream};
    use async_channel::unbounded;
    use std::str::FromStr;
    use stratum_apps::{
        key_utils::Secp256k1PublicKey,
        stratum_core::mining_sv2::{
            OpenExtendedMiningChannelSuccessOwned, OpenMiningChannelErrorOwned, SetTargetOwned,
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
    ) {
        let (downstream_sv1_sender, _downstream_sv1_receiver) = unbounded();
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
            sv1_server_receiver,
            target,
            Some(hashrate),
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
            server
                .channel_id_to_downstream_id
                .insert(channel_id, downstream_id);
        }
        server
            .sv1_server_io
            .sv1_server_to_downstream_sender
            .insert(downstream_id, sv1_server_sender);
    }

    #[test]
    fn test_sv1_server_creation() {
        let server = create_test_sv1_server();

        assert_eq!(server.shares_per_minute, 5.0);
        assert_eq!(server.listener_addr.ip().to_string(), "127.0.0.1");
        assert_eq!(server.listener_addr.port(), 3333);
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
    async fn test_send_set_difficulty_to_all_downstreams_empty() {
        let server = create_test_sv1_server();
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();

        // Test with empty downstreams
        _ = server
            .send_set_difficulty_to_all_downstreams(target, None)
            .await;

        // Should not crash with empty downstreams
    }

    #[tokio::test]
    async fn test_send_set_difficulty_to_specific_downstream_not_found() {
        let server = create_test_sv1_server();
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();
        let channel_id = 1u32;

        // Test with no downstreams
        _ = server
            .send_set_difficulty_to_specific_downstream(channel_id, target, None)
            .await;

        // Should not crash when no downstreams are found
    }

    #[tokio::test]
    async fn test_handle_set_target_without_vardiff_aggregated() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);

        let server = Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode);
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();

        let set_target = SetTargetOwned {
            channel_id: 1,
            maximum_target: target.to_le_bytes().into(),
        };

        // Test should not panic and should handle the message
        _ = server.handle_set_target_without_vardiff(set_target).await;
    }

    #[tokio::test]
    async fn test_handle_set_target_without_vardiff_non_aggregated() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode);
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();

        let set_target = SetTargetOwned {
            channel_id: 1,
            maximum_target: target.to_le_bytes().into(),
        };

        // Test should not panic and should handle the message
        _ = server.handle_set_target_without_vardiff(set_target).await;
    }

    #[tokio::test]
    async fn missing_downstream_requests_disconnect() {
        let server = create_test_sv1_server();

        let error = server.handle_open_channel_request(7).await.unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
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
                    error_code: "channel-capacity-exhausted".try_into().unwrap(),
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
    async fn aggregated_channel_open_refreshes_hashrate() {
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

        let (message, _) = channel_manager_receiver.recv().await.unwrap();
        let MiningOwned::UpdateChannel(update) = message else {
            panic!("expected UpdateChannel");
        };
        assert_eq!(update.nominal_hash_rate, 200.0);
    }

    #[tokio::test]
    async fn closed_downstream_does_not_shutdown_on_aggregated_set_target() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();
        let tproxy_mode = TproxyMode::from(config.aggregate_channels);
        let server = Sv1Server::new(addr, cm_receiver, cm_sender, config, tproxy_mode);
        register_test_downstream(&server, 7, Some(9), 100.0, true);

        let target = hash_rate_to_target(200.0, 5.0).unwrap();
        let error = server
            .handle_set_target_without_vardiff(SetTargetOwned {
                channel_id: AGGREGATED_CHANNEL_ID,
                maximum_target: target.to_le_bytes().into(),
            })
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelErrorSender));
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
            .handle_set_target_without_vardiff(SetTargetOwned {
                channel_id: 9,
                maximum_target: target.to_le_bytes().into(),
            })
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Disconnect(7)));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelErrorSender));
    }

    #[test]
    fn pending_vardiff_target_keeps_only_latest_per_downstream() {
        let server = create_test_sv1_server();
        let first_target = hash_rate_to_target(100.0, 5.0).unwrap();
        let latest_target = hash_rate_to_target(200.0, 5.0).unwrap();

        server.pending_target_updates.insert(7, first_target);
        server.pending_target_updates.insert(7, latest_target);

        assert_eq!(server.pending_target_updates.len(), 1);
        assert_eq!(
            server.pending_target_updates.get_cloned(&7),
            Some(latest_target)
        );
    }

    #[tokio::test]
    async fn disconnect_removes_pending_vardiff_target() {
        let server = create_test_sv1_server();
        let target = hash_rate_to_target(100.0, 5.0).unwrap();
        server.pending_target_updates.insert(7, target);
        server.pending_target_updates.insert(8, target);

        server.handle_downstream_disconnect(7).await.unwrap();

        assert_eq!(server.pending_target_updates.len(), 1);
        assert!(server.pending_target_updates.contains_key(&8));
    }

    #[tokio::test]
    async fn immediate_vardiff_update_clears_stale_pending_target() {
        use stratum_apps::stratum_core::channels_sv2::Vardiff;

        let server = create_test_sv1_server();
        register_test_downstream(&server, 7, Some(9), 100.0, false);

        let upstream_target = hash_rate_to_target(100.0, 5.0).unwrap();
        server
            .downstreams
            .with(&7, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| data.set_upstream_target(upstream_target, 7))
                    .unwrap()
            })
            .unwrap();

        // Parked update from an earlier tick that wanted a harder target.
        let stale_pending = hash_rate_to_target(200.0, 5.0).unwrap();
        server.pending_target_updates.insert(7, stale_pending);

        // Drive vardiff to a deterministic downward adjustment: one share in the
        // last two minutes against 5 shares/minute expected collapses the hashrate
        // estimate, so the new (easier) target takes the immediate path.
        let mut vardiff_state = VardiffState::new().unwrap();
        let two_minutes_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 120;
        vardiff_state.set_timestamp_of_last_update(two_minutes_ago);
        vardiff_state.set_shares_since_last_update(1);
        server.vardiff.insert(7, vardiff_state);

        // The UpdateChannel send fails in this harness (no channel manager task);
        // the pending-map bookkeeping we assert on happens before that.
        let _ = server.handle_vardiff_updates().await;

        assert!(
            !server.pending_target_updates.contains_key(&7),
            "immediate update must clear the superseded pending target"
        );
    }

    #[tokio::test]
    async fn stale_set_target_keeps_pending_update_until_satisfied() {
        let server = create_test_sv1_server();
        register_test_downstream(&server, 7, Some(9), 100.0, false);

        // The downstream wants a harder (lower) target than the upstream currently has.
        let pending_target = hash_rate_to_target(200.0, 5.0).unwrap();
        let stale_upstream_target = hash_rate_to_target(100.0, 5.0).unwrap();
        server.pending_target_updates.insert(7, pending_target);

        // A SetTarget that does not satisfy the pending update (e.g. the reply to an
        // older UpdateChannel) must leave it pending instead of dropping it.
        server
            .handle_set_target_message(SetTargetOwned {
                channel_id: 9,
                maximum_target: stale_upstream_target.to_le_bytes().into(),
            })
            .await
            .unwrap();
        assert_eq!(
            server.pending_target_updates.get_cloned(&7),
            Some(pending_target)
        );

        // The satisfying SetTarget applies the pending update and clears it.
        server
            .handle_set_target_message(SetTargetOwned {
                channel_id: 9,
                maximum_target: pending_target.to_le_bytes().into(),
            })
            .await
            .unwrap();
        assert!(server.pending_target_updates.is_empty());
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
}
