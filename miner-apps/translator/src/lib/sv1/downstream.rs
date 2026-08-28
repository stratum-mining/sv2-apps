use crate::{
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
    utils::SubmitShareWithChannelId,
};
use async_channel::{Receiver, Sender};
#[cfg(feature = "monitoring")]
use std::net::IpAddr;
use std::{collections::HashMap, future::Future, sync::Arc, time::Instant};
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    stratum_core::{
        bitcoin::Target,
        sv1_api::{
            json_rpc, server_to_client,
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
#[derive(Clone, Debug)]
pub(super) enum Sv1ServerEvent {
    Notify(Arc<server_to_client::Notify>),
    SetDifficulty(json_rpc::Message),
    SetExtranonce {
        message: server_to_client::SetExtranonce,
        /// Decided by the server in the same task that builds and records subscribe responses.
        /// Otherwise the prefix is already included in the upcoming subscribe response.
        notify_miner: bool,
    },
    SetupComplete,
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
    pub cached_notify: Option<Arc<server_to_client::Notify>>,
    /// Prefix notification paired with the next deliverable job. Capability is checked only
    /// when that job is sent, allowing the miner to announce support during setup.
    pub(super) cached_set_extranonce: Option<server_to_client::SetExtranonce>,
    pub(super) session_state: Sv1SessionState,
    /// Extranonce1 advertised for each job that reached this miner. This preserves validation of
    /// old-job shares across `mining.set_extranonce` transitions.
    pub(super) job_extranonces: HashMap<String, Extranonce>,
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
            cached_set_extranonce: None,
            session_state: Sv1SessionState::default(),
            job_extranonces: HashMap::new(),
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

    fn record_job_extranonce(&mut self, notify: &server_to_client::Notify) {
        if notify.clean_jobs {
            self.job_extranonces.clear();
        }
        self.job_extranonces
            .insert(notify.job_id.clone(), self.extranonce1.clone());
        if self.pending_set_extranonce_notifications == 0 {
            self.keepalive_timer_anchor = Some(Instant::now());
        }
    }

    pub(super) fn extranonce_for_job(&self, job_id: &str) -> Option<Extranonce> {
        self.job_extranonces.get(job_id).cloned()
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

    /// Processes typed notifications in per-downstream FIFO order.
    ///
    /// Jobs wait for subscribe and authorize responses. Difficulty and extranonce changes are
    /// sent with the next deliverable job; prefix support is required only at that boundary.
    pub(super) async fn handle_sv1_server_message(&self) -> TproxyResult<(), error::Downstream> {
        let event = self
            .downstream_io
            .sv1_server_receiver
            .recv()
            .await
            .map_err(|error| TproxyError::disconnect(error, self.downstream_id))?;
        match event {
            Sv1ServerEvent::SetupComplete => self.enable_notification_forwarding().await?,
            Sv1ServerEvent::SetDifficulty(message) => {
                self.downstream_data
                    .with(|data| data.cached_set_difficulty = Some(message))
                    .map_err(TproxyError::shutdown)?;
            }
            Sv1ServerEvent::Notify(notify) => {
                let ready = self
                    .downstream_data
                    .with(|data| {
                        if data.session_state.is_ready() {
                            true
                        } else {
                            data.cached_notify = Some(notify.clone());
                            false
                        }
                    })
                    .map_err(TproxyError::shutdown)?;
                if ready {
                    self.send_job(notify).await?;
                }
            }
            Sv1ServerEvent::SetExtranonce {
                message,
                notify_miner,
            } => {
                self.downstream_data
                    .with(|data| {
                        data.pending_set_extranonce_notifications =
                            data.pending_set_extranonce_notifications.saturating_sub(1);
                        // Pre-subscribe updates were applied by the server before response
                        // construction. Reapplying this event could overwrite a newer prefix
                        // already included in that response.
                        if notify_miner {
                            data.extranonce1 = message.extra_nonce1.clone();
                            data.extranonce2_len = message.extra_nonce2_size;
                            data.cached_set_extranonce = Some(message);
                        }
                        // A cached job predates this transition and has not reached the miner.
                        data.cached_notify = None;
                    })
                    .map_err(TproxyError::shutdown)?;
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

    /// Releases the latest cached job once both setup responses have been queued.
    /// Capability announcements may also arrive after setup, provided they are processed before
    /// the first job delivery requiring a changed extranonce.
    async fn enable_notification_forwarding(&self) -> TproxyResult<(), error::Downstream> {
        let (enable, notify) = self
            .downstream_data
            .with(|data| {
                if data.session_state.is_ready() || !data.session_state.setup_complete() {
                    return (false, None);
                }
                (true, data.cached_notify.take())
            })
            .map_err(TproxyError::shutdown)?;
        if !enable {
            return Ok(());
        }
        if let Some(notify) = notify {
            self.send_job(notify).await?;
        }
        self.downstream_data
            .with(|data| data.session_state = Sv1SessionState::Ready)
            .map_err(TproxyError::shutdown)?;
        Ok(())
    }

    /// Sends pending difficulty, then the required extranonce immediately before its job.
    /// A miner that has not announced prefix-update support is disconnected without advertising
    /// work it would hash with the wrong extranonce. No capability timeout is introduced.
    async fn send_job(
        &self,
        mut notify: Arc<server_to_client::Notify>,
    ) -> TproxyResult<(), error::Downstream> {
        let messages = self
            .downstream_data
            .with(|data| {
                if data.cached_set_extranonce.is_some() && !data.supports_set_extranonce {
                    return None;
                }
                let difficulty = data.cached_set_difficulty.take();
                if difficulty.is_some() {
                    Arc::make_mut(&mut notify).clean_jobs = true;
                    if let Some(target) = data.pending_target.take() {
                        data.target = target;
                    }
                    if let Some(hashrate) = data.pending_hashrate.take() {
                        data.hashrate = Some(hashrate);
                    }
                }
                let extranonce = data.cached_set_extranonce.take();
                data.record_job_extranonce(&notify);
                Some((difficulty, extranonce))
            })
            .map_err(TproxyError::shutdown)?;
        let Some((difficulty, extranonce)) = messages else {
            warn!(
                downstream_id = self.downstream_id,
                "Disconnecting SV1 miner before a job requiring unannounced mining.set_extranonce support"
            );
            self.disconnect();
            return Ok(());
        };
        for message in [
            difficulty,
            extranonce.map(json_rpc::Message::from),
            Some(json_rpc::Message::from((*notify).clone())),
        ]
        .into_iter()
        .flatten()
        {
            self.downstream_io
                .downstream_sv1_sender
                .send(message)
                .await
                .map_err(|error| {
                    error!("Down: Failed to send mining job messages: {error:?}");
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
    use stratum_apps::stratum_core::sv1_api::json_rpc::Message;

    // JSON fixtures exercise the same typed events as production, without parsing internal messages
    // on the per-miner job delivery path.
    impl From<json_rpc::Message> for Sv1ServerEvent {
        fn from(message: json_rpc::Message) -> Self {
            let json_rpc::Message::Notification(notification) = &message else {
                panic!("expected a notification fixture");
            };
            match notification.method.as_str() {
                "mining.notify" => Self::Notify(Arc::new(
                    server_to_client::Notify::try_from(notification.clone()).unwrap(),
                )),
                "mining.set_difficulty" => Self::SetDifficulty(message),
                "mining.set_extranonce" => Self::SetExtranonce {
                    message: server_to_client::SetExtranonce::try_from(notification.clone())
                        .unwrap(),
                    notify_miner: true,
                },
                _ => panic!("unexpected notification fixture"),
            }
        }
    }

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
            .send(Sv1ServerEvent::from(queued_notify.clone()))
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
            .send(Sv1ServerEvent::from(queued_difficulty.clone()))
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

        let next_notify = notify("next");
        sv1_server_message_sender
            .send(Sv1ServerEvent::from(next_notify.clone()))
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
        assert!(forwarded_notify.clean_jobs);
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
            .send(Sv1ServerEvent::from(old_notify.clone()))
            .await
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::SetupComplete)
            .await
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::from(Message::from(
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
        assert!(downstream_sv1_receiver.try_recv().is_err());
        downstream
            .downstream_data
            .with(|data| {
                assert_eq!(
                    data.extranonce_for_job("old").as_ref(),
                    Some(&old_extranonce)
                );
                assert_eq!(data.extranonce1, new_extranonce);
                assert!(data.cached_set_extranonce.is_some());
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
            .send(Sv1ServerEvent::from(old_notify.clone()))
            .await
            .unwrap();
        downstream
            .downstream_data
            .with(|data| {
                data.pending_set_extranonce_notifications = 1;
            })
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::from(Message::from(
                server_to_client::SetExtranonce {
                    extra_nonce1: new_extranonce.clone(),
                    extra_nonce2_size: 4,
                },
            )))
            .await
            .unwrap();

        downstream.handle_sv1_server_message().await.unwrap();
        downstream.handle_sv1_server_message().await.unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::from(new_notify.clone()))
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
                    data.extranonce_for_job("old").as_ref(),
                    Some(&old_extranonce)
                );
                assert_eq!(
                    data.extranonce_for_job("new").as_ref(),
                    Some(&new_extranonce)
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
                .send(Sv1ServerEvent::from(message))
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
                    data.extranonce_for_job("first").as_ref(),
                    Some(&first_extranonce)
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
                    data.extranonce_for_job("second").as_ref(),
                    Some(&second_extranonce)
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn presubscribe_event_does_not_overwrite_a_newer_response_prefix() {
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
                let Sv1ServerEvent::Notify(notify) = Sv1ServerEvent::from(notify("old")) else {
                    panic!("expected notify fixture");
                };
                data.cached_notify = Some(notify);
                data.pending_set_extranonce_notifications = 1;
                // The server has already applied the latest prefix for response construction.
                data.extranonce1 = new_extranonce.clone();
            })
            .unwrap();
        sv1_server_message_sender
            .send(Sv1ServerEvent::SetExtranonce {
                message: server_to_client::SetExtranonce {
                    extra_nonce1: vec![9; 4].try_into().unwrap(),
                    extra_nonce2_size: 4,
                },
                notify_miner: false,
            })
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
                assert!(data.cached_set_extranonce.is_none());
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
