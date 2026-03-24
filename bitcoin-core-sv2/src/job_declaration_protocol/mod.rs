//! Module for interacting with Bitcoin Core via Sv2 Job Declaration Protocol.

use crate::job_declaration_protocol::{
    error::BitcoinCoreSv2JDPError, io::JdRequest, mempool::MempoolMirror,
};
use async_channel::Receiver;
use bitcoin_capnp_types::{
    init_capnp::init::Client as InitIpcClient,
    mining_capnp::{
        block_template::Client as BlockTemplateIpcClient, mining::Client as MiningIpcClient,
    },
    proxy_capnp::{thread::Client as ThreadIpcClient, thread_map::Client as ThreadMapIpcClient},
};
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use std::{cell::RefCell, path::Path, rc::Rc};
use stratum_core::bitcoin::{Block, consensus::deserialize};
use tokio::net::UnixStream;
use tokio_util::compat::*;
pub use tokio_util::sync::CancellationToken;
use tracing::info;

pub mod error;
mod handlers;
pub mod io;
mod mempool;
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
                tracing::error!("Failed to get template IPC client request options: {e}");
                e
            })?;
        template_ipc_client_request_options.set_use_mempool(true);

        tracing::debug!("Sending createNewBlock request to Bitcoin Core");
        let template_ipc_client_response = template_ipc_client_request
            .send()
            .promise
            .await
            .map_err(|e| {
                tracing::error!("Failed to send template IPC client request: {}", e);
                e
            })?;

        let template_ipc_client_result = template_ipc_client_response.get().map_err(|e| {
            tracing::error!("Failed to get template IPC client result: {}", e);
            e
        })?;

        let template_ipc_client = template_ipc_client_result.get_result().map_err(|e| {
            tracing::error!("Failed to get template IPC client result: {}", e);
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
        tracing::debug!("Bootstrapping initial mempool state");
        if let Err(e) = self_.update_mempool_mirror().await {
            tracing::error!("Failed to bootstrap mempool mirror: {:?}", e);
            // Don't send readiness signal on failure (ready_tx dropped)
            return Err(e);
        }
        tracing::debug!("Initial mempool state bootstrapped successfully");

        // Signal that we're ready to accept requests
        ready_tx.send(()).map_err(|_| {
            tracing::error!("Ready signal receiver dropped - caller gave up waiting");
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
                    tracing::info!("BitcoinCoreSv2JDP shutting down");
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
                            tracing::info!("Incoming requests channel closed");
                            self.cancellation_token.cancel();
                            break;
                        }
                    }
                }
            }
        }

        // Wait for the monitor_mempool_mirror task to finish gracefully
        tracing::debug!("Waiting for monitor_mempool_mirror() task to finish");
        match monitor_handle.await {
            Ok(()) => {
                tracing::debug!("monitor_mempool_mirror() task finished successfully");
            }
            Err(e) => {
                tracing::error!(
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
        tracing::debug!("Deserializing block ({} bytes)", block_bytes.len());
        let block: Block =
            deserialize(&block_bytes).map_err(BitcoinCoreSv2JDPError::FailedToDeserializeBlock)?;

        self.mempool_mirror.borrow_mut().update(&block);

        Ok(())
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
            JdRequest::PushSolution { push_solution } => {
                self.handle_push_solution(push_solution).await;
            }
        }
    }

    /// Interrupts the current `waitNext` request to Bitcoin Core for graceful shutdown.
    async fn interrupt_wait_request(&self) -> Result<(), BitcoinCoreSv2JDPError> {
        let template_ipc_client = self.current_template_ipc_client.borrow().clone();

        let interrupt_wait_request = template_ipc_client.interrupt_wait_request();
        if let Err(e) = interrupt_wait_request.send().promise.await {
            tracing::error!("Failed to send interrupt wait request: {}", e);
            return Err(BitcoinCoreSv2JDPError::CapnpError(e));
        }

        Ok(())
    }
}
