//! End-to-end IPC integration coverage for Sv2 Job Declaration Protocol (JDP).
//!
//! Flow covered per Bitcoin Core Sv2 runtime behavior and Sv2 JDP expectations:
//! - `DeclareMiningJob` returns `MissingTransactions` when unknown wtxids are declared.
//! - `DeclareMiningJob` returns `Success` for a minimal valid declaration.
//! - `DeclareMiningJob` returns `Error(stale-chain-tip)` when tip drift is detected during a
//!   missing-transactions retry flow.
//! - `DeclareMiningJob` returns `Error(stale-chain-tip)` for stale-at-arrival coinbase jobs
//!   rejected by `checkBlock` with `bad-cb-height`.
//!
//! File structure:
//! - top: version-specific `#[tokio::test]` wrappers.
//! - bottom: shared version-agnostic harness/helpers.

use async_channel::Sender;
use integration_tests_sv2::{
    start_bitcoin_core, start_tracing,
    template_provider::{BitcoinCore, DifficultyLevel},
};
use std::time::Duration;
use stratum_apps::{
    bitcoin_core_sv2::{
        runtime_api::{
            job_declaration_protocol::{
                self,
                io::{JdRequest, JdResponse},
            },
            BitcoinCoreVersion,
        },
        CancellationToken,
    },
    stratum_core::{
        bitcoin::{
            absolute::LockTime, block::Version as BlockVersion, hashes::Hash,
            transaction::Version as TxVersion, Amount, OutPoint, ScriptBuf, Sequence, Transaction,
            TxIn, TxOut, Witness, Wtxid,
        },
        job_declaration_sv2::ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
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
    let missing_wtxid = Wtxid::from_byte_array([0x42; 32]);
    assert_jdp_missing_transactions_scenario(&incoming_sender, coinbase_tx.clone(), missing_wtxid)
        .await;
    assert_jdp_success_scenario(&incoming_sender, coinbase_tx).await;
    assert_jdp_stale_chain_tip_scenario(&incoming_sender, &bitcoin_core, next_height).await;
    assert_jdp_stale_chain_tip_at_arrival_scenario(&incoming_sender, &bitcoin_core).await;

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
        JdResponse::Success { txdata, .. } => {
            assert!(
                txdata.is_empty(),
                "txdata should be empty when no non-coinbase txs were declared"
            );
        }
        response => panic!("expected Success, got: {response:?}"),
    }
}

async fn assert_jdp_stale_chain_tip_scenario(
    incoming_sender: &Sender<JdRequest>,
    bitcoin_core: &BitcoinCore,
    next_height: u32,
) {
    // This coinbase is valid for the tip height observed before the missing-txs round-trip.
    let coinbase_tx = build_valid_coinbase_tx(next_height);

    let missing_tx = Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![0x01]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: ScriptBuf::new(),
        }],
    };
    let missing_wtxid = missing_tx.compute_wtxid();

    // Round 1: declare unknown wtxid so JDP asks for missing transactions.
    let first_response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx.clone(),
        vec![missing_wtxid],
        vec![],
        "jdp/stale-chain-tip/round1",
    )
    .await;
    match first_response {
        JdResponse::MissingTransactions { missing_wtxids, .. } => {
            assert_eq!(missing_wtxids, vec![missing_wtxid]);
        }
        response => panic!("expected MissingTransactions, got: {response:?}"),
    }

    // Move chain tip between the MissingTransactions response and retry.
    bitcoin_core.generate_blocks(1);

    // Round 2: provide the missing tx and expect stale-chain-tip due to tip drift.
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx,
        vec![missing_wtxid],
        vec![missing_tx],
        "jdp/stale-chain-tip/round2",
    )
    .await;

    match response {
        JdResponse::Error { error_code, .. } => {
            assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
                "expected stale-chain-tip after tip moved between missing-txs round-trip"
            );
        }
        response => panic!("expected Error(stale-chain-tip), got: {response:?}"),
    }
}

async fn assert_jdp_stale_chain_tip_at_arrival_scenario(
    incoming_sender: &Sender<JdRequest>,
    bitcoin_core: &BitcoinCore,
) {
    // Build a coinbase that is valid for the current tip, then advance the tip so the same
    // declaration becomes stale-at-arrival.
    let current_height = bitcoin_core
        .get_blockchain_info()
        .expect("failed to get blockchain info")
        .blocks;
    let current_height = u32::try_from(current_height).expect("current height should fit in u32");
    let coinbase_tx =
        build_valid_coinbase_tx(current_height.checked_add(1).expect("height overflow"));

    bitcoin_core.generate_blocks(1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let response = send_declare_mining_job_and_recv_response(
            incoming_sender,
            coinbase_tx.clone(),
            vec![],
            vec![],
            "jdp/stale-chain-tip/at-arrival",
        )
        .await;

        match response {
            JdResponse::Error { error_code, .. } => {
                assert_eq!(
                    error_code, ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
                    "expected stale-chain-tip for stale-at-arrival coinbase"
                );
                return;
            }
            JdResponse::Success { .. } => {
                if tokio::time::Instant::now() >= deadline {
                    panic!(
                        "timed out waiting for stale-at-arrival classification after tip update"
                    );
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            response => panic!(
                "expected Error(stale-chain-tip) or transient Success while tip update propagates, got: {response:?}"
            ),
        }
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
