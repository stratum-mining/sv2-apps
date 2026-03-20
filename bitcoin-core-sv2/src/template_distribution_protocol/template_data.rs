use crate::template_distribution_protocol::error::TemplateDataError;

use bitcoin_capnp_types::{
    mining_capnp::block_template::Client as BlockTemplateIpcClient,
    proxy_capnp::{thread::Client as ThreadIpcClient, thread_map::Client as ThreadMapIpcClient},
};
use std::{fs::File, io::Write, path::Path};
use stratum_core::bitcoin::{
    Target, Transaction, TxOut,
    amount::{Amount, CheckedSum},
    block::{Block, Header, Version},
    consensus::{deserialize, serialize},
    hashes::{Hash, HashEngine, sha256d},
};

use stratum_core::{
    binary_sv2::{B016M, B064K, B0255, Seq064K, Seq0255, U256},
    template_distribution_sv2::{
        NewTemplate, RequestTransactionDataSuccess, SetNewPrevHash, SubmitSolution,
    },
};

#[derive(Clone)]
pub struct TemplateData {
    template_id: u64,
    header: Header,
    coinbase_tx: Transaction,
    merkle_path: Vec<Vec<u8>>,
    template_ipc_client: BlockTemplateIpcClient,
}

// impl block for public methods
impl TemplateData {
    pub fn new(
        template_id: u64,
        header: Header,
        coinbase_tx: Transaction,
        merkle_path: Vec<Vec<u8>>,
        template_ipc_client: BlockTemplateIpcClient,
    ) -> Self {
        Self {
            template_id,
            header,
            coinbase_tx,
            merkle_path,
            template_ipc_client,
        }
    }

    /// Destroys the template IPC client, cleaning up the resources on the Bitcoin Core side
    pub async fn destroy_ipc_client(
        &self,
        thread_ipc_client: ThreadIpcClient,
    ) -> Result<(), TemplateDataError> {
        tracing::debug!("Destroying template IPC client: {}", self.template_id);
        let mut destroy_ipc_client_request = self.template_ipc_client.destroy_request();
        let destroy_ipc_client_request_params = destroy_ipc_client_request.get();

        destroy_ipc_client_request_params
            .get_context()?
            .set_thread(thread_ipc_client);

        destroy_ipc_client_request.send().promise.await?;

        Ok(())
    }

    pub fn get_template_id(&self) -> u64 {
        self.template_id
    }

    pub fn get_new_template_message(
        &self,
        future_template: bool,
    ) -> Result<NewTemplate<'static>, TemplateDataError> {
        let new_template = NewTemplate {
            template_id: self.template_id,
            future_template,
            version: self.get_version()?,
            coinbase_tx_version: self.get_coinbase_tx_version()?,
            coinbase_prefix: self.get_coinbase_script_sig()?,
            coinbase_tx_input_sequence: self.get_coinbase_input_sequence(),
            coinbase_tx_value_remaining: self.get_coinbase_tx_value_remaining()?,
            coinbase_tx_outputs_count: self.get_empty_coinbase_outputs().len() as u32,
            coinbase_tx_outputs: self.get_serialized_empty_coinbase_outputs()?,
            coinbase_tx_locktime: self.get_coinbase_tx_lock_time(),
            merkle_path: self.get_merkle_path()?,
        };
        Ok(new_template.into_static())
    }

    // please note that `SetNewPrevHash.target` is consensus and not weak-block
    // so it's essentially redundant with `SetNewPrevHash.n_bits`
    pub fn get_set_new_prev_hash_message(&self) -> SetNewPrevHash<'static> {
        let set_new_prev_hash = SetNewPrevHash {
            template_id: self.template_id,
            prev_hash: self.get_prev_hash(),
            header_timestamp: self.get_ntime(),
            n_bits: self.get_nbits(),
            target: self.get_target(),
        };
        set_new_prev_hash.into_static()
    }

    pub async fn get_request_transaction_data_success_message(
        &self,
        thread_map: ThreadMapIpcClient,
    ) -> Result<RequestTransactionDataSuccess<'static>, TemplateDataError> {
        let request_transaction_data_success = RequestTransactionDataSuccess {
            template_id: self.template_id,
            transaction_list: self.get_tx_data(thread_map).await?,
            excess_data: vec![]
                .try_into()
                .expect("empty vec should always be valid for B064K"),
        };
        Ok(request_transaction_data_success.into_static())
    }

    pub fn get_prev_hash(&self) -> U256<'static> {
        self.header.prev_blockhash.to_byte_array().into()
    }

    async fn dump_solution_to_disk(
        &self,
        thread_map: ThreadMapIpcClient,
        solution_coinbase_tx: Transaction,
        solution_header_version: u32,
        solution_header_timestamp: u32,
        solution_header_nonce: u32,
        path_dir: &Path,
    ) {
        let self_clone = self.clone();
        let path_dir = path_dir.to_path_buf();
        tokio::task::spawn_local(async move {
            tracing::debug!("Creating a dedicated thread IPC client for getBlock request");

            // validate the solution
            let solution_header = {
                if solution_coinbase_tx.version != self_clone.coinbase_tx.version
                    || solution_coinbase_tx.lock_time != self_clone.coinbase_tx.lock_time
                    || solution_coinbase_tx.input.len() != 1
                    || solution_coinbase_tx.input[0].sequence
                        != self_clone.coinbase_tx.input[0].sequence
                    || solution_coinbase_tx.input[0].witness
                        != self_clone.coinbase_tx.input[0].witness
                    || solution_coinbase_tx.input[0].previous_output
                        != self_clone.coinbase_tx.input[0].previous_output
                {
                    tracing::error!(
                        "Solution coinbase tx is not congruent with original coinbase tx"
                    );
                    return;
                }

                // Compute merkle root from coinbase transaction and merkle path
                let coinbase_txid = solution_coinbase_tx.compute_txid();
                let mut current_hash = *coinbase_txid.as_byte_array();

                // Combine with each sibling hash in the merkle path
                for sibling_hash_bytes in &self_clone.merkle_path {
                    // Combine current hash with sibling hash and double SHA256
                    let mut hasher = sha256d::Hash::engine();
                    HashEngine::input(&mut hasher, &current_hash);
                    HashEngine::input(&mut hasher, sibling_hash_bytes);
                    current_hash = *sha256d::Hash::from_engine(hasher).as_byte_array();
                }

                let solution_header = Header {
                    version: Version::from_consensus(solution_header_version as i32),
                    prev_blockhash: self_clone.header.prev_blockhash,
                    merkle_root: sha256d::Hash::from_byte_array(current_hash).into(),
                    time: solution_header_timestamp,
                    nonce: solution_header_nonce,
                    bits: self_clone.header.bits,
                };

                if let Err(e) = solution_header.validate_pow(solution_header.target()) {
                    tracing::error!("Solution header is not valid: {}", e);
                    return;
                }

                solution_header
            };

            let thread_ipc_client = thread_map
                .make_thread_request()
                .send()
                .promise
                .await
                .expect("Failed to send thread IPC client request")
                .get()
                .expect("Failed to get thread IPC client reader")
                .get_result()
                .expect("Failed to get thread IPC client result");

            let mut template_block_request = self_clone.template_ipc_client.get_block_request();
            let mut template_block_request_context = template_block_request
                .get()
                .get_context()
                .expect("Failed to get template block request context");
            template_block_request_context.set_thread(thread_ipc_client.clone());

            let template_block_response = template_block_request
                .send()
                .promise
                .await
                .expect("Failed to send template block request");
            let template_block_reader = template_block_response
                .get()
                .expect("Failed to get template block response");
            let template_block_bytes = template_block_reader
                .get_result()
                .expect("Failed to get template block result");

            // Deserialize the complete block template from Bitcoin Core's serialization format
            let mut solution_block: Block =
                deserialize(template_block_bytes).expect("Failed to deserialize block template");

            solution_block.txdata[0] = solution_coinbase_tx;
            solution_block.header = solution_header;

            let solution_block_bytes = serialize(&solution_block);
            let solution_block_hash = solution_block.block_hash().to_string();
            let solution_block_path = path_dir.join(format!("{}.dat", solution_block_hash));

            let mut file =
                File::create(&solution_block_path).expect("Failed to create solution block file");
            file.write_all(&solution_block_bytes)
                .expect("Failed to write solution block to file");
            tracing::info!(
                "Solution block dumped to: {}",
                solution_block_path.display()
            );
        });
    }

    pub async fn submit_solution(
        &self,
        submit_solution: SubmitSolution<'static>,
        thread_ipc_client: ThreadIpcClient,
        thread_map: ThreadMapIpcClient,
        path_dir: &Path,
    ) -> Result<(), TemplateDataError> {
        let solution_coinbase_tx_bytes: Vec<u8> = submit_solution.coinbase_tx.to_vec();

        let solution_coinbase_tx: Transaction =
            deserialize(&solution_coinbase_tx_bytes).map_err(|e| {
                tracing::error!("SubmitSolution.coinbase_tx is invalid: {}", e);
                TemplateDataError::InvalidCoinbaseTx(e)
            })?;

        // spawn a task to dump the solution to disk
        self.dump_solution_to_disk(
            thread_map.clone(),
            solution_coinbase_tx,
            submit_solution.version,
            submit_solution.header_timestamp,
            submit_solution.header_nonce,
            path_dir,
        )
        .await;

        let mut submit_solution_request = self.template_ipc_client.submit_solution_request();
        let mut submit_solution_request_params = submit_solution_request.get();

        submit_solution_request_params.set_version(submit_solution.version);
        submit_solution_request_params.set_timestamp(submit_solution.header_timestamp);
        submit_solution_request_params.set_nonce(submit_solution.header_nonce);
        submit_solution_request_params.set_coinbase(&solution_coinbase_tx_bytes);

        submit_solution_request_params
            .get_context()?
            .set_thread(thread_ipc_client.clone());

        let submit_solution_response = submit_solution_request.send().promise.await?;

        if !submit_solution_response.get()?.get_result() {
            return Err(TemplateDataError::FailedIpcSubmitSolution);
        }

        Ok(())
    }
}

// impl block for private methods
impl TemplateData {
    fn get_nbits(&self) -> u32 {
        self.header.bits.to_consensus()
    }

    fn get_target(&self) -> U256<'_> {
        let target = Target::from(self.header.bits);
        let target_bytes: [u8; 32] = target.to_le_bytes();
        U256::from(target_bytes)
    }

    fn get_ntime(&self) -> u32 {
        self.header.time
    }

    fn get_version(&self) -> Result<u32, TemplateDataError> {
        self.header
            .version
            .to_consensus()
            .try_into()
            .map_err(|_| TemplateDataError::InvalidBlockVersion)
    }

    fn get_coinbase_tx_version(&self) -> Result<u32, TemplateDataError> {
        self.coinbase_tx
            .version
            .0
            .try_into()
            .map_err(|_| TemplateDataError::InvalidCoinbaseTxVersion)
    }

    fn get_coinbase_script_sig(&self) -> Result<B0255<'_>, TemplateDataError> {
        let coinbase_script_sig: B0255 = self.coinbase_tx.input[0]
            .script_sig
            .to_bytes()
            .try_into()
            .map_err(|_| TemplateDataError::InvalidCoinbaseScriptSig)?;
        Ok(coinbase_script_sig)
    }

    fn get_coinbase_input_sequence(&self) -> u32 {
        self.coinbase_tx.input[0].sequence.to_consensus_u32()
    }

    fn get_empty_coinbase_outputs(&self) -> Vec<TxOut> {
        self.coinbase_tx
            .output
            .iter()
            .filter(|output| output.value == Amount::from_sat(0))
            .cloned()
            .collect()
    }

    fn get_serialized_empty_coinbase_outputs(&self) -> Result<B064K<'_>, TemplateDataError> {
        let empty_coinbase_outputs = self.get_empty_coinbase_outputs();
        let mut serialized_empty_coinbase_outputs = Vec::new();
        for output in empty_coinbase_outputs {
            serialized_empty_coinbase_outputs.extend_from_slice(&serialize(&output));
        }
        let serialized_empty_coinbase_outputs: B064K = serialized_empty_coinbase_outputs
            .try_into()
            .map_err(|_| TemplateDataError::FailedToSerializeEmptyCoinbaseOutputs)?;
        Ok(serialized_empty_coinbase_outputs)
    }

    fn get_coinbase_tx_value_remaining(&self) -> Result<u64, TemplateDataError> {
        Ok(self
            .coinbase_tx
            .output
            .iter()
            .map(|output| output.value)
            .checked_sum()
            .ok_or(TemplateDataError::FailedToSumCoinbaseOutputs)?
            .to_sat())
    }

    fn get_coinbase_tx_lock_time(&self) -> u32 {
        self.coinbase_tx.lock_time.to_consensus_u32()
    }

    async fn get_tx_data(
        &self,
        thread_map: ThreadMapIpcClient,
    ) -> Result<Seq064K<'_, B016M<'static>>, TemplateDataError> {
        tracing::debug!("Creating a dedicated thread IPC client for get_tx_data");
        let thread_ipc_client_request = thread_map.make_thread_request();
        let thread_ipc_client_response = thread_ipc_client_request.send().promise.await?;
        let thread_ipc_client = thread_ipc_client_response.get()?.get_result()?;

        let mut template_block_request = self.template_ipc_client.get_block_request();
        template_block_request
            .get()
            .get_context()?
            .set_thread(thread_ipc_client.clone());

        let template_block_response = template_block_request.send().promise.await?;
        let template_block_bytes = template_block_response.get()?.get_result()?;

        // Deserialize the complete block template from Bitcoin Core's serialization format
        tracing::debug!(
            "Deserializing block template ({} bytes)",
            template_block_bytes.len()
        );
        let block: Block = deserialize(template_block_bytes)?;
        tracing::debug!(
            "Block deserialized - prev_hash from header: {:?}",
            block.header.prev_blockhash
        );

        let tx_data: Vec<B016M<'static>> = block
            .txdata
            .iter()
            .skip(1) // skip coinbase tx
            .map(|tx| {
                serialize(tx)
                    .try_into()
                    .expect("tx data should always be valid for B016M")
            })
            .collect();
        Ok(Seq064K::new(tx_data).expect("tx data should always be valid for Seq064K"))
    }

    fn get_merkle_path(&self) -> Result<Seq0255<'_, U256<'_>>, TemplateDataError> {
        // Convert each Vec<u8> in the merkle path to U256
        let merkle_path_u256: Vec<U256<'_>> = self
            .merkle_path
            .iter()
            .map(|hash_bytes| {
                // Convert Vec<u8> to U256
                U256::try_from(hash_bytes.clone())
                    .map_err(|_| TemplateDataError::FailedToConvertMerklePathHashToU256)
            })
            .collect::<Result<Vec<_>, _>>()?;

        Seq0255::new(merkle_path_u256).map_err(|_| TemplateDataError::FailedToCreateMerklePathSeq)
    }
}
