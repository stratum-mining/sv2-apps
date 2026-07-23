//! Module for interacting with Bitcoin Core v30.x via Sv2 Template Distribution Protocol via
//! capnp over UNIX socket.

use crate::unix_capnp::{
    MIN_BLOCK_RESERVED_WEIGHT, STALE_TEMPLATE_GRACE_PERIOD_SECS, WEIGHT_FACTOR,
    v30x::template_distribution_protocol::template_data::TemplateData,
};
use async_channel::{Receiver, Sender};
use bitcoin_capnp_types::{
    capnp,
    capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty},
    init_capnp::init::Client as InitIpcClient,
    mining_capnp::{
        block_template::{
            Client as BlockTemplateIpcClient, wait_next_params::Owned as WaitNextParams,
            wait_next_results::Owned as WaitNextResults,
        },
        mining::Client as MiningIpcClient,
    },
    proxy_capnp::{thread::Client as ThreadIpcClient, thread_map::Client as ThreadMapIpcClient},
};
use bitcoin_capnp_types_v30 as bitcoin_capnp_types;
use capnp::capability::Request;
use error::BitcoinCoreSv2TDPError;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
use stratum_core::{
    binary_sv2::U256,
    bitcoin::{Transaction, block::Header, consensus::deserialize},
    parsers_sv2::TemplateDistribution,
    template_distribution_sv2::CoinbaseOutputConstraints,
};

use std::sync::RwLock;
use tokio::{net::UnixStream, task::JoinHandle};
use tokio_util::compat::*;
pub use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod error;
mod handlers;
mod monitors;
mod template_data;

/// The main abstraction for interacting with Bitcoin Core via Sv2 Template Distribution Protocol.
///
/// It is instantiated with:
/// - A `&`[`std::path::Path`] to the Bitcoin Core UNIX socket
/// - A `u64` for the fee delta threshold in satoshis
/// - A `u8` for the minimum interval in seconds between mempool-driven template updates
///   (chain tip updates are never throttled)
/// - A [`async_channel::Receiver`] for incoming [`TemplateDistribution`] messages (handles
///   [`CoinbaseOutputConstraints`],
///   [`stratum_core::template_distribution_sv2::RequestTransactionData`], and
///   [`stratum_core::template_distribution_sv2::SubmitSolution`])
/// - A [`async_channel::Sender`] for outgoing [`TemplateDistribution`] messages
/// - A [`tokio_util::sync::CancellationToken`] to stop the internally spawned tasks
///
/// The instance waits for the first [`CoinbaseOutputConstraints`] message to be received via the
/// incoming channel before initializing the template IPC client. Upon receiving this message and
/// successfully initializing, the [`BitcoinCoreSv2TDP`] instance sends a `NewTemplate` followed by
/// a corresponding `SetNewPrevHash` message over the outgoing channel.
///
/// As configured via `fee_threshold`, the [`BitcoinCoreSv2TDP`] instance will monitor the mempool
/// for changes and send a `NewTemplate` message if the fee delta is greater than the configured
/// threshold.
///
/// When there's a new Chain Tip, the [`BitcoinCoreSv2TDP`] instance will send a `NewTemplate`
/// followed by a corresponding `SetNewPrevHash` message over the outgoing channel.
///
/// Incoming [`stratum_core::template_distribution_sv2::RequestTransactionData`] messages are used
/// to request transactions relative to a specific template, for which a corresponding
/// `RequestTransactionDataSuccess` or `RequestTransactionDataError` message is sent over the
/// outgoing channel.
///
/// Incoming [`stratum_core::template_distribution_sv2::SubmitSolution`] messages are used to submit
/// solutions to a specific template.
#[derive(Clone)]
pub struct BitcoinCoreSv2TDP {
    fee_threshold: u64,
    min_interval: u8,
    thread_map: ThreadMapIpcClient,
    thread_ipc_client: ThreadIpcClient,
    mining_ipc_client: MiningIpcClient,
    monitor_ipc_templates_handle: Rc<RefCell<Option<JoinHandle<()>>>>,
    current_template_ipc_client: Rc<RefCell<Option<BlockTemplateIpcClient>>>,
    current_prev_hash: Rc<RefCell<Option<U256<'static>>>>,
    template_data: Rc<RwLock<HashMap<u64, TemplateData>>>,
    stale_template_ids: Rc<RwLock<HashSet<u64>>>,
    template_id_factory: Rc<AtomicU64>,
    incoming_messages: Receiver<TemplateDistribution<'static>>,
    outgoing_messages: Sender<TemplateDistribution<'static>>,
    global_cancellation_token: CancellationToken,
    template_ipc_client_cancellation_token: CancellationToken,
    last_sent_template_instant: Option<Instant>,
    unix_socket_path: PathBuf,
}

impl BitcoinCoreSv2TDP {
    /// Creates a new [`BitcoinCoreSv2TDP`] instance.
    #[allow(clippy::too_many_arguments)]
    pub async fn new<P>(
        bitcoin_core_unix_socket_path: P,
        fee_threshold: u64,
        min_interval: u8,
        incoming_messages: Receiver<TemplateDistribution<'static>>,
        outgoing_messages: Sender<TemplateDistribution<'static>>,
        global_cancellation_token: CancellationToken,
    ) -> Result<Self, BitcoinCoreSv2TDPError>
    where
        P: AsRef<Path>,
    {
        let bitcoin_core_unix_socket_path = bitcoin_core_unix_socket_path.as_ref();
        info!(
            "Creating new BitcoinCoreSv2TDP via IPC over UNIX socket: {}",
            bitcoin_core_unix_socket_path.display()
        );

        let stream = UnixStream::connect(bitcoin_core_unix_socket_path)
            .await
            .map_err(|e| {
                BitcoinCoreSv2TDPError::CannotConnectToUnixSocket(
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

        info!("IPC mining client successfully created.");

        let template_ipc_client_cancellation_token = CancellationToken::new();

        Ok(Self {
            fee_threshold,
            min_interval,
            thread_map,
            thread_ipc_client,
            mining_ipc_client,
            monitor_ipc_templates_handle: Rc::new(RefCell::new(None)),
            template_id_factory: Rc::new(AtomicU64::new(0)),
            current_template_ipc_client: Rc::new(RefCell::new(None)),
            current_prev_hash: Rc::new(RefCell::new(None)),
            template_data: Rc::new(RwLock::new(HashMap::new())),
            stale_template_ids: Rc::new(RwLock::new(HashSet::new())),
            global_cancellation_token,
            incoming_messages,
            outgoing_messages,
            template_ipc_client_cancellation_token,
            last_sent_template_instant: None,
            unix_socket_path: bitcoin_core_unix_socket_path.to_path_buf(),
        })
    }

    /// Runs the [`BitcoinCoreSv2TDP`] instance, monitoring for:
    /// - Chain Tip changes, for which it will send a `NewTemplate` message, followed by a
    ///   `SetNewPrevHash` message
    /// - incoming [`stratum_core::template_distribution_sv2::RequestTransactionData`] messages, for
    ///   which it will send a `RequestTransactionDataSuccess` or `RequestTransactionDataError`
    ///   message as a response
    /// - incoming [`stratum_core::template_distribution_sv2::SubmitSolution`] messages, for which
    ///   it will submit the solution to the Bitcoin Core IPC client
    /// - incoming [`CoinbaseOutputConstraints`] messages, for which it will update the coinbase
    ///   output constraints
    ///
    /// Blocks until the cancellation token is activated.
    pub async fn run(&mut self) {
        // wait for first CoinbaseOutputConstraints message
        info!("Waiting for first CoinbaseOutputConstraints message");
        debug!("run() started, waiting for initial CoinbaseOutputConstraints");
        loop {
            tokio::select! {
                _ = self.global_cancellation_token.cancelled() => {
                    warn!("Exiting run");
                    debug!("run() early exit - global cancellation token activated before first CoinbaseOutputConstraints");
                    return;
                }
                Ok(message) = self.incoming_messages.recv() => {
                    debug!("run() received message during initial loop: {:?}", message);
                    match message {
                        TemplateDistribution::CoinbaseOutputConstraints(coinbase_output_constraints) => {
                            info!("Received: {:?}", coinbase_output_constraints);
                            debug!("First CoinbaseOutputConstraints received - max_additional_size: {}, max_additional_sigops: {}",
                                coinbase_output_constraints.coinbase_output_max_additional_size,
                                coinbase_output_constraints.coinbase_output_max_additional_sigops);

                            match self
                                .bootstrap_template_ipc_client_from_coinbase_output_constraints(
                                    coinbase_output_constraints,
                                )
                                .await
                            {
                                Ok(()) => {
                                    debug!(
                                        "Successfully bootstrapped initial template IPC client"
                                    );
                                    break;
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to bootstrap initial template IPC client: {:?}",
                                        e
                                    );
                                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                    self.global_cancellation_token.cancel();
                                    return;
                                }
                            }
                        }
                        _ => {
                            warn!("Received unexpected message: {:?}", message);
                            warn!("Ignoring...");
                            continue;
                        }
                    }
                }
            }
        }

        // spawn the monitoring tasks
        debug!("Spawning monitoring tasks...");
        self.monitor_ipc_templates();
        debug!("monitor_ipc_templates() spawned");
        self.monitor_incoming_messages();
        debug!("monitor_incoming_messages() spawned");

        // block until the global cancellation token is activated
        debug!("run() entering main blocking wait for global_cancellation_token");
        self.global_cancellation_token.cancelled().await;
        debug!("global_cancellation_token cancelled - beginning shutdown sequence");

        // Wait for the monitor_ipc_templates task to finish gracefully
        debug!("Waiting for monitor_ipc_templates() task to finish");
        let handle = self.monitor_ipc_templates_handle.borrow_mut().take();
        if let Some(handle) = handle {
            match handle.await {
                Ok(()) => {
                    debug!("monitor_ipc_templates() task finished successfully");
                }
                Err(e) => {
                    error!(
                        "error waiting for monitor_ipc_templates task to finish: {:?}",
                        e
                    );
                }
            }
        }

        debug!("run() exiting");
    }

    async fn fetch_template_data(
        &self,
        template_ipc_client: BlockTemplateIpcClient,
        thread_ipc_client: ThreadIpcClient,
    ) -> Result<TemplateData, BitcoinCoreSv2TDPError> {
        debug!("Fetching template data over IPC");
        let template_id = self.template_id_factory.fetch_add(1, Ordering::Relaxed);
        debug!(
            "fetch_template_data() - assigned template_id: {}",
            template_id
        );

        let mut template_header_request = template_ipc_client.get_block_header_request();
        template_header_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());

        let template_header_bytes = template_header_request
            .send()
            .promise
            .await?
            .get()?
            .get_result()?
            .to_vec();

        // Deserialize the template header from Bitcoin Core's serialization format
        debug!(
            "Deserializing template header ({} bytes)",
            template_header_bytes.len()
        );
        let header: Header = deserialize(&template_header_bytes)?;
        debug!(
            "Template header deserialized - prev_hash: {:?}",
            header.prev_blockhash
        );

        let mut coinbase_tx_request = template_ipc_client.get_coinbase_tx_request();
        coinbase_tx_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());

        let coinbase_tx_bytes = coinbase_tx_request
            .send()
            .promise
            .await?
            .get()?
            .get_result()?
            .to_vec();

        // Deserialize the coinbase tx from Bitcoin Core's serialization format
        debug!(
            "Deserializing coinbase tx ({} bytes)",
            coinbase_tx_bytes.len()
        );
        let coinbase_tx: Transaction = deserialize(&coinbase_tx_bytes)?;
        debug!("Coinbase tx deserialized: {:?}", coinbase_tx);

        let mut merkle_path_request = template_ipc_client.get_coinbase_merkle_path_request();
        merkle_path_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());

        let merkle_path: Vec<Vec<u8>> = merkle_path_request
            .send()
            .promise
            .await?
            .get()?
            .get_result()?
            .iter()
            .map(|x| x.map(|slice| slice.to_vec()))
            .collect::<Result<Vec<_>, _>>()?;

        // Create the template data structure
        let template_data = TemplateData::new(
            template_id,
            header,
            coinbase_tx,
            merkle_path,
            template_ipc_client,
        );
        debug!("TemplateData created successfully");

        Ok(template_data)
    }

    async fn new_thread_ipc_client(&self) -> Result<ThreadIpcClient, BitcoinCoreSv2TDPError> {
        debug!("Creating new thread IPC client");
        let thread_ipc_client_request = self.thread_map.make_thread_request();
        let thread_ipc_client_response = thread_ipc_client_request.send().promise.await?;
        let thread_ipc_client = thread_ipc_client_response.get()?.get_result()?;

        Ok(thread_ipc_client)
    }

    fn set_current_template_ipc_client(&self, template_ipc_client: BlockTemplateIpcClient) {
        let mut current_template_ipc_client_guard = self.current_template_ipc_client.borrow_mut();
        *current_template_ipc_client_guard = Some(template_ipc_client);
        debug!("Updated current_template_ipc_client");
    }

    fn current_template_ipc_client(
        &self,
    ) -> Result<BlockTemplateIpcClient, BitcoinCoreSv2TDPError> {
        match self.current_template_ipc_client.borrow().clone() {
            Some(template_ipc_client) => Ok(template_ipc_client),
            None => {
                error!("Template IPC client not found");
                Err(BitcoinCoreSv2TDPError::TemplateIpcClientNotFound)
            }
        }
    }

    fn store_template_data(
        &self,
        template_data: &TemplateData,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        let mut template_data_guard = self.template_data.write().map_err(|e| {
            error!("Failed to acquire write lock on template_data: {:?}", e);
            BitcoinCoreSv2TDPError::FailedToSendNewTemplateMessage
        })?;

        template_data_guard.insert(template_data.get_template_id(), template_data.clone());
        debug!(
            "Saved template data with template_id: {}",
            template_data.get_template_id()
        );

        Ok(())
    }

    fn current_template_ids(&self) -> Result<HashSet<u64>, BitcoinCoreSv2TDPError> {
        let template_data_guard = self.template_data.read().map_err(|e| {
            error!("Failed to acquire read lock on template_data: {:?}", e);
            BitcoinCoreSv2TDPError::FailedToSendNewTemplateMessage
        })?;

        Ok(template_data_guard.keys().copied().collect())
    }

    async fn publish_template(
        &mut self,
        template_data: TemplateData,
        future_template: bool,
        send_set_new_prev_hash: bool,
        update_last_sent_template_instant: bool,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        let new_template = template_data
            .get_new_template_message(future_template)
            .map_err(|e| {
                error!("Failed to get NewTemplate message: {:?}", e);
                BitcoinCoreSv2TDPError::FailedToSendNewTemplateMessage
            })?;
        let set_new_prev_hash = if send_set_new_prev_hash {
            Some(template_data.get_set_new_prev_hash_message())
        } else {
            None
        };

        self.store_template_data(&template_data)?;

        if send_set_new_prev_hash {
            self.current_prev_hash
                .replace(Some(template_data.get_prev_hash()));
            debug!(
                "Set current_prev_hash to: {}",
                template_data.get_prev_hash()
            );
        }

        debug!(
            "Sending NewTemplate (future={}) with template_id: {}",
            future_template,
            template_data.get_template_id()
        );
        self.outgoing_messages
            .send(TemplateDistribution::NewTemplate(new_template))
            .await
            .map_err(|e| {
                error!("Failed to send NewTemplate message: {:?}", e);
                BitcoinCoreSv2TDPError::FailedToSendNewTemplateMessage
            })?;
        debug!("Successfully sent NewTemplate message");

        if let Some(set_new_prev_hash) = set_new_prev_hash {
            debug!(
                "Sending SetNewPrevHash with prev_hash: {}",
                template_data.get_prev_hash()
            );
            self.outgoing_messages
                .send(TemplateDistribution::SetNewPrevHash(set_new_prev_hash))
                .await
                .map_err(|e| {
                    error!("Failed to send SetNewPrevHash message: {:?}", e);
                    BitcoinCoreSv2TDPError::FailedToSendSetNewPrevHashMessage
                })?;
            debug!("Successfully sent SetNewPrevHash message");
        }

        if update_last_sent_template_instant {
            self.last_sent_template_instant = Some(Instant::now());
        }

        Ok(())
    }

    /// Creates a fresh Bitcoin Core Template IPC client from the given
    /// [`CoinbaseOutputConstraints`] and immediately sends a `NewTemplate` + `SetNewPrevHash`.
    ///
    /// This method intentionally couples these operations because every constraints update should
    /// make a newly constrained template visible to the Sv2 side right away. On success, it:
    ///
    /// - creates a new `BlockTemplateIpcClient` configured with the provided constraints
    /// - fetches the corresponding `TemplateData`
    /// - stores the fetched `TemplateData`
    /// - sends `NewTemplate(future_template = true)`
    /// - sends the matching `SetNewPrevHash`
    /// - updates `current_prev_hash` and `last_sent_template_instant`
    /// - stores the client as `current_template_ipc_client`
    async fn bootstrap_template_ipc_client_from_coinbase_output_constraints(
        &mut self,
        coinbase_output_constraints: CoinbaseOutputConstraints,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        debug!(
            "bootstrap_template_ipc_client_from_coinbase_output_constraints() called - max_size: {}, max_sigops: {}",
            coinbase_output_constraints.coinbase_output_max_additional_size,
            coinbase_output_constraints.coinbase_output_max_additional_sigops
        );

        let mut template_ipc_client_request = self.mining_ipc_client.create_new_block_request();
        let mut template_ipc_client_request_options = template_ipc_client_request
            .get()
            .get_options()
            .map_err(|e| {
                error!("Failed to get template IPC client request options: {e}");
                e
            })?;

        let coinbase_weight = (coinbase_output_constraints.coinbase_output_max_additional_size
            * WEIGHT_FACTOR) as u64;
        let block_reserved_weight = coinbase_weight.max(MIN_BLOCK_RESERVED_WEIGHT); // 2000 is the minimum block reserved weight
        debug!("Setting block_reserved_weight: {block_reserved_weight}");
        template_ipc_client_request_options.set_block_reserved_weight(block_reserved_weight);
        template_ipc_client_request_options.set_coinbase_output_max_additional_sigops(
            coinbase_output_constraints.coinbase_output_max_additional_sigops as u64,
        );
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

        debug!("Fetching template data from bootstrapped template IPC client");
        let template_data = self
            .fetch_template_data(template_ipc_client.clone(), self.thread_ipc_client.clone())
            .await
            .map_err(|e| {
                error!("Failed to fetch template data: {:?}", e);
                e
            })?;

        self.publish_template(template_data, true, true, true)
            .await?;
        self.set_current_template_ipc_client(template_ipc_client);

        Ok(())
    }

    async fn interrupt_wait_request(
        &self,
        template_ipc_client: &BlockTemplateIpcClient,
    ) -> Result<(), BitcoinCoreSv2TDPError> {
        let interrupt_wait_request = template_ipc_client.interrupt_wait_request();
        if let Err(e) = interrupt_wait_request.send().promise.await {
            error!("Failed to send interrupt wait request: {}", e);
            return Err(BitcoinCoreSv2TDPError::FailedToSendInterruptWaitRequest);
        }

        Ok(())
    }

    async fn new_wait_next_request(
        &self,
        template_ipc_client: &BlockTemplateIpcClient,
        thread_ipc_client: ThreadIpcClient,
        fee_threshold: i64,
        timeout_ms: f64,
    ) -> Result<Request<WaitNextParams, WaitNextResults>, BitcoinCoreSv2TDPError> {
        let mut wait_next_request = template_ipc_client.wait_next_request();

        match wait_next_request.get().get_context() {
            Ok(mut context) => context.set_thread(thread_ipc_client.clone()),
            Err(e) => {
                error!("Failed to set thread: {}", e);
                return Err(BitcoinCoreSv2TDPError::FailedToSetThread);
            }
        }

        let mut wait_next_request_options = match wait_next_request.get().get_options() {
            Ok(options) => options,
            Err(e) => {
                error!("Failed to get waitNext request options: {}", e);
                return Err(BitcoinCoreSv2TDPError::FailedToGetWaitNextRequestOptions);
            }
        };

        wait_next_request_options.set_fee_threshold(fee_threshold);

        // the timeout is NOT how often we expect to get new templates
        // it's just the max time we'll wait for the current waitNext request to complete
        wait_next_request_options.set_timeout(timeout_ms);

        Ok(wait_next_request)
    }

    // Spawns a task that processes stale template data after a `STALE_TEMPLATE_GRACE_PERIOD_SECS` grace period.
    //
    // Takes a snapshot of [`current_template_ids`] at call time, then schedules their
    // retirement. This ensures the snapshot is always taken at the epoch boundary rather
    // than relying on the caller to pre-compute the stale set.
    //
    // The grace period allows in-flight RequestTransactionData and SubmitSolution requests
    // to complete before the template data is retired. After the grace period:
    // - Stale template IDs are written to stale_template_ids, causing
    //   handle_request_transaction_data to return an error.
    // - Stale entries are removed from template_data, causing both handle_request_transaction_data
    //   and handle_submit_solution to return errors.
    // - The underlying IPC client capabilities are released via destroy_ipc_client.
    async fn process_stale_template_data(&self) -> Result<(), BitcoinCoreSv2TDPError> {
        let stale_template_ids = self.current_template_ids()?;
        if stale_template_ids.is_empty() {
            return Ok(());
        }
        let self_clone = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_secs(
                STALE_TEMPLATE_GRACE_PERIOD_SECS,
            ))
            .await;

            // update the stale template ids
            {
                let mut stale_template_ids_guard = match self_clone.stale_template_ids.write() {
                    Ok(guard) => guard,
                    Err(e) => {
                        error!(
                            "Failed to acquire write lock on stale_template_ids: {:?}",
                            e
                        );
                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                };
                *stale_template_ids_guard = stale_template_ids.clone();

                debug!(
                    "Marked {} templates as stale: {:?}",
                    stale_template_ids.len(),
                    stale_template_ids
                );
            }

            // remove the stale template data from the template_data HashMap
            let removed_template_data = {
                let mut template_data_guard = match self_clone.template_data.write() {
                    Ok(guard) => guard,
                    Err(e) => {
                        error!("Failed to acquire write lock on template_data: {:?}", e);
                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                };

                let mut removed_template_data: Vec<TemplateData> = Vec::new();

                for stale_template_id in &stale_template_ids {
                    if let Some(template_data) = template_data_guard.remove(stale_template_id) {
                        removed_template_data.push(template_data);
                    }
                }

                removed_template_data
            };

            debug!("Creating a dedicated thread IPC client for destroy_ipc_client");
            let thread_ipc_client = match self_clone.new_thread_ipc_client().await {
                Ok(thread_ipc_client) => thread_ipc_client,
                Err(e) => {
                    error!("Failed to create thread IPC client: {:?}", e);
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self_clone.global_cancellation_token.cancel();
                    return;
                }
            };

            for template_data in removed_template_data {
                match template_data
                    .destroy_ipc_client(thread_ipc_client.clone())
                    .await
                {
                    Ok(()) => (),
                    Err(e) => {
                        error!("Failed to destroy template IPC client: {:?}", e);
                        warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                }
            }
        });

        Ok(())
    }
}
