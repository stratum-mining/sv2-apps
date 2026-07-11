//! v32.x-specific JDP handlers.

use super::{BitcoinCoreSv2JDP, error::BitcoinCoreSv2JDPError};
use crate::runtime_api::job_declaration_protocol::io::{JdResponse, ValidationContext};
use stratum_core::{
    bitcoin::{
        Block, Transaction, TxMerkleNode, Wtxid,
        block::{Header, Version},
        consensus::{deserialize, serialize},
        hashes::Hash,
    },
    job_declaration_sv2::{
        ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
        ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
    },
};
use std::collections::HashMap;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

const MAX_SUBMIT_BLOCK_ATTEMPTS: usize = 3;
const SUBMIT_BLOCK_RETRY_BACKOFF_MS: u64 = 15;

impl BitcoinCoreSv2JDP {
    /// Validates a declared mining job by checking transaction availability and block structure.
    ///
    /// Fetches declared transactions from Bitcoin Core via `getTransactionsByWitnessID`,
    /// deserialises the response, and verifies wtxid consistency.
    ///
    /// Missing transactions (where Core returns empty data) trigger `MissingTransactions`.
    /// On retry after `ProvideMissingTransactionsSuccess`, the provided transactions are
    /// combined with the witness-id response to assemble the full transaction list.
    ///
    /// After transaction assembly, builds a candidate block and uses Bitcoin Core's
    /// `checkBlock` for structural validation.
    pub(crate) async fn handle_declare_mining_job(
        &self,
        version: Version,
        coinbase_tx: Transaction,
        wtxid_list: Vec<Wtxid>,
        missing_txs: Vec<Transaction>,
        response_tx: oneshot::Sender<JdResponse>,
    ) {
        info!(
            "Validating DeclareMiningJob - version: {:?}, coinbase inputs: {}, outputs: {}, locktime: {}",
            version,
            coinbase_tx.input.len(),
            coinbase_tx.output.len(),
            coinbase_tx.lock_time.to_consensus_u32()
        );
        debug!(
            "Declared coinbase scriptSig: {:?}",
            coinbase_tx.input[0].script_sig
        );

        let (initial_validation_context, initial_ntime) = {
            let chain_tip_state = self.chain_tip_state.borrow();
            let validation_context = ValidationContext {
                prev_hash: chain_tip_state
                    .get_current_prev_hash()
                    .expect("current_prev_hash must be set"),
                nbits: chain_tip_state
                    .get_next_nbits()
                    .expect("next_nbits must be set"),
            };
            let ntime = chain_tip_state
                .get_current_ntime()
                .expect("current_ntime must be set");
            (validation_context, ntime)
        };

        info!(
            "Using prevhash: {:?}, nbits: {:?}, ntime: {} from chain-tip state",
            initial_validation_context.prev_hash,
            initial_validation_context.nbits,
            initial_ntime,
        );

        debug!(
            wtxids_requested = wtxid_list.len(),
            "Fetching declared transactions from Bitcoin Core via getTransactionsByWitnessID"
        );

        let mut get_transactions_by_witness_id_request =
            self.mining_ipc_client.get_transactions_by_witness_i_d_request();

        match get_transactions_by_witness_id_request.get().get_context() {
            Ok(mut context) => context.set_thread(self.thread_ipc_client.clone()),
            Err(e) => {
                error!(
                    "Failed to set getTransactionsByWitnessID request thread context: {e}"
                );
                let _ = response_tx.send(JdResponse::Error {
                    error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                    validation_context: initial_validation_context,
                });
                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                self.cancellation_token.cancel();
                return;
            }
        }

        {
            let mut requested_wtxids = get_transactions_by_witness_id_request
                .get()
                .init_wtxids(wtxid_list.len() as u32);

            for (position, wtxid) in wtxid_list.iter().enumerate() {
                requested_wtxids.set(position as u32, &wtxid.to_byte_array());
            }
        }

        let get_transactions_by_witness_id_response =
            match get_transactions_by_witness_id_request.send().promise.await {
                Ok(response) => response,
                Err(e) => {
                    error!("Failed to send getTransactionsByWitnessID request: {e}");
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

        let get_transactions_by_witness_id_result = match get_transactions_by_witness_id_response
            .get()
        {
            Ok(result) => result,
            Err(e) => {
                error!("Failed to read getTransactionsByWitnessID response: {e}");
                let _ = response_tx.send(JdResponse::Error {
                    error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                    validation_context: initial_validation_context,
                });
                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                self.cancellation_token.cancel();
                return;
            }
        };

        let retrieved_transaction_bytes = match get_transactions_by_witness_id_result.get_result() {
            Ok(result) => result,
            Err(e) => {
                error!("Failed to read getTransactionsByWitnessID result list: {e}");
                let _ = response_tx.send(JdResponse::Error {
                    error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                    validation_context: initial_validation_context,
                });
                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                self.cancellation_token.cancel();
                return;
            }
        };

        if retrieved_transaction_bytes.len() != wtxid_list.len() as u32 {
            error!(
                expected = wtxid_list.len(),
                received = retrieved_transaction_bytes.len(),
                "Unexpected getTransactionsByWitnessID response length"
            );
            let _ = response_tx.send(JdResponse::Error {
                error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                validation_context: initial_validation_context,
            });
            warn!("Terminating Sv2 Bitcoin Core IPC Connection");
            self.cancellation_token.cancel();
            return;
        }

        // Build a map of wtxid -> Transaction from the witness-id response
        // and from ProvideMissingTransactionsSuccess.
        let mut tx_by_wtxid = HashMap::with_capacity(wtxid_list.len() + missing_txs.len());

        for tx in missing_txs {
            tx_by_wtxid.insert(tx.compute_wtxid(), tx);
        }

        let mut missing_wtxids = Vec::new();

        for (position, declared_wtxid) in wtxid_list.iter().enumerate() {
            let tx_bytes = match retrieved_transaction_bytes.get(position as u32) {
                Ok(tx_bytes) => tx_bytes,
                Err(e) => {
                    error!(
                        position,
                        "Failed to read transaction bytes from getTransactionsByWitnessID result: {e}"
                    );
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            if tx_bytes.is_empty() {
                if !tx_by_wtxid.contains_key(declared_wtxid) {
                    debug!(
                        wtxid = ?declared_wtxid,
                        position,
                        "Declared transaction missing from Bitcoin Core mempool"
                    );
                    missing_wtxids.push(*declared_wtxid);
                }
                continue;
            }

            let transaction: Transaction = match deserialize(tx_bytes) {
                Ok(transaction) => transaction,
                Err(e) => {
                    error!(
                        wtxid = ?declared_wtxid,
                        position,
                        "Failed to deserialize transaction bytes from getTransactionsByWitnessID: {e}"
                    );
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            if transaction.compute_wtxid() != *declared_wtxid {
                error!(
                    declared_wtxid = ?declared_wtxid,
                    fetched_wtxid = ?transaction.compute_wtxid(),
                    position,
                    "getTransactionsByWitnessID returned transaction with mismatched wtxid"
                );
                let _ = response_tx.send(JdResponse::Error {
                    error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                    validation_context: initial_validation_context,
                });
                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                self.cancellation_token.cancel();
                return;
            }

            tx_by_wtxid.entry(*declared_wtxid).or_insert(transaction);
        }

        if !missing_wtxids.is_empty() {
            let _ = response_tx.send(JdResponse::MissingTransactions {
                missing_wtxids,
                validation_context: initial_validation_context,
            });
            return;
        }

        // Build ordered txdata from the declared wtxid_list.
        let txdata: Vec<Transaction> = wtxid_list
            .iter()
            .map(|wtxid| {
                tx_by_wtxid
                    .remove(wtxid)
                    .expect("all declared wtxids must be resolved")
            })
            .collect();

        let txdata_for_response = txdata.clone();

        let mut check_block_reason_for_stale: Option<String> = None;

        let valid_job = {
            let mut all_transactions = Vec::with_capacity(1 + txdata.len());
            all_transactions.push(coinbase_tx.clone());
            all_transactions.extend(txdata);

            let num_transactions = all_transactions.len();

            let block_time = initial_ntime;

            let header = Header {
                version,
                prev_blockhash: initial_validation_context.prev_hash,
                merkle_root: TxMerkleNode::all_zeros(),
                time: block_time,
                bits: initial_validation_context.nbits,
                nonce: 0,
            };

            let block = Block {
                header,
                txdata: all_transactions,
            };

            let block_bytes: Vec<u8> = serialize(&block);

            debug!(
                "Assembled block for checkBlock: {} bytes, {} transactions",
                block_bytes.len(),
                num_transactions
            );

            let mut check_block_request = self.mining_ipc_client.check_block_request();

            match check_block_request.get().get_context() {
                Ok(mut context) => context.set_thread(self.thread_ipc_client.clone()),
                Err(e) => {
                    error!("Failed to set check block request thread context: {e}");
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            }

            check_block_request.get().set_block(&block_bytes);

            let mut options = match check_block_request.get().get_options() {
                Ok(options) => options,
                Err(e) => {
                    error!("Failed to get check block options: {e}");
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };
            options.set_check_merkle_root(false);
            options.set_check_pow(false);

            let check_block_response = match check_block_request.send().promise.await {
                Ok(response) => response,
                Err(e) => {
                    error!("Failed to send check block request: {e}");
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };
            let check_block_result = match check_block_response.get() {
                Ok(result) => result,
                Err(e) => {
                    error!("Failed to get check block result: {e}");
                    let _ = response_tx.send(JdResponse::Error {
                        error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                        validation_context: initial_validation_context,
                    });
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let result = check_block_result.get_result();
            let check_block_reason = check_block_result.get_reason();
            let check_block_debug = check_block_result.get_debug();

            debug!("checkBlock returned: {}", result);
            if !result {
                error!(
                    reason = ?check_block_reason,
                    debug = ?check_block_debug,
                    "Bitcoin Core rejected the block via checkBlock"
                );
                check_block_reason_for_stale =
                    check_block_reason.ok().and_then(|r| r.to_string().ok());
                debug!(
                    "Block details - version: {:?}, prev_blockhash: {:?}, bits: {:?}, num_txs: {}",
                    version,
                    initial_validation_context.prev_hash,
                    initial_validation_context.nbits,
                    num_transactions
                );
                debug!(
                    "Coinbase tx inputs: {}, outputs: {}",
                    coinbase_tx.input.len(),
                    coinbase_tx.output.len()
                );
                debug!(
                    "Block header time: {}, merkle_root: {:?}",
                    header.time, header.merkle_root
                );
            }
            result
        };

        if !valid_job {
            // On checkBlock failure, force-refresh template before classifying the error.
            // The chain-tip monitor updates chain_tip_state asynchronously, so we need to avoid
            // races where validation can run on context A while chain tip has already moved to
            // context B. Refreshing here narrows this TOCTOU window and lets us correctly emit
            // `stale-chain-tip` instead of generic `invalid-job` when context drift occurred.
            if let Err(e) = self.force_update_chain_tip_state().await {
                debug!(
                    error = ?e,
                    "Failed to force-refresh template after checkBlock failure; continuing with current validation context"
                );
            }
        }

        let latest_validation_context = {
            let chain_tip_state = self.chain_tip_state.borrow();
            ValidationContext {
                prev_hash: chain_tip_state
                    .get_current_prev_hash()
                    .expect("current_prev_hash must be set"),
                nbits: chain_tip_state
                    .get_next_nbits()
                    .expect("next_nbits must be set"),
            }
        };

        let response = if valid_job {
            JdResponse::Success {
                prev_hash: initial_validation_context.prev_hash,
                nbits: initial_validation_context.nbits,
                txdata: txdata_for_response,
            }
        } else {
            let context_drifted =
                initial_validation_context.prev_hash != latest_validation_context.prev_hash;

            let stale_at_arrival = matches!(
                check_block_reason_for_stale.as_deref(),
                Some("bad-cb-height")
            );

            let error_code = if context_drifted || stale_at_arrival {
                debug!(
                    initial_prev_hash = ?initial_validation_context.prev_hash,
                    latest_prev_hash = ?latest_validation_context.prev_hash,
                    "Detected stale chain tip during DeclareMiningJob validation; classifying error as stale-chain-tip"
                );
                ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP
            } else {
                ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB
            };

            JdResponse::Error {
                error_code,
                validation_context: latest_validation_context,
            }
        };

        let _ = response_tx.send(response);
    }

    /// Submits a solved block to Bitcoin Core via `submitBlock`.
    pub(crate) async fn handle_push_solution(&self, block: Block) {
        let block_bytes: Vec<u8> = serialize(&block);
        debug!(
            block_bytes_len = block_bytes.len(),
            tx_count = block.txdata.len(),
            "Submitting solved block via submitBlock"
        );

        // a dedicated thread is used to submit blocks to Bitcoin Core
        // therefore retries should be extremely rare
        for attempt in 1..=MAX_SUBMIT_BLOCK_ATTEMPTS {
            let mut submit_block_request = self.mining_ipc_client.submit_block_request();

            match submit_block_request.get().get_context() {
                Ok(mut context) => context.set_thread(self.submit_block_thread_ipc_client.clone()),
                Err(e) => {
                    error!("Failed to set submitBlock request thread context: {e}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            }

            submit_block_request.get().set_block(&block_bytes);

            let submit_block_response = match submit_block_request.send().promise.await {
                Ok(response) => response,
                Err(e) => {
                    let err: BitcoinCoreSv2JDPError = e.into();
                    if err.is_thread_busy() && attempt < MAX_SUBMIT_BLOCK_ATTEMPTS {
                        warn!(
                            attempt,
                            max_attempts = MAX_SUBMIT_BLOCK_ATTEMPTS,
                            "Transient IPC contention during submitBlock (thread busy); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            SUBMIT_BLOCK_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }

                    error!("Failed to send submitBlock request: {err:?}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let submit_block_result = match submit_block_response.get() {
                Ok(result) => result,
                Err(e) => {
                    let err: BitcoinCoreSv2JDPError = e.into();
                    if err.is_thread_busy() && attempt < MAX_SUBMIT_BLOCK_ATTEMPTS {
                        warn!(
                            attempt,
                            max_attempts = MAX_SUBMIT_BLOCK_ATTEMPTS,
                            "Transient IPC contention while reading submitBlock response (thread busy); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            SUBMIT_BLOCK_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }

                    error!("Failed to get submitBlock result: {err:?}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let accepted = submit_block_result.get_result();
            let reason = submit_block_result.get_reason();
            let debug_msg = submit_block_result.get_debug();

            if accepted {
                info!(
                    reason = ?reason,
                    debug = ?debug_msg,
                    "Bitcoin Core accepted block via submitBlock"
                );
            } else {
                warn!(
                    reason = ?reason,
                    debug = ?debug_msg,
                    "Bitcoin Core rejected block via submitBlock"
                );
            }

            return;
        }
    }
}
