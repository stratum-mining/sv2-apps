//! Module for validating and propagating solutions for Custom Jobs using Bitcoin Core over IPC.

use crate::{
    error::JDSErrorKind,
    job_declarator::{
        job_validation::{DeclareMiningJobResult, JobValidationEngine, SetCustomMiningJobResult},
        ALLOCATED_TOKEN_TIMEOUT_SECS, JANITOR_INTERVAL_SECS,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use stratum_apps::{
    bitcoin_core_sv2::common::{
        job_declaration_protocol::{
            self,
            io::{JdRequest, JdResponse},
            CancellationToken,
        },
        BitcoinCoreVersion,
    },
    stratum_core::{
        bitcoin::{
            self,
            block::{Header, Version},
            consensus::{Decodable, Encodable},
            hashes::Hash,
            Block, BlockHash, CompactTarget, Transaction, TxMerkleNode, Wtxid,
        },
        job_declaration_sv2::{
            DeclareMiningJob, ProvideMissingTransactionsSuccess, PushSolution,
            ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
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

// Accept version rolling in PushSolution by ignoring these bits when comparing with the
// declared job version.
const PUSH_SOLUTION_VERSION_ROLLING_MASK: u32 = 0x1fff_ffe0;

/// Snapshot of a previously declared mining job, stored after a `DeclareMiningJob` is
/// successfully validated (or while waiting for missing transactions).
///
/// Used by [`BitcoinCoreIPCEngine::handle_set_custom_mining_job`] to cross-check that a
/// subsequent `SetCustomMiningJob` matches the original declaration.
#[derive(Clone)]
struct DeclaredCustomJob {
    declare_mining_job: DeclareMiningJob<'static>,
    /// Chain tip the declaration was evaluated against; used for stale-tip classification
    /// (see https://github.com/stratum-mining/sv2-apps/issues/597).
    prev_hash: BlockHash,
    /// Populated once the job passes Bitcoin Core validation ([`JdResponse::Success`]);
    /// `None` while waiting for missing transactions.
    validated: Option<ValidatedJobData>,
}

/// Template parameters and transaction data of a fully validated declared job.
#[derive(Clone)]
struct ValidatedJobData {
    nbits: CompactTarget,
    /// Full non-coinbase transaction list in declaration order, used for merkle-path
    /// validation and solved block reconstruction.
    txdata: Vec<Transaction>,
}

/// Latest `DeclaredCustomJob` accepted via `SetCustomMiningJob` for a downstream.
type ActiveCustomJob = DeclaredCustomJob;

#[derive(Clone, Copy)]
struct AllocatedTokenEntry {
    request_id: RequestId,
    inserted_at: Instant,
}

#[derive(Default)]
struct DownstreamState {
    declared_custom_jobs: HashMap<RequestId, DeclaredCustomJob>,
    allocated_token_entries: HashMap<JdToken, AllocatedTokenEntry>,
    active_custom_job: Option<ActiveCustomJob>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl DeclaredCustomJob {
    /// Returns the block version from the original `DeclareMiningJob`.
    fn get_version(&self) -> u32 {
        self.declare_mining_job.version
    }

    /// Reconstructs the declared coinbase transaction by concatenating prefix, extranonce, and
    /// suffix.
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

    /// Computes the coinbase merkle branch in the txid merkle tree.
    ///
    /// Returns the sibling hashes at each level from leaf to root, needed to
    /// reconstruct the block header's merkle root from the coinbase position (index 0).
    ///
    /// Requires the job to have been validated via `JdResponse::Success`.
    /// The coinbase txid is derived from the declared coinbase prefix/suffix.
    ///
    /// Used to compare with a `SetCustomMiningJob.merkle_path`.
    ///
    /// Internally, errors may come from missing txdata
    /// so error_code = "declared-job-not-yet-validated"
    /// therefore () error type is sufficient.
    fn get_merkle_path(&self) -> Result<Vec<TxMerkleNode>, ()> {
        let txdata = &self.validated.as_ref().ok_or(())?.txdata;

        let coinbase_tx = self
            .get_coinbase_tx(None)
            .expect("coinbase tx already validated");
        let coinbase_txid: TxMerkleNode = coinbase_tx.compute_txid().into();

        let mut hashes: Vec<TxMerkleNode> = Vec::with_capacity(1 + txdata.len());
        hashes.push(coinbase_txid);
        for tx in txdata {
            hashes.push(tx.compute_txid().into());
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
    /// `version` selects the Bitcoin Core IPC schema family (v30.x, v31.x, or v32.x).
    ///
    /// Blocks until the backend is ready to process requests (mempool mirror bootstrapped for
    /// v30.x/v31.x, IBD finished for v32.x).
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

        // Wait for BitcoinCoreSv2JDP to become ready (mempool bootstrap or IBD wait,
        // depending on the backend), mirroring the pool's Template Provider startup
        // behavior during IBD.
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
                                "Bitcoin Core JDP backend did not become ready".to_string(),
                            ));
                        }
                    }
                }
                _ = cancellation_token.cancelled() => {
                    tracing::info!("BitcoinCoreIPCEngine stopped before the JDP backend became ready");
                    if let Err(e) = jdp_thread_handle.join() {
                        tracing::warn!("BitcoinCoreSv2JDP thread join failed during startup cancellation: {e:?}");
                    }
                    return Err(JDSErrorKind::BitcoinCoreIPC(
                        "Bitcoin Core JDP backend did not become ready".to_string(),
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
                            let expired_tokens: Vec<JdToken> = state
                                .allocated_token_entries
                                .iter()
                                .filter_map(|(token, entry)| {
                                    if now.saturating_duration_since(entry.inserted_at)
                                        > token_timeout
                                    {
                                        Some(*token)
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            for token in expired_tokens {
                                if let Some(entry) = state.allocated_token_entries.remove(&token) {
                                    state.declared_custom_jobs.remove(&entry.request_id);
                                    tracing::debug!(
                                        downstream_id,
                                        token,
                                        request_id = entry.request_id,
                                        "Removed expired declared custom job state"
                                    );
                                }
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
                prev_hash: BlockHash::all_zeros(), // irrelevant for coinbase tx validation
                validated: None,                   // irrelevant for coinbase tx validation
            };

            match temp_job.get_coinbase_tx(None) {
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

        // A declared job must not list the same transaction twice.
        let declared_wtxids: HashSet<Wtxid> = wtxid_list.iter().copied().collect();
        if declared_wtxids.len() != wtxid_list.len() {
            tracing::debug!(
                downstream_id,
                request_id = declare_mining_job.request_id,
                "DeclareMiningJob wtxid list contains duplicates"
            );
            return DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB);
        }

        // Parse missing transactions from ProvideMissingTransactionsSuccess, ignoring any
        // transaction that is not part of the declared job. Anything the validator still
        // considers missing afterwards is reported through another
        // ProvideMissingTransactions round.
        let missing_txs: Vec<Transaction> = if let Some(ref pmts) =
            provide_missing_transactions_success
        {
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
                    .filter(|tx: &Transaction| {
                        let declared = declared_wtxids.contains(&tx.compute_wtxid());
                        if !declared {
                            tracing::warn!(
                                downstream_id,
                                request_id = declare_mining_job.request_id,
                                "Ignoring provided missing transaction that is not part of the declared job"
                            );
                        }
                        declared
                    })
                    .collect()
        } else {
            Vec::new()
        };

        let previous_pending_prev_hash =
            provide_missing_transactions_success.as_ref().and_then(|_| {
                self.downstream_states
                    .with(&downstream_id, |state| {
                        state
                            .declared_custom_jobs
                            .get(&declare_mining_job.request_id)
                            .map(|job| job.prev_hash)
                    })
                    .flatten()
            });

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
                min_ntime: _,
                txdata,
            } => {
                let declared_custom_job = DeclaredCustomJob {
                    declare_mining_job: declare_mining_job_static,
                    prev_hash,
                    validated: Some(ValidatedJobData { nbits, txdata }),
                };
                self.downstream_states
                    .with_mut_or_default(downstream_id, |state| {
                        state
                            .declared_custom_jobs
                            .insert(declare_mining_job.request_id, declared_custom_job);
                        state.allocated_token_entries.insert(
                            allocated_token,
                            AllocatedTokenEntry {
                                request_id: declare_mining_job.request_id,
                                inserted_at: Instant::now(),
                            },
                        );
                    });
                DeclareMiningJobResult::Success
            }
            JdResponse::Error {
                error_code,
                prev_hash,
            } => {
                self.downstream_states.with_mut(&downstream_id, |state| {
                    state
                        .declared_custom_jobs
                        .remove(&declare_mining_job.request_id);
                    state.allocated_token_entries.remove(&allocated_token);
                });

                let tip_drifted = match (previous_pending_prev_hash, prev_hash) {
                    (Some(previous_prev_hash), Some(current_prev_hash)) => {
                        previous_prev_hash != current_prev_hash
                    }
                    _ => false,
                };

                if tip_drifted {
                    DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP)
                } else {
                    DeclareMiningJobResult::Error(error_code)
                }
            }
            JdResponse::MissingTransactions {
                missing_wtxids,
                prev_hash,
            } => {
                let tip_drifted = previous_pending_prev_hash
                    .map(|previous_prev_hash| previous_prev_hash != prev_hash)
                    .unwrap_or(false);

                // If this is a retry after ProvideMissingTransactionsSuccess and context drifted,
                // classify as stale-chain-tip instead of asking for yet another missing-txs round.
                if provide_missing_transactions_success.is_some() && tip_drifted {
                    self.downstream_states.with_mut(&downstream_id, |state| {
                        state
                            .declared_custom_jobs
                            .remove(&declare_mining_job.request_id);
                        state.allocated_token_entries.remove(&allocated_token);
                    });

                    DeclareMiningJobResult::Error(ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP)
                } else {
                    let declared_custom_job = DeclaredCustomJob {
                        declare_mining_job: declare_mining_job_static,
                        prev_hash,
                        validated: None, // this is only populated on JdResponse::Success
                    };
                    self.downstream_states
                        .with_mut_or_default(downstream_id, |state| {
                            state
                                .declared_custom_jobs
                                .insert(declare_mining_job.request_id, declared_custom_job);
                            state.allocated_token_entries.insert(
                                allocated_token,
                                AllocatedTokenEntry {
                                    request_id: declare_mining_job.request_id,
                                    inserted_at: Instant::now(),
                                },
                            );
                        });

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
        let prev_hash = BlockHash::from_byte_array(push_solution.prev_hash.to_array());

        // Validate PushSolution fields and consume the matching active custom job atomically.
        // prev_hash and nbits must match exactly; version is matched on non-rollable bits only.
        let active_job = self
            .downstream_states
            .with_mut(&downstream_id, |state| {
                let (declared_prev_hash, declared_nbits, declared_version) =
                    match state.active_custom_job.as_ref() {
                        Some(active_job) => match active_job.validated.as_ref() {
                            Some(validated) => (
                                active_job.prev_hash,
                                validated.nbits.to_consensus(),
                                active_job.get_version(),
                            ),
                            None => {
                                tracing::error!(
                                    "Active custom job on downstream {} was never validated",
                                    downstream_id,
                                );
                                return None;
                            }
                        },
                        None => {
                            tracing::error!(
                                "No active custom job found for PushSolution on downstream {}",
                                downstream_id,
                            );
                            return None;
                        }
                    };

                let declared_fixed_version_bits =
                    declared_version & !PUSH_SOLUTION_VERSION_ROLLING_MASK;
                let solved_fixed_version_bits =
                    push_solution.version & !PUSH_SOLUTION_VERSION_ROLLING_MASK;

                if prev_hash != declared_prev_hash
                    || push_solution.nbits != declared_nbits
                    || solved_fixed_version_bits != declared_fixed_version_bits
                {
                    tracing::error!(
                        "Ignoring PushSolution that does not match latest declared custom job on downstream {}: expected prev_hash={:?}, nbits={}, version={}, got prev_hash={:?}, nbits={}, version={} (mask=0x{:08x}, expected_fixed_version_bits=0x{:08x}, got_fixed_version_bits=0x{:08x})",
                        downstream_id,
                        declared_prev_hash,
                        declared_nbits,
                        declared_version,
                        prev_hash,
                        push_solution.nbits,
                        push_solution.version,
                        PUSH_SOLUTION_VERSION_ROLLING_MASK,
                        declared_fixed_version_bits,
                        solved_fixed_version_bits
                    );
                    return None;
                }

                state.active_custom_job.take()
            })
            .flatten();

        let Some(active_job) = active_job else {
            return;
        };

        let declared_prev_hash = active_job.prev_hash;

        let mut txdata = match active_job.validated {
            Some(ref validated) => validated.txdata.clone(),
            None => {
                tracing::error!("Active custom job is missing transaction data");
                return;
            }
        };

        let coinbase_tx = match active_job.get_coinbase_tx(Some(push_solution.extranonce.as_ref()))
        {
            Ok(coinbase_tx) => coinbase_tx,
            Err(_) => {
                tracing::error!("Failed to reconstruct solved coinbase transaction");
                return;
            }
        };

        txdata.insert(0, coinbase_tx);

        let mut block = Block {
            header: Header {
                version: Version::from_consensus(push_solution.version as i32),
                prev_blockhash: declared_prev_hash,
                merkle_root: TxMerkleNode::all_zeros(),
                time: push_solution.ntime,
                bits: CompactTarget::from_consensus(push_solution.nbits),
                nonce: push_solution.nonce,
            },
            txdata,
        };

        let Some(merkle_root) = block.compute_merkle_root() else {
            tracing::error!("Failed to compute merkle root for PushSolution block");
            return;
        };
        block.header.merkle_root = merkle_root;

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
        set_custom_mining_job: SetCustomMiningJob<'_>,
        allocated_token: JdToken, // Note: This is the corresponding DeclareMiningJob token
    ) -> SetCustomMiningJobResult {
        // Look up request_id using the allocated token
        let request_id = match self
            .downstream_states
            .with(&downstream_id, |state| {
                state
                    .allocated_token_entries
                    .get(&allocated_token)
                    .map(|entry| entry.request_id)
            })
            .flatten()
        {
            Some(request_id) => request_id,
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

        let declared_custom_job = match self
            .downstream_states
            .with_mut(&downstream_id, |state| {
                // Clean up immediately - the job is being consumed regardless of validation result.
                state.allocated_token_entries.remove(&allocated_token);
                state.declared_custom_jobs.remove(&request_id)
            })
            .flatten()
        {
            Some(declared_custom_job) => declared_custom_job,
            None => {
                tracing::debug!(
                    downstream_id,
                    allocated_token,
                    request_id,
                    "DeclaredCustomJob associated with allocated token and request id not found"
                );
                return SetCustomMiningJobResult::Error(
                    ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_MINING_JOB_TOKEN,
                );
            }
        };

        // Job may be pending retry after missing txs and not fully validated yet.
        let Some(validated) = declared_custom_job.validated.as_ref() else {
            tracing::error!("Job not yet validated");
            return SetCustomMiningJobResult::Error(
                ERROR_CODE_SET_CUSTOM_MINING_JOB_JOB_NOT_YET_VALIDATED,
            );
        };

        // Get declared values from stored job
        let declared_prev_hash = declared_custom_job.prev_hash;
        let declared_nbits = validated.nbits.to_consensus();
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

        self.downstream_states
            .with_mut_or_default(downstream_id, |state| {
                if state
                    .active_custom_job
                    .replace(declared_custom_job)
                    .is_some()
                {
                    tracing::debug!(
                        "Replaced previous active custom job for downstream {} with newer SetCustomMiningJob",
                        downstream_id,
                    );
                }
            });

        SetCustomMiningJobResult::Success
    }
}
