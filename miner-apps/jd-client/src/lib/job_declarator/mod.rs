use std::{net::SocketAddr, sync::Arc};
use stratum_apps::stratum_core::parsers_sv2::{AnyMessageOwned, JobDeclarationOwned};

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
        parsers_sv2::JobDeclaration,
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{Message, OutboundFrame, SerializedFrame, Sv2Frame},
    },
};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::{
    error::{self, Action, JDCError, JDCErrorKind, JDCResult, LoopControl},
    io_task::spawn_io_tasks,
    jd_mode::JDMode,
    utils::{UpstreamEntry, get_setup_connection_message_jds},
};

mod message_handler;

/// Holds all channels required for Job Declarator communication.
#[derive(Clone)]
pub struct JobDeclaratorIo {
    channel_manager_sender: Sender<JobDeclarationOwned>,
    channel_manager_receiver: Receiver<JobDeclarationOwned>,
    jds_sender: Sender<OutboundFrame>,
    jds_receiver: Receiver<SerializedFrame>,
}

impl JobDeclaratorIo {
    fn close(&self) {
        self.channel_manager_sender.close();
        self.jds_sender.close();
        self.channel_manager_receiver.close_and_drain();
        self.jds_receiver.close_and_drain();
    }
}

/// Manages the lifecycle and communication with a Job Declarator (JDS)
#[allow(warnings)]
#[derive(Clone)]
pub struct JobDeclarator {
    /// Messaging channels to/from the channel manager and JD.
    job_declarator_io: JobDeclaratorIo,
    /// Socket address of the Job Declarator server.
    socket_address: SocketAddr,
    /// Config JDC mode
    mode: JDMode,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl JobDeclarator {
    fn handle_error_action(
        context: &str,
        e: &JDCError<error::JobDeclarator>,
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

    /// Creates a new JobDeclarator instance by connecting and performing a Noise handshake.
    ///
    /// - Resolves hostname to IP address via DNS (if not already an IP)
    /// - Establishes TCP connection.
    /// - Performs SV2 Noise handshake.
    /// - Spawns background IO tasks for reading/writing frames.
    pub async fn new(
        upstream_entry: &UpstreamEntry,
        channel_manager_sender: Sender<JobDeclarationOwned>,
        channel_manager_receiver: Receiver<JobDeclarationOwned>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        mode: JDMode,
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
        let stream = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(JDCError::fallback)?
            .map_err(JDCError::fallback)?;
        info!("Connection established with JD Server at {addr} in mode: {mode:?}");

        let (noise_stream_reader, noise_stream_writer) = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                info!("Shutdown received during handshake, dropping connection");
                return Err(JDCError::shutdown(JDCErrorKind::CouldNotInitiateSystem));
            }
            result = connect_with_noise(stream, Some(upstream_entry.authority_pubkey)) => {
                result.map_err(JDCError::fallback)?.into_split()
            }
        };

        let (inbound_tx, inbound_rx) = unbounded::<SerializedFrame>();
        let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            cancellation_token,
            Some(fallback_coordinator),
        );

        let job_declarator_io = JobDeclaratorIo {
            channel_manager_receiver,
            channel_manager_sender,
            jds_sender: outbound_tx,
            jds_receiver: inbound_rx,
        };
        Ok(JobDeclarator {
            job_declarator_io,
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
        task_manager: Arc<TaskManager>,
    ) {
        // we just spawned a new task that's relevant to fallback coordination
        // so register it with the fallback coordinator
        let fallback_handler = fallback_coordinator.register();

        // get the cancellation token that signals fallback
        let fallback_token = fallback_coordinator.token();
        if let Err(e) = self.setup_connection().await {
            _ = Self::handle_error_action(
                "JobDeclarator::setup_connection",
                &e,
                &cancellation_token,
                &fallback_token,
            );
            self.job_declarator_io.close();
            fallback_handler.done();
            return;
        }

        task_manager.spawn(async move {
            loop {
                let mut self_clone_1 = self.clone();
                let self_clone_2 = self.clone();
                tokio::select! {
                    biased;
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
                            if let LoopControl::Break = Self::handle_error_action(
                                "JobDeclarator::handle_job_declarator_message",
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
                            error!(error = ?e, "Channel Manager message handling failed");
                            if let LoopControl::Break = Self::handle_error_action(
                                "JobDeclarator::handle_channel_manager_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    },
                }
            }
            self.job_declarator_io.close();
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

        if let Err(e) = self
            .job_declarator_io
            .jds_sender
            .send(sv2_frame.into())
            .await
        {
            error!(error=?e, "Failed to send SetupConnection frame.");
            return Err(JDCError::fallback(JDCErrorKind::ChannelErrorSender));
        }
        debug!("SetupConnection frame sent successfully.");

        let mut incoming = self
            .job_declarator_io
            .jds_receiver
            .recv()
            .await
            .map_err(|e| {
                error!(error=?e, "No handshake response received from Job declarator.");
                JDCError::fallback(JDCErrorKind::ChannelErrorSender)
            })?;

        let header = incoming.header();

        debug!(ext_type = ?header.ext_type(),
            msg_type = ?header.msg_type(),
            "Processing handshake response.");

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

        info!("Job declarator: SV2 handshake completed successfully.");
        Ok(())
    }

    // Handles messages coming from the Channel Manager and forwards them to the Job Declarator.
    async fn handle_channel_manager_message(&self) -> JDCResult<(), error::JobDeclarator> {
        match self.job_declarator_io.channel_manager_receiver.recv().await {
            Ok(msg) => {
                debug!("Forwarding message from channel manager to JDS.");
                let message = AnyMessageOwned::JobDeclaration(msg);
                let sv2_frame: Sv2Frame = message.try_into().map_err(JDCError::shutdown)?;
                self.job_declarator_io
                    .jds_sender
                    .send(sv2_frame.into())
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
            .job_declarator_io
            .jds_receiver
            .recv()
            .await
            .map_err(JDCError::fallback)?;

        debug!("Received SV2 frame from JDS.");
        let header = sv2_frame.header();
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
                    .into_owned();
                self.job_declarator_io
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

#[cfg(test)]
mod tests {

    // A peer sends framed bytes, so a test standing in for one has to frame and serialize the
    // message it injects.
    fn serialize_frame(message: Message) -> SerializedFrame {
        use stratum_apps::stratum_core::codec_sv2::EncodableFrame as _;
        let frame: Sv2Frame = message
            .try_into()
            .expect("Failed to frame the injected message");
        let mut bytes = vec![0u8; frame.encoded_length()];
        frame
            .encode_into(&mut bytes)
            .expect("Failed to serialize the injected frame");
        SerializedFrame::from_bytes(bytes.into()).expect("Injected frame is a whole frame")
    }
    use super::*;
    use crate::config::ConfigJDCMode;
    use stratum_apps::stratum_core::{
        common_messages_sv2::ChannelEndpointChangedOwned, parsers_sv2::CommonMessagesOwned,
    };

    #[tokio::test]
    async fn setup_connection_rejects_channel_endpoint_changed_response() {
        let (channel_manager_sender, _jd_to_cm_receiver) = unbounded();
        let (_cm_to_jd_sender, channel_manager_receiver) = unbounded();
        let (jds_sender, _jds_outbound_receiver) = unbounded();
        let (jds_inbound_sender, jds_receiver) = unbounded();

        let mut jd = JobDeclarator {
            job_declarator_io: JobDeclaratorIo {
                channel_manager_sender,
                channel_manager_receiver,
                jds_sender,
                jds_receiver,
            },
            socket_address: "127.0.0.1:1234".parse().expect("valid socket address"),
            mode: JDMode::new(ConfigJDCMode::FullTemplate),
        };

        let response: Message = Message::Common(CommonMessagesOwned::ChannelEndpointChanged(
            ChannelEndpointChangedOwned { channel_id: 0 },
        ));

        jds_inbound_sender
            .send(serialize_frame(response))
            .await
            .expect("Failed to inject ChannelEndpointChanged response");

        assert!(
            jd.setup_connection().await.is_err(),
            "setup_connection must reject responses other than SetupConnectionSuccess"
        );
        assert!(jd.mode.is_solo_mining());
    }
}
