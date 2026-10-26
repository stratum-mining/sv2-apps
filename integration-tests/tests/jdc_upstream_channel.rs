use integration_tests_sv2::{
    interceptor::MessageDirection, mock_roles::channel_open_success, mocked_jdc::MockedJdc,
    utils::ABSENCE_WINDOW,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
    mining_sv2::{
        MESSAGE_TYPE_CLOSE_CHANNEL, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
    },
};

// Once the upstream channel is open, a replay of the success that opened it, or an error for the
// same request, answers nothing JDC asked for: JDC neither closes the channel, falls back from the
// Pool, nor opens the channel again.
#[tokio::test]
async fn jdc_ignores_replayed_upstream_channel_open_answers() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    jdc.send_chain_tip().await;
    let first = jdc.connect_downstream("jdc-downstream-first").await;
    first.request_extended_channel().await;
    let request = jdc.upstream_channel_request().await;
    jdc.send_upstream_channel_success(channel_open_success(&request))
        .await;
    first
        .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;

    jdc.send_upstream_channel_success(channel_open_success(&request))
        .await;
    jdc.send_upstream_channel_error(request.request_id).await;
    assert!(
        jdc.pool_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_CLOSE_CHANNEL,
                ABSENCE_WINDOW,
            )
            .await,
        "a replayed success must not close the upstream channel"
    );
    assert!(
        jdc.next_pool_sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "a replayed error must not make JDC fall back"
    );

    // The channel is still open: a second downstream's channel opens on it, without JDC asking
    // the Pool for another.
    let second = jdc.connect_downstream("jdc-downstream-second").await;
    second.request_extended_channel().await;
    second
        .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert!(
        jdc.pool_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                ABSENCE_WINDOW,
            )
            .await,
        "a replayed success must not make JDC open the upstream channel again"
    );

    jdc.jdc.shutdown().await;
}

// While JDC waits for the Pool to open its upstream channel, a success or an error for another
// request answers nothing JDC asked for: JDC keeps waiting, and opens the channel with the Pool's
// answer to its own request.
#[tokio::test]
async fn jdc_ignores_upstream_channel_open_answers_for_another_request() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    jdc.send_chain_tip().await;
    let downstream = jdc.connect_downstream("jdc-downstream").await;
    downstream.request_extended_channel().await;
    let request = jdc.upstream_channel_request().await;

    let mut other_request = channel_open_success(&request);
    other_request.request_id = request.request_id + 1;
    jdc.send_upstream_channel_success(other_request).await;
    jdc.send_upstream_channel_error(request.request_id + 1)
        .await;
    assert!(
        downstream
            .sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "a success for another request must not open the upstream channel"
    );
    assert!(
        jdc.next_pool_sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "an error for another request must not make JDC fall back"
    );

    jdc.send_upstream_channel_success(channel_open_success(&request))
        .await;
    downstream
        .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;

    jdc.jdc.shutdown().await;
}
