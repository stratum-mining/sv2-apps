use integration_tests_sv2::{
    mocks::{MockServer, ServerSetupConnection},
    start_jdc, start_tracing, sv2_tp_config,
    utils::get_available_address,
};
use stratum_apps::stratum_core::{
    codec_sv2::SerializedFrame,
    common_messages_sv2::{
        MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol,
    },
};

// How many times JDC connects to a Pool/JDS pair before it moves on to the next one: the
// `MAX_RETRIES` of `jd_client_sv2::jdc_runtime`, which is private to it.
const JDC_ATTEMPTS_PER_PAIR: usize = 3;

// A `SetupConnection.Success` header with an empty payload: the six bytes of `used_version` and
// `flags` are missing, so the message can't be parsed.
fn truncated_setup_connection_success() -> SerializedFrame {
    SerializedFrame::from_bytes(vec![0, 0, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, 0, 0, 0].into())
        .expect("a header with an empty payload is a whole frame")
}

// A Pool whose `SetupConnection.Success` can't be parsed fails its Pool/JDS pair: JDC moves on to
// the next configured pair instead of starting without a Pool.
#[tokio::test]
async fn jdc_moves_to_the_next_pair_when_the_pool_setup_answer_is_malformed() {
    start_tracing();

    let tp_addr = get_available_address();
    let _tp = MockServer::new(
        tp_addr,
        Some(ServerSetupConnection::new([(
            Protocol::TemplateDistributionProtocol,
            0,
        )])),
    )
    .await;

    let malformed_pool_addr = get_available_address();
    let malformed_pool = MockServer::new(malformed_pool_addr, None).await;
    let first_jds_addr = get_available_address();
    let _first_jds = MockServer::new(
        first_jds_addr,
        Some(ServerSetupConnection::new([(
            Protocol::JobDeclarationProtocol,
            0,
        )])),
    )
    .await;

    let next_pool_addr = get_available_address();
    let next_pool = MockServer::new(
        next_pool_addr,
        Some(ServerSetupConnection::new([(Protocol::MiningProtocol, 0)])),
    )
    .await;
    let next_jds_addr = get_available_address();
    let _next_jds = MockServer::new(
        next_jds_addr,
        Some(ServerSetupConnection::new([(
            Protocol::JobDeclarationProtocol,
            0,
        )])),
    )
    .await;

    let (jdc, _jdc_addr, _) = start_jdc(
        &[
            (malformed_pool_addr, first_jds_addr),
            (next_pool_addr, next_jds_addr),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    for attempt in 0..JDC_ATTEMPTS_PER_PAIR {
        malformed_pool
            .wait_for_message_type(attempt, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
        malformed_pool
            .send_frame(attempt, truncated_setup_connection_success())
            .await;
    }

    next_pool
        .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    jdc.shutdown().await;
}

// A JDS whose `SetupConnection.Success` can't be parsed fails its Pool/JDS pair: JDC closes the
// connection it opened to the pair's Pool and moves on to the next configured pair, instead of
// starting with a Pool and no JDS.
#[tokio::test]
async fn jdc_moves_to_the_next_pair_when_the_jds_setup_answer_is_malformed() {
    start_tracing();

    let tp_addr = get_available_address();
    let _tp = MockServer::new(
        tp_addr,
        Some(ServerSetupConnection::new([(
            Protocol::TemplateDistributionProtocol,
            0,
        )])),
    )
    .await;

    let first_pool_addr = get_available_address();
    let first_pool = MockServer::new(
        first_pool_addr,
        Some(ServerSetupConnection::new([(Protocol::MiningProtocol, 0)])),
    )
    .await;
    let malformed_jds_addr = get_available_address();
    let malformed_jds = MockServer::new(malformed_jds_addr, None).await;

    let next_pool_addr = get_available_address();
    let next_pool = MockServer::new(
        next_pool_addr,
        Some(ServerSetupConnection::new([(Protocol::MiningProtocol, 0)])),
    )
    .await;
    let next_jds_addr = get_available_address();
    let _next_jds = MockServer::new(
        next_jds_addr,
        Some(ServerSetupConnection::new([(
            Protocol::JobDeclarationProtocol,
            0,
        )])),
    )
    .await;

    let (jdc, _jdc_addr, _) = start_jdc(
        &[
            (first_pool_addr, malformed_jds_addr),
            (next_pool_addr, next_jds_addr),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    for attempt in 0..JDC_ATTEMPTS_PER_PAIR {
        malformed_jds
            .wait_for_message_type(attempt, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
        malformed_jds
            .send_frame(attempt, truncated_setup_connection_success())
            .await;
    }

    next_pool
        .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    // JDC closes the connection it opened to the first pair's Pool.
    first_pool.wait_for_close(0).await;

    jdc.shutdown().await;
}
