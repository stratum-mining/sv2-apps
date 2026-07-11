//! Module for interacting with Bitcoin Core v32.x via Sv2 Job Declaration Protocol via capnp over
//! UNIX socket.

use super::bitcoin_capnp_types;
use crate::{
    runtime_api::job_declaration_protocol::io::JdRequest,
    unix_capnp::{
        v32x::job_declaration_protocol::error::BitcoinCoreSv2JDPError,
        v32x::job_declaration_protocol::chain_tip_state::ChainTipState,
    },
};
use async_channel::Receiver;
use bitcoin_capnp_types::{
    capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty},
    init_capnp::init::Client as InitIpcClient,
    mining_capnp::{
        mining::Client as MiningIpcClient,
    },
    proxy_capnp::{thread::Client as ThreadIpcClient, thread_map::Client as ThreadMapIpcClient},
    rpc_capnp::rpc::Client as RpcIpcClient,
};
use serde_json::Value;
use std::{cell::RefCell, path::Path, rc::Rc};
use stratum_core::bitcoin::{
    BlockHash, CompactTarget,
    block::Header,
    consensus::encode::deserialize_hex,
    hashes::Hash,
};
use tokio::net::UnixStream;
use tokio_util::compat::*;
pub use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

mod error;
mod handlers;
mod monitors;
mod chain_tip_state;

/// The main abstraction for interacting with Bitcoin Core via Sv2 Job Declaration Protocol.
///
/// It is instantiated with:
/// - A `&`[`std::path::Path`] to the Bitcoin Core UNIX socket
/// - A [`async_channel::Receiver`] for incoming [`JdRequest`] messages (handles
///   [`DeclareMiningJob`] and [`PushSolution`] requests)
/// - A [`tokio_util::sync::CancellationToken`] to stop the internally spawned tasks
///
/// The instance bootstraps its internal chain-tip state by fetching the current tip and header
/// from Bitcoin Core before accepting requests. It also calls `getmininginfo` to source the
/// **next-block** `nBits` (see [`ChainTipState::next_nbits`] for why this matters at
/// difficulty-adjustment boundaries). It then spawns a background monitor task that
/// tracks tip changes via `waitTipChanged`, refreshes header fields via `getblockheader` RPC,
/// and re-fetches next-block `nBits` via `getmininginfo`.
///
/// Incoming [`DeclareMiningJob`] requests are validated by:
/// - Fetching declared transactions from Bitcoin Core via
///   `getTransactionsByWitnessID`
/// - Combining non-empty responses with `ProvideMissingTransactionsSuccess` payloads
/// - Assembling a test block with the declared coinbase and transactions
/// - Using Bitcoin Core's `checkBlock` to validate block structure
///
/// Unlike v30.x / v31.x, v32.x does **not** maintain a local mempool mirror
/// for transaction lookup — every `DeclareMiningJob` queries Bitcoin Core
/// directly.
///
/// If transactions are missing, a [`MissingTransactions`] response is sent. If validation
/// succeeds, a [`Success`] response with current template parameters is sent.
///
/// Incoming [`PushSolution`] requests are used to submit mining solutions to Bitcoin Core.
#[derive(Clone)]
pub struct BitcoinCoreSv2JDP {
    thread_map: ThreadMapIpcClient,
    thread_ipc_client: ThreadIpcClient,
    submit_block_thread_ipc_client: ThreadIpcClient,
    mining_ipc_client: MiningIpcClient,
    rpc_ipc_client: RpcIpcClient,
    cancellation_token: CancellationToken,
    chain_tip_state: Rc<RefCell<ChainTipState>>,
    incoming_requests: Receiver<JdRequest>,
}

impl BitcoinCoreSv2JDP {
    /// Creates a new [`BitcoinCoreSv2JDP`] instance.
    ///
    /// Bootstraps the chain-tip state and signals readiness before returning.
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

        let mut rpc_client_request = bootstrap_client.make_rpc_request();
        rpc_client_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());
        let rpc_client_response = rpc_client_request.send().promise.await?;
        let rpc_ipc_client: RpcIpcClient = rpc_client_response.get()?.get_result()?;

        info!("IPC JDP client successfully created.");

        let self_ = Self {
            thread_map,
            thread_ipc_client,
            submit_block_thread_ipc_client,
            mining_ipc_client,
            rpc_ipc_client,
            cancellation_token,
            chain_tip_state: Rc::new(RefCell::new(ChainTipState::new())),
            incoming_requests,
        };

        // Bootstrap initial chain-tip state before signaling readiness
        debug!("Bootstrapping initial chain-tip state");
        if let Err(e) = self_.update_chain_tip_state(None).await {
            error!("Failed to bootstrap chain-tip state: {:?}", e);
            return Err(e);
        }
        debug!("Initial chain-tip state bootstrapped successfully");

        // Signal that we're ready to accept requests
        ready_tx.send(()).map_err(|_| {
            error!("Ready signal receiver dropped - caller gave up waiting");
            BitcoinCoreSv2JDPError::ReadinessSignalFailed
        })?;

        Ok(self_)
    }

    /// Creates a new dedicated thread IPC client.
    async fn new_thread_ipc_client(&self) -> Result<ThreadIpcClient, BitcoinCoreSv2JDPError> {
        let thread_request = self.thread_map.make_thread_request();
        let thread_response = thread_request.send().promise.await.map_err(|e| {
            let details = format!("Failed to send make_thread request: {e}");
            error!("{}", details);
            BitcoinCoreSv2JDPError::FailedToCreateThreadIpcClient(details)
        })?;

        let thread_ipc_client = thread_response
            .get()
            .map_err(|e| {
                let details = format!("Failed to read make_thread response: {e}");
                error!("{}", details);
                BitcoinCoreSv2JDPError::FailedToCreateThreadIpcClient(details)
            })?
            .get_result()
            .map_err(|e| {
                let details = format!("Failed to get thread IPC client: {e}");
                error!("{}", details);
                BitcoinCoreSv2JDPError::FailedToCreateThreadIpcClient(details)
            })?;

        Ok(thread_ipc_client)
    }

    /// Main event loop - runs in a LocalSet on dedicated thread.
    ///
    /// Spawns the monitor task and processes incoming job declaration requests until shutdown.
    pub async fn run(&self) {
        // spawn chain-tip state monitor task
        let monitor_handle = self.monitor_and_update_chain_tip_state();

        // Main request processing loop
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

        // Wait for the monitor task to finish gracefully
        debug!("Waiting for monitor_chain_tip_state() task to finish");
        match monitor_handle.await {
            Ok(()) => {
                debug!("monitor_chain_tip_state() task finished successfully");
            }
            Err(e) => {
                error!(
                    "error waiting for monitor_chain_tip_state task to finish: {:?}",
                    e
                );
            }
        }
    }

    /// Updates the chain-tip state.
    ///
    /// If `tip_hash_override` is provided (e.g. from `waitTipChanged`), it is used directly.
    /// Otherwise, the current tip hash is fetched via `getTip` first.
    async fn update_chain_tip_state(
        &self,
        tip_hash_override: Option<BlockHash>,
    ) -> Result<(), BitcoinCoreSv2JDPError> {
        let tip_hash = if let Some(tip_hash) = tip_hash_override {
            tip_hash
        } else {
            // Query the current tip hash from Bitcoin Core.
            {
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

                let get_tip_result = get_tip_response.get().map_err(|e| {
                    error!("Failed to read getTip response: {e}");
                    e
                })?;

                if !get_tip_result.get_has_result() {
                    return Err(BitcoinCoreSv2JDPError::GetTipReturnedNoResult);
                }

                let tip_hash_bytes = get_tip_result
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

                BlockHash::from_slice(tip_hash_bytes).map_err(|e| {
                    error!("Failed to parse tip hash from getTip: {e}");
                    BitcoinCoreSv2JDPError::FailedToParseTipHashFromGetTip(e.to_string())
                })?
            }
        };

        // Query `getblockheader` via RPC and deserialize the returned header hex.
        let header: Header = {
            let mut execute_rpc_request = self.rpc_ipc_client.execute_rpc_request();
            execute_rpc_request
                .get()
                .get_context()
                .map_err(|e| {
                    error!("Failed to get executeRpc request context: {e}");
                    e
                })?
                .set_thread(self.thread_ipc_client.clone());

            let request = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"v32-jdp-chain-tip\",\"method\":\"getblockheader\",\"params\":[\"{}\",false]}}",
                tip_hash
            );
            execute_rpc_request.get().set_request(request.as_str());

            let execute_rpc_response = execute_rpc_request.send().promise.await.map_err(|e| {
                error!("Failed to send executeRpc request: {e}");
                e
            })?;

            let rpc_response_text = execute_rpc_response
                .get()
                .map_err(|e| {
                    error!("Failed to read executeRpc response: {e}");
                    e
                })?
                .get_result()
                .map_err(|e| {
                    error!("Failed to get executeRpc result: {e}");
                    e
                })?
                .to_string()
                .map_err(|e| {
                    error!("Failed to decode executeRpc result as text: {e}");
                    BitcoinCoreSv2JDPError::FailedToDecodeExecuteRpcResultText(e.to_string())
                })?;

            let rpc_response_json: Value = serde_json::from_str(&rpc_response_text)
                .map_err(BitcoinCoreSv2JDPError::FailedToParseExecuteRpcJsonResponse)?;

            if !rpc_response_json["error"].is_null() {
                return Err(BitcoinCoreSv2JDPError::GetBlockHeaderRpcReturnedError(
                    rpc_response_json["error"].to_string(),
                ));
            }

            let header_hex = rpc_response_json["result"].as_str().ok_or_else(|| {
                BitcoinCoreSv2JDPError::GetBlockHeaderRpcResultIsNotHexString
            })?;
            debug!(
                header_hex_len = header_hex.len(),
                tip_hash = %tip_hash,
                "Deserializing block header from getblockheader RPC"
            );

            deserialize_hex(header_hex)
                .map_err(BitcoinCoreSv2JDPError::FailedToDeserializeHeaderHex)?
        };

        // Fetch `getmininginfo.next.bits` for the **next-block** `nBits`.
        // At difficulty-adjustment boundaries the tip header's `nBits` differs from
        // the next block's required `nBits`.  `checkBlock` enforces next-block `nBits`
        // even when `check_pow` is disabled, so we must source the value from
        // `getmininginfo` rather than from the tip header.
        let next_nbits = {
            let mut execute_rpc_request = self.rpc_ipc_client.execute_rpc_request();
            execute_rpc_request
                .get()
                .get_context()
                .map_err(|e| {
                    error!("Failed to get executeRpc request context: {e}");
                    e
                })?
                .set_thread(self.thread_ipc_client.clone());

            let request = r#"{"jsonrpc":"2.0","id":"v32-jdp-nbits","method":"getmininginfo","params":[]}"#;
            execute_rpc_request.get().set_request(request);

            let execute_rpc_response = execute_rpc_request.send().promise.await.map_err(|e| {
                error!("Failed to send executeRpc request: {e}");
                e
            })?;

            let rpc_response_text = execute_rpc_response
                .get()
                .map_err(|e| {
                    error!("Failed to read executeRpc response: {e}");
                    e
                })?
                .get_result()
                .map_err(|e| {
                    error!("Failed to get executeRpc result: {e}");
                    e
                })?
                .to_string()
                .map_err(|e| {
                    error!("Failed to decode executeRpc result as text: {e}");
                    BitcoinCoreSv2JDPError::FailedToDecodeExecuteRpcResultText(e.to_string())
                })?;

            parse_next_nbits_from_getmininginfo(&rpc_response_text)?
        };

        let mut chain_tip_state = self.chain_tip_state.borrow_mut();
        chain_tip_state.set(tip_hash, next_nbits, header.time);

        Ok(())
    }

    /// Forces a synchronous chain-tip refresh from Bitcoin Core.
    ///
    /// This is used after `checkBlock` failures to reduce stale-tip classification races.
    /// On transient `"thread busy"` IPC contention, this method retries a few times with
    /// a short backoff before returning the error.
    pub(crate) async fn force_update_chain_tip_state(
        &self,
    ) -> Result<(), BitcoinCoreSv2JDPError> {
        const MAX_ATTEMPTS: usize = 3;
        const RETRY_BACKOFF_MS: u64 = 25;

        let mut last_error: Option<BitcoinCoreSv2JDPError> = None;

        for attempt in 1..=MAX_ATTEMPTS {
            match self.update_chain_tip_state(None).await {
                Ok(()) => return Ok(()),
                Err(e) if e.is_thread_busy() && attempt < MAX_ATTEMPTS => {
                    warn!(
                        error = ?e,
                        attempt,
                        max_attempts = MAX_ATTEMPTS,
                        "Transient IPC contention during force_update_chain_tip_state (thread busy); retrying"
                    );
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_BACKOFF_MS)).await;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error
            .unwrap_or(BitcoinCoreSv2JDPError::ForceUpdateChainTipStateExhaustedRetries))
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

    /// Interrupts an in-flight `waitTipChanged` request to Bitcoin Core.
    async fn interrupt_wait_tip_changed_request(&self) -> Result<(), BitcoinCoreSv2JDPError> {
        let interrupt_request = self.mining_ipc_client.interrupt_request();
        if let Err(e) = interrupt_request.send().promise.await {
            error!("Failed to send interrupt waitTipChanged request: {}", e);
            return Err(BitcoinCoreSv2JDPError::CapnpError(e));
        }

        Ok(())
    }
}

/// Parse `next.bits` from a `getmininginfo` JSON-RPC response.
///
/// Returns the parsed [`CompactTarget`] on success, or a
/// [`BitcoinCoreSv2JDPError`] variant if the response is malformed.
fn parse_next_nbits_from_getmininginfo(
    rpc_response_text: &str,
) -> Result<CompactTarget, BitcoinCoreSv2JDPError> {
    let rpc_response_json: Value = serde_json::from_str(rpc_response_text)
        .map_err(BitcoinCoreSv2JDPError::FailedToParseExecuteRpcJsonResponse)?;

    if !rpc_response_json["error"].is_null() {
        return Err(BitcoinCoreSv2JDPError::GetMiningInfoRpcReturnedError(
            rpc_response_json["error"].to_string(),
        ));
    }

    let next_bits_hex = rpc_response_json["result"]["next"]["bits"]
        .as_str()
        .ok_or(BitcoinCoreSv2JDPError::GetMiningInfoMissingNextBits)?;

    let next_bits_u32 = u32::from_str_radix(next_bits_hex, 16).map_err(|e| {
        BitcoinCoreSv2JDPError::FailedToParseGetMiningInfoNextBits(e.to_string())
    })?;

    Ok(CompactTarget::from_consensus(next_bits_u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::error::BitcoinCoreSv2JDPError;

    #[test]
    fn parse_getmininginfo_next_bits_valid() {
        let json = r#"{"result":{"blocks":864000,"currentblockweight":3999999,"currentblocktx":1234,"next":{"height":864001,"bits":"1702e4d1","difficulty":12345.6789,"target":"0000000000000002e4d10000000000000000000000000000000000000000000000"},"errors":"","warnings":""},"error":null,"id":"v32-jdp-nbits"}"#;
        let result = parse_next_nbits_from_getmininginfo(json);
        assert!(result.is_ok(), "expected ok, got {:?}", result);
        let nbits = result.unwrap();
        assert_eq!(nbits.to_consensus(), 0x1702e4d1);
    }

    #[test]
    fn parse_getmininginfo_rpc_error() {
        let json = r#"{"result":null,"error":{"code":-1,"message":"test error"},"id":"v32-jdp-nbits"}"#;
        let result = parse_next_nbits_from_getmininginfo(json);
        match result {
            Err(BitcoinCoreSv2JDPError::GetMiningInfoRpcReturnedError(_)) => {}
            other => panic!("expected GetMiningInfoRpcReturnedError, got {:?}", other),
        }
    }

    #[test]
    fn parse_getmininginfo_missing_next_bits() {
        let json = r#"{"result":{"next":{}},"error":null,"id":"v32-jdp-nbits"}"#;
        let result = parse_next_nbits_from_getmininginfo(json);
        match result {
            Err(BitcoinCoreSv2JDPError::GetMiningInfoMissingNextBits) => {}
            other => panic!("expected GetMiningInfoMissingNextBits, got {:?}", other),
        }
    }

    #[test]
    fn parse_getmininginfo_next_bits_invalid_hex() {
        let json = r#"{"result":{"next":{"bits":"xyz"}},"error":null,"id":"v32-jdp-nbits"}"#;
        let result = parse_next_nbits_from_getmininginfo(json);
        match result {
            Err(BitcoinCoreSv2JDPError::FailedToParseGetMiningInfoNextBits(_)) => {}
            other => panic!(
                "expected FailedToParseGetMiningInfoNextBits, got {:?}",
                other
            ),
        }
    }
}
