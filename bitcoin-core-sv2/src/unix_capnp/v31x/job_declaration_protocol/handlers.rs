//! Handlers for Bitcoin Core v31.x Sv2 Job Declaration Protocol via capnp over UNIX socket.

use crate::{
    runtime_api::job_declaration_protocol::io::{JdResponse, ValidationContext},
    unix_capnp::v31x::job_declaration_protocol::BitcoinCoreSv2JDP,
};
use stratum_core::{
    bitcoin::{
        Block, Transaction, TxMerkleNode, Wtxid,
        block::{Header, Version},
        consensus::serialize,
        hashes::Hash,
    },
    job_declaration_sv2::{
        ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
        ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP, PushSolution,
    },
};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

impl BitcoinCoreSv2JDP {
    /// Validates a declared mining job by checking transaction availability and block structure.
    ///
    /// Adds missing transactions to the mempool mirror, verifies all transactions are available,
    /// assembles a test block, sets IPC thread context, and uses Bitcoin Core's `checkBlock` to
    /// validate the block structure. Returns success with current template parameters or an error
    /// if validation fails.
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

        let (initial_validation_context, ntime, txdata) = {
            let mut mempool_mirror = self.mempool_mirror.borrow_mut();

            // Add the missing transactions to the mempool mirror
            mempool_mirror.add_transactions(missing_txs);

            let prev_hash = mempool_mirror
                .get_current_prev_hash()
                .expect("current_prev_hash must be set");
            let nbits = mempool_mirror
                .get_current_nbits()
                .expect("current_nbits must be set");
            let ntime = mempool_mirror
                .get_current_ntime()
                .expect("current_ntime must be set");

            let initial_validation_context = ValidationContext { prev_hash, nbits };

            // Now verify that all wtxids from the declared job are available
            let missing_wtxids = mempool_mirror.verify(&wtxid_list);
            if !missing_wtxids.is_empty() {
                // deliberately ignore potential errors
                // we don't care if the receiver dropped the channel
                let _ = response_tx.send(JdResponse::MissingTransactions {
                    missing_wtxids,
                    validation_context: initial_validation_context,
                });
                return;
            }

            let txdata = mempool_mirror.get_txdata(&wtxid_list);

            info!(
                "Using prevhash: {:?}, nbits: {:?}, ntime: {} from mempool mirror",
                initial_validation_context.prev_hash, initial_validation_context.nbits, ntime,
            );

            (initial_validation_context, ntime, txdata)
        }; // mempool_mirror dropped here, we don't want to hold it across await points

        let txdata_for_response = txdata.clone();

        let mut check_block_reason_for_stale: Option<String> = None;

        let valid_job = {
            let mut all_transactions = Vec::with_capacity(1 + txdata.len());
            all_transactions.push(coinbase_tx.clone());
            all_transactions.extend(txdata);

            let num_transactions = all_transactions.len();

            // Use the template ntime as the block timestamp
            // This ensures we meet Bitcoin Core's timestamp validation rules
            let block_time = ntime;

            let header = Header {
                version,
                prev_blockhash: initial_validation_context.prev_hash,
                merkle_root: TxMerkleNode::all_zeros(), // doesn't matter
                time: block_time,
                bits: initial_validation_context.nbits,
                nonce: 0, // doesn't matter
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
                    // send error response to the client
                    // deliberately ignore potential send errors
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
                    // send error response to the client
                    // deliberately ignore potential send errors
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
                    // send error response to the client
                    // deliberately ignore potential send errors
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
                    // send error response to the client
                    // deliberately ignore potential send errors
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
            // On checkBlock failure, update local prev_hash before classifying the error.
            // The template monitor updates mempool_mirror asynchronously, so we need to avoid races
            // where validation can run on context A while chain tip has already moved to context B.
            // Updating prev_hash here narrows this TOCTOU window and lets us correctly emit
            // `stale-chain-tip` instead of generic `invalid-job` when context drift occurred.
            //
            // Additionally, a `bad-cb-height` rejection from checkBlock signals a job that was
            // already stale at arrival (coinbase built from an old tip) and is mapped to
            // stale-chain-tip even without prev_hash drift.
            if let Err(e) = self.force_update_mempool_mirror_prev_hash().await {
                debug!(
                    error = ?e,
                    "Failed to update prev_hash after checkBlock failure; continuing with current validation context"
                );
            }
        }

        let latest_validation_context = {
            let mempool_mirror = self.mempool_mirror.borrow();
            ValidationContext {
                prev_hash: mempool_mirror
                    .get_current_prev_hash()
                    .expect("current_prev_hash must be set"),
                nbits: mempool_mirror
                    .get_current_nbits()
                    .expect("current_nbits must be set"),
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

        // deliberately ignore potential send errors
        // we don't care if the receiver dropped the channel
        let _ = response_tx.send(response);
    }

    /// Submits a mining solution to Bitcoin Core.
    ///
    /// Not yet implemented — deliberately left as a stub for future work.
    pub(crate) async fn handle_push_solution(&self, _push_solution: PushSolution<'_>) {
        // todo
    }
}
