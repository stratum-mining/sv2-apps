use std::{net::SocketAddr, sync::Arc};

use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::template_distribution_protocol::CancellationToken;
use stratum_apps::{
    custom_mutex::Mutex,
    fallback_coordinator::FallbackCoordinator,
    network_helpers::{connect_with_noise, resolve_host},
    stratum_core::{
        framing_sv2,
        handlers_sv2::HandleCommonMessagesFromServerAsync,
        parsers_sv2::{AnyMessage, JobDeclaration},
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
    config::ConfigJDCMode,
    error::{self, JDCError, JDCErrorKind, JDCResult},
    io_task::spawn_io_tasks,
    status::{handle_error, Status, StatusSender},
    utils::{get_setup_connection_message_jds, UpstreamEntry},
};

mod message_handler;

/// Shared state for Job Declarator
pub struct JobDeclaratorData;

/// Holds all channels required for Job Declarator communication.
#[derive(Clone)]
pub struct JobDeclaratorChannel {
    channel_manager_sender: Sender<JobDeclaration<'static>>,
    channel_manager_receiver: Receiver<JobDeclaration<'static>>,
    jds_sender: Sender<Sv2Frame>,
    jds_receiver: Receiver<Sv2Frame>,
}

/// Manages the lifecycle and communication with a Job Declarator (JDS)
#[allow(warnings)]
#[derive(Clone)]
pub struct JobDeclarator {
    /// Internal state
    job_declarator_data: Arc<Mutex<JobDeclaratorData>>,
    /// Messaging channels to/from the channel manager and JD.
    job_declarator_channel: JobDeclaratorChannel,
    /// Socket address of the Job Declarator server.
    socket_address: SocketAddr,
    /// Config JDC mode
    mode: ConfigJDCMode,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl JobDeclarator {
    /// Creates a new JobDeclarator instance by connecting and performing a Noise handshake.
    ///
    /// - Resolves hostname to IP address via DNS (if not already an IP)
    /// - Establishes TCP connection.
    /// - Performs SV2 Noise handshake.
    /// - Spawns background IO tasks for reading/writing frames.
    pub async fn new(
        upstream_entry: &UpstreamEntry,
        channel_manager_sender: Sender<JobDeclaration<'static>>,
        channel_manager_receiver: Receiver<JobDeclaration<'static>>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        mode: ConfigJDCMode,
        task_manager: Arc<TaskManager>,
    ) -> JDCResult<Self, error::JobDeclarator> {
        let addr = resolve_host(&upstream_entry.jds_host, upstream_entry.jds_port)
            .await
            .map_err(|e| {
                error!(
                    "Failed to resolve JDS address {}:{}: {e}",
                    upstream_entry.jds_host, upstream_entry.jds_port
                );
                JDCError::fallback(JDCErrorKind::NetworkHelpersError(e.into()))
            })?;

        info!("Connecting to JD Server at {addr}");
        let stream = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            TcpStream::connect(addr),
        )
        .await
        .map_err(JDCError::fallback)?
        .map_err(JDCError::fallback)?;
        info!("Connection established with JD Server at {addr} in mode: {mode:?}");

        let (noise_stream_reader, noise_stream_writer) = tokio::select! {
            result = connect_with_noise(stream, Some(upstream_entry.authority_pubkey)) => {
                result.map_err(JDCError::fallback)?.into_split()
            }
            _ = cancellation_token.cancelled() => {
                info!("Shutdown received during handshake, dropping connection");
                return Err(JDCError::shutdown(JDCErrorKind::CouldNotInitiateSystem));
            }
        };

        let (inbound_tx, inbound_rx) = unbounded::<Sv2Frame>();
        let (outbound_tx, outbound_rx) = unbounded::<Sv2Frame>();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            cancellation_token,
            fallback_coordinator,
        );
        let job_declarator_data = Arc::new(Mutex::new(JobDeclaratorData));
        let job_declarator_channel = JobDeclaratorChannel {
            channel_manager_receiver,
            channel_manager_sender,
            jds_sender: outbound_tx,
            jds_receiver: inbound_rx,
        };
        Ok(JobDeclarator {
            job_declarator_channel,
            job_declarator_data,
            socket_address: addr,
            mode,
        })
    }

    /// Starts the JobDeclarator message loop.
    ///
    /// - Waits for shutdown signals.
    /// - Handles incoming messages from Job Declarator and Channel Manager.
    /// - Cleans up on termination.
    pub async fn start(
        mut self,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
    ) {
        let status_sender = StatusSender::JobDeclarator(status_sender);

        if let Err(e) = self.setup_connection().await {
            handle_error(&status_sender, e).await;
            return;
        }

        task_manager.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();

            loop {
                let mut self_clone_1 = self.clone();
                let self_clone_2 = self.clone();
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Job Declarator: received shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("Job Declarator: fallback triggered");
                        break;
                    }
                    res = self_clone_1.handle_job_declarator_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Job Declarator message handling failed");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_channel_manager_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Channel Manager message handling failed");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    },
                }
            }
            warn!("JobDeclarator: unified message loop exited.");

            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });
    }

    /// Performs SV2 setup connection handshake with Job Declarator server.
    ///
    /// - Sends `SetupConnection` message.
    /// - Waits for and validates server response.
    /// - Completes SV2 protocol handshake.
    pub async fn setup_connection(&mut self) -> JDCResult<(), error::JobDeclarator> {
        info!("Sending SetupConnection to JDS at {}", self.socket_address);

        let setup_connection = get_setup_connection_message_jds(&self.socket_address, &self.mode);
        let sv2_frame: Sv2Frame = Message::Common(setup_connection.into())
            .try_into()
            .map_err(|e| {
                error!(error=?e, "Failed to serialize SetupConnection message.");
                JDCError::shutdown(e)
            })?;

        if let Err(e) = self.job_declarator_channel.jds_sender.send(sv2_frame).await {
            error!(error=?e, "Failed to send SetupConnection frame.");
            return Err(JDCError::fallback(JDCErrorKind::ChannelErrorSender));
        }
        debug!("SetupConnection frame sent successfully.");

        let mut incoming = self
            .job_declarator_channel
            .jds_receiver
            .recv()
            .await
            .map_err(|e| {
                error!(error=?e, "No handshake response received from Job declarator.");
                JDCError::fallback(JDCErrorKind::ChannelErrorSender)
            })?;

        let header = incoming.get_header().ok_or_else(|| {
            error!("Handshake frame missing header.");
            JDCError::fallback(framing_sv2::Error::MissingHeader)
        })?;

        debug!(ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Processing handshake response.");

        self.handle_common_message_frame_from_server(None, header, incoming.payload())
            .await?;

        info!("Job declarator: SV2 handshake completed successfully.");
        Ok(())
    }

    // Handles messages coming from the Channel Manager and forwards them to the Job Declarator.
    async fn handle_channel_manager_message(&self) -> JDCResult<(), error::JobDeclarator> {
        match self
            .job_declarator_channel
            .channel_manager_receiver
            .recv()
            .await
        {
            Ok(msg) => {
                debug!("Forwarding message from channel manager to JDS.");
                let message = AnyMessage::JobDeclaration(msg);
                let sv2_frame: Sv2Frame = message.try_into().map_err(JDCError::shutdown)?;
                self.job_declarator_channel
                    .jds_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!("Failed to send message to outbound channel: {:?}", e);
                        JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            Err(e) => {
                warn!("Channel manager receiver closed or errored: {:?}", e);
            }
        }
        Ok(())
    }

    // Handles messages received from the Job Declarator.
    //
    // - Forwards `JobDeclaration` messages to Channel Manager.
    // - Processes `Common` messages via handler.
    // - Rejects unsupported message types.
    async fn handle_job_declarator_message(&mut self) -> JDCResult<(), error::JobDeclarator> {
        let mut sv2_frame = self
            .job_declarator_channel
            .jds_receiver
            .recv()
            .await
            .map_err(JDCError::fallback)?;

        debug!("Received SV2 frame from JDS.");
        let header = sv2_frame.get_header().ok_or_else(|| {
            error!("SV2 frame missing header");
            JDCError::fallback(framing_sv2::Error::MissingHeader)
        })?;
        let message_type = header.msg_type();
        let extension_type = header.ext_type();

        match protocol_message_type(extension_type, message_type) {
            MessageType::Common => {
                info!(ext_type = ?extension_type, msg_type = ?message_type, "Handling common message from Upstream.");
                self.handle_common_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::JobDeclaration => {
                let message = JobDeclaration::try_from((message_type, sv2_frame.payload()))
                    .map_err(JDCError::fallback)?
                    .into_static();
                self.job_declarator_channel
                    .channel_manager_sender
                    .send(message)
                    .await
                    .map_err(|e| {
                        error!(error=?e, "Failed to send Job declaration message to channel manager.");
                        JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            _ => {
                warn!("Received unsupported message type from Job declarator: {message_type}");
            }
        }

        Ok(())
    }
}
