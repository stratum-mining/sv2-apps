//! Per-downstream connection management.
//!
//! Each TCP client that connects to the JDS downstream server is represented by a
//! [`Downstream`] instance. It owns the Noise-encrypted I/O tasks, the pending
//! `DeclareMiningJob` map, and a per-downstream [`CancellationToken`] for clean teardown.

use super::{
    ALLOCATED_TOKEN_TIMEOUT_SECS, DownstreamJobDeclarationMessage, JANITOR_INTERVAL_SECS,
    JobDeclarationMessage,
};
use crate::{
    error,
    error::{JDSResult, LoopControl},
    io_task::spawn_io_tasks,
};
use async_channel::{Receiver, Sender, unbounded};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        common_messages_sv2::MESSAGE_TYPE_SETUP_CONNECTION,
        handlers_sv2::HandleCommonMessagesFromClientOwnedAsync,
        job_declaration_sv2::DeclareMiningJobOwned,
        parsers_sv2::{AnyMessageOwned, parse_message_frame_with_tlvs},
    },
    sync::{SharedLock, SharedMap},
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{DownstreamId, InboundFrame, OutboundFrame, RequestId},
    },
};
use tracing::{debug, error, info, warn};

mod common_message_handler;

/// Data associated with a pending declare mining job.
/// - Instant is the insertion timestamp
/// - DeclareMiningJob is the declare mining job
pub type PendingDeclareMiningJob = (Instant, DeclareMiningJobOwned);

/// Channel endpoints for a single downstream connection.
#[derive(Clone)]
pub struct DownstreamIo {
    to_job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
    from_job_declarator_receiver: Receiver<JobDeclarationMessage>,
    to_downstream_sender: Sender<OutboundFrame>,
    from_downstream_receiver: Receiver<InboundFrame>,
}

/// Represents a downstream client connected to this node.
#[derive(Clone)]
pub struct Downstream {
    /// Extensions that have been successfully negotiated with this client.
    pub negotiated_extensions: SharedLock<Vec<u16>>,
    /// Jobs waiting for missing transactions (keyed by `request_id`).
    pub pending_declare_mining_jobs: SharedMap<RequestId, PendingDeclareMiningJob>,
    pub downstream_io: DownstreamIo,
    pub downstream_id: DownstreamId,
    /// Extensions that JDS supports
    #[allow(unused)]
    pub supported_extensions: Vec<u16>,
    /// Extensions that JDS requires
    #[allow(unused)]
    pub required_extensions: Vec<u16>,
    /// Per-downstream cancellation token (child of the global token).
    /// Cancelling this stops IO tasks, the pending jobs janitor, and the downstream loop
    /// without affecting other downstreams or the server.
    downstream_cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    fn handle_error_action(
        &self,
        context: &str,
        e: &error::JDSError<error::Downstream>,
    ) -> LoopControl {
        match e.action {
            error::Action::Log => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
            }
            error::Action::Disconnect(_) => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested disconnect"
                );
                LoopControl::Break
            }
            error::Action::Shutdown => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested shutdown"
                );
                LoopControl::Break
            }
        }
    }

    /// Creates a new [`Downstream`] and spawns its Noise I/O tasks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_id: DownstreamId,
        noise_stream: NoiseTcpStream,
        to_job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
        from_job_declarator_receiver: Receiver<JobDeclarationMessage>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        task_manager: Arc<TaskManager>,
        global_cancellation_token: CancellationToken,
    ) -> Self {
        let (noise_stream_reader, noise_stream_writer) = noise_stream.into_split();
        let (inbound_tx, inbound_rx) = unbounded::<InboundFrame>();
        let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

        let downstream_cancellation_token = global_cancellation_token.child_token();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            downstream_cancellation_token.clone(),
        );

        let negotiated_extensions = SharedLock::new(Vec::new());
        let pending_declare_mining_jobs = SharedMap::new();

        let downstream_io = DownstreamIo {
            to_job_declarator_sender,
            from_job_declarator_receiver,
            to_downstream_sender: outbound_tx,
            from_downstream_receiver: inbound_rx,
        };

        Self {
            negotiated_extensions,
            pending_declare_mining_jobs,
            downstream_io,
            downstream_id,
            supported_extensions,
            required_extensions,
            downstream_cancellation_token,
        }
    }

    /// Starts the downstream message loop.
    ///
    /// Performs the initial `SetupConnection` handshake, spawns the pending-jobs janitor, then
    /// enters a `tokio::select!` loop over inbound and outbound JDP messages.
    /// On exit (error, disconnect, or cancellation) it cancels its own
    /// [`CancellationToken`] and notifies [`JobDeclarator`](super::JobDeclarator)
    /// via `disconnect_sender`.
    pub async fn start(
        mut self,
        task_manager: Arc<TaskManager>,
        cleanup: impl FnOnce(DownstreamId) + Send + 'static,
    ) {
        // Setup initial connection
        if let Err(e) = self.setup_connection_with_downstream().await {
            error!(?e, "Failed to set up downstream connection");

            // sleep to make sure SetupConnectionError is sent
            // before we break the TCP connection
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            self.downstream_cancellation_token.cancel();

            warn!(
                downstream_id = self.downstream_id,
                "Downstream exiting before main loop; invoking cleanup callback"
            );

            // cleanup entry on JobDeclarator
            cleanup(self.downstream_id);

            return;
        }

        self.spawn_pending_jobs_janitor(task_manager.clone());

        let cancellation_token = self.downstream_cancellation_token.clone();
        task_manager.spawn(async move {
            let downstream_id = self.downstream_id;
            let mut self_clone_1 = self.clone();
            let self_clone_2 = self.clone();
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Downstream {downstream_id}: cancellation token triggered");
                        break;
                    }

                    res = self_clone_1.handle_message_from_downstream() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling downstream message for {downstream_id}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_message_from_downstream",
                                &e,
                            ) {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_job_declarator_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling job declarator message for {downstream_id}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_job_declarator_message",
                                &e,
                            ) {
                                break;
                            }
                        }
                    }
                }
            }
            cancellation_token.cancel();

            // cleanup entry on JobDeclarator
            cleanup(downstream_id);

            info!("Downstream {downstream_id}: disconnected.");
        });
    }

    /// Spawns a periodic task that evicts stale [`PendingDeclareMiningJob`] entries
    /// older than `ALLOCATED_TOKEN_TIMEOUT_SECS`.
    fn spawn_pending_jobs_janitor(&self, task_manager: Arc<TaskManager>) {
        let cancellation_token = self.downstream_cancellation_token.clone();
        let pending_declare_mining_jobs = self.pending_declare_mining_jobs.clone();
        let downstream_id = self.downstream_id;
        let token_timeout = Duration::from_secs(ALLOCATED_TOKEN_TIMEOUT_SECS);
        let janitor_interval = Duration::from_secs(JANITOR_INTERVAL_SECS);
        task_manager.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        break;
                    }
                    _ = tokio::time::sleep(janitor_interval) => {
                        pending_declare_mining_jobs.retain(|request_id, (inserted_at, _)| {
                            let expired = Instant::now().duration_since(*inserted_at) > token_timeout;
                            if expired {
                                debug!(
                                    downstream_id,
                                    request_id,
                                    "Removing expired pending declare mining job",
                                );
                            }
                            !expired
                        });
                    }
                }
            }
        });
    }

    async fn setup_connection_with_downstream(&mut self) -> JDSResult<(), error::Downstream> {
        let mut frame = self
            .downstream_io
            .from_downstream_receiver
            .recv()
            .await
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

        let header = frame.header();

        if header.ext_type() == 0 && header.msg_type() == MESSAGE_TYPE_SETUP_CONNECTION {
            self.handle_common_message_frame_from_client(
                Some(self.downstream_id),
                header,
                frame.payload(),
            )
            .await?;
        } else {
            return Err(error::JDSError::disconnect(
                error::JDSErrorKind::UnexpectedMessage(
                    header.ext_type_without_channel_msg(),
                    header.msg_type(),
                ),
                self.downstream_id,
            ));
        }

        Ok(())
    }

    // Handles messages from the job declarator to this downstream client.
    async fn handle_job_declarator_message(&self) -> JDSResult<(), error::Downstream> {
        let receiver = &self.downstream_io.from_job_declarator_receiver;

        // todo: handle tlv fields?
        let (msg, _tlv_fields) = match receiver.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Error receiving message: {:?}", e);
                return Err(error::JDSError::<error::Downstream>::disconnect(
                    e,
                    self.downstream_id,
                ));
            }
        };

        let message = AnyMessageOwned::JobDeclaration(msg);
        let std_frame = OutboundFrame::from_message(message)
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

        self.downstream_io
            .to_downstream_sender
            .send(std_frame)
            .await
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

        Ok(())
    }

    // Handles messages from this downstream client.
    async fn handle_message_from_downstream(&mut self) -> JDSResult<(), error::Downstream> {
        let mut sv2_frame = self
            .downstream_io
            .from_downstream_receiver
            .recv()
            .await
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;
        let header = sv2_frame.header();

        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::JobDeclaration => {
                debug!("Received mining SV2 frame from downstream.");
                let negotiated_extensions = self
                    .negotiated_extensions
                    .get()
                    .map_err(error::JDSError::shutdown)?;
                let (any_message, tlv_fields) = parse_message_frame_with_tlvs(
                    header,
                    sv2_frame.payload(),
                    &negotiated_extensions,
                )
                .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

                let jd_message = match any_message.into_owned() {
                    AnyMessageOwned::JobDeclaration(msg) => msg,
                    _ => {
                        error!("Expected JobDeclaration message but got different type");
                        return Err(error::JDSError::disconnect(
                            error::JDSErrorKind::UnexpectedMessage(
                                header.ext_type_without_channel_msg(),
                                header.msg_type(),
                            ),
                            self.downstream_id,
                        ));
                    }
                };

                self.downstream_io
                    .to_job_declarator_sender
                    .send((self.downstream_id, jd_message, tlv_fields))
                    .await
                    .map_err(error::JDSError::shutdown)?;
            }
            MessageType::Extensions => {
                // todo
                // self.handle_extensions_message_frame_from_client
            }
            MessageType::Common | MessageType::Mining | MessageType::TemplateDistribution => {
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

    // activates the downstream cancellation token, causing any pending tasks to be cancelled
    pub fn shutdown(&mut self) {
        self.downstream_cancellation_token.cancel();
    }
}
