use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32},
    },
    time::Duration,
};
use stratum_apps::stratum_core::parsers_sv2::{AnyMessageOwned, MiningOwned};

use async_channel::{Receiver, Sender, unbounded};
#[cfg(feature = "monitoring")]
use stratum_apps::monitoring::client::Sv2ClientKind;
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        channels_sv2::server::{
            extended::ExtendedChannel, group::GroupChannel, standard::StandardChannel,
        },
        common_messages_sv2::MESSAGE_TYPE_SETUP_CONNECTION,
        handlers_sv2::{
            HandleCommonMessagesFromClientOwnedAsync, HandleExtensionsFromClientOwnedAsync,
        },
        parsers_sv2::{Tlv, parse_message_frame_with_tlvs},
    },
    sync::{SharedLock, SharedMap},
    task_manager::TaskManager,
    utils::types::{DownstreamId, OutboundFrame, SerializedFrame, Sv2Frame},
};
use tracing::{debug, error, warn};

use crate::{
    error::{self, Action, JDCError, JDCErrorKind, JDCResult, LoopControl},
    io_task::spawn_io_tasks,
};

use stratum_apps::utils::types::ChannelId;

mod common_message_handler;
mod extensions_message_handler;

/// Communication layer for a downstream connection.
///
/// Provides the messaging primitives for interacting with the
/// channel manager and the downstream peer:
/// - `channel_manager_sender`: sends frames to the channel manager.
/// - `channel_manager_receiver`: receives messages from the channel manager.
/// - `downstream_sender`: sends frames to the downstream.
/// - `downstream_receiver`: receives frames from the downstream.
#[derive(Clone)]
pub struct DownstreamIo {
    channel_manager_sender: Sender<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
    channel_manager_receiver: Receiver<(MiningOwned, Option<Vec<Tlv>>)>,
    downstream_sender: Sender<OutboundFrame>,
    downstream_receiver: Receiver<SerializedFrame>,
}

impl DownstreamIo {
    fn close(&self) {
        self.downstream_sender.close();
        self.channel_manager_receiver.close_and_drain();
        self.downstream_receiver.close_and_drain();
    }
}

/// Represents a downstream client connected to this node.
#[derive(Clone)]
pub struct Downstream {
    #[cfg(feature = "monitoring")]
    pub client_kind: SharedLock<Sv2ClientKind>,
    /// Whether the downstream requires standard jobs.
    pub require_std_job: Arc<AtomicBool>,
    pub group_channel: SharedLock<GroupChannel>,
    pub extended_channels: SharedMap<ChannelId, ExtendedChannel>,
    pub standard_channels: SharedMap<ChannelId, StandardChannel>,
    pub channel_id_factory: Arc<AtomicU32>,
    /// Extensions that have been successfully negotiated with this client.
    pub negotiated_extensions: SharedLock<Vec<u16>>,
    /// Extensions that the JDC supports.
    pub supported_extensions: Vec<u16>,
    /// Extensions that the JDC requires.
    pub required_extensions: Vec<u16>,
    downstream_io: DownstreamIo,
    pub downstream_id: DownstreamId,
    /// Per-connection cancellation token (child of the global token).
    /// Cancelled when this downstream's message loop exits, causing
    /// the associated I/O tasks to shut down.
    pub downstream_cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    fn handle_error_action(
        &self,
        context: &str,
        e: &JDCError<error::Downstream>,
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

    /// Creates a new [`Downstream`] instance and spawns the necessary I/O tasks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_id: DownstreamId,
        channel_id_factory: AtomicU32,
        group_channel: GroupChannel,
        channel_manager_sender: Sender<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
        channel_manager_receiver: Receiver<(MiningOwned, Option<Vec<Tlv>>)>,
        noise_stream: NoiseTcpStream,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> Self {
        let (noise_stream_reader, noise_stream_writer) = noise_stream.into_split();
        let (inbound_tx, inbound_rx) = unbounded::<SerializedFrame>();
        let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

        // Create a per-connection child token so we can cancel this
        // connection's I/O tasks independently of the global shutdown.
        let downstream_cancellation_token = cancellation_token.child_token();
        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            downstream_cancellation_token.clone(),
            Some(fallback_coordinator.clone()),
        );

        let downstream_io = DownstreamIo {
            channel_manager_receiver,
            channel_manager_sender,
            downstream_sender: outbound_tx,
            downstream_receiver: inbound_rx,
        };

        Downstream {
            downstream_io,
            #[cfg(feature = "monitoring")]
            client_kind: SharedLock::new(Sv2ClientKind::Unknown),
            require_std_job: Arc::new(AtomicBool::new(false)),
            group_channel: SharedLock::new(group_channel),
            extended_channels: SharedMap::new(),
            standard_channels: SharedMap::new(),
            channel_id_factory: Arc::new(channel_id_factory),
            negotiated_extensions: SharedLock::new(Vec::new()),
            supported_extensions,
            required_extensions,
            downstream_id,
            downstream_cancellation_token,
        }
    }

    /// Starts the downstream loop.
    ///
    /// Responsibilities:
    /// - Performs the initial `SetupConnection` handshake with the downstream.
    /// - Forwards mining-related messages to the channel manager.
    /// - Forwards channel manager messages back to the downstream peer.
    pub async fn start(
        mut self,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        on_disconnect: impl FnOnce(DownstreamId) + Send + 'static,
    ) {
        let fallback_handler = fallback_coordinator.register();
        let fallback_token = fallback_coordinator.token();
        // Setup initial connection
        if let Err(e) = self.setup_connection_with_downstream().await {
            error!(?e, "Failed to set up downstream connection");

            // sleep to make sure SetupConnectionError is sent
            // before we break the TCP connection
            tokio::time::sleep(Duration::from_secs(1)).await;

            _ = self.handle_error_action(
                "Downstream::setup_connection_with_downstream",
                &e,
                &cancellation_token,
                &fallback_token,
            );
            on_disconnect(self.downstream_id);
            self.downstream_io.close();
            fallback_handler.done();
            return;
        }

        task_manager.spawn(async move {
            loop {
                let self_clone_1 = self.clone();
                let downstream_id = self_clone_1.downstream_id;
                let self_clone_2 = self.clone();
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        debug!("Downstream {downstream_id}: received shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        debug!("Downstream {downstream_id}: received fallback signal");
                        break;
                    }
                    res = self_clone_1.handle_downstream_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling downstream message for {downstream_id}");
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
                    res = self_clone_2.handle_channel_manager_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling channel manager message for {downstream_id}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_channel_manager_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }

                }
            }
            if !cancellation_token.is_cancelled() && !fallback_token.is_cancelled() {
                // Only remove downstream when system is not going through shutdown
                // or fallback. As in those cases we initialize new set of subsystems
                // and free old allocated memory.
                on_disconnect(self.downstream_id);
            }
            self.downstream_cancellation_token.cancel();
            self.downstream_io.close();
            warn!("Downstream: unified message loop exited.");
            fallback_handler.done();
        });
    }

    // Performs the initial handshake with a downstream peer.
    async fn setup_connection_with_downstream(&mut self) -> JDCResult<(), error::Downstream> {
        let mut frame = self
            .downstream_io
            .downstream_receiver
            .recv()
            .await
            .map_err(|error| JDCError::disconnect(error, self.downstream_id))?;
        let header = frame.header();
        if header.ext_type() == 0 && header.msg_type() == MESSAGE_TYPE_SETUP_CONNECTION {
            self.handle_common_message_frame_from_client(None, header, frame.payload())
                .await?;
            return Ok(());
        }
        Err(JDCError::disconnect(
            JDCErrorKind::UnexpectedMessage(header.ext_type(), header.msg_type()),
            self.downstream_id,
        ))
    }

    // Handles messages sent from the channel manager to this downstream.
    async fn handle_channel_manager_message(self) -> JDCResult<(), error::Downstream> {
        let (message, _tlv_fields) = match self.downstream_io.channel_manager_receiver.recv().await
        {
            Ok(msg) => msg,
            Err(e) => {
                warn!(
                    ?e,
                    "Channel manager receiver closed - disconnecting downstream"
                );
                return Err(JDCError::disconnect(
                    JDCErrorKind::ChannelErrorReceiver(e),
                    self.downstream_id,
                ));
            }
        };

        let message = AnyMessageOwned::Mining(message);
        let sv2_frame: Sv2Frame = message.try_into().map_err(JDCError::shutdown)?;

        self.downstream_io
            .downstream_sender
            .send(sv2_frame.into())
            .await
            .map_err(|e| {
                error!(?e, "Downstream send failed");
                JDCError::disconnect(JDCErrorKind::ChannelErrorSender, self.downstream_id)
            })?;

        Ok(())
    }

    // Handles incoming messages from the downstream peer.
    async fn handle_downstream_message(mut self) -> JDCResult<(), error::Downstream> {
        let mut sv2_frame = self
            .downstream_io
            .downstream_receiver
            .recv()
            .await
            .map_err(|error| JDCError::disconnect(error, self.downstream_id))?;
        let header = sv2_frame.header();
        let payload = sv2_frame.payload();
        let negotiated_extensions = self
            .negotiated_extensions
            .get()
            .map_err(JDCError::shutdown)?;
        let (any_message, tlv_fields) =
            parse_message_frame_with_tlvs(header, payload, &negotiated_extensions)
                .map_err(|error| JDCError::disconnect(error, self.downstream_id))?;
        match any_message.into_owned() {
            AnyMessageOwned::Mining(message) => {
                self.downstream_io
                    .channel_manager_sender
                    .send((self.downstream_id, message, tlv_fields))
                    .await
                    .map_err(|e| {
                        error!(?e, "Failed to send mining message to channel manager.");
                        JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            AnyMessageOwned::Extensions(message) => {
                self.handle_extensions_message_from_client(None, message, tlv_fields.as_deref())
                    .await?;
            }
            _ => {
                warn!(
                    "Received unsupported message type from downstream: {}",
                    header.msg_type()
                );
                return Ok(());
            }
        }
        Ok(())
    }
}
