use std::sync::Arc;
use stratum_apps::stratum_core::parsers_sv2::{AnyMessageOwned, TemplateDistributionOwned};
mod common_message_handler;
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
        parsers_sv2::TemplateDistribution,
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{Message, OutboundFrame, SerializedFrame, Sv2Frame},
    },
};
use tokio::{net::TcpStream, time::timeout};
use tracing::{debug, error, info, warn};

use crate::{
    error::{self, Action, LoopControl, PoolError, PoolErrorKind, PoolResult},
    io_task::spawn_io_tasks,
    utils::get_setup_connection_message_tp,
};

#[derive(Clone)]
pub struct Sv2TpIo {
    channel_manager_sender: Sender<TemplateDistributionOwned>,
    channel_manager_receiver: Receiver<TemplateDistributionOwned>,
    tp_sender: Sender<OutboundFrame>,
    tp_receiver: Receiver<SerializedFrame>,
}

impl Sv2TpIo {
    fn close(&self) {
        self.channel_manager_sender.close();
        self.tp_sender.close();
        self.channel_manager_receiver.close_and_drain();
        self.tp_receiver.close_and_drain();
    }
}

#[derive(Clone)]
pub struct Sv2Tp {
    sv2_tp_io: Sv2TpIo,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv2Tp {
    fn handle_error_action(
        context: &str,
        e: &PoolError<error::TemplateProvider>,
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
                warn!(error_kind = ?e.kind, "{context} returned a log-only error");
                LoopControl::Continue
            }
            Action::Shutdown => {
                warn!(error_kind = ?e.kind, "{context} requested shutdown");
                cancellation_token.cancel();
                LoopControl::Break
            }
            other => {
                warn!(action = ?other, error_kind = ?e.kind, "{context} returned an unhandled action");
                LoopControl::Continue
            }
        }
    }

    /// Establish a new connection to a Sv2 Template Provider.
    ///
    /// - Opens a TCP connection
    /// - Performs Noise handshake
    /// - Spawns IO tasks for inbound/outbound frames
    ///
    /// Retries up to 3 times before returning [`PoolError::shutdown`].
    pub async fn new(
        tp_address: String,
        public_key: Option<Secp256k1PublicKey>,
        channel_manager_receiver: Receiver<TemplateDistributionOwned>,
        channel_manager_sender: Sender<TemplateDistributionOwned>,
        cancellation_token: CancellationToken,
        task_manager: Arc<TaskManager>,
    ) -> PoolResult<Sv2Tp, error::TemplateProvider> {
        const MAX_RETRIES: usize = 3;

        for attempt in 1..=MAX_RETRIES {
            info!(attempt, MAX_RETRIES, "Connecting to template provider");

            match timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(tp_address.as_str()))
                .await
                .map_err(PoolError::shutdown)?
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
                            return Err(PoolError::shutdown(PoolErrorKind::CouldNotInitiateSystem))
                        }
                        result = connect_with_noise(stream, public_key) => {
                            match result {
                                Ok(noise_stream) => {
                                    info!(attempt, "Noise handshake completed successfully");

                                    let (noise_stream_reader, noise_stream_writer) =
                                        noise_stream.into_split();

                                    let (inbound_tx, inbound_rx) = unbounded::<SerializedFrame>();
                                    let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

                                    info!(attempt, "Spawning IO tasks for template receiver");

                                    spawn_io_tasks(
                                        task_manager.clone(),
                                        noise_stream_reader,
                                        noise_stream_writer,
                                        outbound_rx,
                                        inbound_tx,
                                        cancellation_token.clone(),
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
        task_manager: Arc<TaskManager>,
    ) -> PoolResult<(), error::TemplateProvider> {
        info!("Initialized state for starting template receiver");
        if let Err(e) = self.setup_connection(socket_address).await {
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
                        info!("Template Receiver: received shutdown signal");
                        break;
                    }
                    res = self_clone_1.handle_template_provider_message() => {
                        if let Err(e) = res {
                            error!("TemplateReceiver template provider handler failed: {e:?}");
                            if let LoopControl::Break = Self::handle_error_action(
                                "Sv2Tp::handle_template_provider_message",
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
                                "Sv2Tp::handle_channel_manager_message",
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
    /// - `TemplateDistribution` messages → forwarded to ChannelManager
    /// - Unsupported messages → logged and ignored
    pub async fn handle_template_provider_message(
        &mut self,
    ) -> PoolResult<(), error::TemplateProvider> {
        let mut sv2_frame = self
            .sv2_tp_io
            .tp_receiver
            .recv()
            .await
            .map_err(PoolError::shutdown)?;
        debug!("Received SV2 frame from Template provider.");
        let header = sv2_frame.header();

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
                        .into_owned();

                self.sv2_tp_io
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
            .sv2_tp_io
            .channel_manager_receiver
            .recv()
            .await
            .map_err(PoolError::shutdown)?;
        let message = AnyMessageOwned::TemplateDistribution(msg);
        let frame: Sv2Frame = message.try_into().map_err(PoolError::shutdown)?;

        debug!("Forwarding message from channel manager to outbound_tx");
        self.sv2_tp_io
            .tp_sender
            .send(frame.into())
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
        self.sv2_tp_io
            .tp_sender
            .send(frame.into())
            .await
            .map_err(|_| {
                error!("Failed to send setup connection message upstream");
                PoolError::shutdown(PoolErrorKind::ChannelErrorSender)
            })?;

        info!("Waiting for upstream handshake response");
        let mut incoming: SerializedFrame =
            self.sv2_tp_io.tp_receiver.recv().await.map_err(|e| {
                error!(?e, "Upstream connection closed during handshake");
                PoolError::shutdown(e)
            })?;

        let header = incoming.header();
        debug!(
            ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Received upstream handshake response"
        );

        if header.ext_type() != 0
            || !matches!(
                header.msg_type(),
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS | MESSAGE_TYPE_SETUP_CONNECTION_ERROR
            )
        {
            return Err(PoolError::shutdown(PoolErrorKind::UnexpectedMessage(
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
