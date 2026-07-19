//! Module for interacting with Bitcoin Core v30.x via Sv2 Job Declaration Protocol via capnp over
//! UNIX socket.

use crate::{
    runtime_api::job_declaration_protocol::io::JdRequest,
    unix_capnp::{
        v30x::job_declaration_protocol::error::BitcoinCoreSv2JDPError,
        v32x_v31x_v30x::job_declaration_protocol::mempool::MempoolMirror,
    },
};
use async_channel::Receiver;
use bitcoin_capnp_types::{
    capnp,
    capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty},
    init_capnp::init::Client as InitIpcClient,
    mining_capnp::{
        block_template::Client as BlockTemplateIpcClient, mining::Client as MiningIpcClient,
    },
    proxy_capnp::{thread::Client as ThreadIpcClient, thread_map::Client as ThreadMapIpcClient},
};
use bitcoin_capnp_types_v30 as bitcoin_capnp_types;
use std::{cell::RefCell, path::Path, rc::Rc};
use stratum_core::bitcoin::{Block, BlockHash, consensus::deserialize, hashes::Hash};
use tokio::net::UnixStream;
use tokio_util::compat::*;
pub use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod error;
mod handlers;
mod monitors;

/// The main abstraction for interacting with Bitcoin Core via Sv2 Job Declaration Protocol.
///
/// It is instantiated with:
/// - A `&`[`std::path::Path`] to the Bitcoin Core UNIX socket
/// - A [`async_channel::Receiver`] for incoming [`JdRequest`] messages (handles
///   [`DeclareMiningJob`] and [`PushSolution`] requests)
/// - A [`tokio_util::sync::CancellationToken`] to stop the internally spawned tasks
///
/// The instance bootstraps its internal mempool state by fetching the current block template
/// from Bitcoin Core before accepting requests. It then spawns a background monitor task that
/// tracks mempool changes via `waitNext` requests.
///
/// Incoming [`DeclareMiningJob`] requests are validated by:
/// - Verifying all transactions exist in the mempool
/// - Assembling a test block with the declared coinbase and transactions
/// - Using Bitcoin Core's `checkBlock` to validate block structure
///
/// If transactions are missing, a [`MissingTransactions`] response is sent. If validation
/// succeeds, a [`Success`] response with current template parameters is sent.
///
/// Incoming [`PushSolution`] requests are used to submit mining solutions to Bitcoin Core.
#[derive(Clone)]
pub struct BitcoinCoreSv2JDP {
    thread_ipc_client: ThreadIpcClient,
    mining_ipc_client: MiningIpcClient,
    current_template_ipc_client: Rc<RefCell<BlockTemplateIpcClient>>,
    cancellation_token: CancellationToken,
    mempool_mirror: Rc<RefCell<MempoolMirror>>,
    incoming_requests: Receiver<JdRequest>,
}

impl BitcoinCoreSv2JDP {
    /// Creates a new [`BitcoinCoreSv2JDP`] instance.
    ///
    /// Bootstraps the mempool mirror and signals readiness before returning.
    pub async fn new<P>(
        bitcoin_core_unix_socket_path: P,
        incoming_requests: Receiver<JdRequest>,
        cancellation_token: CancellationToken,
        ready_tx: tokio::sync::oneshot::Sender<()>,
    ) -> Result<Self, BitcoinCoreSv2JDPError>
    where
        P: AsRef<Path>,
    {
        let bitcoin_core_unix_socket_path = bitcoin_core_unix_socket_path.as_ref();

        info!(
            "Creating new BitcoinCoreSv2JDP via IPC over UNIX socket: {}",
            bitcoin_core_unix_socket_path.display()
        );

        let stream = UnixStream::connect(bitcoin_core_unix_socket_path)
            .await
            .map_err(|e| {
                BitcoinCoreSv2JDPError::CannotConnectToUnixSocket(
                    bitcoin_core_unix_socket_path.into(),
                    e.to_string(),
                )
            })?;
        let (reader, writer) = stream.into_split();
        let reader_compat = reader.compat();
        let writer_compat = writer.compat_write();

        let rpc_network = Box::new(twoparty::VatNetwork::new(
            reader_compat,
            writer_compat,
            rpc_twoparty_capnp::Side::Client,
            Default::default(),
        ));

        let mut rpc_system = RpcSystem::new(rpc_network, None);
        let bootstrap_client: InitIpcClient =
            rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);

        tokio::task::spawn_local(rpc_system);

        let construct_response = bootstrap_client.construct_request().send().promise.await?;

        let thread_map: ThreadMapIpcClient = construct_response.get()?.get_thread_map()?;
        let thread_request = thread_map.make_thread_request();
        let thread_response = thread_request.send().promise.await?;

        let thread_ipc_client: ThreadIpcClient = thread_response.get()?.get_result()?;

        info!("IPC execution thread client successfully created.");

        let mut mining_client_request = bootstrap_client.make_mining_request();
        mining_client_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());
        let mining_client_response = mining_client_request.send().promise.await?;
        let mining_ipc_client: MiningIpcClient = mining_client_response.get()?.get_result()?;

        let mut template_ipc_client_request = mining_ipc_client.create_new_block_request();
        let mut template_ipc_client_request_options = template_ipc_client_request
            .get()
            .get_options()
            .map_err(|e| {
                error!("Failed to get template IPC client request options: {e}");
                e
            })?;
        template_ipc_client_request_options.set_use_mempool(true);

        debug!("Sending createNewBlock request to Bitcoin Core");
        let template_ipc_client_response = template_ipc_client_request
            .send()
            .promise
            .await
            .map_err(|e| {
                error!("Failed to send template IPC client request: {}", e);
                e
            })?;

        let template_ipc_client_result = template_ipc_client_response.get().map_err(|e| {
            error!("Failed to get template IPC client result: {}", e);
            e
        })?;

        let template_ipc_client = template_ipc_client_result.get_result().map_err(|e| {
            error!("Failed to get template IPC client result: {}", e);
            e
        })?;

        info!("IPC JDP client successfully created.");

        let self_ = Self {
            thread_ipc_client,
            mining_ipc_client,
            current_template_ipc_client: Rc::new(RefCell::new(template_ipc_client)),
            cancellation_token,
            mempool_mirror: Rc::new(RefCell::new(MempoolMirror::new())),
            incoming_requests,
        };

        // Bootstrap initial mempool state before signaling readiness
        debug!("Bootstrapping initial mempool state");
        if let Err(e) = self_.update_mempool_mirror().await {
            error!("Failed to bootstrap mempool mirror: {:?}", e);
            // Don't send readiness signal on failure (ready_tx dropped)
            return Err(e);
        }
        debug!("Initial mempool state bootstrapped successfully");

        // Signal that we're ready to accept requests
        ready_tx.send(()).map_err(|_| {
            error!("Ready signal receiver dropped - caller gave up waiting");
            BitcoinCoreSv2JDPError::ReadinessSignalFailed
        })?;

        Ok(self_)
    }

    /// Main event loop - runs in a LocalSet on dedicated thread.
    ///
    /// Spawns the monitor task and processes incoming job declaration requests until shutdown.
    pub async fn run(&self) {
        // spawn mempool mirror monitor task
        let monitor_handle = self.monitor_and_update_mempool_mirror();

        // Main request processing loop
        loop {
            tokio::select! {
                // Handle shutdown
                _ = self.cancellation_token.cancelled() => {
                    info!("BitcoinCoreSv2JDP shutting down");
                    break;
                }

                // Process incoming requests
                // Note: requests are processed sequentially for two reasons:
                // 1. This loop awaits each request before reading the next one
                // 2. On the Bitcoin Core side, `checkBlock` lacks a `context :Proxy.Context`
                //    parameter in its capnp definition (mining.capnp), so it runs synchronously
                //    on the Cap'n Proto event loop thread, blocking all other IPC operations on
                //    this connection until it completes
                // Pending requests are unboundedly buffered in the async_channel
                request = self.incoming_requests.recv() => {
                    match request {
                        Ok(request) => {
                            self.process_request(request).await;
                        }
                        Err(_) => {
                            info!("Incoming requests channel closed");
                            self.cancellation_token.cancel();
                            break;
                        }
                    }
                }
            }
        }

        // Wait for the monitor_mempool_mirror task to finish gracefully
        debug!("Waiting for monitor_mempool_mirror() task to finish");
        match monitor_handle.await {
            Ok(()) => {
                debug!("monitor_mempool_mirror() task finished successfully");
            }
            Err(e) => {
                error!(
                    "error waiting for monitor_mempool_mirror task to finish: {:?}",
                    e
                );
            }
        }
    }

    /// Updates the mempool mirror with the current block template from Bitcoin Core.
    async fn update_mempool_mirror(&self) -> Result<(), BitcoinCoreSv2JDPError> {
        let mut get_block_request = self
            .current_template_ipc_client
            .borrow()
            .get_block_request();
        get_block_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());

        let block_bytes = get_block_request
            .send()
            .promise
            .await?
            .get()?
            .get_result()?
            .to_vec();
        debug!("Deserializing block ({} bytes)", block_bytes.len());
        let block: Block =
            deserialize(&block_bytes).map_err(BitcoinCoreSv2JDPError::FailedToDeserializeBlock)?;

        self.mempool_mirror.borrow_mut().update(&block);

        Ok(())
    }

    /// Checks whether the chain tip has changed via `getTip` and, if so, updates the mempool
    /// mirror's prev_hash.
    ///
    /// This is called after `checkBlock` failures to reduce stale-tip classification races.
    /// We avoid `createNewBlock` (which is expensive) because only the prev_hash is needed for
    /// stale detection.
    ///
    /// On transient `"thread busy"` IPC contention, this method retries a few times with
    /// a short backoff before returning the error.
    pub(crate) async fn force_update_mempool_mirror_prev_hash(
        &self,
    ) -> Result<(), BitcoinCoreSv2JDPError> {
        const MAX_ATTEMPTS: usize = 3;
        const RETRY_BACKOFF_MS: u64 = 25;

        let mut last_error: Option<BitcoinCoreSv2JDPError> = None;

        for attempt in 1..=MAX_ATTEMPTS {
            let result: Result<(), BitcoinCoreSv2JDPError> = async {
                let mut get_tip_request = self.mining_ipc_client.get_tip_request();

                get_tip_request
                    .get()
                    .get_context()
                    .map_err(|e| {
                        error!("Failed to get getTip request context: {e}");
                        e
                    })?
                    .set_thread(self.thread_ipc_client.clone());

                let get_tip_response = get_tip_request.send().promise.await.map_err(|e| {
                    error!("Failed to send getTip request: {e}");
                    e
                })?;

                let tip_hash_bytes = get_tip_response
                    .get()
                    .map_err(|e| {
                        error!("Failed to read getTip response: {e}");
                        e
                    })?
                    .get_result()
                    .map_err(|e| {
                        error!("Failed to get tip result from getTip: {e}");
                        e
                    })?
                    .get_hash()
                    .map_err(|e| {
                        error!("Failed to get tip hash from getTip result: {e}");
                        e
                    })?;

                let tip_hash = BlockHash::from_slice(tip_hash_bytes).map_err(|e| {
                    error!("Failed to parse tip hash from getTip: {e}");
                    BitcoinCoreSv2JDPError::CapnpError(capnp::Error::failed(format!(
                        "Failed to parse tip hash: {e}"
                    )))
                })?;

                {
                    let mut mempool_mirror = self.mempool_mirror.borrow_mut();
                    if mempool_mirror.get_current_prev_hash() != Some(tip_hash) {
                        debug!(
                            old_prev_hash = ?mempool_mirror.get_current_prev_hash(),
                            new_prev_hash = ?tip_hash,
                            "Chain tip changed; updating mempool mirror prev_hash"
                        );
                        mempool_mirror.set_current_prev_hash(tip_hash);
                    }
                }

                Ok(())
            }
            .await;

            match result {
                Ok(()) => return Ok(()),
                Err(e) if e.is_thread_busy() && attempt < MAX_ATTEMPTS => {
                    warn!(
                        error = ?e,
                        attempt,
                        max_attempts = MAX_ATTEMPTS,
                        "Transient IPC contention during force_update_mempool_mirror_prev_hash (thread busy); retrying"
                    );
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_BACKOFF_MS)).await;
                }
                Err(e) => return Err(e),
            }
        }

        // Normally one of the retry attempts returns early (Ok or Err).
        // If execution reaches here, no terminal error was captured, so synthesize one.
        Err(last_error.unwrap_or_else(|| {
            BitcoinCoreSv2JDPError::CapnpError(capnp::Error::failed(
                "force_update_mempool_mirror_prev_hash exhausted retries without a terminal error"
                    .to_string(),
            ))
        }))
    }

    /// Processes a single job declaration request and dispatches to the appropriate handler.
    async fn process_request(&self, request: JdRequest) {
        match request {
            // Handle DeclareMiningJob requests
            JdRequest::DeclareMiningJob {
                version,
                coinbase_tx,
                wtxid_list,
                missing_txs,
                response_tx,
            } => {
                self.handle_declare_mining_job(
                    version,
                    coinbase_tx,
                    wtxid_list,
                    missing_txs,
                    response_tx,
                )
                .await;
            }

            // Handle PushSolution requests (no response needed)
            JdRequest::PushSolution { block } => {
                self.handle_push_solution(block).await;
            }
        }
    }

    /// Interrupts the current `waitNext` request to Bitcoin Core for graceful shutdown.
    async fn interrupt_wait_request(&self) -> Result<(), BitcoinCoreSv2JDPError> {
        let template_ipc_client = self.current_template_ipc_client.borrow().clone();

        let interrupt_wait_request = template_ipc_client.interrupt_wait_request();
        if let Err(e) = interrupt_wait_request.send().promise.await {
            error!("Failed to send interrupt wait request: {}", e);
            return Err(BitcoinCoreSv2JDPError::CapnpError(e));
        }

        Ok(())
    }
}
