//! Handlers for Job Declaration Protocol messages.

use crate::job_declaration_protocol::{BitcoinCoreSv2JDP, io::JdResponse};
use stratum_core::{
    bitcoin::{
        Block, Transaction, TxMerkleNode, Txid, Wtxid,
        block::{Header, Version},
        consensus::serialize,
        hashes::Hash,
    },
    job_declaration_sv2::PushSolution,
};
use tokio::sync::oneshot;

impl BitcoinCoreSv2JDP {
    /// Validates a declared mining job by checking transaction availability and block structure.
    ///
    /// Adds missing transactions to the mempool mirror, verifies all transactions are available,
    /// assembles a test block, and uses Bitcoin Core's `checkBlock` to validate the block
    /// structure. Returns success with current template parameters or an error if validation
    /// fails.
    pub(crate) async fn handle_declare_mining_job(
        &self,
        version: Version,
        coinbase_tx: Transaction,
        wtxid_list: Vec<Wtxid>,
        missing_txs: Vec<Transaction>,
        response_tx: oneshot::Sender<JdResponse>,
    ) {
        tracing::info!(
            "Validating DeclareMiningJob - version: {:?}, coinbase inputs: {}, outputs: {}, locktime: {}",
            version,
            coinbase_tx.input.len(),
            coinbase_tx.output.len(),
            coinbase_tx.lock_time.to_consensus_u32()
        );
        tracing::debug!(
            "Declared coinbase scriptSig: {:?}",
            coinbase_tx.input[0].script_sig
        );

        let (prevhash, nbits, min_ntime, txdata) = {
            let mut mempool_mirror = self.mempool_mirror.borrow_mut();

            // Add the missing transactions to the mempool mirror
            mempool_mirror.add_transactions(missing_txs);

            // Now verify that all wtxids from the declared job are available
            let missing_wtxids = mempool_mirror.verify(&wtxid_list);
            if !missing_wtxids.is_empty() {
                // deliberately ignore potential errors
                // we don't care if the receiver dropped the channel
                let _ = response_tx.send(JdResponse::MissingTransactions(missing_wtxids));
                return;
            }

            let prevhash = mempool_mirror
                .get_current_prev_hash()
                .expect("current_prev_hash must be set");
            let nbits = mempool_mirror
                .get_current_nbits()
                .expect("current_nbits must be set");
            let min_ntime = mempool_mirror
                .get_current_min_ntime()
                .expect("current_min_ntime must be set");
            let txdata = mempool_mirror.get_txdata(&wtxid_list);

            tracing::info!(
                "Using prevhash: {:?}, nbits: {:?}, min_ntime: {} from mempool mirror",
                prevhash,
                nbits,
                min_ntime
            );

            (prevhash, nbits, min_ntime, txdata)
        }; // mempool_mirror dropped here, we don't want to hold it across await points

        let txid_list: Vec<Txid> = txdata.iter().map(|tx| tx.compute_txid()).collect();

        let valid_job = {
            let mut all_transactions = Vec::with_capacity(1 + txdata.len());
            all_transactions.push(coinbase_tx.clone());
            all_transactions.extend(txdata);

            let num_transactions = all_transactions.len();

            // Use the min_ntime from the template as the block timestamp
            // This ensures we meet Bitcoin Core's timestamp validation rules
            let block_time = min_ntime;

            let header = Header {
                version,
                prev_blockhash: prevhash,
                merkle_root: TxMerkleNode::all_zeros(), // doesn't matter
                time: block_time,
                bits: nbits,
                nonce: 0, // doesn't matter
            };

            let block = Block {
                header,
                txdata: all_transactions,
            };

            let block_bytes: Vec<u8> = serialize(&block);

            tracing::debug!(
                "Assembled block for checkBlock: {} bytes, {} transactions",
                block_bytes.len(),
                num_transactions
            );

            let mut check_block_request = self.mining_ipc_client.check_block_request();
            let mut check_block_params = check_block_request.get();

            check_block_params.set_block(&block_bytes);

            let mut options = match check_block_params.get_options() {
                Ok(options) => options,
                Err(e) => {
                    tracing::error!("Failed to get check block options: {e}");
                    // send error response to the client
                    // deliberately ignore potential send errors
                    let _ = response_tx.send(JdResponse::Error("internal-error".to_string()));
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };
            options.set_check_merkle_root(false);
            options.set_check_pow(false);

            let check_block_response = match check_block_request.send().promise.await {
                Ok(response) => response,
                Err(e) => {
                    tracing::error!("Failed to send check block request: {e}");
                    // send error response to the client
                    // deliberately ignore potential send errors
                    let _ = response_tx.send(JdResponse::Error("internal-error".to_string()));
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };
            let check_block_result = match check_block_response.get() {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!("Failed to get check block result: {e}");
                    // send error response to the client
                    // deliberately ignore potential send errors
                    let _ = response_tx.send(JdResponse::Error("internal-error".to_string()));
                    tracing::warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let result = check_block_result.get_result();
            tracing::debug!("checkBlock returned: {}", result);
            if !result {
                tracing::error!("Bitcoin Core rejected the block via checkBlock");
                tracing::debug!(
                    "Block details - version: {:?}, prev_blockhash: {:?}, bits: {:?}, num_txs: {}",
                    version,
                    prevhash,
                    nbits,
                    num_transactions
                );
                tracing::debug!(
                    "Coinbase tx inputs: {}, outputs: {}",
                    coinbase_tx.input.len(),
                    coinbase_tx.output.len()
                );
                tracing::debug!(
                    "Block header time: {}, merkle_root: {:?}",
                    header.time,
                    header.merkle_root
                );
            }
            result
        };

        let response = if valid_job {
            JdResponse::Success {
                prev_hash: prevhash,
                nbits,
                txid_list,
            }
        } else {
            JdResponse::Error("invalid-job".to_string())
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
