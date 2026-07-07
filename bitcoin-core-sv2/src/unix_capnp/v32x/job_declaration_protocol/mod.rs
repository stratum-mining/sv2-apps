//! Module for interacting with Bitcoin Core v32.x via Sv2 Job Declaration Protocol via capnp over
//! UNIX socket.

use crate::{
    common::job_declaration_protocol::io::{DownstreamId, JdRequest, RequestId},
    unix_capnp::v32x::job_declaration_protocol::error::BitcoinCoreSv2JDPError,
};
use async_channel::Receiver;
use bitcoin_capnp_types::{
    capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty},
    init_capnp::init::Client as InitIpcClient,
    mining_capnp::{
        block_template::Client as BlockTemplateIpcClient, mining::Client as MiningIpcClient,
    },
    proxy_capnp::{thread::Client as ThreadIpcClient, thread_map::Client as ThreadMapIpcClient},
};
use bitcoin_capnp_types_v32 as bitcoin_capnp_types;
use std::{cell::RefCell, collections::HashMap, path::Path, rc::Rc};
use stratum_core::bitcoin::{BlockHash, hashes::Hash};
use tokio::net::UnixStream;
use tokio_util::compat::*;
pub use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod error;
mod handlers;

/// How often to poll `isInitialBlockDownload` while waiting for IBD to finish during startup.
const IBD_POLL_INTERVAL_SECS: u64 = 1;

/// The main abstraction for interacting with Bitcoin Core via Sv2 Job Declaration Protocol.
///
/// It is instantiated with:
/// - A `&`[`std::path::Path`] to the Bitcoin Core UNIX socket
/// - A [`async_channel::Receiver`] for incoming [`JdRequest`] messages (handles
///   [`DeclareMiningJob`] and [`PushSolution`] requests)
/// - A [`tokio_util::sync::CancellationToken`] to stop the internally spawned tasks
///
/// Unlike the v30.x/v31.x backends, this implementation does not keep a local mempool mirror.
/// Incoming [`DeclareMiningJob`] requests are validated with Bitcoin Core's `TxCollection`
/// interface (https://github.com/bitcoin/bitcoin/pull/35671):
/// - `collectTxs` references the declared transactions directly in Bitcoin Core's mempool
/// - `unknownTxPos` reports which declared transactions Bitcoin Core does not know about
/// - `addMissingTxs` completes the collection with transactions provided by the client
/// - `makeTemplate` reconstructs and validates the block, returning a `BlockTemplate`
///
/// If transactions are missing, a [`MissingTransactions`] response is sent. If validation
/// succeeds, a [`Success`] response with the template parameters is sent and the validated
/// `BlockTemplate` is retained, keyed by `(downstream_id, request_id)`.
///
/// Incoming [`PushSolution`] requests submit mining solutions to Bitcoin Core via the
/// retained template's `submitSolution` method; the block itself never leaves the node.
/// [`ReleaseDeclaredJob`] and [`CleanupDownstream`] requests discard retained templates
/// that will no longer be used.
#[derive(Clone)]
pub struct BitcoinCoreSv2JDP {
    thread_ipc_client: ThreadIpcClient,
    submit_block_thread_ipc_client: ThreadIpcClient,
    mining_ipc_client: MiningIpcClient,
    cancellation_token: CancellationToken,
    /// Validated templates of declared jobs, retained for `PushSolution`.
    ///
    /// Dropping a client releases the corresponding `BlockTemplate` inside Bitcoin Core,
    /// but an explicit `destroy` is preferred for prompt cleanup.
    declared_templates: Rc<RefCell<HashMap<(DownstreamId, RequestId), BlockTemplateIpcClient>>>,
    incoming_requests: Receiver<JdRequest>,
}

impl BitcoinCoreSv2JDP {
    /// Creates a new [`BitcoinCoreSv2JDP`] instance.
    ///
    /// Waits for Bitcoin Core to leave IBD and signals readiness before returning.
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

        let submit_block_thread_request = thread_map.make_thread_request();
        let submit_block_thread_response = submit_block_thread_request
            .send()
            .promise
            .await
            .map_err(|e| {
                let details =
                    format!("Failed to send make_thread request for submitBlock thread: {e}");
                error!("{}", details);
                BitcoinCoreSv2JDPError::FailedToCreateThreadIpcClient(details)
            })?;
        let submit_block_thread_ipc_client: ThreadIpcClient = submit_block_thread_response
            .get()
            .map_err(|e| {
                let details =
                    format!("Failed to read make_thread response for submitBlock thread: {e}");
                error!("{}", details);
                BitcoinCoreSv2JDPError::FailedToCreateThreadIpcClient(details)
            })?
            .get_result()
            .map_err(|e| {
                let details = format!("Failed to get submitBlock thread IPC client: {e}");
                error!("{}", details);
                BitcoinCoreSv2JDPError::FailedToCreateThreadIpcClient(details)
            })?;

        info!("IPC submitBlock thread client successfully created.");

        let mut mining_client_request = bootstrap_client.make_mining_request();
        mining_client_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());
        let mining_client_response = mining_client_request.send().promise.await?;
        let mining_ipc_client: MiningIpcClient = mining_client_response.get()?.get_result()?;

        let self_ = Self {
            thread_ipc_client,
            submit_block_thread_ipc_client,
            mining_ipc_client,
            cancellation_token: cancellation_token.clone(),
            declared_templates: Rc::new(RefCell::new(HashMap::new())),
            incoming_requests,
        };

        // Wait for IBD to finish before signaling readiness, mirroring the behavior of the
        // mirror-based backends (whose initial createNewBlock blocks during IBD).
        loop {
            if self_.is_initial_block_download().await? {
                debug!("Bitcoin Core is in IBD; waiting before accepting JDP requests");
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        return Err(BitcoinCoreSv2JDPError::ReadinessSignalFailed);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(IBD_POLL_INTERVAL_SECS)) => {}
                }
            } else {
                break;
            }
        }

        info!("IPC JDP client successfully created.");

        // Signal that we're ready to accept requests
        ready_tx.send(()).map_err(|_| {
            error!("Ready signal receiver dropped - caller gave up waiting");
            BitcoinCoreSv2JDPError::ReadinessSignalFailed
        })?;

        Ok(self_)
    }

    /// Returns whether Bitcoin Core is still in Initial Block Download.
    async fn is_initial_block_download(&self) -> Result<bool, BitcoinCoreSv2JDPError> {
        let mut ibd_request = self.mining_ipc_client.is_initial_block_download_request();
        ibd_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        let ibd_response = ibd_request.send().promise.await?;
        Ok(ibd_response.get()?.get_result())
    }

    /// Returns the current chain tip as `(prev_hash, height)`.
    pub(crate) async fn get_tip(&self) -> Result<(BlockHash, i32), BitcoinCoreSv2JDPError> {
        let mut get_tip_request = self.mining_ipc_client.get_tip_request();
        get_tip_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        let get_tip_response = get_tip_request.send().promise.await?;
        let get_tip_result = get_tip_response.get()?;
        if !get_tip_result.get_has_result() {
            return Err(BitcoinCoreSv2JDPError::NoChainTip);
        }
        let block_ref = get_tip_result.get_result()?;
        let hash_bytes: [u8; 32] = block_ref
            .get_hash()?
            .try_into()
            .map_err(|_| BitcoinCoreSv2JDPError::NoChainTip)?;
        Ok((
            BlockHash::from_byte_array(hash_bytes),
            block_ref.get_height(),
        ))
    }

    /// Main event loop - runs in a LocalSet on dedicated thread.
    ///
    /// Processes incoming job declaration requests until shutdown.
    pub async fn run(&self) {
        loop {
            tokio::select! {
                // Handle shutdown
                _ = self.cancellation_token.cancelled() => {
                    info!("BitcoinCoreSv2JDP shutting down");
                    break;
                }

                // Process incoming requests.
                // Requests are handled sequentially because this loop awaits each request before
                // reading the next one.
                // Pending requests are unboundedly buffered in the async_channel.
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
        warn!("Exiting BitcoinCoreSv2JDP request loop");
    }

    /// Processes a single job declaration request and dispatches to the appropriate handler.
    async fn process_request(&self, request: JdRequest) {
        match request {
            // Handle DeclareMiningJob requests
            JdRequest::DeclareMiningJob {
                downstream_id,
                request_id,
                version,
                coinbase_tx,
                wtxid_list,
                missing_txs,
                response_tx,
            } => {
                self.handle_declare_mining_job(
                    downstream_id,
                    request_id,
                    version,
                    coinbase_tx,
                    wtxid_list,
                    missing_txs,
                    response_tx,
                )
                .await;
            }

            // Handle PushSolution requests (no response needed)
            JdRequest::PushSolution {
                downstream_id,
                request_id,
                version,
                ntime,
                nonce,
                coinbase_tx,
            } => {
                self.handle_push_solution(
                    downstream_id,
                    request_id,
                    version,
                    ntime,
                    nonce,
                    coinbase_tx,
                )
                .await;
            }

            // Discard retained templates that will no longer be used
            JdRequest::ReleaseDeclaredJob {
                downstream_id,
                request_id,
            } => {
                self.release_declared_job(downstream_id, request_id).await;
            }
            JdRequest::CleanupDownstream { downstream_id } => {
                self.cleanup_downstream(downstream_id).await;
            }
        }
    }
}
