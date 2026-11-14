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
    let first_pool = answer_setup_connections(first_pool_addr, setup_connection_success).await;
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
    tokio::time::timeout(Duration::from_secs(10), first_pool.closed.recv())
        .await
        .expect("JDC closes its connection to the first pair's Pool")
        .expect("the first pair's Pool was connected");

    jdc.shutdown().await;
}

// Long enough for JDC to spend its three attempts on a pair that is down and start mining solo.
const ATTEMPTS_ON_A_DOWN_PAIR: Duration = Duration::from_secs(5);

// A pair stays eligible whatever happened with it before. With it as the only configured pair,
// JDC mines solo while the pair is down, connects once it answers, mines solo again when it drops
// and connects to it again when it is back.
#[tokio::test]
async fn jdc_reconnects_to_a_pair_it_was_connected_to_before() {
    start_tracing();

    let tp_addr = get_available_address();
    MockUpstream::new(
        tp_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;

    let pool_addr = get_available_address();
    let jds_addr = get_available_address();
    let (jdc, _jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );
    tokio::time::sleep(ATTEMPTS_ON_A_DOWN_PAIR).await;

    let pool = answer_setup_connections(pool_addr, setup_connection_success).await;
    let jds = answer_setup_connections(jds_addr, setup_connection_success).await;
    jds.wait_for_held_connection("JDC connects to the pair once it is up")
        .await;

    pool.stop().await;
    jds.stop().await;
    let _pool = answer_setup_connections(pool_addr, setup_connection_success).await;
    let jds = answer_setup_connections(jds_addr, setup_connection_success).await;
    jds.wait_for_held_connection("JDC connects to the pair again once it is back")
        .await;

    jdc.shutdown().await;
}

// While on a lower-ranked pair, JDC probes the pairs ranked above it and switches back to the
// first one that answers, closing its connections to the lower-ranked pair.
#[tokio::test]
async fn jdc_switches_back_to_the_primary_pair_once_it_answers() {
    start_tracing();

    let tp_addr = get_available_address();
    MockUpstream::new(
        tp_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;

    let primary_pool_addr = get_available_address();
    let primary_jds_addr = get_available_address();
    let backup_pool_addr = get_available_address();
    let backup_jds_addr = get_available_address();
    let backup_pool = answer_setup_connections(backup_pool_addr, setup_connection_success).await;
    let backup_jds = answer_setup_connections(backup_jds_addr, setup_connection_success).await;

    let (jdc, _jdc_addr, _) = start_jdc(
        &[
            (primary_pool_addr, primary_jds_addr),
            (backup_pool_addr, backup_jds_addr),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );
    backup_jds
        .wait_for_held_connection("JDC connects to the backup pair while the primary is down")
        .await;

    let _primary_pool = answer_setup_connections(primary_pool_addr, setup_connection_success).await;
    let primary_jds = answer_setup_connections(primary_jds_addr, setup_connection_success).await;
    primary_jds
        .wait_for_held_connection("JDC switches to the primary pair once it answers")
        .await;
    tokio::time::timeout(Duration::from_secs(10), backup_pool.closed.recv())
        .await
        .expect("JDC closes its connection to the backup pair's Pool")
        .expect("the backup Pool was connected");

    jdc.shutdown().await;
}

// JDC mining solo, with its only pair down and probed, still shuts down when asked.
#[tokio::test]
async fn jdc_shuts_down_while_mining_solo() {
    start_tracing();

    let tp_addr = get_available_address();
    MockUpstream::new(
        tp_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;

    let (jdc, _jdc_addr, _) = start_jdc(
        &[(get_available_address(), get_available_address())],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );
    tokio::time::sleep(ATTEMPTS_ON_A_DOWN_PAIR).await;

    tokio::time::timeout(Duration::from_secs(15), jdc.shutdown())
        .await
        .expect("JDC shuts down while mining solo");
}
