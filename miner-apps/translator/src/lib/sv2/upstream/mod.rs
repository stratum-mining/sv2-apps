pub mod common_message_handler;

use crate::{
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
    io_task::spawn_io_tasks,
    utils::UpstreamEntry,
};
use async_channel::{Receiver, Sender, unbounded};
use std::{net::SocketAddr, sync::Arc};
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    network_helpers::{self, TCP_CONNECT_TIMEOUT, connect_with_noise, resolve_host},
    stratum_core::{
        binary_sv2::Seq064KOwned,
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol,
            SetupConnectionOwned,
        },
        extensions_sv2::RequestExtensionsOwned,
        handlers_sv2::HandleCommonMessagesFromServerOwnedAsync,
        parsers_sv2::AnyMessageOwned,
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{Message, OutboundFrame, SerializedFrame, Sv2Frame},
    },
};

use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
struct UpstreamIo {
    /// Receiver for the SV2 Upstream role
    upstream_receiver: Receiver<SerializedFrame>,
    /// Sender for the SV2 Upstream role
    upstream_sender: Sender<OutboundFrame>,
    /// Sender for the ChannelManager to send SV2 frames
    channel_manager_sender: Sender<SerializedFrame>,
    /// Receiver for the ChannelManager to receive SV2 frames
    channel_manager_receiver: Receiver<OutboundFrame>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl UpstreamIo {
    fn new(
        upstream_receiver: Receiver<SerializedFrame>,
        upstream_sender: Sender<OutboundFrame>,
        channel_manager_sender: Sender<SerializedFrame>,
        channel_manager_receiver: Receiver<OutboundFrame>,
    ) -> Self {
        Self {
            upstream_receiver,
            upstream_sender,
            channel_manager_sender,
            channel_manager_receiver,
        }
    }

    fn close(&self) {
        debug!("Closing all upstream channels");
        self.upstream_sender.close();
        self.channel_manager_sender.close();
        self.upstream_receiver.close_and_drain();
        self.channel_manager_receiver.close_and_drain();
    }
}

/// Manages the upstream SV2 connection to a mining pool or proxy.
///
/// This struct handles the SV2 protocol communication with upstream servers,
/// including:
/// - Connection establishment with multiple upstream fallbacks
/// - SV2 handshake and setup procedures
/// - Message routing between channel manager and upstream
/// - Connection monitoring and error handling
/// - Graceful shutdown coordination
///
/// The upstream connection supports automatic failover between multiple
/// configured upstream servers and implements retry logic for connection
/// establishment.
#[derive(Debug, Clone)]
pub struct Upstream {
    upstream_io: UpstreamIo,
    /// Extensions that the translator requires (must be supported by server)
    required_extensions: Vec<u16>,
    address: SocketAddr,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Upstream {
    fn handle_error_action(
        context: &str,
        e: &TproxyError<error::Upstream>,
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
                    "{context} requested shutdown"
                );
                cancellation_token.cancel();
                LoopControl::Break
            }
            other => {
                warn!(
                    action = ?other,
                    error_kind = ?e.kind,
                    "{context} returned an unhandled action"
                );
                LoopControl::Continue
            }
        }
    }

    /// Creates a new upstream connection by attempting to connect to configured servers.
    ///
    /// This method tries to establish a connection to one of the provided upstream
    /// servers, implementing retry logic and fallback behavior. It will attempt
    /// to connect to each server multiple times before giving up.
    ///
    /// # Arguments
    /// * `upstreams` - A single `UpstreamEntry` representing the upstream candidate currently being
    ///   attempted. The `tried_or_flagged` is set once the upstream has either been connected to
    ///   successfully or marked as malicious. Because `new` is only called from
    ///   `try_initialize_upstream`, we can treat this flag as the definitive state for that
    ///   upstream.
    /// * `channel_manager_sender` - Channel to send messages to the channel manager
    /// * `channel_manager_receiver` - Channel to receive messages from the channel manager
    /// * `cancellation_token` - Global application cancellation token
    /// * `fallback_coordinator` - Coordinator for upstream fallback
    ///
    /// # Returns
    /// * `Ok(Upstream)` - Successfully connected to an upstream server
    /// * `Err(TproxyError)` - Failed to connect to any upstream server
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        upstream: &UpstreamEntry,
        channel_manager_sender: Sender<SerializedFrame>,
        channel_manager_receiver: Receiver<OutboundFrame>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        required_extensions: Vec<u16>,
    ) -> TproxyResult<Self, error::Upstream> {
        info!(
            "Trying to connect to upstream at {}:{}",
            upstream.host, upstream.port
        );

        if cancellation_token.is_cancelled() {
            info!("Shutdown signal received during upstream connection attempt. Aborting.");
            return Err(TproxyError::shutdown(
                TproxyErrorKind::CouldNotInitiateSystem,
            ));
        }

        let resolved_addr = resolve_host(&upstream.host, upstream.port)
            .await
            .map_err(|e| {
                error!(
                    "Failed to resolve upstream address {}:{}: {e}",
                    upstream.host, upstream.port
                );
                TproxyError::fallback(TproxyErrorKind::NetworkHelpersError(e.into()))
            })?;

        match tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(resolved_addr))
            .await
            .map_err(TproxyError::fallback)?
        {
            Ok(socket) => {
                info!("Connected to upstream at {}", resolved_addr);

                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        info!("Shutdown received during handshake, dropping connection");
                        Err(TproxyError::shutdown(TproxyErrorKind::CouldNotInitiateSystem))
                    }
                    result = connect_with_noise(socket, Some(upstream.authority_pubkey)) => {
                        match result {
                            Ok(stream) => {
                                let (reader, writer) = stream.into_split();

                                let (outbound_tx, outbound_rx) = unbounded();
                                let (inbound_tx, inbound_rx) = unbounded();

                                spawn_io_tasks(
                                    task_manager,
                                    reader,
                                    writer,
                                    outbound_rx,
                                    inbound_tx,
                                    cancellation_token.clone(),
                                    fallback_coordinator.clone(),
                                );

                                let upstream_io = UpstreamIo::new(
                                    inbound_rx,
                                    outbound_tx,
                                    channel_manager_sender,
                                    channel_manager_receiver,
                                );
                                debug!(
                                    "Successfully initialized upstream channel with {}",
                                    resolved_addr
                                );

                                Ok(Self {
                                    upstream_io,
                                    required_extensions: required_extensions.clone(),
                                    address: resolved_addr,
                                })
                            }
                            Err(network_helpers::Error::InvalidKey) => {
                                Err(TproxyError::fallback(TproxyErrorKind::InvalidKey))
                            }
                            Err(e) => {
                                error!(
                                    "Failed Noise handshake with {}: {e}.",
                                    resolved_addr
                                );
                                Err(TproxyError::fallback(
                                    TproxyErrorKind::NetworkHelpersError(e),
                                ))
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to {}: {e}.", resolved_addr);
                Err(TproxyError::fallback(e))
            }
        }
    }

    /// Starts the upstream connection and begins message processing.
    ///
    /// This method:
    /// - Completes the SV2 handshake with the upstream server
    /// - Spawns the main message processing task
    /// - Handles graceful shutdown coordination
    ///
    /// The method will first attempt to complete the SV2 setup connection
    /// handshake. If successful, it spawns a task to handle bidirectional
    /// message flow between the channel manager and upstream server.
    pub async fn start(
        mut self,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) -> TproxyResult<(), error::Upstream> {
        let fallback_token: CancellationToken = fallback_coordinator.token();

        // wait for connection setup or cancellation signal
        tokio::select! {
            biased;

            _ = cancellation_token.cancelled() => {
                info!("Upstream: shutdown signal received during connection setup.");
                self.upstream_io.close();
                return Ok(());
            }
            _ = fallback_token.cancelled() => {
                info!("Upstream: fallback signal received during connection setup.");
                self.upstream_io.close();
                return Ok(());
            }
            result = self.setup_connection() => {
                if let Err(e) = result {
                    error!("Upstream: failed to set up SV2 connection: {e:?}");
                    return Err(e);
                }
            }
        }

        self.run_upstream_task(cancellation_token, fallback_coordinator, task_manager)?;

        Ok(())
    }

    /// Performs the SV2 handshake setup with the upstream server.
    ///
    /// This method handles the initial SV2 protocol handshake by:
    /// - Creating and sending a SetupConnection message
    /// - Waiting for the handshake response
    /// - Validating and processing the response
    ///
    /// The handshake establishes the protocol version, capabilities, and
    /// other connection parameters needed for SV2 communication.
    async fn setup_connection(&mut self) -> TproxyResult<(), error::Upstream> {
        debug!("Upstream: initiating SV2 handshake...");
        // Build SetupConnection message
        let setup_conn_msg = Self::get_setup_connection_message(2, 2, &self.address, false)
            .map_err(TproxyError::shutdown)?;
        let sv2_frame: Sv2Frame =
            Message::Common(setup_conn_msg.into())
                .try_into()
                .map_err(|error| {
                    error!("Failed to serialize SetupConnection message: {error:?}");
                    TproxyError::shutdown(error)
                })?;

        // Send SetupConnection message to upstream
        self.upstream_io
            .upstream_sender
            .send(sv2_frame.into())
            .await
            .map_err(|e| {
                error!("Failed to send SetupConnection to upstream: {:?}", e);
                TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
            })?;

        let mut incoming: SerializedFrame = match self.upstream_io.upstream_receiver.recv().await {
            Ok(frame) => {
                debug!("Received handshake response from upstream.");
                frame
            }
            Err(e) => {
                error!("Failed to receive handshake response from upstream: {}", e);
                return Err(TproxyError::fallback(e));
            }
        };

        let header = incoming.header();

        if header.ext_type() != 0
            || !matches!(
                header.msg_type(),
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS | MESSAGE_TYPE_SETUP_CONNECTION_ERROR
            )
        {
            return Err(TproxyError::fallback(TproxyErrorKind::UnexpectedMessage(
                header.ext_type(),
                header.msg_type(),
            )));
        }

        let payload = incoming.payload();

        self.handle_common_message_frame_from_server(None, header, payload)
            .await?;
        debug!("Upstream: handshake completed successfully.");

        // Send RequestExtensions message if there are any required extensions
        if !self.required_extensions.is_empty() {
            let require_extensions = RequestExtensionsOwned {
                request_id: 1,
                requested_extensions: Seq064KOwned::new(self.required_extensions.clone())
                    .map_err(TproxyError::shutdown)?,
            };

            info!(
                "Sending RequestExtensions message to upstream: {:?}",
                require_extensions
            );

            let sv2_frame: Sv2Frame = AnyMessageOwned::Extensions(require_extensions.into())
                .try_into()
                .map_err(TproxyError::shutdown)?;

            self.upstream_io
                .upstream_sender
                .send(sv2_frame.into())
                .await
                .map_err(|e| {
                    error!("Failed to send message to upstream: {:?}", e);
                    TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
                })?;
        }
        Ok(())
    }

    /// Handles one SV2 frame received from upstream.
    async fn handle_upstream_message(mut self) -> TproxyResult<(), error::Upstream> {
        let mut sv2_frame = self
            .upstream_io
            .upstream_receiver
            .recv()
            .await
            .map_err(TproxyError::fallback)?;

        debug!("Upstream: received frame.");
        let header = sv2_frame.header();

        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::Common => {
                info!(
                    extension_type = header.ext_type(),
                    message_type = header.msg_type(),
                    "Handling common message from Upstream."
                );
                self.handle_common_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::Mining | MessageType::Extensions => {
                self.upstream_io
                    .channel_manager_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!("Failed to send mining message to channel manager: {:?}", e);
                        TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                    })?;
            }
            _ => {
                warn!(
                    extension_type = header.ext_type(),
                    message_type = header.msg_type(),
                    "Received unsupported message type from upstream."
                );
                return Err(TproxyError::fallback(TproxyErrorKind::UnexpectedMessage(
                    header.ext_type(),
                    header.msg_type(),
                )));
            }
        }

        Ok(())
    }

    /// Handles one SV2 frame received from the channel manager and forwards it upstream.
    async fn handle_channel_manager_message(&self) -> TproxyResult<(), error::Upstream> {
        let sv2_frame = self
            .upstream_io
            .channel_manager_receiver
            .recv()
            .await
            .map_err(TproxyError::shutdown)?;

        debug!(
            "Upstream: sending sv2 frame from channel manager: {:?}",
            sv2_frame
        );
        self.upstream_io
            .upstream_sender
            .send(sv2_frame)
            .await
            .map_err(|e| {
                error!("Upstream: failed to send sv2 frame: {e:?}");
                TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
            })?;

        Ok(())
    }

    /// Spawns a unified task to handle upstream message I/O and shutdown logic.
    #[allow(clippy::result_large_err)]
    fn run_upstream_task(
        self,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) -> TproxyResult<(), error::Upstream> {
        task_manager.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();

            loop {
                tokio::select! {
                    biased;

                    // Handle app shutdown signal
                    _ = cancellation_token.cancelled() => {
                        info!("Upstream: received shutdown signal. Exiting loop.");
                        break;
                    }

                    // Handle fallback trigger
                    _ = fallback_token.cancelled() => {
                        info!("Upstream: fallback triggered");
                        break;
                    }

                    res = self.clone().handle_upstream_message() => {
                        if let Err(e) = res {
                            if let LoopControl::Break = Self::handle_error_action(
                                "Upstream::handle_upstream_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = self.handle_channel_manager_message() => {
                        if let Err(e) = res {
                            if let LoopControl::Break = Self::handle_error_action(
                                "Upstream::handle_channel_manager_message",
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

            self.upstream_io.close();
            warn!("Upstream: task shutting down cleanly.");

            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });

        Ok(())
    }

    /// Constructs the `SetupConnection` message.
    #[allow(clippy::result_large_err)]
    fn get_setup_connection_message(
        min_version: u16,
        max_version: u16,
        address: &SocketAddr,
        is_work_selection_enabled: bool,
    ) -> Result<SetupConnectionOwned, TproxyErrorKind> {
        let endpoint_host = address.ip().to_string().try_into()?;
        let vendor = "SRI".try_into()?;
        let hardware_version = "Translator Proxy".try_into()?;
        let firmware = "".try_into()?;
        let device_id = "".try_into()?;
        let flags = if is_work_selection_enabled {
            0b110
        } else {
            0b100
        };

        Ok(SetupConnectionOwned {
            protocol: Protocol::MiningProtocol,
            min_version,
            max_version,
            flags,
            endpoint_host,
            endpoint_port: address.port(),
            vendor,
            hardware_version,
            firmware,
            device_id,
        })
    }
}
