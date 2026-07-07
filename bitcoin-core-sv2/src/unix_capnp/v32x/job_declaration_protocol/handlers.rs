//! Handlers for Bitcoin Core v32.x Sv2 Job Declaration Protocol via capnp over UNIX socket.

use crate::{
    common::job_declaration_protocol::io::{DownstreamId, JdResponse, RequestId},
    unix_capnp::v32x::job_declaration_protocol::{
        BitcoinCoreSv2JDP, error::BitcoinCoreSv2JDPError,
    },
};
use bitcoin_capnp_types::mining_capnp::{
    block_template::Client as BlockTemplateIpcClient,
    tx_collection::Client as TxCollectionIpcClient,
};
use bitcoin_capnp_types_v32 as bitcoin_capnp_types;
use stratum_core::{
    bitcoin::{
        BlockHash, Transaction, TxMerkleNode, Wtxid,
        block::{Header, Version},
        consensus::{deserialize, serialize},
        hashes::Hash,
    },
    job_declaration_sv2::{
        ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
        ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
    },
};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

const MAX_SUBMIT_SOLUTION_ATTEMPTS: usize = 3;
const SUBMIT_SOLUTION_RETRY_BACKOFF_MS: u64 = 15;

/// `reason` returned by `TxCollection::makeTemplate` when the collection is still incomplete.
const MAKE_TEMPLATE_REASON_MISSING_TXS: &str = "missing-txs";

impl BitcoinCoreSv2JDP {
    /// Validates a declared mining job via Bitcoin Core's `TxCollection` interface.
    ///
    /// The declared wtxids are collected with `collectTxs`, completed with `addMissingTxs`
    /// (transactions from `ProvideMissingTransactions.Success`), and checked with
    /// `unknownTxPos`. Once complete, `makeTemplate` reconstructs the block inside Bitcoin
    /// Core and validates it, so no local mempool mirror is needed. Returns success with the
    /// template parameters or an error if validation fails.
    ///
    /// The declared coinbase (with a zeroed extranonce, which no contextual check depends
    /// on) is passed to `makeTemplate`, so it is fully validated at declaration time (BIP34
    /// height, output value, sigops, weight, witness commitment).
    ///
    /// On success the validated template is retained under `(downstream_id, request_id)`
    /// so a later [`JdRequest::PushSolution`] can submit a solution against it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_declare_mining_job(
        &self,
        downstream_id: DownstreamId,
        request_id: RequestId,
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

        let response = match self
            .validate_declare_mining_job(
                downstream_id,
                request_id,
                &coinbase_tx,
                &wtxid_list,
                &missing_txs,
            )
            .await
        {
            Ok(response) => response,
            Err(e) => {
                error!("DeclareMiningJob validation failed with IPC error: {e:?}");
                // deliberately ignore potential send errors
                // we don't care if the receiver dropped the channel
                let _ = response_tx.send(JdResponse::Error {
                    error_code: ERROR_CODE_DECLARE_MINING_JOB_INTERNAL_ERROR,
                    prev_hash: None,
                });
                warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                self.cancellation_token.cancel();
                return;
            }
        };

        // deliberately ignore potential send errors
        // we don't care if the receiver dropped the channel
        let _ = response_tx.send(response);
    }

    /// Runs the `TxCollection` validation flow and maps the outcome to a [`JdResponse`].
    ///
    /// Returns `Err` only for IPC-level failures; validation failures are expressed as
    /// [`JdResponse::Error`] or [`JdResponse::MissingTransactions`].
    async fn validate_declare_mining_job(
        &self,
        downstream_id: DownstreamId,
        request_id: RequestId,
        coinbase_tx: &Transaction,
        wtxid_list: &[Wtxid],
        missing_txs: &[Transaction],
    ) -> Result<JdResponse, BitcoinCoreSv2JDPError> {
        let (tip_hash, tip_height) = self.get_tip().await?;

        let collection = self.collect_txs(wtxid_list).await?;

        // Complete the collection with transactions from ProvideMissingTransactions.Success
        if !missing_txs.is_empty() {
            self.add_missing_txs(&collection, missing_txs).await?;
        }

        // Ask Bitcoin Core which declared transactions it still doesn't know about
        let missing_positions = self.unknown_tx_pos(&collection).await?;
        if !missing_positions.is_empty() {
            let missing_wtxids = Self::wtxids_at_positions(wtxid_list, &missing_positions);
            self.destroy_tx_collection(&collection).await;
            return Ok(JdResponse::MissingTransactions {
                missing_wtxids,
                prev_hash: tip_hash,
            });
        }

        // A BIP34 height mismatch would also be caught by makeTemplate (bad-cb-height), but
        // checking it here lets us classify the error as a stale chain tip instead of a
        // generically invalid job.
        let declared_bip34_height = coinbase_tx
            .input
            .first()
            .and_then(|input| {
                decode_bip34_height_from_coinbase_script_sig(input.script_sig.as_bytes())
            })
            // Some templates/coinbase formats do not expose BIP34 height in canonical
            // scriptSig push form (e.g. opcode-encoded small integers in tests/regtest).
            // Fall back to coinbase lock_time to avoid panics and keep a stable
            // stale-tip comparison signal.
            .unwrap_or_else(|| coinbase_tx.lock_time.to_consensus_u32());
        let next_height = (tip_height + 1) as u32;
        if declared_bip34_height != next_height {
            debug!(
                ?tip_hash,
                tip_height,
                declared_bip34_height,
                "Declared BIP34 height does not match the next block height; classifying error as stale-chain-tip"
            );
            self.destroy_tx_collection(&collection).await;
            return Ok(JdResponse::Error {
                error_code: ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
                prev_hash: Some(tip_hash),
            });
        }

        // Reconstruct and validate the block, including the declared coinbase, inside
        // Bitcoin Core
        let (reason, debug_msg, template) = self
            .make_template(&collection, tip_hash, coinbase_tx)
            .await?;

        let Some(template) = template else {
            // Transactions can disappear from the mempool between unknownTxPos and
            // makeTemplate (e.g. eviction, replacement, new block). Give the client a chance
            // to provide them instead of failing the declaration outright.
            if reason == MAKE_TEMPLATE_REASON_MISSING_TXS {
                let missing_positions = self.unknown_tx_pos(&collection).await?;
                if !missing_positions.is_empty() {
                    let missing_wtxids = Self::wtxids_at_positions(wtxid_list, &missing_positions);
                    self.destroy_tx_collection(&collection).await;
                    return Ok(JdResponse::MissingTransactions {
                        missing_wtxids,
                        prev_hash: tip_hash,
                    });
                }
            }

            self.destroy_tx_collection(&collection).await;

            error!(
                reason,
                debug = debug_msg,
                "Bitcoin Core rejected the declared job via TxCollection::makeTemplate"
            );

            // The declared BIP34 height matched the tip at arrival, so a makeTemplate
            // failure is either a stale-tip race (tip moved while we were validating) or a
            // genuinely invalid job.
            let (latest_tip_hash, latest_tip_height) = self.get_tip().await?;
            let error_code = if latest_tip_hash != tip_hash {
                debug!(
                    ?tip_hash,
                    tip_height,
                    ?latest_tip_hash,
                    latest_tip_height,
                    "Detected stale chain tip during DeclareMiningJob validation; classifying error as stale-chain-tip"
                );
                ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP
            } else {
                ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB
            };

            return Ok(JdResponse::Error {
                error_code,
                prev_hash: Some(latest_tip_hash),
            });
        };

        // The validated template provides the parameters (header) and the coinbase merkle
        // branch that jd-server needs for SetCustomMiningJob validation. The block itself
        // stays inside Bitcoin Core: the template is retained so a later PushSolution can
        // be submitted against it via submitSolution.
        let header = self.get_template_header(&template).await?;
        let merkle_path = self.get_coinbase_merkle_path(&template).await?;

        self.destroy_tx_collection(&collection).await;

        debug!(
            prev_hash = ?header.prev_blockhash,
            nbits = ?header.bits,
            min_ntime = header.time,
            merkle_path_len = merkle_path.len(),
            "TxCollection::makeTemplate validated the declared job; retaining template"
        );

        let replaced_template = self
            .declared_templates
            .borrow_mut()
            .insert((downstream_id, request_id), template);
        if let Some(replaced_template) = replaced_template {
            // A re-declared request id supersedes the previous declaration.
            self.destroy_template(&replaced_template).await;
        }

        Ok(JdResponse::Success {
            prev_hash: header.prev_blockhash,
            nbits: header.bits,
            min_ntime: header.time,
            merkle_path,
        })
    }

    /// Discards the retained template of a declared job, if any.
    pub(crate) async fn release_declared_job(
        &self,
        downstream_id: DownstreamId,
        request_id: RequestId,
    ) {
        let template = self
            .declared_templates
            .borrow_mut()
            .remove(&(downstream_id, request_id));
        if let Some(template) = template {
            debug!(downstream_id, request_id, "Releasing retained template");
            self.destroy_template(&template).await;
        }
    }

    /// Discards all retained templates of a disconnected downstream.
    pub(crate) async fn cleanup_downstream(&self, downstream_id: DownstreamId) {
        let templates: Vec<BlockTemplateIpcClient> = {
            let mut declared_templates = self.declared_templates.borrow_mut();
            let keys: Vec<(DownstreamId, RequestId)> = declared_templates
                .keys()
                .filter(|(id, _)| *id == downstream_id)
                .copied()
                .collect();
            keys.iter()
                .filter_map(|key| declared_templates.remove(key))
                .collect()
        };
        if !templates.is_empty() {
            debug!(
                downstream_id,
                count = templates.len(),
                "Releasing retained templates for disconnected downstream"
            );
        }
        for template in &templates {
            self.destroy_template(template).await;
        }
    }

    /// Maps `unknownTxPos` positions back to the declared wtxids.
    fn wtxids_at_positions(wtxid_list: &[Wtxid], positions: &[u32]) -> Vec<Wtxid> {
        positions
            .iter()
            .filter_map(|&pos| wtxid_list.get(pos as usize).copied())
            .collect()
    }

    /// Calls `Mining::collectTxs` with the declared wtxids.
    async fn collect_txs(
        &self,
        wtxid_list: &[Wtxid],
    ) -> Result<TxCollectionIpcClient, BitcoinCoreSv2JDPError> {
        let mut collect_txs_request = self.mining_ipc_client.collect_txs_request();
        collect_txs_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        {
            let mut wtxids = collect_txs_request
                .get()
                .init_wtxids(wtxid_list.len() as u32);
            for (pos, wtxid) in wtxid_list.iter().enumerate() {
                wtxids.set(pos as u32, wtxid.as_byte_array());
            }
        }
        let collect_txs_response = collect_txs_request.send().promise.await?;
        Ok(collect_txs_response.get()?.get_result()?)
    }

    /// Calls `TxCollection::addMissingTxs` with client-provided transactions.
    ///
    /// The caller must only pass transactions whose wtxid is part of the collection
    /// (see the [`JdRequest::DeclareMiningJob`] invariants); Bitcoin Core rejects the
    /// whole call otherwise.
    async fn add_missing_txs(
        &self,
        collection: &TxCollectionIpcClient,
        missing_txs: &[Transaction],
    ) -> Result<(), BitcoinCoreSv2JDPError> {
        let mut add_missing_txs_request = collection.add_missing_txs_request();
        add_missing_txs_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        {
            let mut txs = add_missing_txs_request
                .get()
                .init_txs(missing_txs.len() as u32);
            for (pos, tx) in missing_txs.iter().enumerate() {
                txs.set(pos as u32, &serialize(tx));
            }
        }
        add_missing_txs_request.send().promise.await?;
        Ok(())
    }

    /// Calls `TxCollection::unknownTxPos`, returning the positions of transactions Bitcoin
    /// Core does not know about.
    async fn unknown_tx_pos(
        &self,
        collection: &TxCollectionIpcClient,
    ) -> Result<Vec<u32>, BitcoinCoreSv2JDPError> {
        let mut unknown_tx_pos_request = collection.unknown_tx_pos_request();
        unknown_tx_pos_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        let unknown_tx_pos_response = unknown_tx_pos_request.send().promise.await?;
        let positions = unknown_tx_pos_response.get()?.get_result()?;
        Ok((0..positions.len()).map(|pos| positions.get(pos)).collect())
    }

    /// Calls `TxCollection::makeTemplate` with the declared coinbase, returning the BIP-22
    /// style `reason`/`debug` strings and, on success, the validated
    /// [`BlockTemplateIpcClient`].
    async fn make_template(
        &self,
        collection: &TxCollectionIpcClient,
        prev_hash: BlockHash,
        coinbase_tx: &Transaction,
    ) -> Result<(String, String, Option<BlockTemplateIpcClient>), BitcoinCoreSv2JDPError> {
        let mut make_template_request = collection.make_template_request();
        make_template_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        make_template_request
            .get()
            .set_prevhash(prev_hash.as_byte_array());
        make_template_request
            .get()
            .set_coinbase(&serialize(coinbase_tx));
        let make_template_response = make_template_request.send().promise.await?;
        let make_template_result = make_template_response.get()?;
        let reason = make_template_result
            .get_reason()?
            .to_string()
            .map_err(bitcoin_capnp_types::capnp::Error::from)?;
        let debug_msg = make_template_result
            .get_debug()?
            .to_string()
            .map_err(bitcoin_capnp_types::capnp::Error::from)?;
        let template = if make_template_result.has_result() {
            Some(make_template_result.get_result()?)
        } else {
            None
        };
        Ok((reason, debug_msg, template))
    }

    /// Fetches the header of a validated template via `BlockTemplate::getBlockHeader`.
    async fn get_template_header(
        &self,
        template: &BlockTemplateIpcClient,
    ) -> Result<Header, BitcoinCoreSv2JDPError> {
        let mut get_block_header_request = template.get_block_header_request();
        get_block_header_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        let get_block_header_response = get_block_header_request.send().promise.await?;
        let header_bytes = get_block_header_response.get()?.get_result()?;
        deserialize(header_bytes).map_err(BitcoinCoreSv2JDPError::FailedToDeserializeBlock)
    }

    /// Fetches the coinbase merkle branch of a validated template via
    /// `BlockTemplate::getCoinbaseMerklePath`.
    ///
    /// The branch does not depend on the template's (dummy) coinbase, so it also applies to
    /// the declared coinbase.
    async fn get_coinbase_merkle_path(
        &self,
        template: &BlockTemplateIpcClient,
    ) -> Result<Vec<TxMerkleNode>, BitcoinCoreSv2JDPError> {
        let mut get_coinbase_merkle_path_request = template.get_coinbase_merkle_path_request();
        get_coinbase_merkle_path_request
            .get()
            .get_context()?
            .set_thread(self.thread_ipc_client.clone());
        let get_coinbase_merkle_path_response =
            get_coinbase_merkle_path_request.send().promise.await?;
        let get_coinbase_merkle_path_result = get_coinbase_merkle_path_response.get()?;
        let path = get_coinbase_merkle_path_result.get_result()?;
        let mut merkle_path = Vec::with_capacity(path.len() as usize);
        for pos in 0..path.len() {
            let node_bytes: [u8; 32] = path.get(pos)?.try_into().map_err(|_| {
                bitcoin_capnp_types::capnp::Error::failed(
                    "coinbase merkle path node is not 32 bytes".to_string(),
                )
            })?;
            merkle_path.push(TxMerkleNode::from_byte_array(node_bytes));
        }
        Ok(merkle_path)
    }

    /// Best-effort `TxCollection::destroy` so Bitcoin Core can free the collection early.
    async fn destroy_tx_collection(&self, collection: &TxCollectionIpcClient) {
        let mut destroy_request = collection.destroy_request();
        match destroy_request.get().get_context() {
            Ok(mut context) => context.set_thread(self.thread_ipc_client.clone()),
            Err(e) => {
                debug!("Failed to set TxCollection destroy request thread context: {e}");
                return;
            }
        }
        if let Err(e) = destroy_request.send().promise.await {
            debug!("Failed to destroy TxCollection: {e}");
        }
    }

    /// Best-effort `BlockTemplate::destroy` so Bitcoin Core can free the template early.
    async fn destroy_template(&self, template: &BlockTemplateIpcClient) {
        let mut destroy_request = template.destroy_request();
        match destroy_request.get().get_context() {
            Ok(mut context) => context.set_thread(self.thread_ipc_client.clone()),
            Err(e) => {
                debug!("Failed to set BlockTemplate destroy request thread context: {e}");
                return;
            }
        }
        if let Err(e) = destroy_request.send().promise.await {
            debug!("Failed to destroy BlockTemplate: {e}");
        }
    }

    /// Submits a solution for a previously declared job via the retained template's
    /// `submitSolution` method. Bitcoin Core reconstructs the solved block from the
    /// template's transactions plus the given header fields and coinbase, so the block
    /// never crosses the IPC boundary.
    pub(crate) async fn handle_push_solution(
        &self,
        downstream_id: DownstreamId,
        request_id: RequestId,
        version: u32,
        ntime: u32,
        nonce: u32,
        coinbase_tx: Transaction,
    ) {
        let template = self
            .declared_templates
            .borrow_mut()
            .remove(&(downstream_id, request_id));
        let Some(template) = template else {
            error!(
                downstream_id,
                request_id, "No retained template for PushSolution; dropping solution"
            );
            return;
        };

        let coinbase_bytes: Vec<u8> = serialize(&coinbase_tx);
        debug!(
            downstream_id,
            request_id,
            version,
            ntime,
            nonce,
            coinbase_bytes_len = coinbase_bytes.len(),
            "Submitting solution via BlockTemplate::submitSolution"
        );

        // a dedicated thread is used to submit solutions to Bitcoin Core
        // therefore retries should be extremely rare
        for attempt in 1..=MAX_SUBMIT_SOLUTION_ATTEMPTS {
            let mut submit_solution_request = template.submit_solution_request();

            match submit_solution_request.get().get_context() {
                Ok(mut context) => context.set_thread(self.submit_block_thread_ipc_client.clone()),
                Err(e) => {
                    error!("Failed to set submitSolution request thread context: {e}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            }

            submit_solution_request.get().set_version(version);
            submit_solution_request.get().set_timestamp(ntime);
            submit_solution_request.get().set_nonce(nonce);
            submit_solution_request.get().set_coinbase(&coinbase_bytes);

            let submit_solution_response = match submit_solution_request.send().promise.await {
                Ok(response) => response,
                Err(e) => {
                    let err: BitcoinCoreSv2JDPError = e.into();
                    if err.is_thread_busy() && attempt < MAX_SUBMIT_SOLUTION_ATTEMPTS {
                        warn!(
                            attempt,
                            max_attempts = MAX_SUBMIT_SOLUTION_ATTEMPTS,
                            "Transient IPC contention during submitSolution (thread busy); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            SUBMIT_SOLUTION_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }

                    error!("Failed to send submitSolution request: {err:?}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let submit_solution_result = match submit_solution_response.get() {
                Ok(result) => result,
                Err(e) => {
                    let err: BitcoinCoreSv2JDPError = e.into();
                    if err.is_thread_busy() && attempt < MAX_SUBMIT_SOLUTION_ATTEMPTS {
                        warn!(
                            attempt,
                            max_attempts = MAX_SUBMIT_SOLUTION_ATTEMPTS,
                            "Transient IPC contention while reading submitSolution response (thread busy); retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            SUBMIT_SOLUTION_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }

                    error!("Failed to get submitSolution result: {err:?}");
                    warn!("Terminating Sv2 Bitcoin Core IPC Connection");
                    self.cancellation_token.cancel();
                    return;
                }
            };

            let accepted = submit_solution_result.get_result();

            if accepted {
                info!(
                    downstream_id,
                    request_id, "Bitcoin Core accepted solution via submitSolution"
                );
            } else {
                warn!(
                    downstream_id,
                    request_id, "Bitcoin Core rejected solution via submitSolution"
                );
            }

            break;
        }

        self.destroy_template(&template).await;
    }
}

/// Decodes BIP34 height from the first push in coinbase scriptSig.
/// Returns None if scriptSig does not start with a canonical small push.
pub(crate) fn decode_bip34_height_from_coinbase_script_sig(script_sig: &[u8]) -> Option<u32> {
    let first = *script_sig.first()?;

    // Support small-integer opcodes (OP_0, OP_1..OP_16) used by some templates.
    if first == 0x00 {
        return Some(0);
    }
    if (0x51..=0x60).contains(&first) {
        return Some((first - 0x50) as u32);
    }

    // Canonical small push form: first byte is push length (1..=4).
    let push_len = first as usize;
    if push_len == 0 || push_len > 4 || script_sig.len() < 1 + push_len {
        return None;
    }

    let mut height_bytes = [0u8; 4];
    height_bytes[..push_len].copy_from_slice(&script_sig[1..1 + push_len]);
    Some(u32::from_le_bytes(height_bytes))
}
