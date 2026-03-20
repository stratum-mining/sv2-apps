//! Per-downstream connection management.
//!
//! Each TCP client that connects to the JDS downstream server is represented by a
//! [`Downstream`] instance. It owns the Noise-encrypted I/O tasks, the pending
//! `DeclareMiningJob` map, and a per-downstream [`CancellationToken`] for clean teardown.

use super::{
    DownstreamJobDeclarationMessage, JobDeclarationMessage, ALLOCATED_TOKEN_TIMEOUT_SECS,
    JANITOR_INTERVAL_SECS,
};
use crate::{error, error::JDSResult, io_task::spawn_io_tasks};
use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::job_declaration_protocol::CancellationToken;
use dashmap::DashMap;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use stratum_apps::{
    custom_mutex::Mutex,
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        common_messages_sv2::MESSAGE_TYPE_SETUP_CONNECTION,
        framing_sv2,
        handlers_sv2::HandleCommonMessagesFromClientAsync,
        job_declaration_sv2::DeclareMiningJob,
        parsers_sv2::{parse_message_frame_with_tlvs, AnyMessage},
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{protocol_message_type, MessageType},
        types::{DownstreamId, Message, RequestId, Sv2Frame},
    },
};
use tracing::{debug, error, info, warn};

mod common_message_handler;

/// Data associated with a pending declare mining job.
/// - Instant is the insertion timestamp
/// - DeclareMiningJob is the declare mining job
pub type PendingDeclareMiningJob = (Instant, DeclareMiningJob<'static>);

/// Channel endpoints for a single downstream connection.
#[derive(Clone)]
pub struct DownstreamIo {
    to_job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
    from_job_declarator_receiver: Receiver<JobDeclarationMessage>,
    to_downstream_sender: Sender<Sv2Frame>,
    from_downstream_receiver: Receiver<Sv2Frame>,
}

/// Represents a downstream client connected to this node.
#[derive(Clone)]
pub struct Downstream {
    /// Extensions that have been successfully negotiated with this client.
    pub negotiated_extensions: Arc<Mutex<Vec<u16>>>,
    /// Jobs waiting for missing transactions (keyed by `request_id`).
    pub pending_declare_mining_jobs: Arc<DashMap<RequestId, PendingDeclareMiningJob>>,
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
    /// Global/server cancellation token.
    /// Used to trigger full shutdown when critical coordination fails.
    global_cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    /// Creates a new [`Downstream`] and spawns its Noise I/O tasks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_id: DownstreamId,
        noise_stream: NoiseTcpStream<Message>,
        to_job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
        from_job_declarator_receiver: Receiver<JobDeclarationMessage>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        task_manager: Arc<TaskManager>,
        global_cancellation_token: CancellationToken,
    ) -> Self {
        let (noise_stream_reader, noise_stream_writer) = noise_stream.into_split();
        let (inbound_tx, inbound_rx) = unbounded::<Sv2Frame>();
        let (outbound_tx, outbound_rx) = unbounded::<Sv2Frame>();

        let downstream_cancellation_token = global_cancellation_token.child_token();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            downstream_cancellation_token.clone(),
        );

        let negotiated_extensions = Arc::new(Mutex::new(Vec::new()));
        let pending_declare_mining_jobs = Arc::new(DashMap::new());

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
            global_cancellation_token,
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
        disconnect_sender: Sender<DownstreamId>,
        task_manager: Arc<TaskManager>,
    ) {
        // Setup initial connection
        if let Err(e) = self.setup_connection_with_downstream().await {
            error!(?e, "Failed to set up downstream connection");

            // sleep to make sure SetupConnectionError is sent
            // before we break the TCP connection
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            self.downstream_cancellation_token.cancel();

            if let Err(e) = disconnect_sender.send(self.downstream_id).await {
                error!(
                    downstream_id = self.downstream_id,
                    error = ?e,
                    "Failed to notify job declarator about downstream disconnect; triggering global shutdown"
                );
                self.global_cancellation_token.cancel();
            }
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
                            match e.action {
                                error::Action::Disconnect(_) => break,
                                error::Action::Shutdown => break,
                                error::Action::Log => {}
                            }
                        }
                    }
                    res = self_clone_2.handle_job_declarator_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling job declarator message for {downstream_id}");
                            match e.action {
                                error::Action::Disconnect(_) => break,
                                error::Action::Shutdown => break,
                                error::Action::Log => {}
                            }
                        }
                    }
                }
            }
            cancellation_token.cancel();
            info!("Downstream {downstream_id}: disconnected, notifying job declarator");
            if let Err(e) = disconnect_sender.send(downstream_id).await {
                error!(
                    downstream_id,
                    error = ?e,
                    "Failed to notify job declarator about downstream disconnect; triggering global shutdown"
                );
                self.global_cancellation_token.cancel();
            }
        });
    }

    /// Spawns a periodic task that evicts stale [`PendingDeclareMiningJob`] entries
    /// older than `ALLOCATED_TOKEN_TIMEOUT_SECS`.
    fn spawn_pending_jobs_janitor(&self, task_manager: Arc<TaskManager>) {
        let cancellation_token = self.downstream_cancellation_token.clone();
        let pending_declare_mining_jobs = Arc::clone(&self.pending_declare_mining_jobs);
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

        let header = frame.get_header().ok_or_else(|| {
            error!("SV2 frame missing header");
            error::JDSError::disconnect(framing_sv2::Error::MissingHeader, self.downstream_id)
        })?;

        if header.msg_type() == MESSAGE_TYPE_SETUP_CONNECTION {
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

        let message = AnyMessage::JobDeclaration(msg);
        let std_frame: Sv2Frame = message
            .try_into()
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
        let header = sv2_frame.get_header().ok_or_else(|| {
            error!("SV2 frame missing header");
            error::JDSError::disconnect(framing_sv2::Error::MissingHeader, self.downstream_id)
        })?;

        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::JobDeclaration => {
                debug!("Received mining SV2 frame from downstream.");
                let negotiated_extensions = self
                    .negotiated_extensions
                    .super_safe_lock(|extensions| extensions.clone());
                let (any_message, tlv_fields) = parse_message_frame_with_tlvs(
                    header,
                    sv2_frame.payload(),
                    &negotiated_extensions,
                )
                .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

                let jd_message = match any_message {
                    AnyMessage::JobDeclaration(msg) => msg,
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
}
