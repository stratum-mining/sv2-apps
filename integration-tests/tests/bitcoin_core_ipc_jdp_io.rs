//! End-to-end IPC integration coverage for Sv2 Job Declaration Protocol (JDP).
//!
//! Flow covered per Bitcoin Core Sv2 runtime behavior and Sv2 JDP expectations:
//! - `DeclareMiningJob` returns `MissingTransactions` when unknown wtxids are declared.
//! - `DeclareMiningJob` returns `Success` for a minimal valid declaration.
//! - `DeclareMiningJob` returns `Error(stale-chain-tip)` when the declared BIP34 height is
//!   intentionally mismatched.
//! - `DeclareMiningJob` does not retain client-supplied transactions when validation fails.
//! - `DeclareMiningJob` rejects a coinbase that does not carry exactly one input, without tearing
//!   down the IPC connection.
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
    bitcoin_core_sv2::{
        CancellationToken,
        runtime_api::{
            BitcoinCoreVersion,
            job_declaration_protocol::{
                self,
                io::{JdRequest, JdResponse},
            },
        },
    },
    stratum_core::{
        bitcoin::{
            Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, Wtxid,
            absolute::LockTime, block::Version as BlockVersion, hashes::Hash,
            transaction::Version as TxVersion,
        },
        job_declaration_sv2::{
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
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

    let cancellation_token = CancellationToken::new();
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
    // The malformed-coinbase path runs first on purpose: every scenario after it doubles as
    // proof that rejecting it did not tear down the IPC connection.
    assert_jdp_invalid_coinbase_input_scenario(&incoming_sender).await;

    let missing_wtxid = Wtxid::from_byte_array([0x42; 32]);
    assert_jdp_missing_transactions_scenario(&incoming_sender, coinbase_tx.clone(), missing_wtxid)
        .await;
    assert_jdp_success_scenario(&incoming_sender, coinbase_tx.clone()).await;
    assert_jdp_stale_chain_tip_scenario(&incoming_sender, next_height).await;
    assert_jdp_rejected_declaration_does_not_retain_txs(&incoming_sender, coinbase_tx).await;

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
        coinbase_tx,
        vec![],
        vec![],
        "jdp/success",
    )
    .await;

    match response {
        JdResponse::Success { txid_list, .. } => {
            assert!(
                txid_list.is_empty(),
                "txid_list should be empty when no non-coinbase txs were declared"
            );
        }
        response => panic!("expected Success, got: {response:?}"),
    }
}

async fn assert_jdp_stale_chain_tip_scenario(
    incoming_sender: &Sender<JdRequest>,
    next_height: u32,
) {
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
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

/// A coinbase that does not carry exactly one input must be rejected before block assembly.
///
/// A zero-input coinbase re-serializes into bytes Bitcoin Core's deserializer reads as a SegWit
/// marker, so letting it reach `checkBlock` yields a capnp remote exception, which the handler
/// answers with `internal-error` and follows by cancelling the whole IPC connection.
async fn assert_jdp_invalid_coinbase_input_scenario(incoming_sender: &Sender<JdRequest>) {
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        build_zero_input_coinbase_tx(),
        vec![],
        vec![],
        "jdp/invalid-coinbase-tx-input",
    )
    .await;

    match response {
        JdResponse::Error { error_code, .. } => {
            assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
                "expected invalid-coinbase-tx-input for a zero-input declared coinbase"
            );
        }
        response => panic!("expected Error(invalid-coinbase-tx-input), got: {response:?}"),
    }
}

/// A declaration rejected by `checkBlock` must not leave its client-supplied transactions behind
/// in the mempool mirror.
async fn assert_jdp_rejected_declaration_does_not_retain_txs(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
) {
    let invalid_tx = build_invalid_declared_tx();
    let invalid_wtxid = invalid_tx.compute_wtxid();

    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx.clone(),
        vec![invalid_wtxid],
        vec![invalid_tx],
        "jdp/rejected-declaration",
    )
    .await;

    assert!(
        matches!(response, JdResponse::Error { .. }),
        "expected Error for a declaration carrying an invalid transaction, got: {response:?}"
    );

    // Same wtxid, this time supplying no transactions: the rejected transaction must not be
    // served from the mempool mirror.
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx,
        vec![invalid_wtxid],
        vec![],
        "jdp/rejected-declaration-retry",
    )
    .await;

    match response {
        JdResponse::MissingTransactions { missing_wtxids, .. } => {
            assert_eq!(missing_wtxids, vec![invalid_wtxid]);
        }
        response => panic!(
            "expected MissingTransactions (rejected tx must not be retained), got: {response:?}"
        ),
    }
}

async fn send_declare_mining_job_and_recv_response(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
    wtxid_list: Vec<Wtxid>,
    missing_txs: Vec<Transaction>,
    path_name: &'static str,
) -> JdResponse {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    incoming_sender
        .send(JdRequest::DeclareMiningJob {
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

fn build_zero_input_coinbase_tx() -> Transaction {
    Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::new(),
        }],
    }
}

fn build_invalid_declared_tx() -> Transaction {
    Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            // deliberately not `OutPoint::null()`, which would make Bitcoin Core treat this as a
            // second coinbase instead of exercising the empty-outputs rejection
            previous_output: OutPoint {
                txid: Txid::from_byte_array([0x11; 32]),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        // no outputs, so Bitcoin Core's `checkBlock` rejects any block carrying this transaction
        output: vec![],
    }
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
