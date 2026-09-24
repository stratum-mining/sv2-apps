//! Module for validating and propagating solutions for Custom Jobs using Bitcoin Core over IPC.

use crate::{
    error::JDSErrorKind,
    job_declarator::{
        ALLOCATED_TOKEN_TIMEOUT_SECS, JANITOR_INTERVAL_SECS,
        job_validation::{DeclareMiningJobResult, JobValidationEngine, SetCustomMiningJobResult},
    },
};
use std::{
    collections::HashMap,
    path::PathBuf,
    thread::JoinHandle,
    time::{Duration, Instant},
};
use stratum_apps::{
    bitcoin_core_sv2::runtime_api::{
        BitcoinCoreVersion,
        job_declaration_protocol::{
            self, CancellationToken,
            io::{JdRequest, JdResponse, ValidationContext},
        },
    },
    stratum_core::{
        bitcoin::{
            self, Block, BlockHash, CompactTarget, Transaction, TxMerkleNode, Wtxid,
            block::{Header, Version},
            consensus::{Decodable, Encodable},
            hashes::Hash,
        },
        channels_sv2::{VERSION_ROLLING_MASK, merkle_root::merkle_root_from_path_},
        job_declaration_sv2::{
            DeclareMiningJobOwned, ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
            ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP, ProvideMissingTransactionsSuccessOwned,
            PushSolutionOwned,
        },
        mining_sv2::{
            ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_PREFIX,
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
            ERROR_CODE_SET_CUSTOM_MINING_JOB_STALE_CHAIN_TIP, SetCustomMiningJobOwned,
        },
    },
    sync::{SharedLock, SharedMap},
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
    declare_mining_job: DeclareMiningJobOwned,
    validation_context: ValidationContext, // committed at the time we receive DeclareMiningJob
    merkle_path: Vec<TxMerkleNode>,        // populated only on JdResponse::Success
    validated: bool,
}

/// A downstream's most recent `DeclareMiningJob.Success` with its non-coinbase transactions,
/// which [`BitcoinCoreIPCEngine::handle_push_solution`] rebuilds a solved block from.
struct SolvableJob {
    job: DeclaredCustomJob,
    txdata: Vec<Transaction>,
}

#[derive(Clone, Copy)]
struct AllocatedTokenEntry {
    request_id: RequestId,
    inserted_at: Instant,
}

/// Per-downstream client state for declared custom jobs and their token entries.
///
/// A `PushSolution` is rebuilt from the downstream's most recent `DeclareMiningJob.Success`, the
/// only job whose transactions are kept, since they are a block's worth of memory. It stays until
/// a newer success replaces it, whatever happens to its `SetCustomMiningJob` or its token. An
/// older job keeps its coinbase merkle branch, the only part of its transactions
/// `SetCustomMiningJob` validation compares, so it is still accepted, and a solution found on it is
/// propagated by the client alone.
#[derive(Default)]
struct DownstreamState {
    declared_custom_jobs: HashMap<RequestId, DeclaredCustomJob>,
    allocated_token_entries: HashMap<JdToken, AllocatedTokenEntry>,
    most_recent_success: Option<SolvableJob>,
}

impl DownstreamState {
    /// Stores a DeclaredCustomJob and its corresponding allocated token.
    ///
    /// A token stays allocated until a declaration made with it succeeds, so it can be declared
    /// with again under a new request id, and the job it pointed at before goes, since no token
    /// reaches it any more.
    fn insert_declared_custom_job(
        &mut self,
        request_id: RequestId,
        allocated_token: JdToken,
        declared_custom_job: DeclaredCustomJob,
    ) {
        self.declared_custom_jobs
            .insert(request_id, declared_custom_job);
        let previous = self.allocated_token_entries.insert(
            allocated_token,
            AllocatedTokenEntry {
                request_id,
                inserted_at: Instant::now(),
            },
        );
        if let Some(previous) = previous.filter(|previous| previous.request_id != request_id) {
            self.declared_custom_jobs.remove(&previous.request_id);
        }
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

    /// Reconstructs the declared coinbase transaction by concatenating prefix, extranonce, and
    /// suffix. Validation call sites pass `None` to fill the extranonce with zeros; a
    /// `PushSolution` passes the solved extranonce.
    ///
    /// The extranonce size is calculated from the scriptSig size in the coinbase_tx_prefix
    ///
    /// Error type is () because we don't need extra granularity for error_code =
    /// "invalid-coinbase-tx"
    fn get_coinbase_tx(&self, extranonce: Option<&[u8]>) -> Result<Transaction, ()> {
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

        let extranonce_bytes = match extranonce {
            Some(bytes) => {
                if bytes.len() != full_extranonce_size {
                    tracing::error!(
                        "PushSolution extranonce size mismatch: expected {}, got {}",
                        full_extranonce_size,
                        bytes.len()
                    );
                    return Err(());
                }
                bytes.to_vec()
            }
            None => vec![0; full_extranonce_size],
        };

        // Concatenate prefix + extranonce + suffix to form the complete transaction bytes.
        let mut declared_coinbase_tx = declared_coinbase_tx_prefix;
        declared_coinbase_tx.extend_from_slice(&extranonce_bytes);
        declared_coinbase_tx.extend_from_slice(&declared_coinbase_tx_suffix);

        // Deserialize the transaction
        bitcoin::consensus::Decodable::consensus_decode(&mut &declared_coinbase_tx[..]).map_err(
            |e| {
                tracing::error!("Failed to deserialize declared coinbase transaction: {}", e);
            },
        )
    }

    /// The coinbase merkle branch of the declared job, to compare with a
    /// `SetCustomMiningJob.merkle_path`.
    ///
    /// Errors only while the job is not validated yet, so error_code =
    /// "declared-job-not-yet-validated" and the () error type is sufficient.
    fn get_merkle_path(&self) -> Result<Vec<TxMerkleNode>, ()> {
        if !self.validated {
            return Err(());
        }
        Ok(self.merkle_path.clone())
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl SolvableJob {
    /// Rebuilds the block a `PushSolution` solved on this job, with the solved extranonce
    /// spliced into the declared coinbase and the merkle root recomputed, and returns it only
    /// if its header hash meets the target. A solution found on a different job at the same
    /// chain tip passes every field check `PushSolution` allows and fails only here, so the
    /// header is checked before the transactions are touched.
    ///
    /// Only the coinbase leaf of the merkle tree depends on the extranonce, so the root is
    /// folded from the solved coinbase txid and the branch kept since the declaration, and the
    /// other transactions are not hashed again.
    fn solved_block(
        &self,
        downstream_id: DownstreamId,
        push_solution: &PushSolutionOwned,
    ) -> Option<Block> {
        let Ok(coinbase_tx) = self
            .job
            .get_coinbase_tx(Some(push_solution.extranonce.as_ref()))
        else {
            tracing::error!(
                downstream_id,
                "Failed to reconstruct solved coinbase transaction"
            );
            return None;
        };

        let merkle_root = merkle_root_from_path_(
            coinbase_tx.compute_txid().to_byte_array(),
            &self.job.merkle_path,
        );

        let header = Header {
            version: Version::from_consensus(push_solution.version as i32),
            prev_blockhash: self.job.get_prev_hash(),
            merkle_root: TxMerkleNode::from_byte_array(merkle_root),
            time: push_solution.ntime,
            bits: CompactTarget::from_consensus(push_solution.nbits),
            nonce: push_solution.nonce,
        };
        header.validate_pow(header.target()).ok()?;

        Some(Block {
            header,
            txdata: std::iter::once(coinbase_tx)
                .chain(self.txdata.iter().cloned())
                .collect(),
        })
    }
}

/// Computes the coinbase merkle branch of a block whose non-coinbase transactions are `txdata`,
/// in order: the sibling hash at each level from the coinbase leaf (index 0) up to the root.
///
/// The coinbase itself is never its own sibling, so the branch does not depend on it and a
/// placeholder stands in at index 0.
fn coinbase_merkle_path(txdata: &[Transaction]) -> Vec<TxMerkleNode> {
    let mut hashes: Vec<TxMerkleNode> = Vec::with_capacity(1 + txdata.len());
    hashes.push(TxMerkleNode::all_zeros());
    for tx in txdata {
        hashes.push(tx.compute_txid().into());
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

    branch
}

/// Engine for validating and propagating solutions for Custom Jobs using Bitcoin Core over IPC.
///
/// Implements the [`JobValidationEngine`] trait.
#[derive(Clone)]
pub struct BitcoinCoreIPCEngine {
    request_sender: async_channel::Sender<JdRequest>,
    downstream_states: SharedMap<DownstreamId, DownstreamState>,
    cancellation_token: CancellationToken,
    jdp_thread_handle: SharedLock<Option<JoinHandle<()>>>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl BitcoinCoreIPCEngine {
    /// Creates a new [`BitcoinCoreIPCEngine`] instance.
    ///
    /// Spawns a dedicated thread running BitcoinCoreSv2JDP in a LocalSet for handling
    /// the !Send Cap'n Proto client.
    ///
    /// `version` selects the Bitcoin Core IPC schema family (v30.x, v31.x or v32.x).
    ///
    /// Blocks until the JDP runtime has bootstrapped and is ready to process requests.
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
                                if e.is_cancelled() {
                                    tracing::debug!(
                                        "BitcoinCoreSv2JDP bootstrap gave way to cancellation"
                                    );
                                } else {
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

        // Wait for BitcoinCoreSv2JDP to complete its bootstrap, mirroring the
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
                                "JDP bootstrap did not complete".to_string(),
                            ));
                        }
                    }
                }
                _ = cancellation_token.cancelled() => {
                    tracing::info!("BitcoinCoreIPCEngine stopped before JDP bootstrap completed");
                    if let Err(e) = jdp_thread_handle.join() {
                        tracing::warn!("BitcoinCoreSv2JDP thread join failed during startup cancellation: {e:?}");
                    }
                    return Err(JDSErrorKind::BitcoinCoreIPC(
                        "JDP bootstrap did not complete".to_string(),
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
            jdp_thread_handle: SharedLock::new(Some(jdp_thread_handle)),
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
        self.jdp_thread_handle
            .with(|handle| {
                if let Some(handle) = handle.take() {
                    if let Err(e) = handle.join() {
                        tracing::warn!(
                            "BitcoinCoreSv2JDP thread join failed during shutdown: {e:?}"
                        );
                    }
                }
            })
            .unwrap_or_else(|_| {
                tracing::warn!("BitcoinCoreSv2JDP thread handle lock poisoned during shutdown");
            });
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
        declare_mining_job: DeclareMiningJobOwned,
        provide_missing_transactions_success: Option<ProvideMissingTransactionsSuccessOwned>,
    ) -> DeclareMiningJobResult {
        // Extract allocated token from the message
        let allocated_token: JdToken = match declare_mining_job.mining_job_token.try_as_array::<8>()
        {
            Ok(token_bytes) => u64::from_le_bytes(token_bytes),
            Err(_) => {
                return DeclareMiningJobResult::Error(
                    ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
                );
            }
        };

        // Create temporary DeclaredCustomJob for extracting coinbase (without prev_hash/nbits yet)
        let declare_mining_job_static = declare_mining_job.clone();

        // Extract and validate coinbase transaction
        let declared_coinbase_tx = {
            let temp_job = DeclaredCustomJob {
                declare_mining_job: declare_mining_job_static.clone(),
                validation_context: ValidationContext {
                    prev_hash: BlockHash::all_zeros(), // irrelevant for coinbase tx validation
                    nbits: CompactTarget::from_consensus(0), /* irrelevant for coinbase tx
                                                        * validation */
                },
                merkle_path: Vec::new(), // irrelevant for coinbase tx validation
                validated: false,        // irrelevant for coinbase tx validation
            };

            match temp_job.get_coinbase_tx(None) {
                Ok(tx) => {
                    tracing::debug!("Declared coinbase transaction validated successfully");
                    tx
                }
                Err(_) => {
                    return DeclareMiningJobResult::Error(
                        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
                    );
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
                txdata,
            } => {
                let declared_custom_job = DeclaredCustomJob {
                    declare_mining_job: declare_mining_job_static,
                    validation_context: ValidationContext { prev_hash, nbits },
                    merkle_path: coinbase_merkle_path(&txdata),
                    validated: true,
                };
                let updated = self.downstream_states.with_mut(&downstream_id, |state| {
                    state.most_recent_success = Some(SolvableJob {
                        job: declared_custom_job.clone(),
                        txdata,
                    });
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
                        merkle_path: Vec::new(),
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
        push_solution: PushSolutionOwned,
    ) {
        let prev_hash = BlockHash::from_byte_array(push_solution.prev_hash.to_array());
        let solved_fixed_version_bits = push_solution.version & !VERSION_ROLLING_MASK;

        // The block is rebuilt from the most recent DeclareMiningJob.Success.
        // prev_hash and nbits must match exactly; version is matched on non-rollable bits only.
        // Every job declared at the solved chain tip passes this, so a solution found on another
        // job fails the target check on the rebuilt block instead. That one is propagated by the
        // client alone.
        let block = self
            .downstream_states
            .with(&downstream_id, |state| {
                let Some(solvable) = state.most_recent_success.as_ref() else {
                    tracing::info!(
                        downstream_id,
                        "Not propagating PushSolution: this downstream has no validated job to \
                         rebuild it from"
                    );
                    return None;
                };
                let job = &solvable.job;
                if job.get_prev_hash() != prev_hash
                    || job.get_nbits() != push_solution.nbits
                    || job.get_version() & !VERSION_ROLLING_MASK != solved_fixed_version_bits
                {
                    tracing::info!(
                        downstream_id,
                        got_prev_hash = ?prev_hash,
                        got_nbits = push_solution.nbits,
                        got_fixed_version_bits = solved_fixed_version_bits,
                        "Not propagating PushSolution: it does not match the most recent \
                         successfully declared job"
                    );
                    return None;
                }
                let block = solvable.solved_block(downstream_id, &push_solution);
                if block.is_none() {
                    tracing::info!(
                        downstream_id,
                        "Not propagating PushSolution: the most recent successfully declared job \
                         does not rebuild into a block meeting the target, so it was presumably \
                         found on another one"
                    );
                }
                block
            })
            .unwrap_or_else(|| {
                tracing::info!(
                    downstream_id,
                    "Not propagating PushSolution: downstream state is gone"
                );
                None
            });

        let Some(block) = block else {
            return;
        };

        let request = JdRequest::PushSolution { block };

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
        set_custom_mining_job: SetCustomMiningJobOwned,
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
            let declared_coinbase_tx = match declared_custom_job.get_coinbase_tx(None) {
                Ok(tx) => tx,
                Err(_) => {
                    return SetCustomMiningJobResult::Error(
                        ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_COINBASE_TX,
                    );
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
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::stratum_core::bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Txid, Witness, absolute::LockTime,
        transaction::Version as TxVersion,
    };

    const EXTRANONCE_LEN: usize = 8;
    // A target one hash in 2^16 meets: a nonce mined for one merkle root misses it for another,
    // and mining one takes a few tens of thousands of hashes.
    const NBITS: u32 = 0x1f00_ffff;
    const BLOCK_VERSION: u32 = 0x2000_0000;

    /// A job at the shared chain tip whose coinbase pays `payout_tag`, declared with an
    /// `EXTRANONCE_LEN`-byte extranonce hole in its scriptSig and two other transactions, so
    /// the merkle tree has an odd leaf count and a branch of two nodes.
    fn accepted_job(payout_tag: u8) -> SolvableJob {
        let coinbase = Transaction {
            version: TxVersion::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(
                    [vec![0x03, 1, 0, 0], vec![0; EXTRANONCE_LEN]].concat(),
                ),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[[0u8; 32]]),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_0000_0000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x6a, 0x01, payout_tag]),
            }],
        };
        let txdata: Vec<Transaction> = (1..=2)
            .map(|n| Transaction {
                version: TxVersion::TWO,
                lock_time: LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([n; 32]), 0),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                }],
                output: vec![TxOut {
                    value: Amount::from_sat(1),
                    script_pubkey: ScriptBuf::new(),
                }],
            })
            .collect();
        let bytes = bitcoin::consensus::serialize(&coinbase);
        // version(4) + marker and flag(2) + input count(1) + outpoint(36) + scriptSig length(1)
        // + height push(4)
        let extranonce_offset = 4 + 2 + 1 + 36 + 1 + 4;
        let job = DeclaredCustomJob {
            declare_mining_job: DeclareMiningJobOwned {
                request_id: 0,
                mining_job_token: Vec::new().try_into().unwrap(),
                version: BLOCK_VERSION,
                coinbase_tx_prefix: bytes[..extranonce_offset].to_vec().try_into().unwrap(),
                coinbase_tx_suffix: bytes[extranonce_offset + EXTRANONCE_LEN..]
                    .to_vec()
                    .try_into()
                    .unwrap(),
                wtxid_list: Vec::new().try_into().unwrap(),
                excess_data: Vec::new().try_into().unwrap(),
            },
            validation_context: ValidationContext {
                prev_hash: BlockHash::all_zeros(),
                nbits: CompactTarget::from_consensus(NBITS),
            },
            merkle_path: coinbase_merkle_path(&txdata),
            validated: true,
        };
        SolvableJob { job, txdata }
    }

    /// A `PushSolution` mined on `job`: the header fields every job at the tip shares, with a
    /// nonce that meets the target for this job's merkle root.
    fn solution_on(job: &DeclaredCustomJob) -> PushSolutionOwned {
        let extranonce = [7u8; EXTRANONCE_LEN];
        let coinbase = job.get_coinbase_tx(Some(&extranonce)).unwrap();
        let mut header = Header {
            version: Version::from_consensus(BLOCK_VERSION as i32),
            prev_blockhash: job.get_prev_hash(),
            merkle_root: TxMerkleNode::from_byte_array(merkle_root_from_path_(
                coinbase.compute_txid().to_byte_array(),
                &job.merkle_path,
            )),
            time: 1,
            bits: CompactTarget::from_consensus(NBITS),
            nonce: 0,
        };
        while header.validate_pow(header.target()).is_err() {
            header.nonce += 1;
        }
        PushSolutionOwned {
            extranonce: extranonce.to_vec().try_into().unwrap(),
            prev_hash: job.get_prev_hash().to_byte_array().into(),
            nonce: header.nonce,
            ntime: header.time,
            nbits: NBITS,
            version: BLOCK_VERSION,
        }
    }

    #[test]
    fn solved_block_tells_jobs_at_one_tip_apart() {
        let older = accepted_job(1);
        let newer = accepted_job(2);
        let solution = solution_on(&older.job);

        let block = older
            .solved_block(0, &solution)
            .expect("the job the solution was found on rebuilds into a valid block");
        assert_eq!(
            block.txdata[0].output[0].script_pubkey.as_bytes(),
            [0x6a, 0x01, 1]
        );
        assert!(block.header.validate_pow(block.header.target()).is_ok());
        assert_eq!(
            block.compute_merkle_root(),
            Some(block.header.merkle_root),
            "the root folded from the stored branch is the root of the rebuilt block"
        );

        assert!(
            newer.solved_block(0, &solution).is_none(),
            "a job that passes every PushSolution field check but was not solved"
        );
    }

    #[test]
    fn a_job_declared_again_under_its_token_replaces_the_one_before() {
        let mut state = DownstreamState::default();
        let mut waiting = accepted_job(1).job;
        waiting.validated = false;
        state.insert_declared_custom_job(1, 7, waiting);
        state.insert_declared_custom_job(2, 7, accepted_job(2).job);
        assert!(!state.declared_custom_jobs.contains_key(&1));
        assert!(state.take_declared_custom_job(7).is_some());
        assert!(state.declared_custom_jobs.is_empty());
    }
}
