use crate::{
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
    utils::SubmitShareWithChannelId,
};
use async_channel::{Receiver, Sender};
#[cfg(feature = "monitoring")]
use std::net::IpAddr;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    custom_mutex::Mutex,
    fallback_coordinator::FallbackCoordinator,
    stratum_core::{
        bitcoin::Target,
        sv1_api::{
            json_rpc::{self, Message},
            server_to_client,
            utils::{Extranonce, HexU32Be},
        },
    },
    task_manager::TaskManager,
    utils::types::{ChannelId, DownstreamId, Hashrate},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Clone, Debug)]
pub struct DownstreamIo {
    pub downstream_sv1_sender: Sender<json_rpc::Message>,
    downstream_sv1_receiver: Receiver<json_rpc::Message>,
    sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
    sv1_server_receiver: Receiver<json_rpc::Message>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl DownstreamIo {
    fn new(
        downstream_sv1_sender: Sender<json_rpc::Message>,
        downstream_sv1_receiver: Receiver<json_rpc::Message>,
        sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
        sv1_server_receiver: Receiver<json_rpc::Message>,
    ) -> Self {
        Self {
            downstream_sv1_receiver,
            downstream_sv1_sender,
            sv1_server_receiver,
            sv1_server_sender,
        }
    }

    fn close(&self) {
        debug!("Dropping downstream channel state");
        self.downstream_sv1_sender.close();
        self.downstream_sv1_receiver.close_and_drain();
        self.sv1_server_receiver.close_and_drain();
    }
}

#[derive(Debug)]
pub struct DownstreamData {
    pub channel_id: Option<ChannelId>,
    pub extranonce1: Extranonce<'static>,
    pub extranonce2_len: usize,
    // Current SV1 share-validation target. This follows the advertised
    // difficulty sent to the miner, including any SV1 pow2 rounding.
    pub target: Target,
    pub hashrate: Option<Hashrate>,
    #[cfg(feature = "monitoring")]
    pub connection_ip: IpAddr,
    pub version_rolling_mask: Option<HexU32Be>,
    pub version_rolling_min_bit: Option<HexU32Be>,
    pub last_job_version_field: Option<u32>,
    pub sv1_username: String,
    pub sv1_worker_name: String,
    pub cached_set_difficulty: Option<json_rpc::Message>,
    pub cached_notify: Option<json_rpc::Message>,
    // Next advertised SV1 target, applied when the corresponding
    // mining.set_difficulty is sent with a new mining.notify.
    pub pending_target: Option<Target>,
    pub pending_hashrate: Option<Hashrate>,
    pub stable_hashrate: bool,
    // Queue of Sv1 handshake messages received while waiting for SV2 channel to open
    pub queued_sv1_handshake_messages: Vec<json_rpc::Message>,
    // Stores pending shares to be sent to the sv1_server
    pub pending_share: Option<SubmitShareWithChannelId>,
    // Exact target currently accepted upstream, used to decide whether a
    // stricter downstream difficulty must wait for a SetTarget response.
    pub upstream_target: Option<Target>,
    // Timestamp of when the last job was received by this downstream, used for keepalive check
    pub last_job_received_time: Option<Instant>,
}

impl DownstreamData {
    pub fn new(
        hashrate: Option<Hashrate>,
        target: Target,
        #[cfg(feature = "monitoring")] connection_ip: IpAddr,
    ) -> Self {
        DownstreamData {
            channel_id: None,
            extranonce1: vec![0; 8]
                .try_into()
                .expect("8-byte extranonce is always valid"),
            extranonce2_len: 4,
            target,
            hashrate,
            #[cfg(feature = "monitoring")]
            connection_ip,
            version_rolling_mask: None,
            version_rolling_min_bit: None,
            last_job_version_field: None,
            sv1_username: String::new(),
            sv1_worker_name: String::new(),
            cached_set_difficulty: None,
            cached_notify: None,
            pending_target: None,
            pending_hashrate: None,
            stable_hashrate: false,
            queued_sv1_handshake_messages: Vec::new(),
            pending_share: None,
            upstream_target: None,
            last_job_received_time: None,
        }
    }

    pub fn set_pending_target(&mut self, new_target: Target, downstream_id: DownstreamId) {
        self.pending_target = Some(new_target);
        debug!("Downstream {downstream_id}: Set pending target");
    }

    pub fn set_pending_hashrate(
        &mut self,
        new_hashrate: Option<Hashrate>,
        downstream_id: DownstreamId,
    ) {
        self.pending_hashrate = new_hashrate;
        debug!("Downstream {downstream_id}: Set pending hashrate");
    }

    pub fn set_upstream_target(&mut self, upstream_target: Target, downstream_id: DownstreamId) {
        self.upstream_target = Some(upstream_target);
        debug!(
            "Downstream {downstream_id}: Set upstream target to {}",
            upstream_target
        );
    }
}

/// Represents a downstream SV1 miner connection.
///
/// This struct manages the state and communication for a single SV1 miner connected
/// to the translator. It handles:
/// - SV1 protocol message processing (subscribe, authorize, submit)
/// - Bidirectional message routing between miner and SV1 server
/// - Mining job tracking and share validation
/// - Difficulty adjustment coordination
/// - Connection lifecycle management
///
/// Each downstream connection runs in its own async task that processes messages
/// from both the miner and the server, ensuring proper message ordering and
/// handling connection-specific state.
#[derive(Clone, Debug)]
pub struct Downstream {
    pub downstream_id: DownstreamId,
    pub downstream_data: Arc<Mutex<DownstreamData>>,
    pub downstream_io: DownstreamIo,
    // Flag to track if SV1 handshake is complete (subscribe + authorize)
    pub sv1_handshake_complete: Arc<AtomicBool>,
    /// Per-connection cancellation token (child of the global token).
    /// Cancelled when this downstream's task loop exits, causing
    /// the associated SV1 I/O task to shut down.
    downstream_cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    fn handle_error_action(
        &self,
        context: &str,
        e: &TproxyError<error::Downstream>,
        cancellation_token: &CancellationToken,
        fallback_token: &CancellationToken,
    ) -> LoopControl {
        if cancellation_token.is_cancelled() {
            debug!(
                downstream_id = self.downstream_id,
                error = %e,
                "{context} returned an error after shutdown was requested"
            );
            return LoopControl::Continue;
        }

        if fallback_token.is_cancelled() {
            debug!(
                downstream_id = self.downstream_id,
                error = %e,
                "{context} returned an error during fallback"
            );
            return LoopControl::Continue;
        }

        match e.action {
            Action::Log => {
                warn!(
                    downstream_id = self.downstream_id,
                    error = %e,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
            }
            Action::Disconnect(_) => {
                warn!(error = %e, "{context} requested disconnect; cancelling downstream token");
                self.downstream_cancellation_token.cancel();
                LoopControl::Break
            }
            Action::Shutdown => {
                warn!(
                    downstream_id = self.downstream_id,
                    error = %e,
                    "{context} requested shutdown; cancelling global token"
                );
                cancellation_token.cancel();
                LoopControl::Break
            }
            other => {
                warn!(
                    downstream_id = self.downstream_id,
                    action = %other,
                    error = %e,
                    "{context} returned an unhandled action"
                );
                LoopControl::Continue
            }
        }
    }

    /// Creates a new downstream connection instance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_id: DownstreamId,
        downstream_sv1_sender: Sender<json_rpc::Message>,
        downstream_sv1_receiver: Receiver<json_rpc::Message>,
        sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
        sv1_server_receiver: Receiver<json_rpc::Message>,
        target: Target,
        hashrate: Option<Hashrate>,
        #[cfg(feature = "monitoring")] connection_ip: IpAddr,
        downstream_cancellation_token: CancellationToken,
    ) -> Self {
        let downstream_data = Arc::new(Mutex::new(DownstreamData::new(
            hashrate,
            target,
            #[cfg(feature = "monitoring")]
            connection_ip,
        )));
        let downstream_channel_io = DownstreamIo::new(
            downstream_sv1_sender,
            downstream_sv1_receiver,
            sv1_server_sender,
            sv1_server_receiver,
        );
        Self {
            downstream_id,
            downstream_data,
            downstream_io: downstream_channel_io,
            sv1_handshake_complete: Arc::new(AtomicBool::new(false)),
            downstream_cancellation_token,
        }
    }

    /// Spawns and runs the main task loop for this downstream connection.
    ///
    /// This method creates an async task that handles all communication for this
    /// downstream connection. The task runs a select loop that processes:
    /// - Cancellation signals (global via cancellation_token or fallback)
    /// - Messages from the miner (subscribe, authorize, submit)
    /// - Messages from the SV1 server (notify, set_difficulty, etc.)
    ///
    /// The task will continue running until a cancellation signal is received or
    /// an unrecoverable error occurs. It ensures graceful cleanup of resources
    /// and proper error reporting.
    pub(super) fn start<F, Fut>(
        self,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        on_disconnect: F,
    ) where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let downstream_id = self.downstream_id;
        task_manager.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();

            loop {
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        info!("Downstream {downstream_id}: received app shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("Downstream {downstream_id}: fallback triggered");
                        break;
                    }

                    // Handle downstream -> server message
                    res = self.handle_downstream_message() => {
                        if let Err(e) = res {
                            error!("Downstream {downstream_id}: error in downstream message handler: {e}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_downstream_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }

                    // Handle server -> downstream message
                    res = self.handle_sv1_server_message() => {
                        if let Err(e) = res {
                            error!("Downstream {downstream_id}: error in server message handler: {e}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_sv1_server_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }

                    else => {
                        warn!("Downstream {downstream_id}: all channels closed; exiting task");
                        break;
                    }
                }
            }

            warn!("Downstream {downstream_id}: unified task shutting down");
            self.downstream_cancellation_token.cancel();
            self.downstream_io.close();
            on_disconnect().await;
            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });
    }

    /// Handles messages received from the SV1 server.
    ///
    /// This method processes messages broadcast from the SV1 server to downstream
    /// connections. Since `mining.notify` messages are guaranteed to never arrive
    /// before their corresponding `mining.set_difficulty` message, the logic is
    /// simplified to handle only handshake completion timing.
    ///
    /// Key behaviors:
    /// - Filters messages by channel ID and downstream ID
    /// - For `mining.set_difficulty`: Always caches the message (never sent immediately)
    /// - For `mining.notify`: Sends any pending set_difficulty first, then forwards the notify
    /// - For other messages: Forwards directly to the miner
    /// - Caches both `mining.set_difficulty` and `mining.notify` messages if handshake is not yet
    ///   complete
    /// - On handshake completion: sends cached messages in correct order (set_difficulty first,
    ///   then notify)
    async fn handle_sv1_server_message(&self) -> TproxyResult<(), error::Downstream> {
        match self.downstream_io.sv1_server_receiver.recv().await {
            Ok(message) => {
                let downstream_id = self.downstream_id;
                let handshake_complete = self.sv1_handshake_complete.load(Ordering::SeqCst);

                // Handle messages based on message type and handshake state
                if let Message::Notification(notification) = &message {
                    // For notifications (mining.set_difficulty, mining.notify), only send if
                    // handshake is complete
                    if handshake_complete {
                        match notification.method.as_str() {
                            "mining.set_difficulty" => {
                                // Cache the Sv1 set_difficulty message to be sent before the next
                                // notify
                                debug!("Down: Caching mining.set_difficulty to send before next mining.notify");
                                self.downstream_data.super_safe_lock(|d| {
                                    d.cached_set_difficulty = Some(message);
                                });
                                return Ok(());
                            }
                            "mining.notify" => {
                                let (pending_set_difficulty, notify_opt) =
                                    self.downstream_data.super_safe_lock(|d| {
                                        let cached_set_difficulty = d.cached_set_difficulty.take();

                                        // Prepare the notify message and update state
                                        let notify_result = server_to_client::Notify::try_from(
                                            notification.clone(),
                                        );
                                        if let Ok(mut notify) = notify_result {
                                            if cached_set_difficulty.is_some() {
                                                notify.clean_jobs = true;
                                            }
                                            d.last_job_version_field = Some(notify.version.0);

                                            // Update target and hashrate if we're sending
                                            // set_difficulty
                                            if cached_set_difficulty.is_some() {
                                                if let Some(new_target) = d.pending_target.take() {
                                                    d.target = new_target;
                                                }
                                                if let Some(new_hashrate) =
                                                    d.pending_hashrate.take()
                                                {
                                                    d.hashrate = Some(new_hashrate);
                                                }
                                            }
                                            // Update last job received time for keepalive tracking
                                            d.last_job_received_time = Some(Instant::now());
                                            (cached_set_difficulty, Some(notify))
                                        } else {
                                            (cached_set_difficulty, None)
                                        }
                                    });

                                if let Some(set_difficulty_msg) = &pending_set_difficulty {
                                    debug!("Down: Sending pending mining.set_difficulty before mining.notify");
                                    self.downstream_io
                                        .downstream_sv1_sender
                                        .send(set_difficulty_msg.clone())
                                        .await
                                        .map_err(|e| {
                                            error!(
                                                "Down: Failed to send mining.set_difficulty to downstream: {:?}",
                                                e
                                            );
                                            TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                                        })?;
                                }

                                if let Some(notify) = notify_opt {
                                    debug!("Down: Sending mining.notify");
                                    self.downstream_io
                                        .downstream_sv1_sender
                                        .send(notify.into())
                                        .await
                                        .map_err(|e| {
                                            error!("Down: Failed to send mining.notify to downstream: {:?}", e);
                                            TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                                        })?;
                                }
                                return Ok(());
                            }
                            _ => {
                                // Other notifications - forward if handshake complete
                                self.downstream_io
                                    .downstream_sv1_sender
                                    .send(message.clone())
                                    .await
                                    .map_err(|e| {
                                        error!(
                                            "Down: Failed to send notification to downstream: {:?}",
                                            e
                                        );
                                        TproxyError::disconnect(
                                            TproxyErrorKind::ChannelErrorSender,
                                            downstream_id,
                                        )
                                    })?;
                            }
                        }
                    } else {
                        // Handshake not complete - cache mining notifications but skip others
                        match notification.method.as_str() {
                            "mining.set_difficulty" => {
                                debug!("Down: SV1 handshake not complete, caching mining.set_difficulty");
                                self.downstream_data.super_safe_lock(|d| {
                                    d.cached_set_difficulty = Some(message);
                                });
                            }
                            "mining.notify" => {
                                debug!("Down: SV1 handshake not complete, caching mining.notify");
                                self.downstream_data.super_safe_lock(|d| {
                                    d.cached_notify = Some(message.clone());
                                    let notify =
                                        server_to_client::Notify::try_from(notification.clone())
                                            .expect("this must be a mining.notify");
                                    d.last_job_version_field = Some(notify.version.0);
                                });
                            }
                            _ => {
                                debug!(
                                    "Down: SV1 handshake not complete, skipping other notification"
                                );
                            }
                        }
                    }
                } else {
                    // Handshake not complete - skip non-notification messages.
                    debug!("Down: SV1 handshake not complete, skipping non-notification message");
                }
            }
            Err(e) => {
                error!(
                    "Sv1 message handler error for downstream {}: {:?}",
                    self.downstream_id, e
                );
                return Err(TproxyError::disconnect(e, self.downstream_id));
            }
        }

        Ok(())
    }

    /// Handles messages received from the downstream SV1 miner.
    ///
    /// This method processes SV1 protocol messages sent by the miner, including:
    /// - `mining.subscribe` - Subscription requests
    /// - `mining.authorize` - Authorization requests
    /// - `mining.submit` - Share submissions
    /// - Other SV1 protocol messages
    ///
    /// The method delegates message processing to the downstream data handler,
    /// which implements the SV1 protocol logic and generates appropriate responses.
    /// Responses are sent back to the miner, while share submissions are forwarded
    /// to the SV1 server for upstream processing.
    async fn handle_downstream_message(&self) -> TproxyResult<(), error::Downstream> {
        let downstream_id = self.downstream_id;
        let message = match self.downstream_io.downstream_sv1_receiver.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Error receiving downstream message: {:?}", e);
                return Err(TproxyError::disconnect(e, downstream_id));
            }
        };

        self.downstream_io
            .sv1_server_sender
            .send((downstream_id, message))
            .await
            .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;

        Ok(())
    }

    /// Handles SV1 handshake completion after mining.authorize.
    ///
    /// This method is called when the downstream completes the SV1 handshake
    /// (subscribe + authorize). It sends any cached messages in the correct order:
    /// set_difficulty first, then notify.
    pub(super) async fn handle_sv1_handshake_completion(
        &self,
    ) -> TproxyResult<(), error::Downstream> {
        let (cached_set_difficulty, cached_notify, downstream_id) =
            self.downstream_data.super_safe_lock(|d| {
                self.sv1_handshake_complete
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                (
                    d.cached_set_difficulty.take(),
                    d.cached_notify.take(),
                    self.downstream_id,
                )
            });
        debug!("Down: SV1 handshake completed for downstream");

        // Send cached messages in correct order: set_difficulty first, then notify
        if let Some(set_difficulty_msg) = cached_set_difficulty {
            debug!("Down: Sending cached mining.set_difficulty after handshake completion");
            self.downstream_io
                .downstream_sv1_sender
                .send(set_difficulty_msg)
                .await
                .map_err(|e| {
                    error!(
                        "Down: Failed to send cached mining.set_difficulty to downstream: {:?}",
                        e
                    );
                    TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                })?;

            // Update target and hashrate after sending set_difficulty
            self.downstream_data.super_safe_lock(|d| {
                if let Some(new_target) = d.pending_target.take() {
                    d.target = new_target;
                }
                if let Some(new_hashrate) = d.pending_hashrate.take() {
                    d.hashrate = Some(new_hashrate);
                }
            });
        }

        if let Some(notify_msg) = cached_notify {
            debug!("Down: Sending cached mining.notify after handshake completion");
            self.downstream_io
                .downstream_sv1_sender
                .send(notify_msg)
                .await
                .map_err(|e| {
                    error!(
                        "Down: Failed to send cached mining.notify to downstream: {:?}",
                        e
                    );
                    TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, downstream_id)
                })?;
            // Update last job received time for keepalive tracking
            self.downstream_data.super_safe_lock(|d| {
                d.last_job_received_time = Some(Instant::now());
            });
        }

        Ok(())
    }
}
