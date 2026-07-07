//! End-to-end IPC integration coverage for Sv2 Job Declaration Protocol (JDP).
//!
//! Flow covered per Bitcoin Core Sv2 runtime behavior and Sv2 JDP expectations:
//! - `DeclareMiningJob` returns `MissingTransactions` when unknown wtxids are declared.
//! - `DeclareMiningJob` returns `Success` for a minimal valid declaration.
//! - `DeclareMiningJob` returns `Error(invalid-job)` when the declared coinbase pays more
//!   than the block subsidy.
//! - `DeclareMiningJob` returns `Error(stale-chain-tip)` when the declared BIP34 height is
//!   intentionally mismatched.
//! - A transaction provided once via missing-transactions is remembered, so a later
//!   declaration of the same transaction (by another downstream) succeeds without a
//!   missing-transactions round.
//!
//! File structure:
//! - top: version-specific `#[tokio::test]` wrappers.
//! - bottom: shared version-agnostic harness/helpers.

use async_channel::Sender;
use integration_tests_sv2::{
    start_bitcoin_core, start_tracing, template_provider::DifficultyLevel,
};
use std::time::Duration;
use stratum_apps::{
    bitcoin_core_sv2::common::{
        job_declaration_protocol::{
            self,
            io::{JdRequest, JdResponse},
            CancellationToken as JdpCancellationToken,
        },
        BitcoinCoreVersion,
    },
    stratum_core::{
        bitcoin::{
            absolute::LockTime, block::Version as BlockVersion, hashes::Hash,
            transaction::Version as TxVersion, Amount, OutPoint, ScriptBuf, Sequence, Transaction,
            TxIn, TxOut, Witness, Wtxid,
        },
        job_declaration_sv2::{
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
            ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
        },
    },
};

#[tokio::test]
async fn jdp_io_integration_v30x() {
    assert_jdp_io_integration_for_version(BitcoinCoreVersion::V30X).await;
}

#[tokio::test]
async fn jdp_io_integration_v31x() {
    assert_jdp_io_integration_for_version(BitcoinCoreVersion::V31X).await;
}

#[tokio::test]
async fn jdp_io_integration_v32x() {
    assert_jdp_io_integration_for_version(BitcoinCoreVersion::V32X).await;
}

async fn assert_jdp_io_integration_for_version(version: BitcoinCoreVersion) {
    start_tracing();

    // Start a real Bitcoin Core node for the selected major line.
    let bitcoin_core = start_bitcoin_core(DifficultyLevel::Low, version);
    let socket_path = bitcoin_core.ipc_socket_path();

    // Build a minimally valid coinbase for the *next* height.
    let next_height = bitcoin_core
        .get_blockchain_info()
        .expect("failed to get blockchain info")
        .blocks
        + 1;
    let next_height = u32::try_from(next_height).expect("next height should fit in u32");

    let coinbase_tx = build_valid_coinbase_tx(next_height);

    // `incoming_sender` is used by this test, while `incoming_receiver` is consumed by JDP.
    let (incoming_sender, incoming_receiver) = async_channel::unbounded::<JdRequest>();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    let cancellation_token = JdpCancellationToken::new();
    let cancellation_token_clone = cancellation_token.clone();
    let socket_path_clone = socket_path.clone();

    // Run the JDP runtime on a dedicated thread + LocalSet to match production usage.
    let jdp_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to create Tokio runtime");
        let local_set = tokio::task::LocalSet::new();

        local_set.block_on(&runtime, async move {
            let jdp = job_declaration_protocol::new(
                version,
                socket_path_clone,
                incoming_receiver,
                cancellation_token_clone,
                ready_tx,
            )
            .await
            .expect("failed to initialize BitcoinCoreSv2JDP");

            jdp.run().await;
        });
    });

    // Wait until the JDP runtime has fully bootstrapped and can serve requests.
    tokio::time::timeout(Duration::from_secs(30), ready_rx)
        .await
        .expect("timed out waiting for JDP readiness")
        .expect("JDP readiness channel dropped unexpectedly");

    // Execute all JDP paths against the same live runtime to keep this test fully end-to-end.
    let missing_wtxid = Wtxid::from_byte_array([0x42; 32]);
    assert_jdp_missing_transactions_scenario(&incoming_sender, coinbase_tx.clone(), missing_wtxid)
        .await;
    assert_jdp_success_scenario(&incoming_sender, coinbase_tx).await;
    assert_jdp_overpaying_coinbase_scenario(&incoming_sender, next_height).await;
    assert_jdp_stale_chain_tip_scenario(&incoming_sender, next_height).await;
    assert_jdp_provided_tx_remembered_scenario(&incoming_sender, &bitcoin_core).await;

    cancellation_token.cancel();
    jdp_thread
        .join()
        .expect("BitcoinCoreSv2JDP thread join should succeed");
}

async fn assert_jdp_missing_transactions_scenario(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
    missing_wtxid: Wtxid,
) {
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        0,
        0,
        coinbase_tx,
        vec![missing_wtxid],
        vec![],
        "jdp/missing-transactions",
    )
    .await;

    match response {
        JdResponse::MissingTransactions { missing_wtxids, .. } => {
            assert_eq!(missing_wtxids, vec![missing_wtxid]);
        }
        response => panic!("expected MissingTransactions, got: {response:?}"),
    }
}

async fn assert_jdp_success_scenario(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
) {
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        0,
        0,
        coinbase_tx,
        vec![],
        vec![],
        "jdp/success",
    )
    .await;

    match response {
        JdResponse::Success { merkle_path, .. } => {
            assert!(
                merkle_path.is_empty(),
                "merkle path should be empty when no non-coinbase txs were declared"
            );
        }
        response => panic!("expected Success, got: {response:?}"),
    }
}

async fn assert_jdp_overpaying_coinbase_scenario(
    incoming_sender: &Sender<JdRequest>,
    next_height: u32,
) {
    // Regtest initial block subsidy is 50 BTC; with an empty transaction list there are no
    // fees, so paying one extra satoshi must fail contextual coinbase validation
    // (bad-cb-amount).
    let mut coinbase_tx = build_valid_coinbase_tx(next_height);
    coinbase_tx.output[0].value = Amount::from_sat(50 * 100_000_000 + 1);

    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        0,
        0,
        coinbase_tx,
        vec![],
        vec![],
        "jdp/overpaying-coinbase",
    )
    .await;

    match response {
        JdResponse::Error { error_code, .. } => {
            assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
                "expected invalid-job error for overpaying coinbase"
            );
        }
        response => panic!("expected Error(invalid-job), got: {response:?}"),
    }
}

async fn assert_jdp_stale_chain_tip_scenario(
    incoming_sender: &Sender<JdRequest>,
    next_height: u32,
) {
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        0,
        0,
        build_valid_coinbase_tx(next_height.saturating_add(10_000)),
        vec![],
        vec![],
        "jdp/stale-chain-tip",
    )
    .await;

    match response {
        JdResponse::Error { error_code, .. } => {
            assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
                "expected stale-chain-tip error for intentionally mismatched BIP34 height"
            );
        }
        response => panic!("expected Error(stale-chain-tip), got: {response:?}"),
    }
}

/// A transaction provided via the missing-transactions flow must be remembered, so a later
/// declaration of the same transaction by a *different* downstream succeeds without another
/// missing-transactions round (served by the provided-transaction cache on v32.x and by the
/// mempool mirror on v30.x/v31.x).
async fn assert_jdp_provided_tx_remembered_scenario(
    incoming_sender: &Sender<JdRequest>,
    bitcoin_core: &integration_tests_sv2::template_provider::BitcoinCore,
) {
    // Fund with legacy outputs and build a witness-free transaction that is NOT in the
    // node's mempool, so the block needs no witness commitment in its minimal coinbase.
    let funded_address = bitcoin_core
        .fund_wallet_legacy()
        .unwrap_or_else(|e| panic!("failed to fund wallet with legacy outputs: {e}"));
    let tx_bytes = bitcoin_core
        .create_unbroadcast_legacy_transaction(&funded_address)
        .unwrap_or_else(|e| panic!("failed to create unbroadcast transaction: {e}"));
    let tx: Transaction = stratum_apps::stratum_core::bitcoin::consensus::deserialize(&tx_bytes)
        .expect("failed to decode unbroadcast transaction");
    let wtxid = tx.compute_wtxid();

    let next_height = u32::try_from(
        bitcoin_core
            .get_blockchain_info()
            .expect("failed to get blockchain info")
            .blocks
            + 1,
    )
    .expect("next height should fit in u32");

    // Wait until the backend's chain context reflects the newly mined blocks (the
    // mirror-based backends refresh asynchronously).
    let mut caught_up = false;
    for _ in 0..30 {
        let response = send_declare_mining_job_and_recv_response(
            incoming_sender,
            0,
            1,
            build_valid_coinbase_tx(next_height),
            vec![],
            vec![],
            "jdp/provided-tx-catchup",
        )
        .await;
        if matches!(response, JdResponse::Success { .. }) {
            caught_up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        caught_up,
        "backend did not catch up to the funded chain tip"
    );

    // Round 1: the transaction is unknown to the node, so it must be requested.
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        0,
        2,
        build_valid_coinbase_tx(next_height),
        vec![wtxid],
        vec![],
        "jdp/provided-tx-round-1",
    )
    .await;
    match response {
        JdResponse::MissingTransactions { missing_wtxids, .. } => {
            assert_eq!(missing_wtxids, vec![wtxid]);
        }
        response => panic!("expected MissingTransactions, got: {response:?}"),
    }

    // Round 2: provide the transaction; the declaration must now validate.
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        0,
        2,
        build_valid_coinbase_tx(next_height),
        vec![wtxid],
        vec![tx],
        "jdp/provided-tx-round-2",
    )
    .await;
    assert!(
        matches!(response, JdResponse::Success { .. }),
        "expected Success after providing the missing transaction, got: {response:?}"
    );

    // Round 3: a different downstream declares the same transaction WITHOUT providing it;
    // the remembered copy must be used instead of another missing-transactions round.
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        1,
        3,
        build_valid_coinbase_tx(next_height),
        vec![wtxid],
        vec![],
        "jdp/provided-tx-round-3",
    )
    .await;
    assert!(
        matches!(response, JdResponse::Success { .. }),
        "expected Success from the remembered transaction, got: {response:?}"
    );
}

#[allow(clippy::too_many_arguments)]
async fn send_declare_mining_job_and_recv_response(
    incoming_sender: &Sender<JdRequest>,
    downstream_id: usize,
    request_id: u32,
    coinbase_tx: Transaction,
    wtxid_list: Vec<Wtxid>,
    missing_txs: Vec<Transaction>,
    path_name: &'static str,
) -> JdResponse {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    incoming_sender
        .send(JdRequest::DeclareMiningJob {
            downstream_id,
            request_id,
            // Use a fixed, valid block version across scenarios so assertions focus on IO paths.
            version: BlockVersion::from_consensus(0x2000_0000),
            coinbase_tx,
            wtxid_list,
            missing_txs,
            response_tx,
        })
        .await
        .unwrap_or_else(|_| panic!("failed to send DeclareMiningJob request ({path_name})"));

    tokio::time::timeout(Duration::from_secs(20), response_rx)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for response ({path_name})"))
        .unwrap_or_else(|_| panic!("response channel dropped ({path_name})"))
}

fn coinbase_script_sig_for_height(height: u32) -> ScriptBuf {
    // Encode the height as a minimally pushed little-endian integer (BIP34 style).
    let mut encoded_height = Vec::new();
    let mut value = height;

    while value > 0 {
        encoded_height.push((value & 0xff) as u8);
        value >>= 8;
    }

    if encoded_height.last().is_some_and(|byte| byte & 0x80 != 0) {
        encoded_height.push(0x00);
    }

    let mut script = Vec::with_capacity(1 + encoded_height.len());
    script.push(encoded_height.len() as u8);
    script.extend_from_slice(&encoded_height);
    ScriptBuf::from_bytes(script)
}

fn build_valid_coinbase_tx(next_height: u32) -> Transaction {
    Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: coinbase_script_sig_for_height(next_height),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::new(),
        }],
    }
}
