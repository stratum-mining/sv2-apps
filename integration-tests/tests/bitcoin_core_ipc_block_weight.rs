//! Integration test: a template filled to the weight limit still yields a valid block once the
//! coinbase carries everything `CoinbaseOutputConstraints` declared.
//!
//! `block_reserved_weight` is all Bitcoin Core knows about the space a block needs beyond its
//! transactions: header, transaction count, the whole coinbase. Whatever it leaves out, the final
//! block carries over the limit, and that only shows when the mempool holds more than a block.
//!
//! So the mempool is filled past a block first. The TDP publishes a template, a coinbase is built
//! on it the way a JDC would (every declared output byte, a full 100-byte `scriptSig`, the
//! template's own outputs appended) and the JDP declares the two, which has Bitcoin Core check
//! the block. Two assertions:
//!
//! 1. The block weighs more than 1168 under the limit. A lighter block fits whether or not that
//!    weight was reserved, so this is what makes the second assertion mean something.
//! 2. The declaration is answered `Success`. A template short by the fixed weight is caught by the
//!    JDP runtime's own weight budget first, and by Core's `bad-blk-weight` after it; both answer
//!    `invalid-job`.
//!
//! Structure:
//! - top: version-specific `#[tokio::test]` wrappers.
//! - `assert_full_template_block_passes_check_block`: the scenario.
//! - `coinbase_tx_for`: the downstream's coinbase, built from a template.
//! - bottom: runtime spawners and message helpers, as in the TDP and JDP IO tests.

use async_channel::{Receiver, Sender};
use integration_tests_sv2::{
    start_bitcoin_core, start_tracing, template_provider::DifficultyLevel, utils::join_within,
};
use std::time::{Duration, Instant};
use stratum_apps::{
    bitcoin_core_sv2::{
        CancellationToken,
        runtime_api::{
            BitcoinCoreVersion,
            job_declaration_protocol::{
                self,
                io::{JdRequest, JdResponse},
            },
            template_distribution_protocol,
        },
    },
    stratum_core::{
        bitcoin::{
            Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction,
            TxIn, TxMerkleNode, TxOut, Weight, Witness,
            absolute::LockTime,
            block::{Header, Version as BlockVersion},
            consensus::{Decodable, deserialize},
            hashes::Hash,
            transaction::Version as TxVersion,
        },
        parsers_sv2::TemplateDistributionOwned,
        template_distribution_sv2::{
            CoinbaseOutputConstraintsOwned, NewTemplateOwned, RequestTransactionDataOwned,
        },
    },
};

/// Coinbase output bytes the downstream declares, and the coinbase built here adds.
///
/// Forty-two payout outputs and a pool output, as in the deployment that first hit
/// `bad-blk-weight`. Any value above 208 bytes works; below that, Bitcoin Core's 2000 floor
/// exceeds `1168 + 4 * size` and covers the fixed weight by accident. The other IO tests
/// declare 2.
const COINBASE_OUTPUT_MAX_ADDITIONAL_SIZE: u32 = 1385;

/// Consensus limit on a coinbase `scriptSig`; the spec reserves all of it, so the coinbase here
/// uses all of it.
const COINBASE_SCRIPT_SIG_LEN: usize = 100;

/// Block weight that is neither mempool transactions nor the declared coinbase outputs.
///
/// Mirrors `FIXED_BLOCK_RESERVED_WEIGHT` in `bitcoin_core_sv2`, which is private to that crate.
const FIXED_BLOCK_RESERVED_WEIGHT: u64 = 1168;

#[tokio::test]
async fn full_template_block_passes_check_block_v30x() {
    assert_full_template_block_passes_check_block(BitcoinCoreVersion::V30X).await;
}

#[tokio::test]
async fn full_template_block_passes_check_block_v31x() {
    assert_full_template_block_passes_check_block(BitcoinCoreVersion::V31X).await;
}

#[tokio::test]
async fn full_template_block_passes_check_block_v32x() {
    assert_full_template_block_passes_check_block(BitcoinCoreVersion::V32X).await;
}

/// Runs the scenario in the module docs against a live node of the given line. Everything that
/// mines happens before either runtime starts, so the tip never moves under the template.
async fn assert_full_template_block_passes_check_block(version: BitcoinCoreVersion) {
    start_tracing();

    let bitcoin_core = start_bitcoin_core(DifficultyLevel::Low, version);
    bitcoin_core.fund_wallet().expect("failed to fund wallet");
    bitcoin_core
        .fill_mempool_beyond_a_block()
        .unwrap_or_else(|e| {
            // corepc's `Debug` names only the error kind; the RPC reason is one level down.
            panic!(
                "failed to fill the mempool: {e}: {:?}",
                std::error::Error::source(&e)
            )
        });
    let socket_path = bitcoin_core.ipc_socket_path();

    let cancellation_token = CancellationToken::new();

    // The JDP starts first: its mempool mirror comes from its bootstrap template, which is what
    // the declaration below is resolved against.
    let (jd_sender, jd_receiver) = async_channel::unbounded::<JdRequest>();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let jdp_thread = spawn_jdp_thread(
        version,
        socket_path.clone(),
        jd_receiver,
        cancellation_token.clone(),
        ready_tx,
        ready_rx,
    )
    .await;

    let (tdp_in_sender, tdp_in_receiver) = async_channel::unbounded();
    let (tdp_out_sender, tdp_out_receiver) = async_channel::unbounded();
    let tdp_thread = spawn_tdp_thread(
        version,
        socket_path,
        tdp_in_receiver,
        tdp_out_sender,
        cancellation_token.clone(),
    );

    // No sigops: the output added below has none, and sigops are reserved separately.
    tdp_in_sender
        .send(TemplateDistributionOwned::CoinbaseOutputConstraints(
            CoinbaseOutputConstraintsOwned {
                coinbase_output_max_additional_size: COINBASE_OUTPUT_MAX_ADDITIONAL_SIZE,
                coinbase_output_max_additional_sigops: 0,
            },
        ))
        .await
        .expect("failed to send CoinbaseOutputConstraints");

    // The transactions arrive in Bitcoin Core's order, which the witness commitment was computed
    // over.
    let template = match recv_tdp_message(&tdp_out_receiver, Duration::from_secs(30), |msg| {
        matches!(msg, TemplateDistributionOwned::NewTemplate(_))
    })
    .await
    {
        TemplateDistributionOwned::NewTemplate(template) => template,
        _ => unreachable!("message kind already filtered"),
    };

    tdp_in_sender
        .send(TemplateDistributionOwned::RequestTransactionData(
            RequestTransactionDataOwned {
                template_id: template.template_id,
            },
        ))
        .await
        .expect("failed to send RequestTransactionData");
    let txs: Vec<Transaction> =
        match recv_tdp_message(&tdp_out_receiver, Duration::from_secs(30), |msg| {
            matches!(
                msg,
                TemplateDistributionOwned::RequestTransactionDataSuccess(message)
                    if message.template_id == template.template_id
            )
        })
        .await
        {
            TemplateDistributionOwned::RequestTransactionDataSuccess(message) => message
                .transaction_list
                .iter_bytes()
                .map(deserialize)
                .collect::<Result<_, _>>()
                .expect("template transactions should deserialize"),
            _ => unreachable!("message kind already filtered"),
        };

    let coinbase_tx = coinbase_tx_for(&template);

    // Assertion 1: the block is up against the limit. Only the header's size counts here.
    let block_weight = Block {
        header: Header {
            version: BlockVersion::from_consensus(template.version as i32),
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 0,
            bits: CompactTarget::from_consensus(0),
            nonce: 0,
        },
        txdata: std::iter::once(coinbase_tx.clone())
            .chain(txs.iter().cloned())
            .collect(),
    }
    .weight()
    .to_wu();
    assert!(
        block_weight > Weight::MAX_BLOCK.to_wu() - FIXED_BLOCK_RESERVED_WEIGHT,
        "the block weighs {block_weight}, more than {FIXED_BLOCK_RESERVED_WEIGHT} under the limit; \
         the template is not full enough for this test to prove anything"
    );

    // Assertion 2: declared as a JDC would declare it, Bitcoin Core accepts the block.
    let version_from_template = BlockVersion::from_consensus(template.version as i32);
    let wtxid_list: Vec<_> = txs.iter().map(Transaction::compute_wtxid).collect();
    let mut response = send_declare_mining_job_and_recv_response(
        &jd_sender,
        version_from_template,
        coinbase_tx.clone(),
        wtxid_list.clone(),
        vec![],
    )
    .await;
    if let JdResponse::MissingTransactions { missing_wtxids, .. } = &response {
        // The JDP's own template reserved Bitcoin Core's default 8000, the TDP's less, so the
        // TDP's can hold a few small transactions the mirror never saw.
        let missing_txs = txs
            .iter()
            .filter(|tx| missing_wtxids.contains(&tx.compute_wtxid()))
            .cloned()
            .collect();
        response = send_declare_mining_job_and_recv_response(
            &jd_sender,
            version_from_template,
            coinbase_tx,
            wtxid_list,
            missing_txs,
        )
        .await;
    }
    match response {
        // jd-server rebuilds a solved block from these, so they must be exactly what was
        // declared, in declaration order and without the coinbase.
        JdResponse::Success { txdata, .. } => assert_eq!(
            txdata, txs,
            "JdResponse::Success must carry the declared transactions"
        ),
        response => panic!(
            "Bitcoin Core must accept a block built on a full template with the declared \
             coinbase, got: {response:?}"
        ),
    }

    cancellation_token.cancel();
    join_within(jdp_thread, Duration::from_secs(10)).await;
    join_within(tdp_thread, Duration::from_secs(10)).await;
}

/// The coinbase a JDC would build on `template`, at the size the constraints declared.
///
/// Template fields are taken as they come; what a JDC adds is added at its maximum: the
/// `scriptSig` padded to 100 bytes, one all-zero output bringing the added bytes to exactly
/// `COINBASE_OUTPUT_MAX_ADDITIONAL_SIZE`. The template's own outputs follow, witness commitment
/// included; the witness holds the 32 zero bytes that commitment was computed over.
fn coinbase_tx_for(template: &NewTemplateOwned) -> Transaction {
    let mut script_sig = template.coinbase_prefix.as_bytes().to_vec();
    script_sig.resize(COINBASE_SCRIPT_SIG_LEN, 0);

    // 8-byte value, three-byte CompactSize length, script
    let script_len = COINBASE_OUTPUT_MAX_ADDITIONAL_SIZE as usize - 8 - 3;
    let declared_output = TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(vec![0; script_len]),
    };
    assert_eq!(
        declared_output.size(),
        COINBASE_OUTPUT_MAX_ADDITIONAL_SIZE as usize,
        "the declared output must serialize to the declared size"
    );

    let mut output = vec![declared_output];
    let mut required_outputs = template.coinbase_tx_outputs.as_bytes();
    for _ in 0..template.coinbase_tx_outputs_count {
        output.push(
            TxOut::consensus_decode(&mut required_outputs)
                .expect("template coinbase outputs should decode"),
        );
    }

    Transaction {
        version: TxVersion::non_standard(template.coinbase_tx_version as i32),
        lock_time: LockTime::from_consensus(template.coinbase_tx_locktime),
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(script_sig),
            sequence: Sequence::from_consensus(template.coinbase_tx_input_sequence),
            witness: Witness::from_slice(&[[0u8; 32]]),
        }],
        output,
    }
}

/// As in the TDP IO test; no job history, so retention stays on the runtime's default.
fn spawn_tdp_thread(
    version: BitcoinCoreVersion,
    socket_path: std::path::PathBuf,
    incoming_receiver: Receiver<TemplateDistributionOwned>,
    outgoing_sender: Sender<TemplateDistributionOwned>,
    cancellation_token: CancellationToken,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to create Tokio runtime");
        let local_set = tokio::task::LocalSet::new();

        local_set.block_on(&runtime, async move {
            let mut tdp = template_distribution_protocol::new(
                version,
                socket_path,
                0,
                1,
                None,
                incoming_receiver,
                outgoing_sender,
                cancellation_token,
            )
            .await
            .expect("failed to initialize BitcoinCoreSv2TDP");

            tdp.run().await;
        });
    })
}

/// As in the JDP IO test; returns once the runtime reports ready.
async fn spawn_jdp_thread(
    version: BitcoinCoreVersion,
    socket_path: std::path::PathBuf,
    incoming_receiver: Receiver<JdRequest>,
    cancellation_token: CancellationToken,
    ready_tx: tokio::sync::oneshot::Sender<()>,
    ready_rx: tokio::sync::oneshot::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    let jdp_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to create Tokio runtime");
        let local_set = tokio::task::LocalSet::new();

        local_set.block_on(&runtime, async move {
            let jdp = job_declaration_protocol::new(
                version,
                socket_path,
                incoming_receiver,
                cancellation_token,
                ready_tx,
            )
            .await
            .expect("failed to initialize BitcoinCoreSv2JDP");

            jdp.run().await;
        });
    });

    tokio::time::timeout(Duration::from_secs(30), ready_rx)
        .await
        .expect("timed out waiting for JDP readiness")
        .expect("JDP readiness channel dropped unexpectedly");

    jdp_thread
}

/// Drains the TDP's outgoing channel until `predicate` matches or `timeout` passes.
async fn recv_tdp_message<F>(
    receiver: &Receiver<TemplateDistributionOwned>,
    timeout: Duration,
    predicate: F,
) -> TemplateDistributionOwned
where
    F: Fn(&TemplateDistributionOwned) -> bool,
{
    let deadline = Instant::now() + timeout;

    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for template message");
        let remaining = deadline.saturating_duration_since(now);

        let message = tokio::time::timeout(remaining, receiver.recv())
            .await
            .expect("timed out waiting on template channel")
            .expect("template channel closed unexpectedly");

        if predicate(&message) {
            return message;
        }
    }
}

/// Declares a job to the JDP runtime and waits for its answer.
async fn send_declare_mining_job_and_recv_response(
    jd_sender: &Sender<JdRequest>,
    version: BlockVersion,
    coinbase_tx: Transaction,
    wtxid_list: Vec<stratum_apps::stratum_core::bitcoin::Wtxid>,
    missing_txs: Vec<Transaction>,
) -> JdResponse {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    jd_sender
        .send(JdRequest::DeclareMiningJob {
            version,
            coinbase_tx,
            wtxid_list,
            missing_txs,
            response_tx,
        })
        .await
        .expect("failed to send DeclareMiningJob request");

    tokio::time::timeout(Duration::from_secs(60), response_rx)
        .await
        .expect("timed out waiting for DeclareMiningJob response")
        .expect("response channel dropped")
}
