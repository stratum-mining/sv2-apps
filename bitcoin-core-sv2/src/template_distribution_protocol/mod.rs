//! Module for interacting with Bitcoin Core via Sv2 Template Distribution Protocol.

use crate::template_distribution_protocol::template_data::TemplateData;
use async_channel::{Receiver, Sender};
use bitcoin_capnp_types::{
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
use capnp::capability::Request;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
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
};

use std::sync::RwLock;
use tokio::{net::UnixStream, task::JoinHandle};
use tokio_util::compat::*;
pub use tokio_util::sync::CancellationToken;
use tracing::info;

pub mod error;
mod handlers;
mod monitors;
mod template_data;

const WEIGHT_FACTOR: u32 = 4;
const MIN_BLOCK_RESERVED_WEIGHT: u64 = 2000;

/// The main abstraction for interacting with Bitcoin Core via Sv2 Template Distribution Protocol.
///
/// It is instantiated with:
/// - A `&`[`std::path::Path`] to the Bitcoin Core UNIX socket
/// - A `u64` for the fee delta threshold in satoshis
/// - A `u8` for the minimum interval in seconds between template updates
/// - A [`async_channel::Receiver`] for incoming [`TemplateDistribution`] messages (handles
///   [`CoinbaseOutputConstraints`], [`RequestTransactionData`], and [`SubmitSolution`])
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
/// Incoming [`RequestTransactionData`] messages are used to request transactions relative to a
/// specific template, for which a corresponding `RequestTransactionDataSuccess` or
/// `RequestTransactionDataError` message is sent over the outgoing channel.
///
/// Incoming [`SubmitSolution`] messages are used to submit solutions to a specific template.
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
    /// - incoming [`RequestTransactionData`] messages, for which it will send a
    ///   `RequestTransactionDataSuccess` or `RequestTransactionDataError` message as a response
    /// - incoming [`SubmitSolution`] messages, for which it will submit the solution to the Bitcoin
    ///   Core IPC client
    /// - incoming [`CoinbaseOutputConstraints`] messages, for which it will update the coinbase
    ///   output constraints
    ///
    /// Blocks until the cancellation token is activated.
    pub async fn run(&mut self) {
        // wait for first CoinbaseOutputConstraints message
        tracing::info!("Waiting for first CoinbaseOutputConstraints message");
        tracing::debug!("run() started, waiting for initial CoinbaseOutputConstraints");
        loop {
            tokio::select! {
                _ = self.global_cancellation_token.cancelled() => {
                    tracing::warn!("Exiting run");
                    tracing::debug!("run() early exit - global cancellation token activated before first CoinbaseOutputConstraints");
                    return;
                }
                Ok(message) = self.incoming_messages.recv() => {
                    tracing::debug!("run() received message during initial loop: {:?}", message);
                    match message {
                        TemplateDistribution::CoinbaseOutputConstraints(coinbase_output_constraints) => {
                            tracing::info!("Received: {:?}", coinbase_output_constraints);
                            tracing::debug!("First CoinbaseOutputConstraints received - max_additional_size: {}, max_additional_sigops: {}",
                                coinbase_output_constraints.coinbase_output_max_additional_size,
                                coinbase_output_constraints.coinbase_output_max_additional_sigops);

                            let template_ipc_client = match self.new_template_ipc_client(coinbase_output_constraints.coinbase_output_max_additional_size, coinbase_output_constraints.coinbase_output_max_additional_sigops).await {
                                Ok(template_ipc_client) => {
                                    tracing::debug!("Successfully created initial template IPC client");
                                    template_ipc_client
                                },
                                Err(e) => {
                                    tracing::error!("Failed to create new template IPC client: {:?}", e);
                                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                                    self.global_cancellation_token.cancel();
                                    return;
                                }
                            };

                            let mut current_template_ipc_client_guard = self.current_template_ipc_client.borrow_mut();
                            *current_template_ipc_client_guard = Some(template_ipc_client);
                            tracing::debug!("Set current_template_ipc_client to initial template");

                            break;
                        }
                        _ => {
                            tracing::warn!("Received unexpected message: {:?}", message);
                            tracing::warn!("Ignoring...");
                            continue;
                        }
                    }
                }
            }
        }

        // bootstrap the first template
        {
            tracing::debug!("Bootstrapping first template...");
            let template_data = match self
                .fetch_template_data(self.thread_ipc_client.clone())
                .await
            {
                Ok(template_data) => {
                    tracing::debug!(
                        "Successfully fetched initial template data - template_id: {}",
                        template_data.get_template_id()
                    );
                    template_data
                }
                Err(e) => {
                    tracing::error!("Failed to fetch template data: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.global_cancellation_token.cancel();
                    return;
                }
            };

            // send the future NewTemplate message
            let future_template = match template_data.get_new_template_message(true) {
                Ok(future_template) => future_template,
                Err(e) => {
                    tracing::error!("Failed to get future template message: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.global_cancellation_token.cancel();
                    return;
                }
            };

            tracing::debug!(
                "Sending initial NewTemplate (future=true) with template_id: {}",
                template_data.get_template_id()
            );

            match self
                .outgoing_messages
                .send(TemplateDistribution::NewTemplate(future_template.clone()))
                .await
            {
                Ok(_) => {
                    tracing::debug!("Successfully sent initial NewTemplate message");
                }
                Err(e) => {
                    tracing::error!("Failed to send future template message: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.global_cancellation_token.cancel();
                    return;
                }
            }

            // send the SetNewPrevHash message
            let set_new_prev_hash = template_data.get_set_new_prev_hash_message();
            tracing::debug!(
                "Sending initial SetNewPrevHash with prev_hash: {}",
                template_data.get_prev_hash()
            );

            match self
                .outgoing_messages
                .send(TemplateDistribution::SetNewPrevHash(
                    set_new_prev_hash.clone(),
                ))
                .await
            {
                Ok(_) => {
                    tracing::debug!("Successfully sent initial SetNewPrevHash message");
                }
                Err(e) => {
                    tracing::error!("Failed to send set new prev hash message: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.global_cancellation_token.cancel();
                    return;
                }
            }

            // save the template data
            let mut template_data_guard = match self.template_data.write() {
                Ok(guard) => guard,
                Err(e) => {
                    tracing::error!("Failed to acquire write lock on template_data: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.global_cancellation_token.cancel();
                    return;
                }
            };
            template_data_guard.insert(template_data.get_template_id(), template_data.clone());
            tracing::debug!(
                "Saved initial template data with template_id: {}",
                template_data.get_template_id()
            );

            // save the current prev hash
            self.current_prev_hash
                .replace(Some(template_data.get_prev_hash()));
            tracing::debug!(
                "Set current_prev_hash to: {}",
                template_data.get_prev_hash()
            );

            // save last_sent_template_instant
            self.last_sent_template_instant = Some(std::time::Instant::now());
        }

        // spawn the monitoring tasks
        tracing::debug!("Spawning monitoring tasks...");
        self.monitor_ipc_templates();
        tracing::debug!("monitor_ipc_templates() spawned");
        self.monitor_incoming_messages();
        tracing::debug!("monitor_incoming_messages() spawned");

        // block until the global cancellation token is activated
        tracing::debug!("run() entering main blocking wait for global_cancellation_token");
        self.global_cancellation_token.cancelled().await;
        tracing::debug!("global_cancellation_token cancelled - beginning shutdown sequence");

        // Wait for the monitor_ipc_templates task to finish gracefully
        tracing::debug!("Waiting for monitor_ipc_templates() task to finish");
        let handle = self.monitor_ipc_templates_handle.borrow_mut().take();
        if let Some(handle) = handle {
            match handle.await {
                Ok(()) => {
                    tracing::debug!("monitor_ipc_templates() task finished successfully");
                }
                Err(e) => {
                    tracing::error!(
                        "error waiting for monitor_ipc_templates task to finish: {:?}",
                        e
                    );
                }
            }
        }

        tracing::debug!("run() exiting");
    }

    async fn fetch_template_data(
        &self,
        thread_ipc_client: ThreadIpcClient,
    ) -> Result<TemplateData, BitcoinCoreSv2TDPError> {
        tracing::debug!("Fetching template data over IPC");
        let template_id = self.template_id_factory.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            "fetch_template_data() - assigned template_id: {}",
            template_id
        );

        // clone the current template IPC client so it's stored in the template data HashMap
        // this is important in case we need to submit a solution relative to this specific template
        // by the time self.current_template_ipc_client might have already changed
        let template_ipc_client = match self.current_template_ipc_client.borrow().clone() {
            Some(template_ipc_client) => template_ipc_client,
            None => {
                tracing::error!("Template IPC client not found");
                return Err(BitcoinCoreSv2TDPError::TemplateIpcClientNotFound);
            }
        };

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
        tracing::debug!(
            "Deserializing template header ({} bytes)",
            template_header_bytes.len()
        );
        let header: Header = deserialize(&template_header_bytes)?;
        tracing::debug!(
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
        tracing::debug!(
            "Deserializing coinbase tx ({} bytes)",
            coinbase_tx_bytes.len()
        );
        let coinbase_tx: Transaction = deserialize(&coinbase_tx_bytes)?;
        tracing::debug!("Coinbase tx deserialized: {:?}", coinbase_tx);

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
        tracing::debug!("TemplateData created successfully");

        Ok(template_data)
    }

    async fn new_thread_ipc_client(&self) -> Result<ThreadIpcClient, BitcoinCoreSv2TDPError> {
        tracing::debug!("Creating new thread IPC client");
        let thread_ipc_client_request = self.thread_map.make_thread_request();
        let thread_ipc_client_response = thread_ipc_client_request.send().promise.await?;
        let thread_ipc_client = thread_ipc_client_response.get()?.get_result()?;

        Ok(thread_ipc_client)
    }

    async fn new_template_ipc_client(
        &self,
        coinbase_output_max_additional_size: u32,
        coinbase_output_max_additional_sigops: u16,
    ) -> Result<BlockTemplateIpcClient, BitcoinCoreSv2TDPError> {
        tracing::debug!(
            "new_template_ipc_client() called - max_size: {}, max_sigops: {}",
            coinbase_output_max_additional_size,
            coinbase_output_max_additional_sigops
        );

        let mut template_ipc_client_request = self.mining_ipc_client.create_new_block_request();
        let mut template_ipc_client_request_options = template_ipc_client_request
            .get()
            .get_options()
            .map_err(|e| {
                tracing::error!("Failed to get template IPC client request options: {e}");
                e
            })?;

        let coinbase_weight = (coinbase_output_max_additional_size * WEIGHT_FACTOR) as u64;
        let block_reserved_weight = coinbase_weight.max(MIN_BLOCK_RESERVED_WEIGHT); // 2000 is the minimum block reserved weight
        tracing::debug!("Setting block_reserved_weight: {block_reserved_weight}");
        template_ipc_client_request_options.set_block_reserved_weight(block_reserved_weight);
        template_ipc_client_request_options.set_coinbase_output_max_additional_sigops(
            coinbase_output_max_additional_sigops as u64,
        );
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

        Ok(template_ipc_client)
    }

    async fn interrupt_wait_request(&self) -> Result<(), BitcoinCoreSv2TDPError> {
        let template_ipc_client = match self.current_template_ipc_client.borrow().clone() {
            Some(template_ipc_client) => template_ipc_client,
            None => {
                tracing::error!("Template IPC client not found");
                return Err(BitcoinCoreSv2TDPError::TemplateIpcClientNotFound);
            }
        };

        let interrupt_wait_request = template_ipc_client.interrupt_wait_request();
        if let Err(e) = interrupt_wait_request.send().promise.await {
            tracing::error!("Failed to send interrupt wait request: {}", e);
            return Err(BitcoinCoreSv2TDPError::FailedToSendInterruptWaitRequest);
        }

        Ok(())
    }

    async fn new_wait_next_request(
        &self,
        thread_ipc_client: ThreadIpcClient,
    ) -> Result<Request<WaitNextParams, WaitNextResults>, BitcoinCoreSv2TDPError> {
        let template_ipc_client = match self.current_template_ipc_client.borrow().clone() {
            Some(template_ipc_client) => template_ipc_client,
            None => {
                tracing::error!("Template IPC client not found");
                return Err(BitcoinCoreSv2TDPError::TemplateIpcClientNotFound);
            }
        };

        let mut wait_next_request = template_ipc_client.wait_next_request();

        match wait_next_request.get().get_context() {
            Ok(mut context) => context.set_thread(thread_ipc_client.clone()),
            Err(e) => {
                tracing::error!("Failed to set thread: {}", e);
                return Err(BitcoinCoreSv2TDPError::FailedToSetThread);
            }
        }

        let mut wait_next_request_options = match wait_next_request.get().get_options() {
            Ok(options) => options,
            Err(e) => {
                tracing::error!("Failed to get waitNext request options: {}", e);
                return Err(BitcoinCoreSv2TDPError::FailedToGetWaitNextRequestOptions);
            }
        };

        wait_next_request_options.set_fee_threshold(self.fee_threshold as i64);

        // 10 seconds timeout for waitNext requests
        // please note that this is NOT how often we expect to get new templates
        // it's just the max time we'll wait for the current waitNext request to complete
        wait_next_request_options.set_timeout(10_000.0);

        Ok(wait_next_request)
    }

    // spawns a task that processes the stale template data after 10s
    // we wait 10s in case there's any incoming RequestTransactionData referring to stale templates
    // immediately after the chain tip change
    async fn process_stale_template_data(&self, stale_template_ids: HashSet<u64>) {
        let self_clone = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            // update the stale template ids
            {
                let mut stale_template_ids_guard = match self_clone.stale_template_ids.write() {
                    Ok(guard) => guard,
                    Err(e) => {
                        tracing::error!(
                            "Failed to acquire write lock on stale_template_ids: {:?}",
                            e
                        );
                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                };
                *stale_template_ids_guard = stale_template_ids.clone();

                tracing::debug!(
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
                        tracing::error!("Failed to acquire write lock on template_data: {:?}", e);
                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
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

            tracing::debug!("Creating a dedicated thread IPC client for destroy_ipc_client");
            let thread_ipc_client = match self_clone.new_thread_ipc_client().await {
                Ok(thread_ipc_client) => thread_ipc_client,
                Err(e) => {
                    tracing::error!("Failed to create thread IPC client: {:?}", e);
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
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
                        tracing::error!("Failed to destroy template IPC client: {:?}", e);
                        tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                        self_clone.global_cancellation_token.cancel();
                        return;
                    }
                }
            }
        });
    }
}
