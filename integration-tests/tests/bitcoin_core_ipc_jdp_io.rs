//! End-to-end IPC integration coverage for Sv2 Job Declaration Protocol (JDP).
//!
//! Flow covered per Bitcoin Core Sv2 runtime behavior and Sv2 JDP expectations:
//! - `DeclareMiningJob` returns `MissingTransactions` when unknown wtxids are declared.
//! - `DeclareMiningJob` returns `Success` for a minimal valid declaration.
//! - `DeclareMiningJob` returns `Error(stale-chain-tip)` when Bitcoin Core rejects a coinbase built
//!   for another height, as obsolete (`bad-cb-height`) or as non-final (`bad-txns-nonfinal`).
//! - `DeclareMiningJob` returns `Error(invalid-job)` for a declaration Bitcoin Core rejects on its
//!   own merits, and does not retain its client-supplied transactions.
//! - `DeclareMiningJob` rejects a coinbase that does not carry exactly one input, without tearing
//!   down the IPC connection.
//! - `DeclareMiningJob` rejects client-supplied transactions the declaration did not ask for.
//! - `DeclareMiningJob` keeps asking for the transactions an incomplete response left out, without
//!   retaining the ones it did supply.
//! - `DeclareMiningJob` rejects a declaration that repeats a wtxid, lists more transactions than a
//!   block can hold, or weighs more than a block.
//! - bootstrap gives way to cancellation while a peer that accepted the connection never answers.
//!
//! File structure:
//! - top: version-specific `#[tokio::test]` wrappers.
//! - bottom: shared version-agnostic harness/helpers.

use async_channel::Sender;
use integration_tests_sv2::{
    start_bitcoin_core, start_tracing, template_provider::DifficultyLevel, utils::join_within,
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
            Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Weight, Witness,
            Wtxid, absolute::LockTime, block::Version as BlockVersion, hashes::Hash,
            transaction::Version as TxVersion,
        },
        job_declaration_sv2::{
            ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
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
#[ignore = "requires a Bitcoin Core 32.0 release binary; un-gate once v32.0 final is published"]
async fn jdp_io_integration_v32x() {
    assert_jdp_io_integration_for_version(BitcoinCoreVersion::V32X).await;
}

#[tokio::test]
async fn jdp_bootstrap_gives_way_to_cancellation_v30x() {
    assert_jdp_bootstrap_gives_way_to_cancellation(BitcoinCoreVersion::V30X).await;
}

#[tokio::test]
async fn jdp_bootstrap_gives_way_to_cancellation_v31x() {
    assert_jdp_bootstrap_gives_way_to_cancellation(BitcoinCoreVersion::V31X).await;
}

#[tokio::test]
#[ignore = "requires a Bitcoin Core 32.0 release binary; un-gate once v32.0 final is published"]
async fn jdp_bootstrap_gives_way_to_cancellation_v32x() {
    assert_jdp_bootstrap_gives_way_to_cancellation(BitcoinCoreVersion::V32X).await;
}

#[tokio::test]
async fn jdp_runtime_gives_way_to_cancellation_v30x() {
    assert_jdp_runtime_gives_way_to_cancellation(BitcoinCoreVersion::V30X).await;
}

#[tokio::test]
async fn jdp_runtime_gives_way_to_cancellation_v31x() {
    assert_jdp_runtime_gives_way_to_cancellation(BitcoinCoreVersion::V31X).await;
}

#[tokio::test]
#[ignore = "requires a Bitcoin Core 32.0 release binary; un-gate once v32.0 final is published"]
async fn jdp_runtime_gives_way_to_cancellation_v32x() {
    assert_jdp_runtime_gives_way_to_cancellation(BitcoinCoreVersion::V32X).await;
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
    let jdp_thread = spawn_jdp_thread(
        version,
        socket_path,
        incoming_receiver,
        cancellation_token.clone(),
        ready_tx,
        ready_rx,
    )
    .await;

    // Execute all JDP paths against the same live runtime to keep this test fully end-to-end.
    // The malformed-coinbase path runs first on purpose: every scenario after it doubles as
    // proof that rejecting it did not tear down the IPC connection.
    assert_jdp_invalid_coinbase_input_scenario(&incoming_sender).await;

    let missing_wtxid = Wtxid::from_byte_array([0x42; 32]);
    assert_jdp_missing_transactions_scenario(&incoming_sender, coinbase_tx.clone(), missing_wtxid)
        .await;
    assert_jdp_success_scenario(&incoming_sender, coinbase_tx.clone()).await;
    assert_jdp_stale_chain_tip_scenario(&incoming_sender, next_height).await;
    assert_jdp_rejected_declaration_does_not_retain_txs(&incoming_sender, coinbase_tx.clone())
        .await;
    assert_jdp_supplied_txs_must_match_declaration(
        &incoming_sender,
        coinbase_tx.clone(),
        next_height,
    )
    .await;
    assert_jdp_incomplete_missing_txs_response(&incoming_sender, coinbase_tx.clone()).await;
    assert_jdp_declaration_must_fit_in_a_block(&incoming_sender, coinbase_tx).await;

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

/// A coinbase built for another height is rejected by Bitcoin Core in one of two ways, and the
/// runtime answers `stale-chain-tip` to both straight from Core's reason, with no local height
/// bookkeeping and no template refresh.
///
/// Templates give the coinbase a locktime of height minus one and a sequence that enforces it, and
/// Core checks finality before the coinbase height, so a coinbase from ahead of the tip is a
/// non-final transaction (`bad-txns-nonfinal`); one with a final locktime fails the height check
/// instead (`bad-cb-height`).
async fn assert_jdp_stale_chain_tip_scenario(
    incoming_sender: &Sender<JdRequest>,
    next_height: u32,
) {
    let height_ahead = next_height.saturating_add(10_000);

    let mut non_final_coinbase_tx = build_valid_coinbase_tx(height_ahead);
    non_final_coinbase_tx.lock_time =
        LockTime::from_height(height_ahead - 1).expect("height must fit a locktime");
    non_final_coinbase_tx.input[0].sequence = Sequence::from_consensus(0xffff_fffe);

    let scenarios = [
        (
            "jdp/stale-chain-tip/bad-cb-height",
            build_valid_coinbase_tx(height_ahead),
        ),
        (
            "jdp/stale-chain-tip/bad-txns-nonfinal",
            non_final_coinbase_tx,
        ),
    ];

    for (path_name, coinbase_tx) in scenarios {
        let response = send_declare_mining_job_and_recv_response(
            incoming_sender,
            coinbase_tx,
            vec![],
            vec![],
            path_name,
        )
        .await;

        match response {
            JdResponse::Error { error_code, .. } => assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP,
                "expected stale-chain-tip ({path_name})"
            ),
            response => {
                panic!("expected Error(stale-chain-tip) ({path_name}), got: {response:?}")
            }
        }
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

/// A declaration rejected by `checkBlock` on its own merits is answered `invalid-job`, and must
/// not leave its client-supplied transactions behind in the mempool mirror.
async fn assert_jdp_rejected_declaration_does_not_retain_txs(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
) {
    let invalid_tx = build_invalid_declared_tx(0x11);
    let invalid_wtxid = invalid_tx.compute_wtxid();

    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx.clone(),
        vec![invalid_wtxid],
        vec![invalid_tx],
        "jdp/rejected-declaration",
    )
    .await;

    match response {
        JdResponse::Error { error_code, .. } => assert_eq!(
            error_code, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
            "expected invalid-job for a declaration carrying an invalid transaction"
        ),
        response => panic!("expected Error(invalid-job), got: {response:?}"),
    }

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

/// Client-supplied transactions must belong to the declaration that asked for them.
///
/// Transactions the job never declared, repeats of a transaction declared once, and coinbases are
/// all rejected before block assembly, so a client cannot attach arbitrary payload to a retry.
async fn assert_jdp_supplied_txs_must_match_declaration(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
    next_height: u32,
) {
    let declared_tx = build_invalid_declared_tx(0x21);
    let declared_wtxid = declared_tx.compute_wtxid();

    let scenarios = [
        (
            "jdp/unsolicited-supplied-tx",
            vec![declared_tx.clone(), build_invalid_declared_tx(0x22)],
        ),
        (
            "jdp/duplicate-supplied-tx",
            vec![declared_tx.clone(), declared_tx],
        ),
        (
            "jdp/coinbase-supplied-tx",
            vec![build_valid_coinbase_tx(next_height)],
        ),
    ];

    for (path_name, missing_txs) in scenarios {
        let response = send_declare_mining_job_and_recv_response(
            incoming_sender,
            coinbase_tx.clone(),
            vec![declared_wtxid],
            missing_txs,
            path_name,
        )
        .await;

        match response {
            JdResponse::Error { error_code, .. } => assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
                "expected invalid-job ({path_name})"
            ),
            response => panic!("expected Error(invalid-job) ({path_name}), got: {response:?}"),
        }
    }
}

/// An answer that supplies only part of the declared transactions leaves the declaration pending.
///
/// The transactions it did supply were never validated, so they must not be served from the
/// mempool mirror on a later declaration either.
async fn assert_jdp_incomplete_missing_txs_response(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
) {
    let supplied_tx = build_invalid_declared_tx(0x31);
    let supplied_wtxid = supplied_tx.compute_wtxid();
    let withheld_wtxid = build_invalid_declared_tx(0x32).compute_wtxid();

    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx.clone(),
        vec![supplied_wtxid, withheld_wtxid],
        vec![supplied_tx],
        "jdp/incomplete-missing-txs",
    )
    .await;

    match response {
        JdResponse::MissingTransactions { missing_wtxids, .. } => {
            assert_eq!(missing_wtxids, vec![withheld_wtxid]);
        }
        response => panic!(
            "expected MissingTransactions for a partially answered declaration, got: {response:?}"
        ),
    }

    // Declare only the transaction supplied above, this time supplying nothing: the incomplete
    // response must not have left it behind in the mempool mirror.
    let response = send_declare_mining_job_and_recv_response(
        incoming_sender,
        coinbase_tx,
        vec![supplied_wtxid],
        vec![],
        "jdp/incomplete-missing-txs-retry",
    )
    .await;

    match response {
        JdResponse::MissingTransactions { missing_wtxids, .. } => {
            assert_eq!(missing_wtxids, vec![supplied_wtxid]);
        }
        response => panic!(
            "expected MissingTransactions (unvalidated tx must not be retained), got: {response:?}"
        ),
    }
}

/// A declaration must be something that could be mined, before it is looked up or expanded.
///
/// A repeated wtxid would otherwise expand one cached transaction into as many copies as the list
/// names it, and neither a list longer than a block can hold nor a set of transactions heavier
/// than a block can ever validate.
async fn assert_jdp_declaration_must_fit_in_a_block(
    incoming_sender: &Sender<JdRequest>,
    coinbase_tx: Transaction,
) {
    let repeated_wtxid = build_invalid_declared_tx(0x41).compute_wtxid();

    // One more than the smallest transactions that could fit in a block.
    let too_many_wtxids: Vec<Wtxid> = (0..=Weight::MAX_BLOCK.to_wu()
        / Weight::MIN_TRANSACTION.to_wu())
        .map(|index| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&index.to_le_bytes());
            Wtxid::from_byte_array(bytes)
        })
        .collect();

    // Two thirds of a block each, so declaring both weighs more than a block can hold.
    let heavy_txs = vec![build_heavy_declared_tx(0x51), build_heavy_declared_tx(0x52)];
    let heavy_wtxids: Vec<Wtxid> = heavy_txs.iter().map(|tx| tx.compute_wtxid()).collect();

    let scenarios = [
        (
            "jdp/repeated-declared-wtxid",
            vec![repeated_wtxid, repeated_wtxid],
            vec![],
        ),
        ("jdp/too-many-declared-txs", too_many_wtxids, vec![]),
        ("jdp/declaration-too-heavy", heavy_wtxids, heavy_txs),
    ];

    for (path_name, wtxid_list, missing_txs) in scenarios {
        let response = send_declare_mining_job_and_recv_response(
            incoming_sender,
            coinbase_tx.clone(),
            wtxid_list,
            missing_txs,
            path_name,
        )
        .await;

        match response {
            JdResponse::Error { error_code, .. } => assert_eq!(
                error_code, ERROR_CODE_DECLARE_MINING_JOB_INVALID_JOB,
                "expected invalid-job ({path_name})"
            ),
            response => panic!("expected Error(invalid-job) ({path_name}), got: {response:?}"),
        }
    }
}

/// A peer that accepts the IPC connection and never answers must not hold bootstrap past
/// cancellation.
///
/// No Bitcoin Core is involved: a bare Unix listener stands in for one that stalled, which is all
/// bootstrap needs to be kept waiting.
/// Runs the JDP runtime on a dedicated thread + LocalSet to match production usage, returning
/// once it has bootstrapped and can serve requests.
async fn spawn_jdp_thread(
    version: BitcoinCoreVersion,
    socket_path: std::path::PathBuf,
    incoming_receiver: async_channel::Receiver<JdRequest>,
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

async fn assert_jdp_runtime_gives_way_to_cancellation(version: BitcoinCoreVersion) {
    start_tracing();

    let bitcoin_core = start_bitcoin_core(DifficultyLevel::Low, version);
    let next_height = bitcoin_core
        .get_blockchain_info()
        .expect("failed to get blockchain info")
        .blocks
        + 1;
    let next_height = u32::try_from(next_height).expect("next height should fit in u32");

    let (incoming_sender, incoming_receiver) = async_channel::unbounded::<JdRequest>();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let cancellation_token = CancellationToken::new();
    let jdp_thread = spawn_jdp_thread(
        version,
        bitcoin_core.ipc_socket_path(),
        incoming_receiver,
        cancellation_token.clone(),
        ready_tx,
        ready_rx,
    )
    .await;

    // From here on Bitcoin Core answers nothing: the monitor's `waitNext` stays pending, and so
    // does the `checkBlock` behind the declaration sent next.
    bitcoin_core.pause();
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    incoming_sender
        .send(JdRequest::DeclareMiningJob {
            version: BlockVersion::from_consensus(0x2000_0000),
            coinbase_tx: build_valid_coinbase_tx(next_height),
            wtxid_list: vec![],
            missing_txs: vec![],
            response_tx,
        })
        .await
        .expect("failed to send DeclareMiningJob request");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !jdp_thread.is_finished(),
        "the runtime must outlive a node that merely stops answering"
    );

    cancellation_token.cancel();
    join_within(jdp_thread, Duration::from_secs(10)).await;
    bitcoin_core.resume();
}

async fn assert_jdp_bootstrap_gives_way_to_cancellation(version: BitcoinCoreVersion) {
    let socket_path = std::env::temp_dir().join(format!(
        "jdp-stalled-peer-{}-{version:?}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&socket_path);
    let listener =
        tokio::net::UnixListener::bind(&socket_path).expect("failed to bind the stalled peer");

    // The runtime spawns its RPC system with `spawn_local`, so it needs a LocalSet, as in
    // production.
    tokio::task::LocalSet::new()
        .run_until(async {
            let stalled_peer = tokio::task::spawn_local(async move {
                let (_connection, _) = listener.accept().await.expect("failed to accept");
                std::future::pending::<()>().await;
            });

            let cancellation_token = CancellationToken::new();
            let canceller = cancellation_token.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                canceller.cancel();
            });

            let (_incoming_sender, incoming_receiver) = async_channel::unbounded::<JdRequest>();
            let (ready_tx, _ready_rx) = tokio::sync::oneshot::channel::<()>();

            let bootstrap = tokio::time::timeout(
                Duration::from_secs(10),
                job_declaration_protocol::new(
                    version,
                    &socket_path,
                    incoming_receiver,
                    cancellation_token,
                    ready_tx,
                ),
            )
            .await
            .expect("bootstrap must give way to cancellation rather than wait on a silent peer");

            assert!(
                bootstrap.is_err(),
                "a cancelled bootstrap must not produce a runtime"
            );
            stalled_peer.abort();
        })
        .await;

    let _ = std::fs::remove_file(&socket_path);
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

fn build_invalid_declared_tx(prevout_txid_byte: u8) -> Transaction {
    Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            // deliberately not `OutPoint::null()`, which would make Bitcoin Core treat this as a
            // second coinbase instead of exercising the empty-outputs rejection
            previous_output: OutPoint {
                txid: Txid::from_byte_array([prevout_txid_byte; 32]),
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

fn build_heavy_declared_tx(prevout_txid_byte: u8) -> Transaction {
    let mut tx = build_invalid_declared_tx(prevout_txid_byte);
    // Weight is four times the size for a transaction carrying no witness.
    tx.input[0].script_sig = ScriptBuf::from_bytes(vec![
        prevout_txid_byte;
        Weight::MAX_BLOCK.to_wu() as usize / 6
    ]);
    tx
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
