//! Upstream module
//!
//! This module defines the [`Upstream`] struct, which manages communication
//! with an upstream SV2 server (e.g., pool).
//!
//! Responsibilities:
//! - Establish a TCP + Noise encrypted connection to upstream
//! - Perform `SetupConnection` handshake
//! - Forward SV2 mining messages between upstream and channel manager
//! - Handle common messages from upstream

use std::{net::SocketAddr, sync::Arc};
use stratum_apps::stratum_core::{
    binary_sv2::Seq064KOwned, extensions_sv2::RequestExtensionsOwned,
};

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    network_helpers::{TCP_CONNECT_TIMEOUT, connect_with_noise, resolve_host},
    stratum_core::{
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        },
        handlers_sv2::HandleCommonMessagesFromServerOwnedAsync,
        parsers_sv2::AnyMessageOwned,
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{InboundFrame, Message, OutboundFrame},
    },
};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::{
    error::{self, Action, JDCError, JDCErrorKind, JDCResult, LoopControl},
    io_task::spawn_io_tasks,
    utils::{UpstreamEntry, get_setup_connection_message},
};

mod message_handler;

/// Holds channels for communication between upstream and channel manager.
///
/// - `channel_manager_sender` → sends frames to channel manager
/// - `channel_manager_receiver` → receives frames from channel manager
/// - `outbound_tx` → sends frames outbound to upstream
/// - `inbound_rx` → receives frames inbound from upstream
#[derive(Clone)]
pub struct UpstreamIo {
    channel_manager_sender: Sender<InboundFrame>,
    channel_manager_receiver: Receiver<OutboundFrame>,
    upstream_sender: Sender<OutboundFrame>,
    upstream_receiver: Receiver<InboundFrame>,
}

impl UpstreamIo {
    fn close(&self) {
        self.channel_manager_sender.close();
        self.upstream_sender.close();
        self.channel_manager_receiver.close_and_drain();
        self.upstream_receiver.close_and_drain();
    }
}

/// Represents an upstream connection (e.g., a pool).
#[derive(Clone)]
pub struct Upstream {
    /// Messaging channels to/from the channel manager and Upstream.
    upstream_io: UpstreamIo,
    /// Protocol extensions that the JDC requires
    required_extensions: Vec<u16>,
    /// Upstream address
    address: SocketAddr,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Upstream {
    fn handle_error_action(
        context: &str,
        e: &JDCError<error::Upstream>,
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

    /// Create a new [`Upstream`] connection to the given address.
    ///
    /// - Resolves hostname to IP address via DNS (if not already an IP)
    /// - Establishes TCP + Noise connection
    /// - Spawns IO tasks to handle inbound/outbound traffic
    pub async fn new(
        upstream_entry: &UpstreamEntry,
        channel_manager_sender: Sender<InboundFrame>,
        channel_manager_receiver: Receiver<OutboundFrame>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        required_extensions: Vec<u16>,
    ) -> JDCResult<Self, error::Upstream> {
        let addr = resolve_host(&upstream_entry.pool_host, upstream_entry.pool_port)
            .await
            .map_err(|e| {
                error!(
                    "Failed to resolve pool address {}:{}: {e}",
                    upstream_entry.pool_host, upstream_entry.pool_port
                );
                JDCError::fallback(JDCErrorKind::NetworkHelpersError(e.into()))
            })?;

        let stream = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(JDCError::fallback)?
            .map_err(JDCError::fallback)?;
        info!("Connected to upstream at {}", addr);
        debug!("Begin with noise setup in upstream connection");

        let (noise_stream_reader, noise_stream_writer) = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                info!("Shutdown received during handshake, dropping connection");
                Err(JDCError::shutdown(JDCErrorKind::CouldNotInitiateSystem))
            }
            result = connect_with_noise(stream, Some(upstream_entry.authority_pubkey)) => {
                match result {
                    Ok(noise_stream) => Ok(noise_stream.into_split()),
                    Err(e) => Err(JDCError::fallback(e))
                }
            }
        }?;

        let (inbound_tx, inbound_rx) = unbounded::<InboundFrame>();
        let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            cancellation_token.clone(),
            Some(fallback_coordinator.clone()),
        );

        debug!("Noise setup done in upstream connection");
        let upstream_io = UpstreamIo {
            channel_manager_receiver,
            channel_manager_sender,
            upstream_sender: outbound_tx,
            upstream_receiver: inbound_rx,
        };
        Ok(Upstream {
            upstream_io,
            required_extensions,
            address: addr,
        })
    }

    /// Perform `SetupConnection` handshake with upstream.
    ///
    /// Sends [`SetupConnection`] and awaits response.
    pub async fn setup_connection(
        &mut self,
        min_version: u16,
        max_version: u16,
    ) -> JDCResult<(), error::Upstream> {
        info!("Upstream: initiating SV2 handshake...");
        let setup_connection =
            get_setup_connection_message(min_version, max_version, &self.address)
                .map_err(JDCError::shutdown)?;
        debug!(?setup_connection, "Prepared `SetupConnection` message");
        let sv2_frame = OutboundFrame::from_message(Message::Common(setup_connection.into()))
            .map_err(JDCError::shutdown)?;
        debug!(?sv2_frame, "Encoded `SetupConnection` frame");

        // Send SetupConnection
        if let Err(e) = self.upstream_io.upstream_sender.send(sv2_frame).await {
            error!(?e, "Failed to send `SetupConnection` frame to upstream");
            return Err(JDCError::fallback(JDCErrorKind::ChannelErrorSender));
        }
        info!("Sent `SetupConnection` to upstream, awaiting response...");

        let incoming_frame = match self.upstream_io.upstream_receiver.recv().await {
            Ok(frame) => {
                debug!(?frame, "Received raw inbound frame during handshake");
                frame
            }
            Err(e) => {
                error!(?e, "Upstream closed connection during handshake");
                return Err(JDCError::fallback(e));
            }
        };

        let mut incoming: InboundFrame = incoming_frame;
        debug!(?incoming, "Decoded inbound handshake frame");

        let header = incoming.header();

        info!(ext_type = ?header.ext_type(), msg_type = ?header.msg_type(), "Dispatching inbound handshake message");

        if header.ext_type() != 0
            || !matches!(
                header.msg_type(),
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS | MESSAGE_TYPE_SETUP_CONNECTION_ERROR
            )
        {
            return Err(JDCError::fallback(JDCErrorKind::UnexpectedMessage(
                header.ext_type(),
                header.msg_type(),
            )));
        }

        self.handle_common_message_frame_from_server(None, header, incoming.payload())
            .await?;

        // Send RequestExtensions after successful SetupConnection if there are required extensions
        if !self.required_extensions.is_empty() {
            self.send_request_extensions().await?;
        }

        Ok(())
    }

    /// Send `RequestExtensions` message to upstream.
    /// The supported extensions are stored for potential retry if the server requires additional
    /// extensions.
    async fn send_request_extensions(&mut self) -> JDCResult<(), error::Upstream> {
        info!(
            "Sending RequestExtensions to upstream with required extensions: {:?}",
            self.required_extensions
        );
        if self.required_extensions.is_empty() {
            return Ok(());
        }

        let requested_extensions =
            Seq064KOwned::new(self.required_extensions.clone()).map_err(JDCError::shutdown)?;

        let request_extensions = RequestExtensionsOwned {
            request_id: 0,
            requested_extensions,
        };

        info!(
            "Sending RequestExtensions to upstream with required extensions: {:?}",
            self.required_extensions
        );

        let sv2_frame =
            OutboundFrame::from_message(AnyMessageOwned::Extensions(request_extensions.into()))
                .map_err(JDCError::shutdown)?;

        self.upstream_io
            .upstream_sender
            .send(sv2_frame)
            .await
            .map_err(|e| {
                error!(?e, "Failed to send RequestExtensions to upstream");
                JDCError::fallback(JDCErrorKind::ChannelErrorSender)
            })?;

        info!("Sent RequestExtensions to upstream");
        Ok(())
    }

    /// Start unified upstream loop.
    ///
    /// Responsibilities:
    /// - Run `setup_connection`
    /// - Handle messages from upstream (pool) and channel manager
    /// - React to shutdown signals
    ///
    /// This function spawns an async task and returns immediately.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        mut self,
        min_version: u16,
        max_version: u16,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) {
        let setup_fallback_token = fallback_coordinator.token();
        if let Err(e) = self.setup_connection(min_version, max_version).await {
            error!(error = ?e, "Upstream: connection setup failed.");
            if let LoopControl::Break = Self::handle_error_action(
                "Upstream::setup_connection",
                &e,
                &cancellation_token,
                &setup_fallback_token,
            ) {
                self.upstream_io.close();
                return;
            }
            self.upstream_io.close();
            return;
        }

        task_manager.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();

            let mut self_clone_1 = self.clone();
            let mut self_clone_2 = self.clone();
            loop {
                tokio::select! {
                    biased;

                    _ = cancellation_token.cancelled() => {
                        info!("Upstream: received shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("Upstream: fallback triggered");
                        break;
                    }
                    res = self_clone_1.handle_pool_message_frame() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Upstream: error handling pool message.");
                            if let LoopControl::Break = Self::handle_error_action(
                                "Upstream::handle_pool_message_frame",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_channel_manager_message_frame() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Upstream: error handling channel manager message.");
                            if let LoopControl::Break = Self::handle_error_action(
                                "Upstream::handle_channel_manager_message_frame",
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
            warn!("Upstream: unified message loop exited.");

            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });
    }

    // Handle incoming frames from upstream (pool).
    //
    // Routes:
    // - `Common` messages → handled locally
    // - `Mining` messages → forwarded to channel manager
    // - Unsupported → error
    async fn handle_pool_message_frame(&mut self) -> JDCResult<(), error::Upstream> {
        debug!("Received SV2 frame from upstream.");
        let mut sv2_frame = self
            .upstream_io
            .upstream_receiver
            .recv()
            .await
            .map_err(JDCError::fallback)?;
        let header = sv2_frame.header();
        let message_type = header.msg_type();
        let extension_type = header.ext_type();

        match protocol_message_type(extension_type, message_type) {
            MessageType::Common => {
                info!(ext_type = ?extension_type, msg_type = ?message_type, "Handling common message from Upstream.");
                self.handle_common_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::Mining | MessageType::Extensions => {
                self.upstream_io
                    .channel_manager_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!(error=?e, "Failed to send mining message to channel manager.");
                        JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            _ => {
                warn!("Received unsupported message type from upstream: {message_type}");
            }
        }
        Ok(())
    }

    // Handle outbound frames from channel manager → upstream.
    //
    // Forwards messages upstream.
    async fn handle_channel_manager_message_frame(&mut self) -> JDCResult<(), error::Upstream> {
        match self.upstream_io.channel_manager_receiver.recv().await {
            Ok(sv2_frame) => {
                debug!("Received sv2 frame from channel manager, forwarding upstream.");
                self.upstream_io
                    .upstream_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!(error=?e, "Failed to send sv2 frame to upstream.");
                        JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            Err(e) => {
                warn!(error=?e, "Channel manager receiver closed or errored.");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    // A peer sends framed bytes, so a test standing in for one has to frame and serialize the
    // message it injects.
    fn serialize_frame(message: Message) -> InboundFrame {
        use stratum_apps::stratum_core::codec_sv2::EncodableFrame as _;
        let frame =
            OutboundFrame::from_message(message).expect("Failed to frame the injected message");
        let mut bytes = vec![0u8; frame.encoded_length()];
        frame
            .encode_into(&mut bytes)
            .expect("Failed to serialize the injected frame");
        InboundFrame::from_bytes(bytes.into()).expect("Injected frame is a whole frame")
    }
    use super::*;
    use stratum_apps::stratum_core::{
        common_messages_sv2::ChannelEndpointChangedOwned, parsers_sv2::CommonMessagesOwned,
    };

    #[tokio::test]
    async fn setup_connection_rejects_non_setup_response() {
        let (channel_manager_sender, _cm_rx) = unbounded();
        let (_cm_tx, channel_manager_receiver) = unbounded();
        let (upstream_sender, _upstream_outbound_receiver) = unbounded();
        let (upstream_inbound_sender, upstream_receiver) = unbounded();

        let mut upstream = Upstream {
            upstream_io: UpstreamIo {
                channel_manager_sender,
                channel_manager_receiver,
                upstream_sender,
                upstream_receiver,
            },
            required_extensions: vec![],
            address: "127.0.0.1:1234".parse().expect("valid socket address"),
        };

        let response = Message::Common(CommonMessagesOwned::ChannelEndpointChanged(
            ChannelEndpointChangedOwned { channel_id: 0 },
        ));
        upstream_inbound_sender
            .send(serialize_frame(response))
            .await
            .expect("Failed to inject ChannelEndpointChanged response");

        assert!(
            upstream.setup_connection(2, 2).await.is_err(),
            "a non-SetupConnection response must not establish the upstream session"
        );
    }
}
