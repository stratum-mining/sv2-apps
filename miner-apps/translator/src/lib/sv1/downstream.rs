use crate::{
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
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
    /// Records a delivered setup response and returns `true` only when both required responses have
    /// been delivered for the first time.
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
    pub last_job_version_field: Option<u32>,
    pub sv1_username: String,
    pub sv1_worker_name: String,
    pub cached_set_difficulty: Option<json_rpc::Message>,
    pub cached_notify: Option<json_rpc::Message>,
    pub(super) session_state: Sv1SessionState,
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
            last_job_version_field: None,
            sv1_username: String::new(),
            sv1_worker_name: String::new(),
            cached_set_difficulty: None,
            cached_notify: None,
            session_state: Sv1SessionState::default(),
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

                if let Message::Notification(notification) = &message {
                    match notification.method.as_str() {
                        "mining.set_difficulty" => {
                            // Difficulty changes are always paired with the next notify. Keeping
                            // the session state and cache under the same lock prevents setup
                            // completion from draining a newly arrived difficulty by itself.
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
                            let messages_to_send = self
                                .downstream_data
                                .with(|data| {
                                    if !data.session_state.is_ready() {
                                        data.cached_notify = Some(message.clone());
                                        let notify = server_to_client::Notify::try_from(
                                            notification.clone(),
                                        )
                                        .expect("this must be a mining.notify");
                                        data.last_job_version_field = Some(notify.version.0);
                                        return None;
                                    }

                                    let cached_set_difficulty = data.cached_set_difficulty.take();
                                    let mut notify =
                                        server_to_client::Notify::try_from(notification.clone())
                                            .expect("this must be a mining.notify");
                                    if cached_set_difficulty.is_some() {
                                        notify.clean_jobs = true;
                                        if let Some(new_target) = data.pending_target.take() {
                                            data.target = new_target;
                                        }
                                        if let Some(new_hashrate) = data.pending_hashrate.take() {
                                            data.hashrate = Some(new_hashrate);
                                        }
                                    }
                                    data.last_job_version_field = Some(notify.version.0);
                                    data.last_job_received_time = Some(Instant::now());
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
    pub(super) async fn enable_notification_forwarding(
        &self,
    ) -> TproxyResult<(), error::Downstream> {
        let cached_messages = self
            .downstream_data
            .with(|data| {
                if data.session_state.is_ready() || !data.session_state.setup_complete() {
                    return None;
                }
                Some((data.cached_set_difficulty.take(), data.cached_notify.take()))
            })
            .map_err(TproxyError::shutdown)?;
        let Some((cached_set_difficulty, cached_notify)) = cached_messages else {
            debug!(
                "Down: Notification forwarding already enabled for downstream {}",
                self.downstream_id
            );
            return Ok(());
        };

        debug!("Down: SV1 handshake completed for downstream");

        self.send_cached_handshake_messages(cached_set_difficulty, cached_notify)
            .await?;

        // Notifications can arrive while the initial cached pair is being sent. Keep the session
        // in `Starting` until every cached notify has been delivered. A difficulty that arrives
        // without a notify remains cached for the next normal notify instead of being sent bare.
        loop {
            let next_messages = self
                .downstream_data
                .with(|data| {
                    let Some(notify) = data.cached_notify.take() else {
                        data.session_state = Sv1SessionState::Ready;
                        return None;
                    };
                    Some((data.cached_set_difficulty.take(), Some(notify)))
                })
                .map_err(TproxyError::shutdown)?;

            let Some((set_difficulty, notify)) = next_messages else {
                break;
            };
            self.send_cached_handshake_messages(set_difficulty, notify)
                .await?;
        }

        Ok(())
    }

    async fn send_cached_handshake_messages(
        &self,
        set_difficulty: Option<json_rpc::Message>,
        notify: Option<json_rpc::Message>,
    ) -> TproxyResult<(), error::Downstream> {
        let mut did_send_difficulty = false;
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
            did_send_difficulty = true;
        }

        if let Some(notify_msg) = notify {
            debug!("Down: Sending cached mining.notify after handshake completion");
            let mut notify_msg = notify_msg;
            if did_send_difficulty {
                if let json_rpc::Message::Notification(notification) = &notify_msg {
                    let mut parsed = server_to_client::Notify::try_from(notification.clone())
                        .expect("mining.notify is always valid here");
                    parsed.clean_jobs = true;
                    notify_msg = parsed.into();
                }
            }
            self.downstream_io
                .downstream_sv1_sender
                .send(notify_msg)
                .await
                .map_err(|error| {
                    error!("Down: Failed to send cached mining.notify to downstream: {error:?}");
                    TproxyError::disconnect(TproxyErrorKind::ChannelErrorSender, self.downstream_id)
                })?;
            self.downstream_data
                .with(|data| data.last_job_received_time = Some(Instant::now()))
                .map_err(TproxyError::shutdown)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::{bounded, unbounded};

    fn notify(job_id: &str) -> Message {
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
                true
            ]
        }))
        .unwrap()
    }

    fn set_difficulty() -> Message {
        serde_json::from_value(serde_json::json!({
            "id": null,
            "method": "mining.set_difficulty",
            "params": [1.0]
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
    async fn notify_arriving_during_handshake_completion_is_flushed() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = bounded(1);
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
        let initial_notify = notify("initial");
        let concurrent_notify = notify("concurrent");
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: true,
                };
                data.cached_notify = Some(initial_notify.clone());
            })
            .unwrap();

        // Fill the outbound queue so completion pauses while sending its initial cached notify.
        downstream_sv1_sender.try_send(set_difficulty()).unwrap();
        let completing_downstream = downstream.clone();
        let completion =
            tokio::spawn(
                async move { completing_downstream.enable_notification_forwarding().await },
            );
        loop {
            if downstream
                .downstream_data
                .with(|data| !data.session_state.is_ready() && data.cached_notify.is_none())
                .unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        sv1_server_message_sender
            .send(concurrent_notify.clone())
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();

        // Unblock completion and verify it drains the notification that raced with it.
        downstream_sv1_receiver.recv().await.unwrap();
        assert_message_eq(
            &downstream_sv1_receiver.recv().await.unwrap(),
            &initial_notify,
        );
        assert_message_eq(
            &downstream_sv1_receiver.recv().await.unwrap(),
            &concurrent_notify,
        );
        completion.await.unwrap().unwrap();
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.session_state, Sv1SessionState::Ready);
                assert!(data.cached_notify.is_none());
            })
            .unwrap();
    }

    #[tokio::test]
    async fn difficulty_arriving_during_completion_waits_for_next_notify() {
        let (downstream_sv1_sender, downstream_sv1_receiver) = bounded(1);
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
        let initial_notify = notify("initial");
        let concurrent_difficulty = set_difficulty();
        downstream
            .downstream_data
            .with(|data| {
                data.session_state = Sv1SessionState::Starting {
                    subscribed: true,
                    authorized: true,
                };
                data.cached_notify = Some(initial_notify.clone());
            })
            .unwrap();

        downstream_sv1_sender.try_send(set_difficulty()).unwrap();
        let completing_downstream = downstream.clone();
        let completion =
            tokio::spawn(
                async move { completing_downstream.enable_notification_forwarding().await },
            );
        loop {
            if downstream
                .downstream_data
                .with(|data| !data.session_state.is_ready() && data.cached_notify.is_none())
                .unwrap()
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        sv1_server_message_sender
            .send(concurrent_difficulty.clone())
            .await
            .unwrap();
        downstream.handle_sv1_server_message().await.unwrap();

        downstream_sv1_receiver.recv().await.unwrap();
        assert_message_eq(
            &downstream_sv1_receiver.recv().await.unwrap(),
            &initial_notify,
        );
        completion.await.unwrap().unwrap();
        assert!(downstream_sv1_receiver.try_recv().is_err());
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(data.session_state, Sv1SessionState::Ready);
                assert_message_eq(
                    data.cached_set_difficulty.as_ref().unwrap(),
                    &concurrent_difficulty,
                );
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
