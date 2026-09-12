//! End-to-end IPC integration coverage for Sv2 Template Distribution Protocol (TDP).
//!
//! Flow covered per Sv2 TDP expectations:
//! - bootstrap after `CoinbaseOutputConstraints` emits `NewTemplate` and `SetNewPrevHash`.
//! - `RequestTransactionData` succeeds for the current template id.
//! - `RequestTransactionData` returns `template-id-not-found` for an unknown id.
//! - after a chain-tip update, an old template id eventually returns `stale-template-id`.
//! - fee refreshes at one chain tip retire the templates beyond the cap and keep the rest usable.
//! - rotating coinbase output constraints stops the superseded templates from answering at once.
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
    bitcoin_core_sv2::{
        CancellationToken,
        runtime_api::{BitcoinCoreVersion, template_distribution_protocol},
    },
    stratum_core::{
        parsers_sv2::TemplateDistributionOwned,
        template_distribution_sv2::{
            CoinbaseOutputConstraintsOwned, ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID,
            ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND, RequestTransactionDataOwned,
            RequestTransactionDataSuccessOwned,
        },
    },
};

/// Templates kept usable at one chain tip.
///
/// Mirrors `MAX_SAME_TIP_TEMPLATES` in `bitcoin_core_sv2`, which is private to that crate.
const MAX_SAME_TIP_TEMPLATES: usize = 8;

#[tokio::test]
async fn tdp_io_integration_v30x() {
    assert_tdp_io_integration(BitcoinCoreVersion::V30X).await;
}

#[tokio::test]
async fn tdp_io_integration_v31x() {
    assert_tdp_io_integration(BitcoinCoreVersion::V31X).await;
}

async fn assert_tdp_io_integration(version: BitcoinCoreVersion) {
    start_tracing();

    // Start a real Bitcoin Core node for the selected runtime line.
    let bitcoin_core = start_bitcoin_core(DifficultyLevel::Low, version);
    let socket_path = bitcoin_core.ipc_socket_path();

    // Funded before the runtime is driven, so the blocks this mines do not move the chain tip
    // under a scenario. The coins pay for the mempool transactions that drive fee refreshes.
    bitcoin_core.fund_wallet().expect("failed to fund wallet");

    // Incoming channel feeds TDP requests; outgoing channel receives TDP responses/events.
    let (incoming_sender, incoming_receiver) = async_channel::unbounded();
    let (outgoing_sender, outgoing_receiver) = async_channel::unbounded();

    let cancellation_token = CancellationToken::new();
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
    assert_tdp_same_tip_templates_are_capped(&bitcoin_core, &incoming_sender, &outgoing_receiver)
        .await;
    assert_tdp_constraint_churn_retires_superseded_templates(&incoming_sender, &outgoing_receiver)
        .await;

    cancellation_token.cancel();
    tdp_thread
        .join()
        .expect("BitcoinCoreSv2TDP thread join should succeed");
}

async fn bootstrap_tdp_and_get_template_id(
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
) -> u64 {
    // TDP requires CoinbaseOutputConstraints first; this triggers initial template publication.
    incoming_sender
        .send(TemplateDistributionOwned::CoinbaseOutputConstraints(
            CoinbaseOutputConstraintsOwned {
                coinbase_output_max_additional_size: 2,
                coinbase_output_max_additional_sigops: 2,
            },
        ))
        .await
        .expect("failed to send CoinbaseOutputConstraints");

    let new_template = recv_tdp_message(outgoing_receiver, Duration::from_secs(20), |msg| {
        matches!(msg, TemplateDistributionOwned::NewTemplate(_))
    })
    .await;
    let new_template = match new_template {
        TemplateDistributionOwned::NewTemplate(message) => message,
        _ => unreachable!("message kind already filtered"),
    };

    let set_new_prev_hash = recv_tdp_message(outgoing_receiver, Duration::from_secs(20), |msg| {
        matches!(msg, TemplateDistributionOwned::SetNewPrevHash(_))
    })
    .await;
    let set_new_prev_hash = match set_new_prev_hash {
        TemplateDistributionOwned::SetNewPrevHash(message) => message,
        _ => unreachable!("message kind already filtered"),
    };

    assert_eq!(set_new_prev_hash.template_id, new_template.template_id);
    new_template.template_id
}

async fn assert_tdp_request_tx_data_success(
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
    template_id: u64,
) {
    let response = request_tdp_tx_data_and_recv_response_for_template_id(
        incoming_sender,
        outgoing_receiver,
        template_id,
        Duration::from_secs(20),
    )
    .await;

    let request_tx_data_success: RequestTransactionDataSuccessOwned = match response {
        TemplateDistributionOwned::RequestTransactionDataSuccess(message) => message,
        _ => unreachable!("message kind already filtered"),
    };

    assert_eq!(
        request_tx_data_success.template_id, template_id,
        "RequestTransactionDataSuccess must reference the requested template",
    );
}

async fn assert_tdp_request_tx_data_not_found(
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
) {
    let not_found_response = request_tdp_tx_data_and_recv_response_for_template_id(
        incoming_sender,
        outgoing_receiver,
        u64::MAX,
        Duration::from_secs(20),
    )
    .await;

    match not_found_response {
        TemplateDistributionOwned::RequestTransactionDataError(message) => {
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
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
    old_template_id: u64,
) {
    // Force a tip change so the previously active template becomes non-current.
    bitcoin_core.generate_blocks(1);

    let next_set_new_prev_hash =
        recv_tdp_message(outgoing_receiver, Duration::from_secs(20), |msg| {
            matches!(
                msg,
                TemplateDistributionOwned::SetNewPrevHash(message)
                    if message.template_id != old_template_id
            )
        })
        .await;
    match next_set_new_prev_hash {
        TemplateDistributionOwned::SetNewPrevHash(_) => {}
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
            TemplateDistributionOwned::RequestTransactionDataError(message) => {
                let error_code = message.error_code.as_utf8_or_hex();
                if error_code == ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID {
                    break;
                }
                panic!("expected stale-template-id, got error code: {error_code}");
            }
            TemplateDistributionOwned::RequestTransactionDataSuccess(_) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            _ => unreachable!("message kind already filtered"),
        }
    }
}

/// Fee refreshes at one chain tip must not accumulate templates without bound.
///
/// Each one publishes a template without invalidating the previous one, so they are retired by
/// count: publishing beyond the cap retires the oldest, while everything still within it keeps
/// answering.
async fn assert_tdp_same_tip_templates_are_capped(
    bitcoin_core: &BitcoinCore,
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
) {
    // A mempool transaction raises the fees of the next template, which the runtime publishes as
    // a non-future template against the same chain tip. Two more than the cap, so the oldest two
    // of the batch are guaranteed to be retired by the time the newest is published.
    let mut template_ids = Vec::new();
    while template_ids.len() < MAX_SAME_TIP_TEMPLATES + 2 {
        bitcoin_core
            .create_mempool_transaction()
            .expect("failed to create mempool transaction");

        let new_template = recv_tdp_message(outgoing_receiver, Duration::from_secs(30), |msg| {
            matches!(
                msg,
                TemplateDistributionOwned::NewTemplate(message) if !message.future_template
            )
        })
        .await;
        match new_template {
            TemplateDistributionOwned::NewTemplate(message) => {
                template_ids.push(message.template_id)
            }
            _ => unreachable!("message kind already filtered"),
        }
    }

    for template_id in &template_ids[..2] {
        assert_tdp_template_is_retired(incoming_sender, outgoing_receiver, *template_id).await;
    }

    // The oldest template still within the cap is deliberately left out: one more fee refresh
    // landing while these requests are made would retire exactly that one.
    for template_id in &template_ids[template_ids.len() - (MAX_SAME_TIP_TEMPLATES - 1)..] {
        assert_tdp_template_is_usable(incoming_sender, outgoing_receiver, *template_id).await;
    }
}

/// Rotating coinbase output constraints must stop the superseded templates from answering at once.
///
/// Their data outlives the rotation by a grace period so requests already in flight can finish,
/// but a request that arrives afterwards must not be served from them.
async fn assert_tdp_constraint_churn_retires_superseded_templates(
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
) {
    let mut template_ids = Vec::new();

    for coinbase_output_max_additional_size in 3..6 {
        incoming_sender
            .send(TemplateDistributionOwned::CoinbaseOutputConstraints(
                CoinbaseOutputConstraintsOwned {
                    coinbase_output_max_additional_size,
                    coinbase_output_max_additional_sigops: 2,
                },
            ))
            .await
            .expect("failed to send CoinbaseOutputConstraints");

        // Each rotation bootstraps a fresh template IPC client, published as a future template.
        let new_template = recv_tdp_message(outgoing_receiver, Duration::from_secs(30), |msg| {
            matches!(
                msg,
                TemplateDistributionOwned::NewTemplate(message) if message.future_template
            )
        })
        .await;
        match new_template {
            TemplateDistributionOwned::NewTemplate(message) => {
                template_ids.push(message.template_id)
            }
            _ => unreachable!("message kind already filtered"),
        }
    }

    let (current_template_id, superseded_template_ids) = template_ids
        .split_last()
        .expect("every rotation published a template");

    for template_id in superseded_template_ids {
        assert_tdp_template_is_retired(incoming_sender, outgoing_receiver, *template_id).await;
    }

    // The rotations left the runtime serving the template published by the last one.
    assert_tdp_template_is_usable(incoming_sender, outgoing_receiver, *current_template_id).await;
}

/// A retired template must not serve a request that arrives after its retirement.
async fn assert_tdp_template_is_retired(
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
    template_id: u64,
) {
    let response = request_tdp_tx_data_and_recv_response_for_template_id(
        incoming_sender,
        outgoing_receiver,
        template_id,
        Duration::from_secs(20),
    )
    .await;

    match response {
        TemplateDistributionOwned::RequestTransactionDataError(message) => {
            let error_code = message.error_code.as_utf8_or_hex();
            // stale-template-id while a retired template waits out its grace period, and
            // template-id-not-found once it has been destroyed.
            assert!(
                error_code == ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID
                    || error_code == ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND,
                "retired template {template_id} answered with error code: {error_code}",
            );
        }
        response => panic!(
            "retired template {template_id} must not answer a new request, got: {response:?}"
        ),
    }
}

/// A template that has not been retired must still serve transaction data.
async fn assert_tdp_template_is_usable(
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
    template_id: u64,
) {
    let response = request_tdp_tx_data_and_recv_response_for_template_id(
        incoming_sender,
        outgoing_receiver,
        template_id,
        Duration::from_secs(20),
    )
    .await;

    match response {
        TemplateDistributionOwned::RequestTransactionDataSuccess(message) => {
            assert_eq!(
                message.template_id, template_id,
                "RequestTransactionDataSuccess must reference the requested template",
            );
        }
        response => {
            panic!(
                "retained template {template_id} must answer transaction data, got: {response:?}"
            )
        }
    }
}

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
    incoming_sender: &Sender<TemplateDistributionOwned>,
    outgoing_receiver: &Receiver<TemplateDistributionOwned>,
    template_id: u64,
    timeout: Duration,
) -> TemplateDistributionOwned {
    // Send request and then wait for either success or error that matches the same template id.
    incoming_sender
        .send(TemplateDistributionOwned::RequestTransactionData(
            RequestTransactionDataOwned { template_id },
        ))
        .await
        .expect("failed to send RequestTransactionData");

    recv_tdp_message(outgoing_receiver, timeout, |msg| {
        matches!(
            msg,
            TemplateDistributionOwned::RequestTransactionDataSuccess(message)
                if message.template_id == template_id
        ) || matches!(
            msg,
            TemplateDistributionOwned::RequestTransactionDataError(message)
                if message.template_id == template_id
        )
    })
    .await
}
