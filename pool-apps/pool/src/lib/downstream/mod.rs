use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32},
};
use stratum_apps::stratum_core::parsers_sv2::{AnyMessageOwned, MiningOwned};

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
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
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{ChannelId, DownstreamId, OutboundFrame, SerializedFrame, Sv2Frame},
    },
};
use tracing::{debug, error, warn};

use crate::{
    error::{self, Action, LoopControl, PoolError, PoolErrorKind, PoolResult},
    io_task::spawn_io_tasks,
    utils::PayoutMode,
};

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
    pub group_channel: SharedLock<GroupChannel>,
    pub extended_channels: SharedMap<ChannelId, ExtendedChannel>,
    pub standard_channels: SharedMap<ChannelId, StandardChannel>,
    pub channel_id_factory: Arc<AtomicU32>,
    /// Extensions that have been successfully negotiated with this client
    pub negotiated_extensions: SharedLock<Vec<u16>>,
    /// Payout mode derived from user_identity (None until channel is opened)
    pub payout_mode: SharedLock<Option<PayoutMode>>,
    downstream_io: DownstreamIo,
    pub downstream_id: usize,
    pub requires_standard_jobs: Arc<AtomicBool>,
    pub requires_custom_work: Arc<AtomicBool>,
    /// Extensions that the pool supports
    pub supported_extensions: Vec<u16>,
    /// Extensions that the pool requires
    pub required_extensions: Vec<u16>,
    /// Per-connection cancellation token (child of the global token).
    /// Cancelled when this downstream's message loop exits, causing
    /// the associated I/O tasks to shut down.
    downstream_connection_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    fn handle_error_action(
        &self,
        context: &str,
        e: &PoolError<error::Downstream>,
        cancellation_token: &CancellationToken,
    ) -> LoopControl {
        if cancellation_token.is_cancelled() {
            debug!(
                error_kind = ?e.kind,
                "{context} returned an error after shutdown was requested"
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
                self.downstream_connection_token.cancel();
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
        task_manager: Arc<TaskManager>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> Self {
        let (noise_stream_reader, noise_stream_writer) = noise_stream.into_split();
        let (inbound_tx, inbound_rx) = unbounded::<SerializedFrame>();
        let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

        // Create a per-connection child token so we can cancel this
        // connection's I/O tasks independently of the global shutdown.
        let downstream_connection_token = cancellation_token.child_token();
        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            downstream_connection_token.clone(),
        );

        let downstream_io = DownstreamIo {
            channel_manager_receiver,
            channel_manager_sender,
            downstream_sender: outbound_tx,
            downstream_receiver: inbound_rx,
        };

        Downstream {
            downstream_io,
            group_channel: SharedLock::new(group_channel),
            extended_channels: SharedMap::new(),
            standard_channels: SharedMap::new(),
            channel_id_factory: Arc::new(channel_id_factory),
            negotiated_extensions: SharedLock::new(Vec::new()),
            payout_mode: SharedLock::new(None),
            downstream_id,
            requires_standard_jobs: Arc::new(AtomicBool::new(false)),
            requires_custom_work: Arc::new(AtomicBool::new(false)),
            supported_extensions,
            required_extensions,
            downstream_connection_token,
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
        task_manager: Arc<TaskManager>,
        on_disconnect: impl FnOnce(DownstreamId) + Send + 'static,
    ) {
        // Setup initial connection
        if let Err(e) = self.setup_connection_with_downstream().await {
            error!(?e, "Failed to set up downstream connection");

            // sleep to make sure SetupConnectionError is sent
            // before we break the TCP connection
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            _ = self.handle_error_action(
                "Downstream::setup_connection_with_downstream",
                &e,
                &cancellation_token,
            );
            on_disconnect(self.downstream_id);
            self.downstream_io.close();
            return;
        }

        task_manager.spawn(async move {
            loop {
                let mut self_clone_1 = self.clone();
                let downstream_id = self_clone_1.downstream_id;
                let self_clone_2 = self.clone();
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        debug!("Downstream {downstream_id}: received shutdown signal");
                        break;
                    }
                    res = self_clone_1.handle_downstream_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling downstream message for {downstream_id}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_downstream_message",
                                &e,
                                &cancellation_token,
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
                            ) {
                                break;
                            }
                        }
                    }
                }
            }
            self.downstream_connection_token.cancel();
            self.downstream_io.close();
            on_disconnect(self.downstream_id);
            warn!("Downstream: unified message loop exited.");
        });
    }

    // Performs the initial handshake with a downstream peer.
    async fn setup_connection_with_downstream(&mut self) -> PoolResult<(), error::Downstream> {
        let mut frame = self
            .downstream_io
            .downstream_receiver
            .recv()
            .await
            .map_err(|error| PoolError::disconnect(error, self.downstream_id))?;
        let header = frame.header();
        // The first ever message received on a new downstream connection
        // should always be a setup connection message.
        if header.ext_type() == 0 && header.msg_type() == MESSAGE_TYPE_SETUP_CONNECTION {
            self.handle_common_message_frame_from_client(
                Some(self.downstream_id),
                header,
                frame.payload(),
            )
            .await?;
            return Ok(());
        }
        Err(PoolError::disconnect(
            PoolErrorKind::UnexpectedMessage(
                header.ext_type_without_channel_msg(),
                header.msg_type(),
            ),
            self.downstream_id,
        ))
    }

    // Handles messages sent from the channel manager to this downstream.
    async fn handle_channel_manager_message(self) -> PoolResult<(), error::Downstream> {
        let (msg, _tlv_fields) = match self.downstream_io.channel_manager_receiver.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                warn!(
                    ?e,
                    "Channel manager receiver closed - disconnecting downstream"
                );
                return Err(PoolError::disconnect(
                    PoolErrorKind::ChannelRecv(e),
                    self.downstream_id,
                ));
            }
        };

        let message = AnyMessageOwned::Mining(msg);
        let std_frame: Sv2Frame = message.try_into().map_err(PoolError::shutdown)?;

        self.downstream_io
            .downstream_sender
            .send(std_frame.into())
            .await
            .map_err(|e| {
                error!(?e, "Downstream send failed");
                PoolError::disconnect(PoolErrorKind::ChannelErrorSender, self.downstream_id)
            })?;

        Ok(())
    }

    // Handles incoming messages from the downstream peer.
    async fn handle_downstream_message(&mut self) -> PoolResult<(), error::Downstream> {
        let mut sv2_frame = self
            .downstream_io
            .downstream_receiver
            .recv()
            .await
            .map_err(|error| PoolError::disconnect(error, self.downstream_id))?;
        let header = sv2_frame.header();

        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::Mining => {
                debug!("Received mining SV2 frame from downstream.");
                let negotiated_extensions = self
                    .negotiated_extensions
                    .get()
                    .map_err(PoolError::shutdown)?;
                let (any_message, tlv_fields) = parse_message_frame_with_tlvs(
                    header,
                    sv2_frame.payload(),
                    &negotiated_extensions,
                )
                .map_err(|error| PoolError::disconnect(error, self.downstream_id))?;
                let mining_message = match any_message.into_owned() {
                    AnyMessageOwned::Mining(msg) => msg,
                    _ => {
                        error!("Expected Mining message but got different type");
                        return Err(PoolError::disconnect(
                            PoolErrorKind::UnexpectedMessage(
                                header.ext_type_without_channel_msg(),
                                header.msg_type(),
                            ),
                            self.downstream_id,
                        ));
                    }
                };
                self.downstream_io
                    .channel_manager_sender
                    .send((self.downstream_id, mining_message, tlv_fields))
                    .await
                    .map_err(|e| {
                        error!(?e, "Failed to send mining message to channel manager.");
                        PoolError::shutdown(e)
                    })?;
            }
            MessageType::Extensions => {
                self.handle_extensions_message_frame_from_client(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::Common
            | MessageType::JobDeclaration
            | MessageType::TemplateDistribution => {
                warn!(
                    ext_type = ?header.ext_type(),
                    msg_type = ?header.msg_type(),
                    "Received unexpected message from downstream."
                );
            }
            MessageType::Unknown => {
                warn!(
                    ext_type = ?header.ext_type(),
                    msg_type = ?header.msg_type(),
                    "Received unknown message from downstream."
                );
            }
        }

        Ok(())
    }
}
