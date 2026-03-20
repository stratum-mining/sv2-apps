use std::sync::Arc;
mod common_message_handler;
use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::template_distribution_protocol::CancellationToken;
use stratum_apps::{
    key_utils::Secp256k1PublicKey,
    network_helpers::{self, connect_with_noise, resolve_host_port},
    stratum_core::{
        framing_sv2,
        handlers_sv2::HandleCommonMessagesFromServerAsync,
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
    error::{self, PoolError, PoolErrorKind, PoolResult},
    io_task::spawn_io_tasks,
    status::{handle_error, Status, StatusSender},
    utils::get_setup_connection_message_tp,
};

#[derive(Clone)]
pub struct Sv2TpChannel {
    channel_manager_sender: Sender<TemplateDistribution<'static>>,
    channel_manager_receiver: Receiver<TemplateDistribution<'static>>,
    tp_sender: Sender<Sv2Frame>,
    tp_receiver: Receiver<Sv2Frame>,
}

#[derive(Clone)]
pub struct Sv2Tp {
    sv2_tp_channel: Sv2TpChannel,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv2Tp {
    /// Establish a new connection to a Sv2 Template Provider.
    ///
    /// - Opens a TCP connection
    /// - Performs Noise handshake
    /// - Spawns IO tasks for inbound/outbound frames
    ///
    /// Retries up to 3 times before returning [`PoolError::Shutdown`].
    pub async fn new(
        tp_address: String,
        public_key: Option<Secp256k1PublicKey>,
        channel_manager_receiver: Receiver<TemplateDistribution<'static>>,
        channel_manager_sender: Sender<TemplateDistribution<'static>>,
        cancellation_token: CancellationToken,
        task_manager: Arc<TaskManager>,
    ) -> PoolResult<Sv2Tp, error::TemplateProvider> {
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
                                    );

                                    let template_receiver_channel = Sv2TpChannel {
                                        channel_manager_receiver,
                                        channel_manager_sender,
                                        tp_receiver: inbound_rx,
                                        tp_sender: outbound_tx,
                                    };

                                    info!(attempt, "TemplateReceiver initialized successfully");
                                    return Ok(Sv2Tp {
                                        sv2_tp_channel: template_receiver_channel,
                                    });
                                }
                                Err(network_helpers::Error::InvalidKey) => {
                                    return Err(PoolError::shutdown(PoolErrorKind::InvalidKey))
                                }
                                Err(e) => {
                                    error!(attempt, error = ?e, "Noise handshake failed");
                                }
                            }
                        }
                        _ = cancellation_token.cancelled() => {
                            info!("Shutdown received during handshake, dropping connection");
                            return Err(PoolError::shutdown(PoolErrorKind::CouldNotInitiateSystem))
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
        Err(PoolError::shutdown(PoolErrorKind::CouldNotInitiateSystem))
    }

    /// Start unified message loop for Sv2Tp.
    ///
    /// Responsibilities:
    /// - Run handshake (`setup_connection`)
    /// - Handle:
    ///   - Messages from Template Provider
    ///   - Messages from ChannelManager
    ///   - Shutdown signals (upstream/job-declarator fallback)
    pub async fn start(
        mut self,
        socket_address: String,
        cancellation_token: CancellationToken,
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
    ) -> PoolResult<(), error::TemplateProvider> {
        let status_sender = StatusSender::TemplateReceiver(status_sender);

        info!("Initialized state for starting template receiver");
        self.setup_connection(socket_address).await?;

        info!("Setup Connection done. connection with template receiver is now done");
        task_manager.spawn(async move {
            loop {
                let mut self_clone_1 = self.clone();
                let self_clone_2 = self.clone();
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Template Receiver: received shutdown signal");
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
        Ok(())
    }

    /// Handle inbound messages from the template provider.
    ///
    /// Routes:
    /// - `Common` messages → handled locally
    /// - `TemplateDistribution` messages → forwarded to ChannelManager
    /// - Unsupported messages → logged and ignored
    pub async fn handle_template_provider_message(
        &mut self,
    ) -> PoolResult<(), error::TemplateProvider> {
        let mut sv2_frame = self
            .sv2_tp_channel
            .tp_receiver
            .recv()
            .await
            .map_err(PoolError::shutdown)?;
        debug!("Received SV2 frame from Template provider.");
        let header = sv2_frame.get_header().ok_or_else(|| {
            error!("SV2 frame missing header");
            PoolError::shutdown(framing_sv2::Error::MissingHeader)
        })?;

        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::Common => {
                info!(
                    ext_type = ?header.ext_type(),
                    msg_type = ?header.msg_type(),
                    "Handling common message from Template provider."
                );

                self.handle_common_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::TemplateDistribution => {
                let message =
                    TemplateDistribution::try_from((header.msg_type(), sv2_frame.payload()))
                        .map_err(PoolError::shutdown)?
                        .into_static();

                self.sv2_tp_channel
                    .channel_manager_sender
                    .send(message)
                    .await
                    .map_err(|e| {
                        error!(error=?e, "Failed to send template distribution message to channel manager.");
                        PoolError::shutdown(PoolErrorKind::ChannelErrorSender)
                    })?;
            }
            _ => {
                warn!(
                    ext_type = ?header.ext_type(),
                    msg_type = ?header.msg_type(),
                    "Received unsupported message type from template provider."
                );
            }
        }
        Ok(())
    }

    /// Handle messages from channel manager → template provider.
    ///
    /// Forwards outbound frames upstream
    pub async fn handle_channel_manager_message(&self) -> PoolResult<(), error::TemplateProvider> {
        let msg = self
            .sv2_tp_channel
            .channel_manager_receiver
            .recv()
            .await
            .map_err(PoolError::shutdown)?;
        let message = AnyMessage::TemplateDistribution(msg).into_static();
        let frame: Sv2Frame = message.try_into().map_err(PoolError::shutdown)?;

        debug!("Forwarding message from channel manager to outbound_tx");
        self.sv2_tp_channel
            .tp_sender
            .send(frame)
            .await
            .map_err(|_| PoolError::shutdown(PoolErrorKind::ChannelErrorSender))?;

        Ok(())
    }

    // Performs the initial handshake with Template Provider.
    pub async fn setup_connection(
        &mut self,
        addr: String,
    ) -> PoolResult<(), error::TemplateProvider> {
        let socket = resolve_host_port(&addr).await.map_err(|e| {
            error!(%addr, "Failed to resolve template provider address: {e}");
            PoolError::shutdown(PoolErrorKind::InvalidSocketAddress(addr.clone()))
        })?;

        debug!(%socket, "Building SetupConnection message to the Template Provider");
        let setup_msg = get_setup_connection_message_tp(socket).map_err(PoolError::shutdown)?;
        let frame: Sv2Frame = Message::Common(setup_msg.into())
            .try_into()
            .map_err(PoolError::shutdown)?;

        info!("Sending SetupConnection message to the Template Provider");
        self.sv2_tp_channel
            .tp_sender
            .send(frame)
            .await
            .map_err(|_| {
                error!("Failed to send setup connection message upstream");
                PoolError::shutdown(PoolErrorKind::ChannelErrorSender)
            })?;

        info!("Waiting for upstream handshake response");
        let mut incoming: Sv2Frame = self.sv2_tp_channel.tp_receiver.recv().await.map_err(|e| {
            error!(?e, "Upstream connection closed during handshake");
            PoolError::shutdown(e)
        })?;

        let header = incoming.get_header().ok_or_else(|| {
            error!("Handshake frame missing header");
            PoolError::shutdown(framing_sv2::Error::MissingHeader)
        })?;
        debug!(
            ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Received upstream handshake response"
        );

        self.handle_common_message_frame_from_server(None, header, incoming.payload())
            .await?;
        info!("Handshake with upstream completed successfully");
        Ok(())
    }
}
