//! Template Receiver module
//!
//! This module defines the [`TemplateReceiver`] struct, which manages a connection
//! to a Template Provider (TP).
//!
//! Responsibilities:
//! - Establish TCP + Noise encrypted connection to the template provider
//! - Perform `SetupConnection` handshake
//! - Forward SV2 `TemplateDistribution` messages to the channel manager
//! - Forward messages from the channel manager upstream to the template provider
//! - Send [`CoinbaseOutputConstraints`] to the template provider

use std::sync::Arc;
use stratum_apps::stratum_core::parsers_sv2::AnyMessageOwned;

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
    key_utils::Secp256k1PublicKey,
    network_helpers::{self, TCP_CONNECT_TIMEOUT, connect_with_noise, resolve_host_port},
    stratum_core::{
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        },
        handlers_sv2::HandleCommonMessagesFromServerOwnedAsync,
        noise_sv2,
        parsers_sv2::{TemplateDistribution, TemplateDistributionOwned},
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{InboundFrame, Message, OutboundFrame},
    },
};
use tokio::{net::TcpStream, time::timeout};
use tracing::{debug, error, info, warn};

use crate::{
    error::{self, Action, JDCError, JDCErrorKind, JDCResult, LoopControl},
    io_task::spawn_io_tasks,
    utils::get_setup_connection_message_tp,
};

mod message_handler;

/// Holds communication channels between the Sv2Tp, channel manager,
/// and upstream template provider.
///
/// - `channel_manager_sender` → sends frames to the channel manager
/// - `channel_manager_receiver` → receives frames from the channel manager
/// - `outbound_tx` → sends frames upstream to the template provider
/// - `inbound_rx` → receives frames from the template provider
#[derive(Clone)]
pub struct Sv2TpIo {
    channel_manager_sender: Sender<TemplateDistributionOwned>,
    channel_manager_receiver: Receiver<TemplateDistributionOwned>,
    tp_sender: Sender<OutboundFrame>,
    tp_receiver: Receiver<InboundFrame>,
}

impl Sv2TpIo {
    fn close(&self) {
        self.channel_manager_sender.close();
        self.tp_sender.close();
        self.channel_manager_receiver.close_and_drain();
        self.tp_receiver.close_and_drain();
    }
}

/// Manages communication with a Stratum V2 Template Provider.
///
/// Responsibilities:
/// - Establishes TCP + Noise connection to TP
/// - Performs handshake (`SetupConnection`)
/// - Sends [`CoinbaseOutputConstraints`] to TP
/// - Routes messages between TP and channel manager
/// - Handles shutdown/fallback notifications
#[allow(warnings)]
#[derive(Clone)]
pub struct Sv2Tp {
    /// Messaging channels to/from the channel manager and TP.
    sv2_tp_io: Sv2TpIo,
    /// Address of the template provider (string form)
    tp_address: String,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv2Tp {
    fn handle_error_action(
        context: &str,
        e: &JDCError<error::TemplateProvider>,
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
                    error_kind = ?e.kind,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
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

    /// Establish a new connection to a Template Provider.
    ///
    /// - Opens a TCP connection
    /// - Performs Noise handshake
    /// - Spawns IO tasks for inbound/outbound frames
    ///
    /// Retries up to 3 times before returning [`JDCError::Shutdown`].
    pub async fn new(
        tp_address: String,
        public_key: Option<Secp256k1PublicKey>,
        channel_manager_receiver: Receiver<TemplateDistributionOwned>,
        channel_manager_sender: Sender<TemplateDistributionOwned>,
        cancellation_token: CancellationToken,
        task_manager: Arc<TaskManager>,
    ) -> JDCResult<Sv2Tp, error::TemplateProvider> {
        const MAX_RETRIES: usize = 3;

        for attempt in 1..=MAX_RETRIES {
            info!(attempt, MAX_RETRIES, "Connecting to template provider");

            match timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(tp_address.as_str()))
                .await
                .map_err(JDCError::shutdown)?
            {
                Ok(stream) => {
                    info!(
                        attempt,
                        "TCP connection established, starting Noise handshake"
                    );

                    tokio::select! {
                        biased;

                         _ = cancellation_token.cancelled() => {
                            info!("Shutdown received during handshake, dropping connection");
                            return Err(JDCError::shutdown(JDCErrorKind::CouldNotInitiateSystem));
                        }
                        result = connect_with_noise(stream, public_key) => {
                            match result {
                                Ok(noise_stream) => {
                                    info!(attempt, "Noise handshake completed successfully");

                                    let (noise_stream_reader, noise_stream_writer) =
                                        noise_stream.into_split();

                                    let (inbound_tx, inbound_rx) = unbounded::<InboundFrame>();
                                    let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

                                    info!(attempt, "Spawning IO tasks for template receiver");
                                    spawn_io_tasks(
                                        task_manager.clone(),
                                        noise_stream_reader,
                                        noise_stream_writer,
                                        outbound_rx,
                                        inbound_tx,
                                        cancellation_token.clone(),
                                        None,
                                    );

                                    let sv2_tp_io = Sv2TpIo {
                                        channel_manager_receiver,
                                        channel_manager_sender,
                                        tp_receiver: inbound_rx,
                                        tp_sender: outbound_tx,
                                    };

                                    info!(attempt, "TemplateReceiver initialized successfully");
                                    return Ok(Sv2Tp {
                                        sv2_tp_io,
                                        tp_address,
                                    });
                                }
                                Err(network_helpers::Error::InvalidKey) => {
                                    return Err(JDCError::shutdown(JDCErrorKind::InvalidKey));
                                }
                                Err(e) => {
                                    error!(attempt, error = ?e, "Noise handshake failed");
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(attempt, MAX_RETRIES, error = ?e, "Failed to connect to template provider");
                }
            }

            if attempt < MAX_RETRIES {
                debug!(attempt, "Retrying connection after backoff");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        error!("Exhausted all connection attempts, shutting down TemplateReceiver");
        Err(JDCError::shutdown(JDCErrorKind::CouldNotInitiateSystem))
    }

    /// Start unified message loop for template receiver.
    ///
    /// Responsibilities:
    /// - Run handshake (`setup_connection`)
    /// - Send [`CoinbaseOutputConstraints`]
    /// - Handle:
    ///   - Messages from template provider
    ///   - Messages from channel manager
    ///   - Shutdown signals (upstream/job-declarator fallback)
    pub async fn start(
        mut self,
        socket_address: String,
        cancellation_token: CancellationToken,
        task_manager: Arc<TaskManager>,
    ) -> JDCResult<(), error::TemplateProvider> {
        info!("Initialized state for starting template receiver");
        if let Err(e) = self.setup_connection(socket_address).await {
            error!("TemplateReceiver setup connection failed: {e:?}");
            self.sv2_tp_io.close();
            return Err(e);
        }

        info!("Setup Connection done. connection with template receiver is now done");
        task_manager.spawn(async move {
            loop {
                let mut self_clone_1 = self.clone();
                let self_clone_2 = self.clone();
                tokio::select! {
                    biased;

                    _ = cancellation_token.cancelled() => {
                        info!("TemplateReceiver received shutdown signal");
                        break;
                    }
                    res = self_clone_1.handle_template_provider_message() => {
                        if let Err(e) = res {
                            error!("TemplateReceiver template provider handler failed: {e:?}");
                            if let LoopControl::Break = Self::handle_error_action(
                                "TemplateReceiver::handle_template_provider_message",
                                &e,
                                &cancellation_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_channel_manager_message() => {
                        if let Err(e) = res {
                            error!("TemplateReceiver channel manager handler failed: {e:?}");
                            if let LoopControl::Break = Self::handle_error_action(
                                "TemplateReceiver::handle_channel_manager_message",
                                &e,
                                &cancellation_token,
                            ) {
                                break;
                            }
                        }
                    },
                }
            }
            self.sv2_tp_io.close();
            warn!("TemplateReceiver: unified message loop exited.");
        });
        Ok(())
    }

    /// Handle inbound messages from the template provider.
    ///
    /// Routes:
    /// - `Common` messages → handled locally
    /// - `TemplateDistribution` messages → forwarded to channel manager
    /// - Unsupported messages → logged and ignored
    pub async fn handle_template_provider_message(
        &mut self,
    ) -> JDCResult<(), error::TemplateProvider> {
        let mut sv2_frame = self
            .sv2_tp_io
            .tp_receiver
            .recv()
            .await
            .map_err(JDCError::shutdown)?;

        debug!("Received SV2 frame from Template provider.");
        let header = sv2_frame.header();
        let message_type = header.msg_type();
        let extension_type = header.ext_type();

        match protocol_message_type(extension_type, message_type) {
            MessageType::Common => {
                info!(
                    ext_type = ?extension_type,
                    msg_type = ?message_type,
                    "Handling common message from Template provider."
                );
                self.handle_common_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::TemplateDistribution => {
                let message = TemplateDistribution::try_from((message_type, sv2_frame.payload()))
                    .map_err(JDCError::shutdown)?
                    .into_owned();
                self.sv2_tp_io
                    .channel_manager_sender
                    .send(message)
                    .await
                    .map_err(|e| {
                        error!(error=?e, "Failed to send template distribution message to channel manager.");
                        JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            _ => {
                warn!("Received unsupported message type from template provider: {message_type}");
            }
        }
        Ok(())
    }

    /// Handle messages from channel manager → template provider.
    ///
    /// Forwards outbound frames upstream
    pub async fn handle_channel_manager_message(&self) -> JDCResult<(), error::TemplateProvider> {
        let msg = AnyMessageOwned::TemplateDistribution(
            self.sv2_tp_io
                .channel_manager_receiver
                .recv()
                .await
                .map_err(JDCError::shutdown)?,
        );
        debug!("Forwarding message from channel manager to outbound_tx");
        let sv2_frame = OutboundFrame::from_message(msg).map_err(JDCError::shutdown)?;
        self.sv2_tp_io
            .tp_sender
            .send(sv2_frame)
            .await
            .map_err(|_| JDCError::shutdown(JDCErrorKind::ChannelErrorSender))?;

        Ok(())
    }

    // Performs the initial handshake with template provider.
    pub async fn setup_connection(
        &mut self,
        addr: String,
    ) -> JDCResult<(), error::TemplateProvider> {
        let socket = resolve_host_port(&addr).await.map_err(|e| {
            error!(%addr, "Failed to resolve template provider address: {e}");
            JDCError::shutdown(JDCErrorKind::InvalidSocketAddress(addr.clone()))
        })?;

        info!(%socket, "Building setup connection message for upstream");
        let setup_msg = get_setup_connection_message_tp(socket);
        let frame = OutboundFrame::from_message(Message::Common(setup_msg.into()))
            .map_err(JDCError::shutdown)?;

        info!("Sending setup connection message to upstream");
        self.sv2_tp_io.tp_sender.send(frame).await.map_err(|_| {
            error!("Failed to send setup connection message upstream");
            JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
        })?;

        info!("Waiting for upstream handshake response");
        let mut incoming: InboundFrame = self.sv2_tp_io.tp_receiver.recv().await.map_err(|e| {
            error!(?e, "Upstream connection closed during handshake");
            JDCError::shutdown(noise_sv2::Error::ExpectedIncomingHandshakeMessage)
        })?;

        let header = incoming.header();
        debug!(ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Received upstream handshake response");

        if header.ext_type() != 0
            || !matches!(
                header.msg_type(),
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS | MESSAGE_TYPE_SETUP_CONNECTION_ERROR
            )
        {
            return Err(JDCError::shutdown(JDCErrorKind::UnexpectedMessage(
                header.ext_type(),
                header.msg_type(),
            )));
        }

        self.handle_common_message_frame_from_server(None, header, incoming.payload())
            .await?;
        info!("Handshake with upstream completed successfully");
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
    async fn setup_connection_rejects_channel_endpoint_changed_response() {
        let (channel_manager_sender, _cm_rx) = unbounded();
        let (_cm_tx, channel_manager_receiver) = unbounded();
        let (tp_sender, _tp_outbound_rx) = unbounded();
        let (tp_inbound_tx, tp_receiver) = unbounded();

        let mut sv2_tp = Sv2Tp {
            sv2_tp_io: Sv2TpIo {
                channel_manager_sender,
                channel_manager_receiver,
                tp_sender,
                tp_receiver,
            },
            tp_address: "127.0.0.1:1234".to_string(),
        };

        let response: Message = Message::Common(CommonMessagesOwned::ChannelEndpointChanged(
            ChannelEndpointChangedOwned { channel_id: 0 },
        ));

        tp_inbound_tx
            .send(serialize_frame(response))
            .await
            .expect("Failed to inject ChannelEndpointChanged response");

        assert!(
            sv2_tp
                .setup_connection("127.0.0.1:1234".to_string())
                .await
                .is_err(),
            "setup_connection must reject a handshake response other than \
             SetupConnectionSuccess/SetupConnectionError"
        );
    }
}
