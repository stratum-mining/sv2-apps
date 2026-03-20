//! Core Job Declaration engine.
//!
//! [`JobDeclarator`] is the central type: it owns the [`TokenManager`], a
//! [`JobValidationEngine`] backend, and the I/O channels that connect it to downstream
//! clients. Its lifecycle follows a `new` -> `start` / `start_downstream_server` -> `shutdown`
//! pattern.

use crate::{
    error,
    error::{JDSError, JDSErrorKind, JDSResult},
    job_declarator::{
        downstream::Downstream,
        job_validation::{JobValidationEngine, SetCustomMiningJobResult},
        token_management::TokenManager,
    },
};
use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::job_declaration_protocol::CancellationToken;
use dashmap::DashMap;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use stratum_apps::{
    config_helpers::CoinbaseRewardScript,
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::accept_noise_connection,
    stratum_core::{
        handlers_sv2::HandleJobDeclarationMessagesFromClientAsync,
        mining_sv2::{SetCustomMiningJob, SetCustomMiningJobError, SetCustomMiningJobSuccess},
        parsers_sv2::{JobDeclaration, Tlv},
    },
    task_manager::TaskManager,
    utils::types::{DownstreamId, JdToken},
};
use tokio::net::TcpListener;
use tracing::{error, info};

// see https://github.com/stratum-mining/sv2-apps/issues/335
const TEMPORARY_TIMEOUT_MULTIPLIER: u64 = 144;

/// Timeout for allocated tokens that haven't yet been activated.
/// Ideally 10 minutes, temporarily 24h. see https://github.com/stratum-mining/sv2-apps/issues/335
const ALLOCATED_TOKEN_TIMEOUT_SECS: u64 = TEMPORARY_TIMEOUT_MULTIPLIER * 60 * 10;

/// Timeout for active tokens (10 seconds).
const ACTIVE_TOKEN_TIMEOUT_SECS: u64 = 10;

/// How often the janitor tasks run to clean up expired tokens and pending jobs (seconds).
const JANITOR_INTERVAL_SECS: u64 = 10;

mod downstream;
mod job_declaration_message_handler;
pub mod job_validation;
pub mod token_management;

/// Shared JDP payload exchanged between Job Declarator and downstreams.
type JobDeclarationMessage = (JobDeclaration<'static>, Option<Vec<Tlv>>);

/// Shared JDP payload sent from downstreams to Job Declarator, tagged with downstream id.
type DownstreamJobDeclarationMessage = (DownstreamId, JobDeclaration<'static>, Option<Vec<Tlv>>);

/// The response produced by [`JobDeclarator::handle_set_custom_mining_job`].
///
/// This is a Mining Protocol (MP) message, not a JDP message — it is returned to the
/// caller (typically the Pool) rather than sent over the JDP TCP socket.
#[derive(Debug)]
pub enum SetCustomMiningJobResponse<'a> {
    Ok(SetCustomMiningJobSuccess),
    Error(SetCustomMiningJobError<'a>),
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl SetCustomMiningJobResponse<'_> {
    fn error(request_id: u32, channel_id: u32, error_code: &str) -> Self {
        SetCustomMiningJobResponse::Error(SetCustomMiningJobError {
            request_id,
            channel_id,
            error_code: error_code
                .to_string()
                .try_into()
                .expect("error code must be valid Str0255"),
        })
    }
}

/// Channel endpoints that connect `JobDeclarator` to its downstream clients.
///
/// - `downstream_client_senders`: per-downstream senders for JDP responses.
/// - `job_declarator_sender/receiver`: fan-in channel carrying JDP requests from all downstreams to
///   the central message loop.
/// - `disconnect_sender/receiver`: channel through which downstreams signal disconnection.
#[derive(Clone)]
pub struct JobDeclaratorIo {
    downstream_client_senders: DashMap<DownstreamId, Sender<JobDeclarationMessage>>,
    job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
    job_declarator_receiver: Receiver<DownstreamJobDeclarationMessage>,
    disconnect_sender: Sender<DownstreamId>,
    disconnect_receiver: Receiver<DownstreamId>,
}

/// Central engine for the Job Declaration Protocol.
///
/// Owns the [`TokenManager`], shared data, I/O channels, and delegates block-level
/// validation to the backend.
#[derive(Clone)]
pub struct JobDeclarator {
    token_manager: TokenManager,
    job_validator: Arc<dyn JobValidationEngine>,
    job_declarator_io: Arc<JobDeclaratorIo>,
    coinbase_reward_script: CoinbaseRewardScript,
    downstream_clients: Arc<DashMap<DownstreamId, Downstream>>,
    downstream_id_factory: Arc<AtomicUsize>,
}

/// Constructor of `JobDeclarator` with a pluggable [`JobValidationEngine`] backend.
#[cfg_attr(not(test), hotpath::measure_all)]
impl JobDeclarator {
    pub async fn new(
        engine: Arc<dyn JobValidationEngine>,
        cancellation_token: CancellationToken,
        coinbase_reward_script: CoinbaseRewardScript,
        task_manager: Arc<TaskManager>,
    ) -> Result<Self, JDSErrorKind> {
        let (job_declarator_sender, job_declarator_receiver) =
            unbounded::<DownstreamJobDeclarationMessage>();
        let (disconnect_sender, disconnect_receiver) = unbounded::<DownstreamId>();
        let job_declarator_io = Arc::new(JobDeclaratorIo {
            job_declarator_sender,
            job_declarator_receiver,
            downstream_client_senders: DashMap::new(),
            disconnect_sender,
            disconnect_receiver,
        });

        let token_manager =
            TokenManager::new(cancellation_token.clone(), Arc::clone(&task_manager));

        Ok(Self {
            token_manager,
            job_validator: engine,
            job_declarator_io,
            coinbase_reward_script,
            downstream_clients: Arc::new(DashMap::new()),
            downstream_id_factory: Arc::new(AtomicUsize::new(0)),
        })
    }
}

/// Generic implementation for all [`JobValidationEngine`] types.
impl JobDeclarator {
    /// Binds a TCP listener and spawns the accept loop that creates a `Downstream`
    /// for every new Noise-encrypted connection.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_downstream_server(
        self,
        authority_public_key: Secp256k1PublicKey,
        authority_secret_key: Secp256k1SecretKey,
        cert_validity_sec: u64,
        listening_address: SocketAddr,
        task_manager: Arc<TaskManager>,
        cancellation_token: CancellationToken,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> JDSResult<(), error::JobDeclarator> {
        info!("Starting downstream server at {listening_address}");
        let server = TcpListener::bind(listening_address)
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to bind downstream server at {listening_address}");
                e
            })
            .map_err(JDSError::shutdown)?;

        let task_manager_clone = task_manager.clone();
        let cancellation_token_clone = cancellation_token.clone();
        task_manager.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token_clone.cancelled() => {
                        info!("Job Declarator: cancellation token triggered");
                        break;
                    }
                    res = server.accept() => {
                        match res {
                            Ok((stream, socket_address)) => {
                                info!(%socket_address, "New downstream connection");

                                let this = self.clone();
                                let cancellation_token_inner = cancellation_token_clone.clone();
                                let task_manager_inner = task_manager_clone.clone();
                                let supported_extensions_inner = supported_extensions.clone();
                                let required_extensions_inner = required_extensions.clone();

                                task_manager_clone.spawn(async move {
                                    let noise_stream = tokio::select! {
                                        result = accept_noise_connection(
                                            stream,
                                            authority_public_key,
                                            authority_secret_key,
                                            cert_validity_sec,
                                        ) => {
                                            match result {
                                                Ok(r) => r,
                                                Err(e) => {
                                                    error!(error = ?e, "Noise handshake failed");
                                                    return;
                                                }
                                            }
                                        }
                                        _ = cancellation_token_inner.cancelled() => {
                                            info!("Shutdown received during handshake, dropping connection");
                                            return;
                                        }
                                    };

                                    let downstream_id = this
                                        .downstream_id_factory
                                        .fetch_add(1, Ordering::SeqCst);

                                    let (to_downstream_sender, to_downstream_receiver) =
                                        unbounded::<JobDeclarationMessage>();
                                    let to_job_declarator_sender =
                                        this.job_declarator_io.job_declarator_sender.clone();

                                    let downstream = Downstream::new(
                                        downstream_id,
                                        noise_stream,
                                        to_job_declarator_sender,
                                        to_downstream_receiver,
                                        supported_extensions_inner,
                                        required_extensions_inner,
                                        task_manager_inner.clone(),
                                        cancellation_token_inner.clone(),
                                    );

                                    this.downstream_clients
                                        .insert(downstream_id, downstream.clone());

                                    this.job_declarator_io
                                        .downstream_client_senders
                                        .insert(downstream_id, to_downstream_sender);

                                    let disconnect_sender =
                                        this.job_declarator_io.disconnect_sender.clone();

                                    downstream
                                        .start(disconnect_sender, task_manager_inner)
                                        .await;
                                });
                            }
                            Err(e) => {
                                error!(error = ?e, "Failed to accept new downstream connection");
                            }
                        }
                    }
                }
            }
            info!("Downstream server: Unified loop break");
        });

        Ok(())
    }

    /// Spawns the central JDP message loop.
    ///
    /// The loop multiplexes over:
    /// - Incoming JDP messages from all downstreams.
    /// - Disconnect notifications from individual downstreams.
    /// - The global cancellation token.
    pub async fn start(
        mut self,
        cancellation_token: CancellationToken,
        task_manager: Arc<TaskManager>,
    ) -> JDSResult<(), error::JobDeclarator> {
        let disconnect_receiver = self.job_declarator_io.disconnect_receiver.clone();

        task_manager.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Job Declarator: cancellation token triggered");
                        break;
                    }
                    res = self.handle_jdp_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling Job Declaration message");
                            match e.action {
                                error::Action::Disconnect(downstream_id) => {
                                    self.cleanup_downstream(downstream_id);
                                }
                                error::Action::Shutdown => break,
                                error::Action::Log => {}
                            }
                        }
                    }
                    res = disconnect_receiver.recv() => {
                        match res {
                            Ok(downstream_id) => {
                                self.cleanup_downstream(downstream_id);
                            }
                            Err(_) => {
                                error!("Disconnect channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// Graceful shutdown helper.
    ///
    /// Closes internal fan-in/fan-out channels and clears downstream maps so spawned
    /// JDS tasks can drain quickly. We intentionally avoid `token_manager.clear()` here
    /// because it can contend with concurrent downstream cleanup during shutdown.
    pub fn shutdown(&self) {
        info!("JobDeclarator: shutting down");

        self.job_declarator_io.job_declarator_sender.close();
        self.job_declarator_io.job_declarator_receiver.close();
        self.job_declarator_io.disconnect_sender.close();
        self.job_declarator_io.disconnect_receiver.close();
        self.job_declarator_io.downstream_client_senders.clear();
        self.downstream_clients.clear();

        // Let the validation backend tear down any dedicated resources/threads.
        self.job_validator.shutdown();

        info!("JobDeclarator: shutdown complete");
    }

    /// Removes a downstream from all internal maps and cleans up its tokens.
    fn cleanup_downstream(&self, downstream_id: DownstreamId) {
        info!(downstream_id, "Cleaning up disconnected downstream");

        self.downstream_clients.remove(&downstream_id);

        self.job_declarator_io
            .downstream_client_senders
            .remove(&downstream_id);

        self.token_manager.remove_downstream(downstream_id);
    }

    /// Receives and dispatches a single JDP message from the fan-in channel.
    async fn handle_jdp_message(&mut self) -> JDSResult<(), error::JobDeclarator> {
        let receiver = self.job_declarator_io.job_declarator_receiver.clone();
        let (downstream_id, jd_message, tlv_fields) = match receiver.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Error receiving message: {:?}", e);
                return Err(error::JDSError::shutdown(e));
            }
        };

        self.handle_job_declaration_message_from_client(
            Some(downstream_id),
            jd_message,
            tlv_fields.as_deref(),
        )
        .await?;

        Ok(())
    }

    /// Validates a `SetCustomMiningJob` message via the JDS token manager and job validator.
    ///
    /// This method sends a request to the job validator to validate a SetCustomMiningJob message.
    /// It returns a `SetCustomMiningJobResponse` indicating the result of the operation.
    ///
    /// Remember: `jd_server_sv2` TCP sockets only operate JDP messages, and SetCustomMiningJob is
    /// MP message.
    ///
    /// Therefore, this method is key when `jd_server_sv2` crate is used as a library, where Pool
    /// app uses it to validate incoming SetCustomMiningJob messages. It has no usage on a
    /// standalone JDS app, where JobValidationEngine should be persisted into some shared DB with
    /// Pool app.
    ///
    /// Note: `SetCustomMiningJob.Success.job_id` is not handled here.
    /// It is the caller's responsibility to set it.
    pub async fn handle_set_custom_mining_job(
        &mut self,
        set_custom_mining_job: SetCustomMiningJob<'static>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> JDSResult<SetCustomMiningJobResponse<'_>, error::JobDeclarator> {
        let request_id = set_custom_mining_job.request_id;
        let channel_id = set_custom_mining_job.channel_id;

        let active_token: JdToken = match set_custom_mining_job.token.inner_as_ref().try_into() {
            Ok(token_bytes) => u64::from_le_bytes(token_bytes),
            Err(_) => {
                return Ok(SetCustomMiningJobResponse::error(
                    request_id,
                    channel_id,
                    "invalid-mining-job-token",
                ))
            }
        };

        // this allows JobValidationEngine to lookup the corresponding DeclareMiningJob
        let allocated_token = match self.token_manager.allocated_from_active(active_token) {
            Some(token) => token,
            None => {
                return Ok(SetCustomMiningJobResponse::error(
                    request_id,
                    channel_id,
                    "invalid-mining-job-token",
                ))
            }
        };

        // Clean up TokenManager
        self.token_manager.deactivate(active_token);

        match self
            .job_validator
            .handle_set_custom_mining_job(set_custom_mining_job, allocated_token)
            .await
        {
            SetCustomMiningJobResult::Success => {
                Ok(SetCustomMiningJobResponse::Ok(SetCustomMiningJobSuccess {
                    channel_id,
                    request_id,
                    job_id: 0, // caller responsibility to set it
                }))
            }
            SetCustomMiningJobResult::Error(error_code) => Ok(SetCustomMiningJobResponse::error(
                request_id,
                channel_id,
                &error_code,
            )),
        }
    }
}
