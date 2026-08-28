use crate::{
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
    sv1::job_store::Sv1JobStore,
    utils::SubmitShareWithChannelId,
};
use async_channel::{Receiver, Sender};
#[cfg(feature = "monitoring")]
use std::net::IpAddr;
use std::{future::Future, sync::Arc, time::Instant};
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    stratum_core::{
        bitcoin::Target,
        sv1_api::{
            json_rpc::{self, Message},
            server_to_client,
            utils::{Extranonce, HexU32Be},
        },
    },
    sync::SharedLock,
    task_manager::TaskManager,
    utils::types::{ChannelId, DownstreamId, Hashrate},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Work queued by the SV1 server for a single downstream task.
///
/// Setup completion is queued alongside notifications so that this task releases cached mining
/// notifications in FIFO order after both setup responses have been queued to the miner.
#[derive(Debug)]
pub(super) enum Sv1ServerEvent {
    Notification(json_rpc::Message),
    SetupComplete,
}

impl Sv1ServerEvent {
    pub(super) fn notification(message: json_rpc::Message) -> Self {
        Self::Notification(message)
    }
}

#[derive(Clone, Debug)]
pub struct DownstreamIo {
    pub downstream_sv1_sender: Sender<json_rpc::Message>,
    downstream_sv1_receiver: Receiver<json_rpc::Message>,
    sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
    sv1_server_receiver: Receiver<Sv1ServerEvent>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl DownstreamIo {
    fn new(
        downstream_sv1_sender: Sender<json_rpc::Message>,
        downstream_sv1_receiver: Receiver<json_rpc::Message>,
        sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
        sv1_server_receiver: Receiver<Sv1ServerEvent>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sv1SessionState {
    /// Setup responses are still pending, or cached notifications are still being delivered.
    Starting { subscribed: bool, authorized: bool },
    /// Setup is complete and job notifications can be forwarded normally.
    Ready,
}

impl Default for Sv1SessionState {
    fn default() -> Self {
        Self::Starting {
            subscribed: false,
            authorized: false,
        }
    }
}

impl Sv1SessionState {
    /// Records a queued setup response and returns `true` only when both required responses have
    /// been queued for the first time.
    pub(super) fn record_response(&mut self, request: Sv1SetupRequest) -> bool {
        let Self::Starting {
            subscribed,
            authorized,
        } = self
        else {
            return false;
        };

        let was_complete = *subscribed && *authorized;
        match request {
            Sv1SetupRequest::Subscribe => *subscribed = true,
            Sv1SetupRequest::Authorize => *authorized = true,
        }
        !was_complete && *subscribed && *authorized
    }

    pub(super) fn is_ready(self) -> bool {
        self == Self::Ready
    }

    pub(super) fn is_subscribed(self) -> bool {
        match self {
            Self::Starting { subscribed, .. } => subscribed,
            Self::Ready => true,
        }
    }

    fn setup_complete(self) -> bool {
        match self {
            Self::Starting {
                subscribed,
                authorized,
            } => subscribed && authorized,
            Self::Ready => true,
        }
    }
}

/// Downstream-specific values needed to validate and translate a share for one advertised job.
///
/// Difficulty and extranonce assignments take effect on job boundaries, so late shares must use
/// the values that accompanied their own job rather than the downstream's newest values.
#[derive(Clone, Debug)]
pub(super) struct Sv1JobValidationContext {
    pub(super) extranonce: Extranonce,
    pub(super) extranonce2_len: usize,
    pub(super) target: Target,
}

#[derive(Debug)]
pub struct DownstreamData {
    pub channel_id: Option<ChannelId>,
    pub extranonce1: Extranonce,
    pub extranonce2_len: usize,
    // Current SV1 share-validation target. This follows the advertised
    // difficulty sent to the miner, including any SV1 pow2 rounding.
    pub target: Target,
    pub hashrate: Option<Hashrate>,
    #[cfg(feature = "monitoring")]
    pub connection_ip: IpAddr,
    pub version_rolling_mask: Option<HexU32Be>,
    pub version_rolling_min_bit: Option<HexU32Be>,
    pub sv1_username: String,
    pub sv1_worker_name: String,
    pub cached_set_difficulty: Option<json_rpc::Message>,
    pub cached_notify: Option<json_rpc::Message>,
    pub(super) session_state: Sv1SessionState,
    /// Per-job downstream state retained for late-share validation under the current chain tip.
    pub(super) job_validation_contexts: Sv1JobStore<Sv1JobValidationContext>,
    /// Number of queued `mining.set_extranonce` notifications not yet applied by this downstream.
    pub(super) pending_set_extranonce_notifications: usize,
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
    /// Timestamp anchoring the next keepalive interval.
    ///
    /// `None` before the first job and while an extranonce change is waiting for the job that
    /// activates it.
    pub keepalive_timer_anchor: Option<Instant>,
    pub(super) supports_set_extranonce: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sv1SetupRequest {
    Subscribe,
    Authorize,
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
            sv1_username: String::new(),
            sv1_worker_name: String::new(),
            cached_set_difficulty: None,
            cached_notify: None,
            session_state: Sv1SessionState::default(),
            job_validation_contexts: Sv1JobStore::default(),
            pending_set_extranonce_notifications: 0,
            pending_target: None,
            pending_hashrate: None,
            stable_hashrate: false,
            queued_sv1_handshake_messages: Vec::new(),
            pending_share: None,
            upstream_target: None,
            keepalive_timer_anchor: None,
            supports_set_extranonce: false,
        }
    }

    fn record_job_validation_context(&mut self, notify: &server_to_client::Notify) {
        let context = Sv1JobValidationContext {
            extranonce: self.extranonce1.clone(),
            extranonce2_len: self.extranonce2_len,
            target: self.target,
        };
        self.job_validation_contexts
            .activate(notify.job_id.clone(), context, notify.clean_jobs);
        if self.pending_set_extranonce_notifications == 0 {
            self.keepalive_timer_anchor = Some(Instant::now());
        }
    }

    pub(super) fn job_validation_context(&self, job_id: &str) -> Option<Sv1JobValidationContext> {
        self.job_validation_contexts.get(job_id).cloned()
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
    pub downstream_data: SharedLock<DownstreamData>,
    pub downstream_io: DownstreamIo,
    /// Per-connection cancellation token (child of the global token).
    /// Cancelled when this downstream's task loop exits, causing
    /// the associated SV1 I/O task to shut down.
    downstream_cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    /// Stops this miner's connection. Its task cleanup callback removes the associated server and
    /// channel-manager state.
    pub(super) fn disconnect(&self) {
        self.downstream_cancellation_token.cancel();
    }

    #[cfg(test)]
    pub(super) fn is_disconnected(&self) -> bool {
        self.downstream_cancellation_token.is_cancelled()
    }

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
                error_kind = ?e.kind,
                "{context} returned an error after shutdown was requested"
            );
            return LoopControl::Continue;
        }

        if fallback_token.is_cancelled() {
            debug!(
                downstream_id = self.downstream_id,
                error_kind = ?e.kind,
                "{context} returned an error during fallback"
            );
            return LoopControl::Continue;
        }

        match e.action {
            Action::Log => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
            }
            Action::Disconnect(_) => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested disconnect; cancelling downstream token"
                );
                self.downstream_cancellation_token.cancel();
                LoopControl::Break
            }
            Action::Shutdown => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested shutdown; cancelling global token"
                );
                cancellation_token.cancel();
                LoopControl::Break
            }
            other => {
                warn!(
                    downstream_id = self.downstream_id,
                    action = ?other,
                    error_kind = ?e.kind,
                    "{context} returned an unhandled action"
                );
                LoopControl::Continue
            }
        }
    }

    /// Creates a new downstream connection instance.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        downstream_id: DownstreamId,
        downstream_sv1_sender: Sender<json_rpc::Message>,
        downstream_sv1_receiver: Receiver<json_rpc::Message>,
        sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
        sv1_server_receiver: Receiver<Sv1ServerEvent>,
        target: Target,
        hashrate: Option<Hashrate>,
        #[cfg(feature = "monitoring")] connection_ip: IpAddr,
        downstream_cancellation_token: CancellationToken,
    ) -> Self {
        let downstream_data = SharedLock::new(DownstreamData::new(
            hashrate,
            target,
            #[cfg(feature = "monitoring")]
            connection_ip,
        ));
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
                            error!("Downstream {downstream_id}: error in downstream message handler: {e:?}");
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
                            error!("Downstream {downstream_id}: error in server message handler: {e:?}");
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

    /// Handles the next event received from the SV1 server.
    ///
    /// This method observes setup completion and processes messages broadcast from the SV1 server
    /// to downstream connections. Since `mining.notify` messages are guaranteed to never arrive
    /// before their corresponding `mining.set_difficulty` message, the logic is simplified to
    /// handle only handshake completion timing.
    ///
    /// Key behaviors:
    /// - Enables notification forwarding after setup completes
    /// - For `mining.set_difficulty`: Always caches the message (never sent immediately)
    /// - For `mining.notify`: Sends any pending set_difficulty first, then forwards the notify
    /// - For other messages: Forwards directly to the miner
    /// - Caches both `mining.set_difficulty` and `mining.notify` messages if handshake is not yet
    ///   complete
    /// - On handshake completion: sends cached messages in correct order (set_difficulty first,
    ///   then notify)
    pub(super) async fn handle_sv1_server_message(&self) -> TproxyResult<(), error::Downstream> {
        match self.downstream_io.sv1_server_receiver.recv().await {
            Ok(Sv1ServerEvent::SetupComplete) => {
                info!(
                    "Down: mining.subscribe and mining.authorize responses sent; enabling mining notifications"
                );
                self.enable_notification_forwarding().await?;
            }
            Ok(Sv1ServerEvent::Notification(message)) => {
                let downstream_id = self.downstream_id;

                if let Message::Notification(notification) = &message {
                    match notification.method.as_str() {
                        "mining.set_difficulty" => {
                            // Difficulty changes are always paired with the next notify. Keeping
                            // the session state and cache in this task prevents setup completion
                            // from draining a newly arrived difficulty by itself.
                            let session_state = self
                                .downstream_data
                                .with(|data| {
                                    data.cached_set_difficulty = Some(message);
                                    data.session_state
                                })
                                .map_err(TproxyError::shutdown)?;
                            debug!(
                                ?session_state,
                                "Down: Caching mining.set_difficulty to send before next mining.notify"
                            );
                            return Ok(());
                        }
                        "mining.notify" => {
                            let notify = server_to_client::Notify::try_from(notification.clone())
                                .map_err(|error| {
                                TproxyError::shutdown(
                                    TproxyErrorKind::InvalidMiningNotifyNotification(format!(
                                        "{error:?}"
                                    )),
                                )
                            })?;
                            let messages_to_send = self
                                .downstream_data
                                .with(|data| {
                                    if !data.session_state.is_ready() {
                                        data.cached_notify = Some(message.clone());
                                        return None;
                                    }

                                    let cached_set_difficulty = data.cached_set_difficulty.take();
                                    if cached_set_difficulty.is_some() {
                                        if let Some(new_target) = data.pending_target.take() {
                                            data.target = new_target;
                                        }
                                        if let Some(new_hashrate) = data.pending_hashrate.take() {
                                            data.hashrate = Some(new_hashrate);
                                        }
                                    }
                                    data.record_job_validation_context(&notify);
                                    Some((cached_set_difficulty, Message::from(notify)))
                                })
                                .map_err(TproxyError::shutdown)?;

                            let Some((pending_set_difficulty, notify)) = messages_to_send else {
                                debug!("Down: SV1 handshake not complete, caching mining.notify");
                                return Ok(());
                            };

                            if let Some(set_difficulty) = pending_set_difficulty {
                                debug!(
                                    "Down: Sending pending mining.set_difficulty before mining.notify"
                                );
                                self.downstream_io
                                    .downstream_sv1_sender
                                    .send(set_difficulty)
                                    .await
                                    .map_err(|error| {
                                        error!(
                                            "Down: Failed to send mining.set_difficulty to downstream: {error:?}"
                                        );
                                        TproxyError::disconnect(
                                            TproxyErrorKind::ChannelErrorSender,
                                            downstream_id,
                                        )
                                    })?;
                            }

                            debug!("Down: Sending mining.notify");
                            self.downstream_io
                                .downstream_sv1_sender
                                .send(notify)
                                .await
                                .map_err(|error| {
                                    error!(
                                        "Down: Failed to send mining.notify to downstream: {error:?}"
                                    );
                                    TproxyError::disconnect(
                                        TproxyErrorKind::ChannelErrorSender,
                                        downstream_id,
                                    )
                                })?;
                            return Ok(());
                        }
                        "mining.set_extranonce" => {
                            let set_extranonce =
                                server_to_client::SetExtranonce::try_from(notification.clone())
                                    .map_err(|error| {
                                        TproxyError::shutdown(
                                            TproxyErrorKind::InvalidSetExtranonceNotification(
                                                format!("{error:?}"),
                                            ),
                                        )
                                    })?;
                            let (is_subscribed, supports_set_extranonce) = self
                                .downstream_data
                                .with(|data| {
                                    // The pending marker is installed before this message is
                                    // queued, preventing keepalives until a job with the new
                                    // prefix arrives. Processing the notification in this task
                                    // preserves ordering with older jobs already in the queue.
                                    data.pending_set_extranonce_notifications =
                                        data.pending_set_extranonce_notifications.saturating_sub(1);
                                    data.extranonce1 = set_extranonce.extra_nonce1.clone();
                                    data.extranonce2_len = set_extranonce.extra_nonce2_size;

                                    if !data.session_state.is_ready() {
                                        // A cached job predates this prefix change but has not
                                        // reached the miner. Drop it so the new extranonce is
                                        // first applied to a job created after the change.
                                        data.cached_notify = None;
                                    }

                                    (
                                        data.session_state.is_subscribed(),
                                        data.supports_set_extranonce,
                                    )
                                })
                                .map_err(TproxyError::shutdown)?;

                            if is_subscribed && !supports_set_extranonce {
                                warn!(
                                    downstream_id,
                                    "Disconnecting subscribed SV1 miner that does not support mining.set_extranonce"
                                );
                                self.disconnect();
                                return Ok(());
                            }

                            if is_subscribed {
                                self.downstream_io
                                    .downstream_sv1_sender
                                    .send(message)
                                    .await
                                    .map_err(|error| {
                                        error!(
                                            "Down: Failed to send mining.set_extranonce to downstream: {error:?}"
                                        );
                                        TproxyError::disconnect(
                                            TproxyErrorKind::ChannelErrorSender,
                                            downstream_id,
                                        )
                                    })?;
                            }
                            return Ok(());
                        }
                        _ => {
                            let handshake_complete = self
                                .downstream_data
                                .with(|data| data.session_state.is_ready())
                                .map_err(TproxyError::shutdown)?;
                            if !handshake_complete {
                                debug!(
                                    "Down: SV1 handshake not complete, skipping other notification"
                                );
                                return Ok(());
                            }

                            self.downstream_io
                                .downstream_sv1_sender
                                .send(message)
                                .await
                                .map_err(|error| {
                                    error!(
                                        "Down: Failed to send notification to downstream: {error:?}"
                                    );
                                    TproxyError::disconnect(
                                        TproxyErrorKind::ChannelErrorSender,
                                        downstream_id,
                                    )
                                })?;
                        }
                    }
                } else {
                    debug!("Down: Skipping non-notification message from SV1 server");
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

    /// Enables normal notification forwarding after both setup responses are delivered.
    ///
    /// This method is called when the downstream completes the SV1 handshake
    /// (subscribe + authorize). It sends any cached messages in the correct order:
    /// set_difficulty first, then notify.
    async fn enable_notification_forwarding(&self) -> TproxyResult<(), error::Downstream> {
        let (did_enable, cached_messages) = self
            .downstream_data
            .with(|data| {
                if data.session_state.is_ready() || !data.session_state.setup_complete() {
                    return (false, None);
                }

                let Some(notify) = data.cached_notify.take() else {
                    data.session_state = Sv1SessionState::Ready;
                    return (true, None);
                };

                (true, Some((data.cached_set_difficulty.take(), notify)))
            })
            .map_err(TproxyError::shutdown)?;

        if !did_enable {
            debug!(
                "Down: Notification forwarding was already enabled for downstream {}",
                self.downstream_id
            );
            return Ok(());
        }

        let Some((cached_set_difficulty, cached_notify)) = cached_messages else {
            debug!(
                "Down: Notification forwarding enabled without a cached job for downstream {}",
                self.downstream_id
            );
            return Ok(());
        };

        debug!("Down: SV1 handshake completed for downstream");

        self.send_cached_handshake_messages(cached_set_difficulty, Some(cached_notify))
            .await?;

        self.downstream_data
            .with(|data| data.session_state = Sv1SessionState::Ready)
            .map_err(TproxyError::shutdown)?;

        Ok(())
    }

    async fn send_cached_handshake_messages(
        &self,
        set_difficulty: Option<json_rpc::Message>,
        notify: Option<json_rpc::Message>,
    ) -> TproxyResult<(), error::Downstream> {
        if let Some(set_difficulty) = set_difficulty {
            debug!("Down: Sending cached mining.set_difficulty after handshake completion");
            self.downstream_io
                .downstream_sv1_sender
                .send(set_difficulty)
                .await
                .map_err(|error| {
                    error!(
                        "Down: Failed to send cached mining.set_difficulty to downstream: {error:?}"
                    );
                    TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, self.downstream_id)
                })?;

            self.downstream_data
                .with(|data| {
                    if let Some(new_target) = data.pending_target.take() {
                        data.target = new_target;
                    }
                    if let Some(new_hashrate) = data.pending_hashrate.take() {
                        data.hashrate = Some(new_hashrate);
                    }
                })
                .map_err(TproxyError::shutdown)?;
        }

        if let Some(notify_msg) = notify {
            debug!("Down: Sending cached mining.notify after handshake completion");
            let json_rpc::Message::Notification(notification) = notify_msg else {
                return Err(TproxyError::shutdown(
                    TproxyErrorKind::InvalidMiningNotifyNotification(
                        "cached mining.notify was not a notification".to_string(),
                    ),
                ));
            };
            let parsed = server_to_client::Notify::try_from(notification).map_err(|error| {
                TproxyError::shutdown(TproxyErrorKind::InvalidMiningNotifyNotification(format!(
                    "{error:?}"
                )))
            })?;
            self.downstream_data
                .with(|data| data.record_job_validation_context(&parsed))
                .map_err(TproxyError::shutdown)?;
            self.downstream_io
                .downstream_sv1_sender
                .send(parsed.into())
                .await
                .map_err(|error| {
                    error!("Down: Failed to send cached mining.notify to downstream: {error:?}");
                    TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, self.downstream_id)
                })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::unbounded;

    fn notify_with_clean_jobs(job_id: &str, clean_jobs: bool) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": null,
            "method": "mining.notify",
            "params": [
                job_id,
                "00".repeat(32),
                "00",
                "00",
                [],
                "20000000",
                "1d00ffff",
                "00000001",
                clean_jobs
            ]
        }))
        .unwrap()
    }

    fn notify(job_id: &str) -> Message {
        notify_with_clean_jobs(job_id, true)
    }

    fn set_difficulty() -> Message {
        serde_json::from_value(serde_json::json!({
            "id": null,
            "method": "mining.set_difficulty",
            "params": [1.0]
        }))
        .unwrap()
    }

    fn invalid_notify() -> Message {
        serde_json::from_value(serde_json::json!({
            "id": null,
            "method": "mining.notify",
            "params": []
        }))
        .unwrap()
    }

    fn assert_message_eq(actual: &Message, expected: &Message) {
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[tokio::test]
    async fn repeated_handshake_completion_preserves_cached_difficulty() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let old_target = Target::from_le_bytes([0x11; 32]);
        let new_target = Target::from_le_bytes([0x22; 32]);
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            old_target,
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );

        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Ready;
                data.cached_set_difficulty = Some(set_difficulty());
                data.pending_target = Some(new_target);
            })
            .unwrap();

        downstream.enable_notification_forwarding().await.unwrap();

        assert!(downstream_sv1_receiver.try_recv().is_err());
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.target, old_target);
                assert_eq!(data.pending_target, Some(new_target));
                assert!(data.cached_set_difficulty.is_some());
                assert_eq!(data.session_state, Sv1SessionState::Ready);
            })
            .unwrap();
    }

    #[tokio::test]
    async fn invalid_notify_returns_an_error_instead_of_panicking() {
        let (downstream_sv1_sender, _downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| data.session_state = Sv1SessionState::Ready)
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(invalid_notify()))
            .await
            .unwrap();

        let error = downstream.handle_sv1_server_message().await.unwrap_err();

        assert!(matches!(error.action, Action::Shutdown));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::InvalidMiningNotifyNotification(_)
        ));
    }

    #[tokio::test]
    async fn invalid_cached_notify_returns_an_error_instead_of_panicking() {
        let (downstream_sv1_sender, _downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: true,
                };
                data.cached_notify = Some(invalid_notify());
            })
            .unwrap();

        let error = downstream
            .enable_notification_forwarding()
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Shutdown));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::InvalidMiningNotifyNotification(_)
        ));
    }

    #[tokio::test]
    async fn notify_queued_before_setup_completion_is_flushed() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender.clone(),
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        let queued_notify = notify("queued");
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: true,
                };
            })
            .unwrap();

        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(queued_notify.clone()))
            .await
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::SetupComplete)
            .await
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();
        assert!(downstream_sv1_receiver.try_recv().is_err());
        downstream.handle_sv1_server_message().await.unwrap();

        assert_message_eq(
            &downstream_sv1_receiver.recv().await.unwrap(),
            &queued_notify,
        );
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.session_state, Sv1SessionState::Ready);
                assert!(data.cached_notify.is_none());
            })
            .unwrap();
    }

    #[tokio::test]
    async fn difficulty_queued_before_setup_completion_waits_for_next_notify() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender.clone(),
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        let queued_difficulty = set_difficulty();
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: true,
                };
            })
            .unwrap();

        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(queued_difficulty.clone()))
            .await
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::SetupComplete)
            .await
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        assert!(downstream_sv1_receiver.try_recv().is_err());
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.session_state, Sv1SessionState::Ready);
                assert_message_eq(
                    data.cached_set_difficulty.as_ref().unwrap(),
                    &queued_difficulty,
                );
            })
            .unwrap();

        let next_notify = notify_with_clean_jobs("next", false);
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(next_notify.clone()))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        assert_message_eq(
            &downstream_sv1_receiver.recv().await.unwrap(),
            &queued_difficulty,
        );
        let forwarded_notify = downstream_sv1_receiver.recv().await.unwrap();
        let Message::Notification(notification) = &forwarded_notify else {
            panic!("expected mining.notify");
        };
        let forwarded_notify = server_to_client::Notify::try_from(notification.clone()).unwrap();
        assert_eq!(forwarded_notify.job_id, "next");
        assert!(!forwarded_notify.clean_jobs);
    }

    #[tokio::test]
    async fn difficulty_change_preserves_previous_job_validation_context() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let old_target = Target::from_le_bytes([0x22; 32]);
        let new_target = Target::from_le_bytes([0x11; 32]);
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            old_target,
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| data.session_state = Sv1SessionState::Ready)
            .unwrap();

        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(notify("old")))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        downstream_sv1_receiver.recv().await.unwrap();

        downstream
            .downstream_data
            .with(|data| data.pending_target = Some(new_target))
            .unwrap();
        for message in [set_difficulty(), notify_with_clean_jobs("new", false)] {
            sv1_server_message_sender
                .send(Sv1ServerEvent::notification(message))
                .await
                .unwrap();
            downstream.handle_sv1_server_message().await.unwrap();
        }
        downstream_sv1_receiver.recv().await.unwrap();
        let forwarded_notify = downstream_sv1_receiver.recv().await.unwrap();
        let Message::Notification(notification) = forwarded_notify else {
            panic!("expected mining.notify");
        };
        let forwarded_notify = server_to_client::Notify::try_from(notification).unwrap();
        assert!(!forwarded_notify.clean_jobs);

        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.target, new_target);
                assert_eq!(
                    data.job_validation_context("old")
                        .map(|context| context.target),
                    Some(old_target)
                );
                assert_eq!(
                    data.job_validation_context("new")
                        .map(|context| context.target),
                    Some(new_target)
                );
                assert_eq!(data.job_validation_contexts.len(), 2);
            })
            .unwrap();

        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(notify("new-tip")))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        downstream_sv1_receiver.recv().await.unwrap();

        downstream
            .downstream_data
            .with(|data| {
                assert!(data.job_validation_context("old").is_none());
                assert!(data.job_validation_context("new").is_none());
                assert!(data.job_validation_context("new-tip").is_some());
                assert_eq!(data.job_validation_contexts.len(), 1);
            })
            .unwrap();
    }

    #[tokio::test]
    async fn extranonce_change_cannot_overtake_queued_setup_job() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender.clone(),
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        let old_extranonce = downstream
            .downstream_data
            .with(|data| data.extranonce1.clone())
            .unwrap();
        let new_extranonce: Extranonce = vec![1, 2, 3, 4].try_into().unwrap();
        let old_notify = notify("old");
        downstream
            .downstream_data
            .with(|data| {
                data.supports_set_extranonce = true;
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: true,
                };
                data.pending_set_extranonce_notifications = 1;
            })
            .unwrap();

        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(old_notify.clone()))
            .await
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::SetupComplete)
            .await
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(Message::from(
                server_to_client::SetExtranonce {
                    extra_nonce1: new_extranonce.clone(),
                    extra_nonce2_size: 4,
                },
            )))
            .await
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();

        assert_message_eq(&downstream_sv1_receiver.recv().await.unwrap(), &old_notify);
        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            Message::Notification(notification)
                if notification.method == "mining.set_extranonce"
        ));
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(
                    data.job_validation_context("old")
                        .map(|context| context.extranonce),
                    Some(old_extranonce)
                );
                assert_eq!(data.extranonce1, new_extranonce);
                assert_eq!(data.session_state, Sv1SessionState::Ready);
                assert!(data.keepalive_timer_anchor.is_none());
            })
            .unwrap();
    }

    #[tokio::test]
    async fn extranonce_change_is_applied_to_the_next_job() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Ready;
                data.supports_set_extranonce = true;
            })
            .unwrap();

        let old_extranonce = downstream
            .downstream_data
            .with(|data| data.extranonce1.clone())
            .unwrap();
        let new_extranonce: Extranonce = vec![1, 2, 3, 4].try_into().unwrap();
        let old_notify = notify("old");
        let new_notify = notify_with_clean_jobs("new", false);

        // The old job was queued before SetExtranoncePrefix, but the server installs the pending
        // marker before this downstream task processes either message.
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(old_notify.clone()))
            .await
            .unwrap();
        downstream
            .downstream_data
            .with(|data| {
                data.pending_set_extranonce_notifications = 1;
            })
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(Message::from(
                server_to_client::SetExtranonce {
                    extra_nonce1: new_extranonce.clone(),
                    extra_nonce2_size: 6,
                },
            )))
            .await
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(new_notify.clone()))
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();

        assert_message_eq(&downstream_sv1_receiver.recv().await.unwrap(), &old_notify);
        assert!(matches!(
            downstream_sv1_receiver.recv().await.unwrap(),
            Message::Notification(notification)
                if notification.method == "mining.set_extranonce"
        ));
        assert_message_eq(&downstream_sv1_receiver.recv().await.unwrap(), &new_notify);
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(
                    data.job_validation_context("old")
                        .map(|context| context.extranonce),
                    Some(old_extranonce)
                );
                assert_eq!(
                    data.job_validation_context("new")
                        .map(|context| context.extranonce),
                    Some(new_extranonce)
                );
                assert_eq!(
                    data.job_validation_context("old")
                        .map(|context| context.extranonce2_len),
                    Some(4)
                );
                assert_eq!(
                    data.job_validation_context("new")
                        .map(|context| context.extranonce2_len),
                    Some(6)
                );
                assert!(data.keepalive_timer_anchor.is_some());
            })
            .unwrap();
    }

    #[tokio::test]
    async fn consecutive_extranonce_changes_wait_for_the_latest_job() {
        let (downstream_sv1_sender, _downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        downstream
            .downstream_data
            .with(|data| {
                data.supports_set_extranonce = true;
                data.session_state = Sv1SessionState::Ready;
                data.pending_set_extranonce_notifications = 2;
            })
            .unwrap();
        let first_extranonce: Extranonce = vec![1, 2, 3, 4].try_into().unwrap();
        let second_extranonce: Extranonce = vec![5, 6, 7, 8].try_into().unwrap();
        for message in [
            Message::from(server_to_client::SetExtranonce {
                extra_nonce1: first_extranonce.clone(),
                extra_nonce2_size: 4,
            }),
            notify_with_clean_jobs("first", false),
            Message::from(server_to_client::SetExtranonce {
                extra_nonce1: second_extranonce.clone(),
                extra_nonce2_size: 4,
            }),
            notify_with_clean_jobs("second", false),
        ] {
            sv1_server_message_sender
                .send(Sv1ServerEvent::notification(message))
                .await
                .unwrap();
        }

        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.pending_set_extranonce_notifications, 1);
                assert!(data.keepalive_timer_anchor.is_none());
                assert_eq!(
                    data.job_validation_context("first")
                        .map(|context| context.extranonce),
                    Some(first_extranonce)
                );
            })
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.pending_set_extranonce_notifications, 0);
                assert!(data.keepalive_timer_anchor.is_some());
                assert_eq!(
                    data.job_validation_context("second")
                        .map(|context| context.extranonce),
                    Some(second_extranonce)
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn extranonce_change_before_subscribe_updates_setup_without_early_notification() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = unbounded();
        let (_downstream_sender, downstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_message_sender, sv1_server_receiver) = unbounded();
        let downstream = Downstream::new(
            1,
            downstream_sv1_sender,
            downstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            Target::from_le_bytes([0x11; 32]),
            None,
            #[cfg(feature = "monitoring")]
            "127.0.0.1".parse().unwrap(),
            CancellationToken::new(),
        );
        let new_extranonce: Extranonce = vec![1, 2, 3, 4].try_into().unwrap();
        downstream
            .downstream_data
            .with(|data| {
                data.cached_notify = Some(notify("old"));
                data.pending_set_extranonce_notifications = 1;
            })
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::notification(Message::from(
                server_to_client::SetExtranonce {
                    extra_nonce1: new_extranonce.clone(),
                    extra_nonce2_size: 4,
                },
            )))
            .await
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();

        assert!(downstream_sv1_receiver.try_recv().is_err());
        assert!(!downstream.is_disconnected());
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.extranonce1, new_extranonce);
                assert!(data.cached_notify.is_none());
                assert_eq!(data.pending_set_extranonce_notifications, 0);
                assert!(data.keepalive_timer_anchor.is_none());
            })
            .unwrap();
    }

    #[test]
    fn session_setup_completes_once_in_either_request_order() {
        for requests in [
            [Sv1SetupRequest::Subscribe, Sv1SetupRequest::Authorize],
            [Sv1SetupRequest::Authorize, Sv1SetupRequest::Subscribe],
        ] {
            let mut state = Sv1SessionState::default();

            assert!(!state.record_response(requests[0]));
            assert!(state.record_response(requests[1]));
            assert!(!state.is_ready());
            assert!(!state.record_response(requests[0]));
            assert!(!state.record_response(requests[1]));
        }
    }
}
