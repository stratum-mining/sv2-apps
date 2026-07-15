//! Module for validating and propagating solutions for Custom Jobs using Bitcoin Core over IPC.

use crate::{
    error::JDSErrorKind,
    job_declarator::{
        job_validation::{DeclareMiningJobResult, JobValidationEngine, SetCustomMiningJobResult},
        ALLOCATED_TOKEN_TIMEOUT_SECS, JANITOR_INTERVAL_SECS,
    },
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use stratum_apps::{
    bitcoin_core_sv2::runtime_api::{
        job_declaration_protocol::{
            self,
            io::{JdRequest, JdResponse, ValidationContext},
            CancellationToken,
        },
        BitcoinCoreVersion,
    },
    stratum_core::{
        bitcoin::{
            self,
            block::Version,
            consensus::{Decodable, Encodable},
            hashes::Hash,
            BlockHash, CompactTarget, Transaction, TxMerkleNode, Txid, Wtxid,
        },
        job_declaration_sv2::{
            DeclareMiningJob, ProvideMissingTransactionsSuccess, PushSolution,
            ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
            ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
        },
        mining_sv2::{
            SetCustomMiningJob, ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_PREFIX,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_INPUT_N_SEQUENCE,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_LOCKTIME,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_OUTPUTS,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_VERSION,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_MERKLE_PATH,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_MINING_JOB_TOKEN,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_NBITS,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_VERSION,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_JOB_NOT_YET_VALIDATED,
            ERROR_CODE_SET_CUSTOM_MINING_JOB_STALE_CHAIN_TIP,
        },
    },
    sync::SharedMap,
    tp_type::BitcoinNetwork,
    utils::types::{DownstreamId, JdToken, RequestId},
};

/// Snapshot of a previously declared mining job, stored after a `DeclareMiningJob` is
/// successfully validated (or while waiting for missing transactions).
///
/// Used by [`BitcoinCoreIPCEngine::handle_set_custom_mining_job`] to cross-check that a
/// subsequent `SetCustomMiningJob` matches the original declaration.
#[derive(Clone)]
struct DeclaredCustomJob {
    declare_mining_job: DeclareMiningJob<'static>,
    validation_context: ValidationContext, // committed at the time we receive DeclareMiningJob
    txid_list: Option<Vec<Txid>>,          // populated only on JdResponse::Success
    validated: bool,
}

#[derive(Clone, Copy)]
struct AllocatedTokenEntry {
    request_id: RequestId,
    inserted_at: Instant,
}

/// Per-downstream client state for declared custom jobs and their token entries.
#[derive(Default)]
struct DownstreamState {
    declared_custom_jobs: HashMap<RequestId, DeclaredCustomJob>,
    allocated_token_entries: HashMap<JdToken, AllocatedTokenEntry>,
}

impl DownstreamState {
    /// Stores a DeclaredCustomJob and its corresponding allocated token.
    fn insert_declared_custom_job(
        &mut self,
        request_id: RequestId,
        allocated_token: JdToken,
        declared_custom_job: DeclaredCustomJob,
    ) {
        self.declared_custom_jobs
            .insert(request_id, declared_custom_job);
        self.allocated_token_entries.insert(
            allocated_token,
            AllocatedTokenEntry {
                request_id,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Removes both the DeclaredCustomJob and its corresponding allocated token.
    fn remove_declared_custom_job(&mut self, request_id: RequestId, allocated_token: JdToken) {
        self.declared_custom_jobs.remove(&request_id);
        self.allocated_token_entries.remove(&allocated_token);
    }

    /// Atomically removes and returns a declared custom job by its token.
    fn take_declared_custom_job(&mut self, allocated_token: JdToken) -> Option<DeclaredCustomJob> {
        let entry = self.allocated_token_entries.remove(&allocated_token)?;
        let job = self.declared_custom_jobs.remove(&entry.request_id)?;
        Some(job)
    }

    /// Removes expired allocated tokens and their corresponding DeclaredCustomJob.
    /// Returns a list of expired `(token, request_id)` pairs for logging.
    fn prune_expired_allocations(
        &mut self,
        now: Instant,
        token_timeout: Duration,
    ) -> Vec<(JdToken, RequestId)> {
        let mut expired = Vec::new();
        self.allocated_token_entries.retain(|token, entry| {
            let keep = now.saturating_duration_since(entry.inserted_at) <= token_timeout;
            if !keep {
                self.declared_custom_jobs.remove(&entry.request_id);
                expired.push((*token, entry.request_id));
            }
            keep
        });

        expired
    }

    /// Looks up the ValidationContext for a given RequestId.
    fn validation_context_for_request(&self, request_id: RequestId) -> Option<ValidationContext> {
        self.declared_custom_jobs
            .get(&request_id)
            .map(|job| job.validation_context)
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl DeclaredCustomJob {
    /// Returns the block version from the original `DeclareMiningJob`.
    fn get_version(&self) -> u32 {
        self.declare_mining_job.version
    }

    /// Returns `nbits` (difficulty target).
    fn get_nbits(&self) -> u32 {
        self.validation_context.nbits.to_consensus()
    }

    /// Returns `prev_hash`.
    fn get_prev_hash(&self) -> BlockHash {
        self.validation_context.prev_hash
    }

    /// Reconstructs the declared coinbase transaction by concatenating prefix, extranonce (zeros),
    /// and suffix.
    ///
    /// The extranonce size is calculated from the scriptSig size in the coinbase_tx_prefix
    ///
    /// Error type is () because we don't need extra granularity for error_code =
    /// "invalid-coinbase-tx"
    fn get_coinbase_tx(&self) -> Result<Transaction, ()> {
        let declared_coinbase_tx_prefix: Vec<u8> =
            self.declare_mining_job.coinbase_tx_prefix.to_owned_bytes();
        let declared_coinbase_tx_suffix: Vec<u8> =
            self.declare_mining_job.coinbase_tx_suffix.to_owned_bytes();

        // Parse scriptSig size from coinbase prefix
        // Coinbase structure: version(4) + marker+flag(2) + input_count(1) + outpoint(32) +
        // index(4) = 43 bytes Then comes scriptSig length (VarInt) followed by scriptSig
        // data
        const COINBASE_PREFIX_LEN: usize = 43;
        let script_sig_size: usize = {
            let mut cursor = &declared_coinbase_tx_prefix[COINBASE_PREFIX_LEN..];
            match bitcoin::VarInt::consensus_decode(&mut cursor) {
                Ok(varint) => varint.0 as usize,
                Err(e) => {
                    tracing::error!(
                        "Failed to decode scriptSig size from coinbase prefix: {}",
                        e
                    );
                    return Err(());
                }
            }
        };

        // Calculate the size of scriptSig bytes already in the prefix.
        let varint_size = bitcoin::VarInt(script_sig_size as u64).size();
        let script_sig_offset = COINBASE_PREFIX_LEN + varint_size;
        let script_sig_bytes_in_prefix = declared_coinbase_tx_prefix.len() - script_sig_offset;

        // The full extranonce fills the remaining space in scriptSig
        let full_extranonce_size: usize = script_sig_size - script_sig_bytes_in_prefix;

        // Concatenate prefix + full extranonce (zeros) + suffix to form the complete transaction
        // bytes
        let mut declared_coinbase_tx = declared_coinbase_tx_prefix;
        declared_coinbase_tx.extend_from_slice(&vec![0; full_extranonce_size]);
        declared_coinbase_tx.extend_from_slice(&declared_coinbase_tx_suffix);

        // Deserialize the transaction
        bitcoin::consensus::Decodable::consensus_decode(&mut &declared_coinbase_tx[..]).map_err(
            |e| {
                tracing::error!("Failed to deserialize declared coinbase transaction: {}", e);
            },
        )
    }

    /// Computes the coinbase merkle branch in the txid merkle tree.
    ///
    /// Returns the sibling hashes at each level from leaf to root, needed to
    /// reconstruct the block header's merkle root from the coinbase position (index 0).
    ///
    /// Requires `txid_list` to have been populated via `JdResponse::Success`.
    /// The coinbase txid is derived from the declared coinbase prefix/suffix.
    ///
    /// Used to compare with a `SetCustomMiningJob.merkle_path`.
    ///
    /// Internally, errors may come from missing txid_list
    /// so error_code = "declared-job-not-yet-validated"
    /// therefore () error type is sufficient.
    fn get_merkle_path(&self) -> Result<Vec<TxMerkleNode>, ()> {
        if !self.validated {
            return Err(());
        }

        let txid_list = self.txid_list.as_ref().ok_or(())?;

        let coinbase_tx = self
            .get_coinbase_tx()
            .expect("coinbase tx already validated");
        let coinbase_txid: TxMerkleNode = coinbase_tx.compute_txid().into();

        let mut hashes: Vec<TxMerkleNode> = Vec::with_capacity(1 + txid_list.len());
        hashes.push(coinbase_txid);
        for txid in txid_list {
            hashes.push((*txid).into());
        }

        if hashes.len() == 1 {
            return Ok(Vec::new());
        }

        let mut branch = Vec::new();

        while hashes.len() > 1 {
            branch.push(hashes[1]);

            let half = hashes.len().div_ceil(2);
            let mut next_level = Vec::with_capacity(half);
            for idx in 0..half {
                let left = hashes[2 * idx];
                let right = hashes[std::cmp::min(2 * idx + 1, hashes.len() - 1)];
                let mut engine = TxMerkleNode::engine();
                left.consensus_encode(&mut engine)
                    .expect("in-memory writers don't error");
                right
                    .consensus_encode(&mut engine)
                    .expect("in-memory writers don't error");
                next_level.push(TxMerkleNode::from_engine(engine));
            }
            hashes = next_level;
        }

        Ok(branch)
    }
}

/// Engine for validating and propagating solutions for Custom Jobs using Bitcoin Core over IPC.
///
/// Implements the [`JobValidationEngine`] trait.
#[derive(Clone)]
pub struct BitcoinCoreIPCEngine {
    request_sender: async_channel::Sender<JdRequest>,
    downstream_states: SharedMap<DownstreamId, DownstreamState>,
    cancellation_token: CancellationToken,
    jdp_thread_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl BitcoinCoreIPCEngine {
    /// Creates a new [`BitcoinCoreIPCEngine`] instance.
    ///
    /// Spawns a dedicated thread running BitcoinCoreSv2JDP in a LocalSet for handling
    /// the !Send Cap'n Proto client.
    ///
    /// `version` selects the Bitcoin Core IPC schema family (v30.x or v31.x).
    ///
    /// Blocks until the mempool mirror is bootstrapped and ready to process requests.
    pub async fn new(
        version: BitcoinCoreVersion,
        network: BitcoinNetwork,
        data_dir: Option<PathBuf>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, JDSErrorKind> {
        // Construct the Bitcoin Core Unix socket path
        let unix_socket_path = {
            let base_dir = match data_dir {
                Some(dir) => dir,
                None => {
                    // Use OS default Bitcoin data directory
                    let home = std::env::var("HOME").map_err(|e| {
                        JDSErrorKind::BitcoinCoreIPC(format!("Cannot get HOME directory: {e}"))
                    })?;

                    #[cfg(target_os = "macos")]
                    let base = PathBuf::from(home).join("Library/Application Support/Bitcoin");

                    #[cfg(target_os = "linux")]
                    let base = PathBuf::from(home).join(".bitcoin");

                    #[cfg(not(any(target_os = "macos", target_os = "linux",)))]
                    return Err(JDSErrorKind::BitcoinCoreIPC("Unsupported OS".to_string()));

                    base
                }
            };

            // Add network subdirectory if not mainnet
            let socket_dir = match network {
                BitcoinNetwork::Mainnet => base_dir,
                BitcoinNetwork::Testnet4 => base_dir.join("testnet4"),
                BitcoinNetwork::Signet => base_dir.join("signet"),
                BitcoinNetwork::Regtest => base_dir.join("regtest"),
            };

            socket_dir.join("node.sock")
        };

        // Create channel for communicating with BitcoinCoreSv2JDP
        let (request_sender, request_receiver) = async_channel::unbounded::<JdRequest>();

        // Create oneshot channel for readiness signaling
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

        let cancellation_token_clone = cancellation_token.clone();

        // Spawn dedicated thread for BitcoinCoreSv2JDP (requires !Send Cap'n Proto client)
        let jdp_thread_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new()
                .expect("Failed to create tokio runtime for BitcoinCoreSv2JDP");

            rt.block_on(async {
                let local_set = tokio::task::LocalSet::new();

                local_set
                    .run_until(async {
                        let bitcoin_core_sv2_jdp = match job_declaration_protocol::new(
                            version,
                            unix_socket_path,
                            request_receiver,
                            cancellation_token_clone.clone(),
                            ready_tx,
                        )
                        .await
                        {
                            Ok(client) => client,
                            Err(e) => {
                                if !cancellation_token_clone.is_cancelled() {
                                    tracing::error!("Failed to create BitcoinCoreSv2JDP: {:?}", e);
                                }
                                // ready_tx dropped here, signaling failure to ready_rx
                                return;
                            }
                        };

                        bitcoin_core_sv2_jdp.run().await;
                    })
                    .await;
            });
        });

        // Wait for BitcoinCoreSv2JDP to complete mempool bootstrap, mirroring the
        // pool's Template Provider startup behavior during IBD.
        // Until `new()` succeeds, this function is still the only owner of the spawned JDP
        // thread handle, so cancellation/bootstrap failure must join here rather than detach it.
        let mut ready_rx = ready_rx;
        loop {
            tokio::select! {
                res = &mut ready_rx => {
                    match res {
                        Ok(()) => break,
                        Err(_) => {
                            if let Err(e) = jdp_thread_handle.join() {
                                tracing::warn!("BitcoinCoreSv2JDP thread join failed: {e:?}");
                            }

                            return Err(JDSErrorKind::BitcoinCoreIPC(
                                "Mempool bootstrap did not complete".to_string(),
                            ));
                        }
                    }
                }
                _ = cancellation_token.cancelled() => {
                    tracing::info!("BitcoinCoreIPCEngine stopped before mempool bootstrap completed");
                    if let Err(e) = jdp_thread_handle.join() {
                        tracing::warn!("BitcoinCoreSv2JDP thread join failed during startup cancellation: {e:?}");
                    }
                    return Err(JDSErrorKind::BitcoinCoreIPC(
                        "Mempool bootstrap did not complete".to_string(),
                    ));
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    tracing::warn!("Waiting for initial template and prevhash from Template Provider...");
                    tracing::warn!("Is the Bitcoin node undergoing IBD?");
                }
            }
        }

        let downstream_states = SharedMap::<DownstreamId, DownstreamState>::new();

        // Spawn janitor task to clean up stale declared jobs that were never
        // consumed by SetCustomMiningJob.
        let janitor_downstream_states = downstream_states.clone();
        let janitor_cancellation = cancellation_token.clone();
        tokio::spawn(async move {
            let janitor_interval = Duration::from_secs(JANITOR_INTERVAL_SECS);
            let token_timeout = Duration::from_secs(ALLOCATED_TOKEN_TIMEOUT_SECS);
            loop {
                tokio::select! {
                    _ = janitor_cancellation.cancelled() => break,
                    _ = tokio::time::sleep(janitor_interval) => {
                        let now = Instant::now();
                        janitor_downstream_states.for_each_mut(|downstream_id, state| {
                            let expired = state.prune_expired_allocations(now, token_timeout);
                            for (token, request_id) in expired {
                                tracing::debug!(
                                    downstream_id,
                                    token,
                                    request_id,
                                    "Removed expired declared custom job state"
                                );
                            }
                        });
                    }
                }
            }
        });

        Ok(Self {
            request_sender,
            downstream_states,
            cancellation_token,
            jdp_thread_handle: Arc::new(Mutex::new(Some(jdp_thread_handle))),
        })
    }
}

fn validation_context_drifted(
    previous_ctx: ValidationContext,
    current_ctx: ValidationContext,
) -> bool {
    previous_ctx.prev_hash != current_ctx.prev_hash
}

#[cfg_attr(not(test), hotpath::measure_all)]
#[async_trait::async_trait]
impl JobValidationEngine for BitcoinCoreIPCEngine {
    fn shutdown(&self) {
        self.cancellation_token.cancel();
        if let Ok(mut handle_guard) = self.jdp_thread_handle.lock() {
            if let Some(handle) = handle_guard.take() {
                if let Err(e) = handle.join() {
                    tracing::warn!("BitcoinCoreSv2JDP thread join failed during shutdown: {e:?}");
                }
            }
        }
    }

    fn cleanup_downstream(&self, downstream_id: DownstreamId) {
        self.downstream_states.remove(&downstream_id);
    }

    /// Validates a `DeclareMiningJob` by forwarding it to Bitcoin Core over IPC.
    ///
    /// Steps:
    /// 1. Reconstruct and sanity-check the declared coinbase transaction.
    /// 2. Extract the wtxid list and any missing transactions.
    /// 3. Send a [`JdRequest::DeclareMiningJob`] to the IPC thread.
    /// 4. Map the `JdResponse` to a `DeclareMiningJobResult` and, on success, store a
    ///    `DeclaredCustomJob` for later `SetCustomMiningJob` validation.
    async fn handle_declare_mining_job(
        &self,
        downstream_id: DownstreamId,
        declare_mining_job: DeclareMiningJob<'_>,
        provide_missing_transactions_success: Option<ProvideMissingTransactionsSuccess<'_>>,
    ) -> DeclareMiningJobResult {
        // Extract allocated token from the message
        let allocated_token: JdToken = match declare_mining_job.mining_job_token.try_as_array::<8>()
        {
            Ok(token_bytes) => u64::from_le_bytes(token_bytes),
            Err(_) => {
                return DeclareMiningJobResult::Error(
                    ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
                )
            }
        };

        // Create temporary DeclaredCustomJob for extracting coinbase (without prev_hash/nbits yet)
        let declare_mining_job_static = declare_mining_job.clone().into_static();

        // Extract and validate coinbase transaction
        let declared_coinbase_tx = {
            let temp_job = DeclaredCustomJob {
                declare_mining_job: declare_mining_job_static.clone(),
                validation_context: ValidationContext {
                    prev_hash: BlockHash::all_zeros(), // irrelevant for coinbase tx validation
                    nbits: CompactTarget::from_consensus(0), /* irrelevant for coinbase tx
                                                        * validation */
                },
                txid_list: None,  // irrelevant for coinbase tx validation
                validated: false, // irrelevant for coinbase tx validation
            };

            match temp_job.get_coinbase_tx() {
                Ok(tx) => {
                    tracing::debug!("Declared coinbase transaction validated successfully");
                    tx
                }
                Err(_) => {
                    return DeclareMiningJobResult::Error(
                        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
                    )
                }
            }
        };

        // fully validate coinbase as a real coinbase
        {
            if declared_coinbase_tx.input.len() != 1 {
                return DeclareMiningJobResult::Error(
                    ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
                );
            }
        }

        // Extract wtxid_list from DeclareMiningJob message
        let wtxid_list: Vec<Wtxid> = declare_mining_job
            .wtxid_list
            .iter()
            .map(|u256| Wtxid::from_byte_array(u256.to_array()))
            .collect();

        // Parse missing transactions from ProvideMissingTransactionsSuccess
        let missing_txs: Vec<Transaction> =
            if let Some(ref pmts) = provide_missing_transactions_success {
                pmts.transaction_list
                    .iter_bytes()
                    .filter_map(|tx_bytes| {
                        match bitcoin::consensus::Decodable::consensus_decode(&mut &tx_bytes[..]) {
                            Ok(tx) => Some(tx),
                            Err(e) => {
                                tracing::error!("Failed to decode transaction: {}", e);
                                None
                            }
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };

        let previous_pending_validation_context =
            provide_missing_transactions_success.as_ref().and_then(|_| {
                self.downstream_states
                    .with(&downstream_id, |state| {
                        state.validation_context_for_request(declare_mining_job.request_id)
                    })
                    .flatten()
            });

        // Ensure downstream state exists before awaiting IPC response.
        // Later writes must use `with_mut` only, so a disconnect cleanup that removes this
        // state cannot be undone by recreating it after the response arrives.
        self.downstream_states
            .with_mut_or_default(downstream_id, |_| {});

        // Create oneshot channel for response
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        // Send request to BitcoinCoreSv2JDP (clone wtxid_list since we need it for error handling)
        let request = JdRequest::DeclareMiningJob {
            version: Version::from_consensus(declare_mining_job.version as i32),
            coinbase_tx: declared_coinbase_tx,
            wtxid_list: wtxid_list.clone(),
            missing_txs,
            response_tx,
        };

        if let Err(e) = self.request_sender.send(request).await {
            tracing::error!("Failed to send DeclareMiningJob request: {}", e);
            // string here is error_code for the DeclareMiningJobError message
            return DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR);
        }

        // Wait for response
        let response = match response_rx.await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::error!("Failed to receive DeclareMiningJob response: {}", e);
                // string here is error_code for the DeclareMiningJobError message
                return DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR);
            }
        };

        // Convert JdResponse to DeclareMiningJobResult
        match response {
            JdResponse::Success {
                prev_hash,
                nbits,
                txid_list,
            } => {
                let declared_custom_job = DeclaredCustomJob {
                    declare_mining_job: declare_mining_job_static,
                    validation_context: ValidationContext { prev_hash, nbits },
                    txid_list: Some(txid_list),
                    validated: true,
                };
                let updated = self.downstream_states.with_mut(&downstream_id, |state| {
                    state.insert_declared_custom_job(
                        declare_mining_job.request_id,
                        allocated_token,
                        declared_custom_job,
                    );
                });
                if updated.is_none() {
                    tracing::error!(downstream_id, "downstream state missing after IPC response");
                    return DeclareMiningJobResult::Error(
                        ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                    );
                }
                DeclareMiningJobResult::Success
            }
            JdResponse::Error {
                error_code,
                validation_context,
            } => {
                self.downstream_states.with_mut(&downstream_id, |state| {
                    state
                        .remove_declared_custom_job(declare_mining_job.request_id, allocated_token);
                });

                let tip_drifted = previous_pending_validation_context
                    .map(|previous_ctx| {
                        validation_context_drifted(previous_ctx, validation_context)
                    })
                    .unwrap_or(false);

                if tip_drifted {
                    DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP)
                } else {
                    DeclareMiningJobResult::Error(error_code)
                }
            }
            JdResponse::MissingTransactions {
                missing_wtxids,
                validation_context,
            } => {
                let tip_drifted = previous_pending_validation_context
                    .map(|previous_ctx| {
                        validation_context_drifted(previous_ctx, validation_context)
                    })
                    .unwrap_or(false);

                // If this is a retry after ProvideMissingTransactionsSuccess and context drifted,
                // classify as stale-chain-tip instead of asking for yet another missing-txs round.
                if provide_missing_transactions_success.is_some() && tip_drifted {
                    self.downstream_states.with_mut(&downstream_id, |state| {
                        state.remove_declared_custom_job(
                            declare_mining_job.request_id,
                            allocated_token,
                        );
                    });

                    DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP)
                } else {
                    let declared_custom_job = DeclaredCustomJob {
                        declare_mining_job: declare_mining_job_static,
                        validation_context,
                        txid_list: None,
                        validated: false, // this is only set to true on JdResponse::Success
                    };
                    let updated = self.downstream_states.with_mut(&downstream_id, |state| {
                        state.insert_declared_custom_job(
                            declare_mining_job.request_id,
                            allocated_token,
                            declared_custom_job,
                        );
                    });
                    if updated.is_none() {
                        tracing::error!(
                            downstream_id,
                            "downstream state missing after IPC response"
                        );
                        return DeclareMiningJobResult::Error(
                            ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        );
                    }

                    DeclareMiningJobResult::MissingTransactions(missing_wtxids)
                }
            }
        }
    }

    async fn handle_push_solution(
        &self,
        downstream_id: DownstreamId,
        push_solution: PushSolution<'_>,
    ) {
        // Convert to static lifetime for channel transfer
        let push_solution_static = push_solution.into_static();

        // Send request to BitcoinCoreSv2JDP (fire-and-forget)
        let request = JdRequest::PushSolution {
            push_solution: push_solution_static,
        };

        if let Err(e) = self.request_sender.send(request).await {
            tracing::error!(downstream_id, "Failed to send PushSolution request: {}", e);
        } else {
            tracing::debug!(downstream_id, "PushSolution request sent successfully");
        }
    }

    // we make sure SetCustomMiningJob matches its corresponding DeclareMiningJob with regards to:
    // - prev_hash
    // - nbits
    // - version
    // - coinbase tx
    // - merkle path
    //
    // it's the caller responsability to make sure allocated_token matches the corresponding
    // DeclareMiningJob token.
    async fn handle_set_custom_mining_job(
        &self,
        downstream_id: DownstreamId,
        set_custom_mining_job: SetCustomMiningJob<'_>,
        allocated_token: JdToken, // Note: This is the corresponding DeclareMiningJob token
    ) -> SetCustomMiningJobResult {
        let declared_custom_job = match self
            .downstream_states
            .with_mut(&downstream_id, |state| {
                state.take_declared_custom_job(allocated_token)
            })
            .flatten()
        {
            Some(declared_custom_job) => declared_custom_job,
            None => {
                tracing::debug!(
                    downstream_id,
                    allocated_token,
                    "Provided token is not associated with any DeclareMiningJob request"
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_MINING_JOB_TOKEN,
                );
            }
        };

        // Job may be pending retry after missing txs and not fully validated yet.
        if !declared_custom_job.validated {
            tracing::error!("Job not yet validated");
            return SetCustomMiningJobResult::Error(
                ERROR_CODE_SET_CUSTOM_MINING_JOB_JOB_NOT_YET_VALIDATED,
            );
        }

        // Get declared values from stored job
        let declared_prev_hash = declared_custom_job.get_prev_hash();
        let declared_nbits = declared_custom_job.get_nbits();
        let declared_version: u32 = declared_custom_job.get_version();

        // Extract values from SetCustomMiningJob message
        let custom_job_prev_hash = {
            let bytes = set_custom_mining_job.prev_hash.to_array();
            BlockHash::from_byte_array(bytes)
        };
        let custom_job_nbits: u32 = set_custom_mining_job.nbits;
        let custom_job_version: u32 = set_custom_mining_job.version;

        // Validate prev_hash
        {
            if custom_job_prev_hash != declared_prev_hash {
                tracing::debug!(
                    "prev_hash mismatch: custom={:?}, declared={:?}",
                    custom_job_prev_hash,
                    declared_prev_hash
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_STALE_CHAIN_TIP,
                );
            }
        }

        // Validate nbits
        {
            if custom_job_nbits != declared_nbits {
                tracing::debug!(
                    "nbits mismatch: custom={}, declared={}",
                    custom_job_nbits,
                    declared_nbits
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_NBITS,
                );
            }
        }

        // Validate version
        {
            if custom_job_version != declared_version {
                tracing::debug!(
                    "version mismatch: custom={}, declared={}",
                    custom_job_version,
                    declared_version
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_VERSION,
                );
            }
        }

        // validate coinbase tx
        {
            let declared_coinbase_tx = match declared_custom_job.get_coinbase_tx() {
                Ok(tx) => tx,
                Err(_) => {
                    return SetCustomMiningJobResult::Error(
                        ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX,
                    )
                }
            };

            if declared_coinbase_tx.version.0 != set_custom_mining_job.coinbase_tx_version as i32 {
                tracing::debug!(
                    "coinbase version mismatch: custom={}, declared={}",
                    set_custom_mining_job.coinbase_tx_version,
                    declared_coinbase_tx.version.0
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_VERSION,
                );
            }

            let script_sig = declared_coinbase_tx.input[0].script_sig.as_bytes();
            let coinbase_prefix = set_custom_mining_job.coinbase_prefix.as_bytes();
            if !script_sig.starts_with(coinbase_prefix) {
                tracing::debug!("coinbase prefix mismatch");
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_PREFIX,
                );
            }

            if declared_coinbase_tx.input[0].sequence.0
                != set_custom_mining_job.coinbase_tx_input_n_sequence
            {
                tracing::debug!(
                    "coinbase input sequence mismatch: custom={}, declared={}",
                    set_custom_mining_job.coinbase_tx_input_n_sequence,
                    declared_coinbase_tx.input[0].sequence.0
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_INPUT_N_SEQUENCE,
                );
            }

            let declared_outputs_bytes =
                bitcoin::consensus::serialize(&declared_coinbase_tx.output);
            if declared_outputs_bytes != set_custom_mining_job.coinbase_tx_outputs.as_bytes() {
                tracing::debug!("coinbase outputs mismatch");
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_OUTPUTS,
                );
            }

            if declared_coinbase_tx.lock_time.to_consensus_u32()
                != set_custom_mining_job.coinbase_tx_locktime
            {
                tracing::debug!(
                    "coinbase locktime mismatch: custom={}, declared={}",
                    set_custom_mining_job.coinbase_tx_locktime,
                    declared_coinbase_tx.lock_time.to_consensus_u32()
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX_LOCKTIME,
                );
            }
        }

        // validate merkle path
        {
            let declared_merkle_path = match declared_custom_job.get_merkle_path() {
                Ok(path) => path,
                Err(_) => {
                    return SetCustomMiningJobResult::Error(
                        ERROR_CODE_SET_CUSTOM_MINING_JOB_JOB_NOT_YET_VALIDATED,
                    )
                }
            };

            let custom_merkle_path: Vec<TxMerkleNode> = set_custom_mining_job
                .merkle_path
                .iter()
                .map(|u256| TxMerkleNode::from_byte_array(u256.to_array()))
                .collect();

            if declared_merkle_path != custom_merkle_path {
                tracing::debug!(
                    "merkle path mismatch: custom={:?}, declared={:?}",
                    custom_merkle_path,
                    declared_merkle_path
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_MERKLE_PATH,
                );
            }
        }

        SetCustomMiningJobResult::Success
    }
}
