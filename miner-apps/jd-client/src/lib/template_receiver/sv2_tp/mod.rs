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

use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::template_distribution_protocol::CancellationToken;
use stratum_apps::{
    custom_mutex::Mutex,
    fallback_coordinator::FallbackCoordinator,
    key_utils::Secp256k1PublicKey,
    network_helpers::{self, connect_with_noise, resolve_host_port},
    stratum_core::{
        framing_sv2,
        handlers_sv2::HandleCommonMessagesFromServerAsync,
        noise_sv2,
        parsers_sv2::{AnyMessage, TemplateDistribution},
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{protocol_message_type, MessageType},
        types::{Message, Sv2Frame},
    },
};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::{
    error::{self, JDCError, JDCErrorKind, JDCResult},
    io_task::spawn_io_tasks,
    status::{handle_error, Status, StatusSender},
    utils::get_setup_connection_message_tp,
};

mod message_handler;

/// Placeholder for future Sv2Tp–specific state.
pub struct Sv2TpData;

/// Holds communication channels between the Sv2Tp, channel manager,
/// and upstream template provider.
///
/// - `channel_manager_sender` → sends frames to the channel manager
/// - `channel_manager_receiver` → receives frames from the channel manager
/// - `outbound_tx` → sends frames upstream to the template provider
/// - `inbound_rx` → receives frames from the template provider
#[derive(Clone)]
pub struct Sv2TpChannel {
    channel_manager_sender: Sender<TemplateDistribution<'static>>,
    channel_manager_receiver: Receiver<TemplateDistribution<'static>>,
    tp_sender: Sender<Sv2Frame>,
    tp_receiver: Receiver<Sv2Frame>,
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
    /// Internal state
    sv2_tp_data: Arc<Mutex<Sv2TpData>>,
    /// Messaging channels to/from the channel manager and TP.
    sv2_tp_channel: Sv2TpChannel,
    /// Address of the template provider (string form)
    tp_address: String,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv2Tp {
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
        channel_manager_receiver: Receiver<TemplateDistribution<'static>>,
        channel_manager_sender: Sender<TemplateDistribution<'static>>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) -> JDCResult<Sv2Tp, error::TemplateProvider> {
        const MAX_RETRIES: usize = 3;

        for attempt in 1..=MAX_RETRIES {
            info!(attempt, MAX_RETRIES, "Connecting to template provider");

            match TcpStream::connect(tp_address.as_str()).await {
                Ok(stream) => {
                    info!(
                        attempt,
                        "TCP connection established, starting Noise handshake"
                    );

                    tokio::select! {
                        result = connect_with_noise(stream, public_key) => {
                            match result {
                                Ok(noise_stream) => {
                                    info!(attempt, "Noise handshake completed successfully");

                                    let (noise_stream_reader, noise_stream_writer) =
                                        noise_stream.into_split();

                                    let (inbound_tx, inbound_rx) = unbounded::<Sv2Frame>();
                                    let (outbound_tx, outbound_rx) = unbounded::<Sv2Frame>();

                                    info!(attempt, "Spawning IO tasks for template receiver");
                                    spawn_io_tasks(
                                        task_manager.clone(),
                                        noise_stream_reader,
                                        noise_stream_writer,
                                        outbound_rx,
                                        inbound_tx,
                                        cancellation_token.clone(),
                                        fallback_coordinator.clone(),
                                    );

                                    let template_receiver_data = Arc::new(Mutex::new(Sv2TpData));
                                    let template_receiver_channel = Sv2TpChannel {
                                        channel_manager_receiver,
                                        channel_manager_sender,
                                        tp_receiver: inbound_rx,
                                        tp_sender: outbound_tx,
                                    };

                                    info!(attempt, "TemplateReceiver initialized successfully");
                                    return Ok(Sv2Tp {
                                        sv2_tp_channel: template_receiver_channel,
                                        sv2_tp_data: template_receiver_data,
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
                         _ = cancellation_token.cancelled() => {
                            info!("Shutdown received during handshake, dropping connection");
                            return Err(JDCError::shutdown(JDCErrorKind::CouldNotInitiateSystem));
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
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
    ) {
        let status_sender = StatusSender::TemplateReceiver(status_sender);

        info!("Initialized state for starting template receiver");
        _ = self.setup_connection(socket_address).await;

        info!("Setup Connection done. connection with template receiver is now done");
        task_manager.spawn(async move {
            loop {
                let mut self_clone_1 = self.clone();
                let self_clone_2 = self.clone();
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("TemplateReceiver received shutdown signal");
                        break;
                    }
                    res = self_clone_1.handle_template_provider_message() => {
                        if let Err(e) = res {
                            error!("TemplateReceiver template provider handler failed: {e:?}");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_channel_manager_message() => {
                        if let Err(e) = res {
                            error!("TemplateReceiver channel manager handler failed: {e:?}");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    },
                }
            }
            warn!("TemplateReceiver: unified message loop exited.");
        });
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
            .sv2_tp_channel
            .tp_receiver
            .recv()
            .await
            .map_err(JDCError::shutdown)?;

        debug!("Received SV2 frame from Template provider.");
        let header = sv2_frame.get_header().ok_or_else(|| {
            error!("SV2 frame missing header");
            JDCError::shutdown(framing_sv2::Error::MissingHeader)
        })?;
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
                    .into_static();
                self.sv2_tp_channel
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
        let msg = AnyMessage::TemplateDistribution(
            self.sv2_tp_channel
                .channel_manager_receiver
                .recv()
                .await
                .map_err(JDCError::shutdown)?,
        );
        debug!("Forwarding message from channel manager to outbound_tx");
        let sv2_frame: Sv2Frame = msg.try_into().map_err(JDCError::shutdown)?;
        self.sv2_tp_channel
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
        let frame: Sv2Frame = Message::Common(setup_msg.into())
            .try_into()
            .map_err(JDCError::shutdown)?;

        info!("Sending setup connection message to upstream");
        self.sv2_tp_channel
            .tp_sender
            .send(frame)
            .await
            .map_err(|_| {
                error!("Failed to send setup connection message upstream");
                JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
            })?;

        info!("Waiting for upstream handshake response");
        let mut incoming: Sv2Frame = self.sv2_tp_channel.tp_receiver.recv().await.map_err(|e| {
            error!(?e, "Upstream connection closed during handshake");
            JDCError::shutdown(noise_sv2::Error::ExpectedIncomingHandshakeMessage)
        })?;

        let header = incoming.get_header().ok_or_else(|| {
            error!("Handshake frame missing header");
            JDCError::shutdown(framing_sv2::Error::MissingHeader)
        })?;
        debug!(ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Received upstream handshake response");

        self.handle_common_message_frame_from_server(None, header, incoming.payload())
            .await?;
        info!("Handshake with upstream completed successfully");
        Ok(())
    }
}
