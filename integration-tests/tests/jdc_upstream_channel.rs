use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{
        channel_open_success, connect_closable_downstream, extended_channel_request, send_message,
    },
    mocked_jdc::MockedJdc,
    utils::ABSENCE_WINDOW,
};
use std::time::Duration;
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

// A Pool that opens the upstream channel with a target easier than the `max_target` JDC asked for
// breaks the spec: JDC falls back from it without opening any downstream channel on that channel.
#[tokio::test]
async fn jdc_falls_back_from_a_pool_assigning_a_target_easier_than_requested() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    jdc.send_chain_tip().await;
    let downstream = jdc.connect_downstream("jdc-downstream").await;
    // 2^248 - 1, in little-endian: the all-ones target the Pool assigns below is easier.
    let mut max_target = [0xff; 32];
    max_target[31] = 0;
    downstream
        .request_extended_channel_with_max_target(max_target)
        .await;
    let request = jdc.upstream_channel_request().await;
    assert_eq!(request.max_target.to_array(), max_target);

    let mut success = channel_open_success(&request);
    success.target = [0xff; 32].into();
    jdc.send_upstream_channel_success(success).await;

    jdc.wait_for_fallback().await;
    assert!(
        downstream
            .sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "no downstream channel may open on an upstream channel with a target above the requested max"
    );

    jdc.jdc.shutdown().await;
}

// A Pool that grants an extranonce prefix and size adding up to more than any extranonce can
// hold leaves JDC no extranonce to allocate downstream channels from: JDC falls back from it
// without opening any downstream channel.
#[tokio::test]
async fn jdc_falls_back_from_a_pool_extranonce_layout_it_cannot_allocate_from() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    jdc.send_chain_tip().await;
    let downstream = jdc.connect_downstream("jdc-downstream").await;
    downstream.request_extended_channel().await;
    let request = jdc.upstream_channel_request().await;

    // One byte more than the 32 an extranonce can hold.
    let mut success = channel_open_success(&request);
    success.extranonce_prefix = vec![0; 33 - request.min_extranonce_size as usize]
        .try_into()
        .expect("extranonce prefix must fit into B032");
    jdc.send_upstream_channel_success(success).await;

    jdc.wait_for_fallback().await;
    assert!(
        downstream
            .sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "no downstream channel may open on an extranonce layout JDC can't allocate from"
    );

    jdc.jdc.shutdown().await;
}

// The coinbase scriptSig holds at most 100 bytes: up to 8 for the template's prefix, 8 for push
// opcodes, "Sv2" and tag delimiters, the miner tag and the full extranonce. With a 60-byte miner
// tag, a downstream's 20-byte solo extranonce fits (96 bytes) but the 32-byte one the Pool grants
// does not (108), so no downstream group could take it: JDC falls back from the Pool instead of
// shutting down.
#[tokio::test]
async fn jdc_falls_back_from_a_pool_extranonce_too_large_for_the_coinbase() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start_with_signature(&"x".repeat(60)).await;
    jdc.send_chain_tip().await;
    let downstream = jdc.connect_downstream("jdc-downstream").await;
    downstream.request_extended_channel().await;
    let request = jdc.upstream_channel_request().await;

    let mut success = channel_open_success(&request);
    success.extranonce_prefix = vec![0; 32 - request.min_extranonce_size as usize]
        .try_into()
        .expect("extranonce prefix must fit into B032");
    jdc.send_upstream_channel_success(success).await;

    jdc.wait_for_fallback().await;
    assert!(
        downstream
            .sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "no downstream channel may open on an extranonce too large for the coinbase"
    );

    jdc.jdc.shutdown().await;
}

// A downstream that asks for a channel and disconnects before the Pool opens the upstream channel
// leaves a request nobody waits for any more. The downstreams queued behind it still get their
// channels once the upstream channel opens.
#[tokio::test]
async fn jdc_opens_the_channels_queued_behind_a_disconnected_requester() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    jdc.send_chain_tip().await;
    let waiting = jdc.connect_downstream("jdc-downstream-waiting").await;
    let (_, gone) = connect_closable_downstream(jdc.address).await;
    send_message(&gone, extended_channel_request([0xff; 32])).await;
    let request = jdc.upstream_channel_request().await;
    waiting.request_extended_channel().await;

    // Nothing JDC sends shows when it has queued the second request or noticed the first
    // downstream is gone, so the test gives it a moment before the Pool answers.
    gone.close();
    tokio::time::sleep(Duration::from_secs(1)).await;

    jdc.send_upstream_channel_success(channel_open_success(&request))
        .await;
    waiting
        .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;

    jdc.jdc.shutdown().await;
}
