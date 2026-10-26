use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{
        MockUpstream, WithSetup, answer_setup_connections, setup_connection_success,
        truncated_setup_connection_success,
    },
    start_jdc, start_sniffer, start_tracing, sv2_tp_config,
    utils::get_available_address,
};
use std::time::Duration;
use stratum_apps::stratum_core::common_messages_sv2::{
    MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol,
};

// A Pool whose `SetupConnection.Success` can't be parsed fails its Pool/JDS pair: JDC moves on to
// the next configured pair instead of starting without a Pool.
#[tokio::test]
async fn jdc_moves_to_the_next_pair_when_the_pool_setup_answer_is_malformed() {
    start_tracing();

    let tp_addr = get_available_address();
    MockUpstream::new(
        tp_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;

    let malformed_pool_addr = get_available_address();
    answer_setup_connections(malformed_pool_addr, truncated_setup_connection_success).await;
    let first_jds_addr = get_available_address();
    MockUpstream::new(
        first_jds_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
    )
    .start()
    .await;

    let next_pool_addr = get_available_address();
    MockUpstream::new(
        next_pool_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    let (next_pool_sniffer, next_pool_sniffer_addr) =
        start_sniffer("jdc-next-pool", next_pool_addr, false, vec![], None);
    let next_jds_addr = get_available_address();
    MockUpstream::new(
        next_jds_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
    )
    .start()
    .await;

    let (jdc, _jdc_addr, _) = start_jdc(
        &[
            (malformed_pool_addr, first_jds_addr),
            (next_pool_sniffer_addr, next_jds_addr),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    next_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
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
    MockUpstream::new(
        tp_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;

    let first_pool_addr = get_available_address();
    let first_pool_closed =
        answer_setup_connections(first_pool_addr, setup_connection_success).await;
    let malformed_jds_addr = get_available_address();
    answer_setup_connections(malformed_jds_addr, truncated_setup_connection_success).await;

    let next_pool_addr = get_available_address();
    MockUpstream::new(
        next_pool_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    let (next_pool_sniffer, next_pool_sniffer_addr) =
        start_sniffer("jdc-next-pool", next_pool_addr, false, vec![], None);
    let next_jds_addr = get_available_address();
    MockUpstream::new(
        next_jds_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
    )
    .start()
    .await;

    let (jdc, _jdc_addr, _) = start_jdc(
        &[
            (first_pool_addr, malformed_jds_addr),
            (next_pool_sniffer_addr, next_jds_addr),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    next_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(10), first_pool_closed.recv())
        .await
        .expect("JDC closes its connection to the first pair's Pool")
        .expect("the first pair's Pool was connected");

    jdc.shutdown().await;
}
