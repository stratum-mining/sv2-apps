//! End-to-end IPC integration coverage for Sv2 Template Distribution Protocol (TDP).
//!
//! Flow covered per Sv2 TDP expectations:
//! - bootstrap after `CoinbaseOutputConstraints` emits `NewTemplate` and `SetNewPrevHash`.
//! - `RequestTransactionData` succeeds for the current template id.
//! - `RequestTransactionData` returns `template-id-not-found` for an unknown id.
//! - after a chain-tip update, an old template id eventually returns `stale-template-id`.
//!
//! File structure:
//! - top: version-specific `#[tokio::test]` wrappers.
//! - bottom: shared version-agnostic harness/helpers.

use async_channel::{Receiver, Sender};
use integration_tests_sv2::{
    start_bitcoin_core, start_tracing,
    template_provider::{BitcoinCore, DifficultyLevel},
};
use std::time::{Duration, Instant};
use stratum_apps::{
    bitcoin_core_sv2::common::{
        template_distribution_protocol::{self, CancellationToken as TdpCancellationToken},
        BitcoinCoreVersion,
    },
    stratum_core::{
        parsers_sv2::TemplateDistribution,
        template_distribution_sv2::{
            CoinbaseOutputConstraints, RequestTransactionData, RequestTransactionDataSuccess,
            ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID,
            ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND,
        },
    },
};

#[tokio::test]
async fn tdp_io_integration_v30x() {
    assert_tdp_io_integration(BitcoinCoreVersion::V30X).await;
}

#[tokio::test]
async fn tdp_io_integration_v31x() {
    assert_tdp_io_integration(BitcoinCoreVersion::V31X).await;
}

#[tokio::test]
async fn tdp_io_integration_v32x() {
    assert_tdp_io_integration(BitcoinCoreVersion::V32X).await;
}

async fn assert_tdp_io_integration(version: BitcoinCoreVersion) {
    start_tracing();

    // Start a real Bitcoin Core node for the selected runtime line.
    let bitcoin_core = start_bitcoin_core(DifficultyLevel::Low, version);
    let socket_path = bitcoin_core.ipc_socket_path();

    // Incoming channel feeds TDP requests; outgoing channel receives TDP responses/events.
    let (incoming_sender, incoming_receiver) = async_channel::unbounded();
    let (outgoing_sender, outgoing_receiver) = async_channel::unbounded();

    let cancellation_token = TdpCancellationToken::new();
    let cancellation_token_clone = cancellation_token.clone();
    let socket_path_clone = socket_path.clone();

    // Run TDP on a dedicated thread + LocalSet so we exercise the same async model as runtime.
    let tdp_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to create Tokio runtime");
        let local_set = tokio::task::LocalSet::new();

        local_set.block_on(&runtime, async move {
            let mut tdp = template_distribution_protocol::new(
                version,
                socket_path_clone,
                0,
                1,
                incoming_receiver,
                outgoing_sender,
                cancellation_token_clone,
            )
            .await
            .expect("failed to initialize BitcoinCoreSv2TDP");

            tdp.run().await;
        });
    });

    // Drive scenarios in protocol order: bootstrap, happy path, not-found, then stale path.
    let template_id = bootstrap_tdp_and_get_template_id(&incoming_sender, &outgoing_receiver).await;
    assert_tdp_request_tx_data_success(&incoming_sender, &outgoing_receiver, template_id).await;
    assert_tdp_request_tx_data_not_found(&incoming_sender, &outgoing_receiver).await;
    assert_tdp_old_template_eventually_stale(
        &bitcoin_core,
        &incoming_sender,
        &outgoing_receiver,
        template_id,
    )
    .await;

    cancellation_token.cancel();
    tdp_thread
        .join()
        .expect("BitcoinCoreSv2TDP thread join should succeed");
}

async fn bootstrap_tdp_and_get_template_id(
    incoming_sender: &Sender<TemplateDistribution<'static>>,
    outgoing_receiver: &Receiver<TemplateDistribution<'static>>,
) -> u64 {
    // TDP requires CoinbaseOutputConstraints first; this triggers initial template publication.
    incoming_sender
        .send(TemplateDistribution::CoinbaseOutputConstraints(
            CoinbaseOutputConstraints {
                coinbase_output_max_additional_size: 2,
                coinbase_output_max_additional_sigops: 2,
            },
        ))
        .await
        .expect("failed to send CoinbaseOutputConstraints");

    let new_template = recv_tdp_message(outgoing_receiver, Duration::from_secs(20), |msg| {
        matches!(msg, TemplateDistribution::NewTemplate(_))
    })
    .await;
    let new_template = match new_template {
        TemplateDistribution::NewTemplate(message) => message,
        _ => unreachable!("message kind already filtered"),
    };

    let set_new_prev_hash = recv_tdp_message(outgoing_receiver, Duration::from_secs(20), |msg| {
        matches!(msg, TemplateDistribution::SetNewPrevHash(_))
    })
    .await;
    let set_new_prev_hash = match set_new_prev_hash {
        TemplateDistribution::SetNewPrevHash(message) => message,
        _ => unreachable!("message kind already filtered"),
    };

    assert_eq!(set_new_prev_hash.template_id, new_template.template_id);
    new_template.template_id
}

async fn assert_tdp_request_tx_data_success(
    incoming_sender: &Sender<TemplateDistribution<'static>>,
    outgoing_receiver: &Receiver<TemplateDistribution<'static>>,
    template_id: u64,
) {
    let response = request_tdp_tx_data_and_recv_response_for_template_id(
        incoming_sender,
        outgoing_receiver,
        template_id,
        Duration::from_secs(20),
    )
    .await;

    let request_tx_data_success: RequestTransactionDataSuccess<'static> = match response {
        TemplateDistribution::RequestTransactionDataSuccess(message) => message,
        _ => unreachable!("message kind already filtered"),
    };

    assert_eq!(
        request_tx_data_success.template_id, template_id,
        "RequestTransactionDataSuccess must reference the requested template",
    );
}

async fn assert_tdp_request_tx_data_not_found(
    incoming_sender: &Sender<TemplateDistribution<'static>>,
    outgoing_receiver: &Receiver<TemplateDistribution<'static>>,
) {
    let not_found_response = request_tdp_tx_data_and_recv_response_for_template_id(
        incoming_sender,
        outgoing_receiver,
        u64::MAX,
        Duration::from_secs(20),
    )
    .await;

    match not_found_response {
        TemplateDistribution::RequestTransactionDataError(message) => {
            assert_eq!(
                message.error_code.as_utf8_or_hex(),
                ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND,
                "unknown template id must return template-id-not-found",
            );
        }
        response => panic!("expected RequestTransactionDataError, got: {response:?}"),
    }
}

async fn assert_tdp_old_template_eventually_stale(
    bitcoin_core: &BitcoinCore,
    incoming_sender: &Sender<TemplateDistribution<'static>>,
    outgoing_receiver: &Receiver<TemplateDistribution<'static>>,
    old_template_id: u64,
) {
    // Force a tip change so the previously active template becomes non-current.
    bitcoin_core.generate_blocks(1);

    let next_set_new_prev_hash =
        recv_tdp_message(outgoing_receiver, Duration::from_secs(20), |msg| {
            matches!(
                msg,
                TemplateDistribution::SetNewPrevHash(message)
                    if message.template_id != old_template_id
            )
        })
        .await;
    match next_set_new_prev_hash {
        TemplateDistribution::SetNewPrevHash(_) => {}
        _ => unreachable!("message kind already filtered"),
    }

    let stale_deadline = Instant::now() + Duration::from_secs(40);
    loop {
        assert!(
            Instant::now() < stale_deadline,
            "timed out waiting for stale-template-id response",
        );

        // stale-template-id is set asynchronously after tip changes, so we retry until observed.
        let stale_response = request_tdp_tx_data_and_recv_response_for_template_id(
            incoming_sender,
            outgoing_receiver,
            old_template_id,
            Duration::from_secs(10),
        )
        .await;

        match stale_response {
            TemplateDistribution::RequestTransactionDataError(message) => {
                let error_code = message.error_code.as_utf8_or_hex();
                if error_code == ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID {
                    break;
                }
                panic!("expected stale-template-id, got error code: {error_code}");
            }
            TemplateDistribution::RequestTransactionDataSuccess(_) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            _ => unreachable!("message kind already filtered"),
        }
    }
}

async fn recv_tdp_message<F>(
    receiver: &Receiver<TemplateDistribution<'static>>,
    timeout: Duration,
    predicate: F,
) -> TemplateDistribution<'static>
where
    F: Fn(&TemplateDistribution<'static>) -> bool,
{
    let deadline = Instant::now() + timeout;

    loop {
        // Drain messages until we find the expected one or hit deadline.
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

async fn request_tdp_tx_data_and_recv_response_for_template_id(
    incoming_sender: &Sender<TemplateDistribution<'static>>,
    outgoing_receiver: &Receiver<TemplateDistribution<'static>>,
    template_id: u64,
    timeout: Duration,
) -> TemplateDistribution<'static> {
    // Send request and then wait for either success or error that matches the same template id.
    incoming_sender
        .send(TemplateDistribution::RequestTransactionData(
            RequestTransactionData { template_id },
        ))
        .await
        .expect("failed to send RequestTransactionData");

    recv_tdp_message(outgoing_receiver, timeout, |msg| {
        matches!(
            msg,
            TemplateDistribution::RequestTransactionDataSuccess(message)
                if message.template_id == template_id
        ) || matches!(
            msg,
            TemplateDistribution::RequestTransactionDataError(message)
                if message.template_id == template_id
        )
    })
    .await
}
