use integration_tests_sv2::{
    mocks::{MockServer, ServerSetupConnection},
    start_jdc, start_jdc_with_user_identities, start_tracing, sv2_tp_config,
    utils::get_available_address,
};
use jd_client_sv2::JobDeclaratorClient;
use std::{net::SocketAddr, time::Duration};
use stratum_apps::{
    stratum_core::{
        codec_sv2::SerializedFrame,
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol,
        },
    },
    upstream_selector::ATTEMPTS_PER_UPSTREAM,
};

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

    for attempt in 0..ATTEMPTS_PER_UPSTREAM {
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

    for attempt in 0..ATTEMPTS_PER_UPSTREAM {
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

// Long enough for JDC to spend its attempts on a pair that is down and start mining solo.
const ATTEMPTS_ON_A_DOWN_PAIR: Duration = Duration::from_secs(5);

// Starts a JDC that backs off for a second after a pair's first failure, in place of the 10
// seconds of a deployment, for the tests that have JDC come back to a pair.
fn start_jdc_backing_off_briefly(
    pairs: &[(SocketAddr, SocketAddr)],
    tp_addr: SocketAddr,
) -> (JobDeclaratorClient, SocketAddr, Option<SocketAddr>) {
    let pairs: Vec<_> = pairs
        .iter()
        .map(|&(pool, jds)| (pool, jds, "IT_TEST".to_string()))
        .collect();
    start_jdc_with_user_identities(
        &pairs,
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
        None,
        "JDC",
        Some(Duration::from_secs(1)),
    )
}

async fn template_provider(address: SocketAddr) -> MockServer {
    MockServer::new(
        address,
        Some(ServerSetupConnection::new([(
            Protocol::TemplateDistributionProtocol,
            0,
        )])),
    )
    .await
}

// A Pool and a JDS that answer `SetupConnection` and nothing after it.
async fn pair(pool_address: SocketAddr, jds_address: SocketAddr) -> (MockServer, MockServer) {
    let pool = MockServer::new(
        pool_address,
        Some(ServerSetupConnection::new([(Protocol::MiningProtocol, 0)])),
    )
    .await;
    let jds = MockServer::new(
        jds_address,
        Some(ServerSetupConnection::new([(
            Protocol::JobDeclarationProtocol,
            0,
        )])),
    )
    .await;
    (pool, jds)
}

// A pair stays eligible whatever happened with it before. With it as the only configured pair,
// JDC mines solo while the pair is down, connects once it answers, mines solo again when it drops
// and connects to it again when it is back.
#[tokio::test]
async fn jdc_reconnects_to_a_pair_it_was_connected_to_before() {
    start_tracing();

    let tp_addr = get_available_address();
    let _tp = template_provider(tp_addr).await;

    let pool_addr = get_available_address();
    let jds_addr = get_available_address();
    let (jdc, _jdc_addr, _) = start_jdc_backing_off_briefly(&[(pool_addr, jds_addr)], tp_addr);
    tokio::time::sleep(ATTEMPTS_ON_A_DOWN_PAIR).await;

    // JDC connects to the pair once it is up.
    let (pool, jds) = pair(pool_addr, jds_addr).await;
    jds.wait_for_held_connection(0).await;

    drop((pool, jds));
    // The servers' accepting tasks run and release the addresses.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // JDC connects to the pair again once it is back.
    let (_pool, jds) = pair(pool_addr, jds_addr).await;
    jds.wait_for_held_connection(0).await;

    jdc.shutdown().await;
}

// While on a lower-ranked pair, JDC probes the pairs ranked above it and switches back to the
// first one that answers, closing its connections to the lower-ranked pair.
#[tokio::test]
async fn jdc_switches_back_to_the_primary_pair_once_it_answers() {
    start_tracing();

    let tp_addr = get_available_address();
    let _tp = template_provider(tp_addr).await;

    let primary_pool_addr = get_available_address();
    let primary_jds_addr = get_available_address();
    let backup_pool_addr = get_available_address();
    let backup_jds_addr = get_available_address();
    let (backup_pool, backup_jds) = pair(backup_pool_addr, backup_jds_addr).await;

    let (jdc, _jdc_addr, _) = start_jdc_backing_off_briefly(
        &[
            (primary_pool_addr, primary_jds_addr),
            (backup_pool_addr, backup_jds_addr),
        ],
        tp_addr,
    );
    // JDC connects to the backup pair while the primary is down.
    let backup_pool_connection = backup_pool.wait_for_held_connection(0).await;
    backup_jds.wait_for_held_connection(0).await;

    // JDC switches to the primary pair once it answers.
    let (_primary_pool, primary_jds) = pair(primary_pool_addr, primary_jds_addr).await;
    primary_jds.wait_for_held_connection(0).await;
    backup_pool.wait_for_close(backup_pool_connection).await;

    jdc.shutdown().await;
}

// JDC mining solo, with its only pair down and probed, still shuts down when asked.
#[tokio::test]
async fn jdc_shuts_down_while_mining_solo() {
    start_tracing();

    let tp_addr = get_available_address();
    let _tp = template_provider(tp_addr).await;

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
