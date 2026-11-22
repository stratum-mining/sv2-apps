use integration_tests_sv2::{
    mocked_jdc::MockedJdc,
    mocks::{ClientSetupConnection, MockClient},
    utils::ABSENCE_WINDOW,
};
use std::time::Duration;
use stratum_apps::{
    stratum_core::{
        codec_sv2::{EncodableFrame, SerializedFrame},
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol,
        },
        framing_sv2::SV2_FRAME_HEADER_SIZE,
        mining_sv2::{
            ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_NOMINAL_HASHRATE,
            ERROR_CODE_OPEN_MINING_CHANNEL_MAX_TARGET_OUT_OF_RANGE, MESSAGE_TYPE_CLOSE_CHANNEL,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR, MESSAGE_TYPE_UPDATE_CHANNEL,
            MESSAGE_TYPE_UPDATE_CHANNEL_ERROR, OpenExtendedMiningChannelOwned,
            OpenExtendedMiningChannelSuccessOwned, OpenMiningChannelErrorOwned,
            OpenStandardMiningChannelOwned, UpdateChannelOwned,
        },
        parsers_sv2::{AnyMessageOwned, MiningOwned, TemplateDistributionOwned},
        template_distribution_sv2::{NewTemplateOwned, SetNewPrevHashOwned},
    },
    utils::types::OutboundFrame,
};

// Once the upstream channel is open, a replay of the success that opened it, or an error for the
// same request, answers nothing JDC asked for: JDC neither closes the channel, falls back from the
// Pool, nor opens the channel again.
#[tokio::test]
async fn jdc_ignores_replayed_upstream_channel_open_answers() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let first = jdc.connect_downstream().await;
    first
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    first
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;

    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(
                OpenMiningChannelErrorOwned {
                    request_id: request.request_id,
                    error_code: "unknown-user".try_into().unwrap(),
                },
            )),
        )
        .await;
    assert!(
        jdc.pool
            .assert_message_not_present(0, MESSAGE_TYPE_CLOSE_CHANNEL, ABSENCE_WINDOW,)
            .await,
        "a replayed success must not close the upstream channel"
    );
    assert!(
        jdc.next_pool
            .assert_message_not_present(0, MESSAGE_TYPE_SETUP_CONNECTION, ABSENCE_WINDOW,)
            .await,
        "a replayed error must not make JDC fall back"
    );

    // The channel is still open: a second downstream's channel opens on it, without JDC asking
    // the Pool for another.
    let second = jdc.connect_downstream().await;
    second
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    second
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert!(
        jdc.pool
            .assert_message_not_present(
                0,
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
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = jdc.connect_downstream().await;
    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };

    let mut other_request = OpenExtendedMiningChannelSuccessOwned {
        request_id: request.request_id,
        channel_id: 9,
        target: request.max_target.clone(),
        extranonce_size: request.min_extranonce_size,
        extranonce_prefix: vec![0; 4]
            .try_into()
            .expect("extranonce prefix must fit into B032"),
        group_channel_id: 1,
    };
    other_request.request_id = request.request_id + 1;
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(other_request)),
        )
        .await;
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(
                OpenMiningChannelErrorOwned {
                    request_id: request.request_id + 1,
                    error_code: "unknown-user".try_into().unwrap(),
                },
            )),
        )
        .await;
    assert!(
        downstream
            .assert_message_not_present(
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
                ABSENCE_WINDOW,
            )
            .await,
        "a success for another request must not open the upstream channel"
    );
    assert!(
        jdc.next_pool
            .assert_message_not_present(0, MESSAGE_TYPE_SETUP_CONNECTION, ABSENCE_WINDOW,)
            .await,
        "an error for another request must not make JDC fall back"
    );

    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;

    jdc.jdc.shutdown().await;
}

// A Pool that opens the upstream channel with a target easier than the `max_target` JDC asked for
// breaks the spec: JDC falls back from it without opening any downstream channel on that channel.
#[tokio::test]
async fn jdc_falls_back_from_a_pool_assigning_a_target_easier_than_requested() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = jdc.connect_downstream().await;
    // 2^248 - 1, in little-endian: the all-ones target the Pool assigns below is easier.
    let mut max_target = [0xff; 32];
    max_target[31] = 0;
    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: max_target.into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    assert_eq!(request.max_target.to_array(), max_target);

    let mut success = OpenExtendedMiningChannelSuccessOwned {
        request_id: request.request_id,
        channel_id: 9,
        target: request.max_target.clone(),
        extranonce_size: request.min_extranonce_size,
        extranonce_prefix: vec![0; 4]
            .try_into()
            .expect("extranonce prefix must fit into B032"),
        group_channel_id: 1,
    };
    success.target = [0xff; 32].into();
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(success)),
        )
        .await;

    jdc.wait_for_fallback().await;
    assert!(
        downstream
            .assert_message_not_present(
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
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = jdc.connect_downstream().await;
    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };

    // One byte more than the 32 an extranonce can hold.
    let mut success = OpenExtendedMiningChannelSuccessOwned {
        request_id: request.request_id,
        channel_id: 9,
        target: request.max_target.clone(),
        extranonce_size: request.min_extranonce_size,
        extranonce_prefix: vec![0; 4]
            .try_into()
            .expect("extranonce prefix must fit into B032"),
        group_channel_id: 1,
    };
    success.extranonce_prefix = vec![0; 33 - request.min_extranonce_size as usize]
        .try_into()
        .expect("extranonce prefix must fit into B032");
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(success)),
        )
        .await;

    jdc.wait_for_fallback().await;
    assert!(
        downstream
            .assert_message_not_present(
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
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = jdc.connect_downstream().await;
    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };

    let mut success = OpenExtendedMiningChannelSuccessOwned {
        request_id: request.request_id,
        channel_id: 9,
        target: request.max_target.clone(),
        extranonce_size: request.min_extranonce_size,
        extranonce_prefix: vec![0; 4]
            .try_into()
            .expect("extranonce prefix must fit into B032"),
        group_channel_id: 1,
    };
    success.extranonce_prefix = vec![0; 32 - request.min_extranonce_size as usize]
        .try_into()
        .expect("extranonce prefix must fit into B032");
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(success)),
        )
        .await;

    jdc.wait_for_fallback().await;
    assert!(
        downstream
            .assert_message_not_present(
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
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let waiting = jdc.connect_downstream().await;
    let gone = MockClient::new(
        jdc.address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    gone.wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;
    gone.send(AnyMessageOwned::Mining(
        MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
            request_id: 1,
            user_identity: "mock-downstream".try_into().unwrap(),
            nominal_hash_rate: 1_000.0,
            max_target: [0xff; 32].into(),
            min_extranonce_size: 0,
        }),
    ))
    .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    waiting
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;

    // Nothing JDC sends shows when it has queued the second request or noticed the first
    // downstream is gone, so the test gives it a moment before the Pool answers.
    gone.close();
    tokio::time::sleep(Duration::from_secs(1)).await;

    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    waiting
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;

    jdc.jdc.shutdown().await;
}

// An `UpdateChannel` naming a channel its downstream doesn't have, or one the channel refuses,
// changes nothing: the downstream gets its `UpdateChannelError`, and the Pool hears nothing of it.
#[tokio::test]
async fn jdc_answers_a_refused_update_channel_only_to_its_downstream() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = jdc.connect_downstream().await;
    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    let channel_id = loop {
        match downstream.next_message() {
            Some((
                _,
                AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(success)),
            )) => break success.channel_id,
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel.Success left the queue before it was read"),
        }
    };

    downstream
        .send(AnyMessageOwned::Mining(MiningOwned::UpdateChannel(
            UpdateChannelOwned {
                channel_id: channel_id + 1,
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
            },
        )))
        .await;
    downstream
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_UPDATE_CHANNEL_ERROR)
        .await;
    // No channel can take a `max_target` of zero.
    downstream
        .send(AnyMessageOwned::Mining(MiningOwned::UpdateChannel(
            UpdateChannelOwned {
                channel_id,
                nominal_hash_rate: 1_000.0,
                max_target: [0; 32].into(),
            },
        )))
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_UPDATE_CHANNEL_ERROR)
        .await;

    assert!(
        jdc.pool
            .assert_message_not_present(0, MESSAGE_TYPE_UPDATE_CHANNEL, ABSENCE_WINDOW,)
            .await,
        "a refused update must not reach the Pool"
    );

    jdc.jdc.shutdown().await;
}

// A channel-open frame whose `nominal_hash_rate` is `nominal_hash_rate`. The request is framed
// with a valid hashrate and then its four hashrate bytes are overwritten: a frame built by hand
// is written out as it is.
fn open_with_hash_rate(request: AnyMessageOwned, nominal_hash_rate: f32) -> SerializedFrame {
    let user_identity_len = match &request {
        AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)) => {
            request.user_identity.as_bytes().len()
        }
        AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannel(request)) => {
            request.user_identity.as_bytes().len()
        }
        _ => panic!("expected a channel open"),
    };

    let frame = OutboundFrame::from_message(request).expect("a valid channel open frames");
    let mut bytes = vec![0; frame.encoded_length()];
    frame
        .encode_into(&mut bytes)
        .expect("a valid channel open encodes");

    // `request_id` and `user_identity` (a length byte plus its bytes) precede
    // `nominal_hash_rate` on the wire.
    let at = SV2_FRAME_HEADER_SIZE + 4 + 1 + user_identity_len;
    bytes[at..at + 4].copy_from_slice(&nominal_hash_rate.to_le_bytes());

    SerializedFrame::from_bytes(bytes.into()).expect("a whole frame")
}

// The `OpenMiningChannel.Error` JDC answers the downstream with. Answering with anything else, or
// not answering at all, fails the test.
async fn next_open_mining_channel_error(downstream: &MockClient) -> OpenMiningChannelErrorOwned {
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR)
        .await;
    match downstream.next_message() {
        Some((_, AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(error)))) => error,
        other => panic!("expected OpenMiningChannel.Error, got {other:?}"),
    }
}

// A first open the channel constructor refuses is answered by JDC itself: the downstream gets its
// `OpenMiningChannel.Error`, the Pool hears nothing, JDC does not fall back, and the next valid
// open still opens the upstream channel.
async fn assert_refused_open_stays_local(jdc: &MockedJdc) {
    assert!(
        jdc.pool
            .assert_message_not_present(
                0,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                ABSENCE_WINDOW,
            )
            .await,
        "an open the channel constructor refuses must not reach the Pool"
    );
    assert!(
        jdc.next_pool
            .assert_message_not_present(0, MESSAGE_TYPE_SETUP_CONNECTION, ABSENCE_WINDOW,)
            .await,
        "an open the channel constructor refuses must not make JDC fall back"
    );
}

// A first open that `binary_sv2` refuses to serialize must not cost JDC the upstream connection.
#[tokio::test]
async fn jdc_refuses_a_first_open_with_an_invalid_hash_rate_without_touching_the_pool() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = MockClient::new(
        jdc.address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    downstream
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;

    downstream
        .send_frame(open_with_hash_rate(
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(
                OpenExtendedMiningChannelOwned {
                    request_id: 1,
                    user_identity: "mock-downstream".try_into().unwrap(),
                    nominal_hash_rate: 1_000.0,
                    max_target: [0xff; 32].into(),
                    min_extranonce_size: 0,
                },
            )),
            f32::NAN,
        ))
        .await;
    assert_eq!(
        next_open_mining_channel_error(&downstream)
            .await
            .error_code
            .as_utf8_or_hex(),
        ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_NOMINAL_HASHRATE
    );
    assert_refused_open_stays_local(&jdc).await;

    // The upstream channel is still JDC's to open: the next valid request opens it.
    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert_eq!(
        downstream
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
    );

    jdc.jdc.shutdown().await;
}

// A first open the Pool would refuse must not cost JDC the upstream channel either, even though
// the request itself serializes.
#[tokio::test]
async fn jdc_refuses_a_first_open_with_a_zero_max_target_without_touching_the_pool() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = MockClient::new(
        jdc.address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    downstream
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;

    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    assert_eq!(
        next_open_mining_channel_error(&downstream)
            .await
            .error_code
            .as_utf8_or_hex(),
        ERROR_CODE_OPEN_MINING_CHANNEL_MAX_TARGET_OUT_OF_RANGE
    );
    assert_refused_open_stays_local(&jdc).await;

    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert_eq!(
        downstream
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
    );

    jdc.jdc.shutdown().await;
}

// JDC opens an extended channel upstream for a standard first open, carrying the same
// `nominal_hash_rate`, so an invalid value is refused here too.
#[tokio::test]
async fn jdc_refuses_a_standard_first_open_with_an_invalid_hash_rate_without_touching_the_pool() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    // The Template Provider sends future template 1 and the prevhash activating it.
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
                NewTemplateOwned {
                    template_id: 1,
                    future_template: true,
                    version: 536_870_912,
                    coinbase_tx_version: 2,
                    coinbase_prefix: vec![82, 0]
                        .try_into()
                        .expect("coinbase prefix must fit into B0255"),
                    coinbase_tx_input_sequence: u32::MAX,
                    coinbase_tx_value_remaining: 5_000_000_000,
                    coinbase_tx_outputs_count: 1,
                    coinbase_tx_outputs: vec![
                        0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63,
                        113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137,
                        121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                    ]
                    .try_into()
                    .expect("coinbase outputs must fit into B064K"),
                    coinbase_tx_locktime: 0,
                    merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
                },
            )),
        )
        .await;
    jdc.tp
        .send(
            0,
            AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::SetNewPrevHash(
                SetNewPrevHashOwned {
                    template_id: 1,
                    prev_hash: [1; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                },
            )),
        )
        .await;
    let downstream = MockClient::new(
        jdc.address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    downstream
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;

    downstream
        .send_frame(open_with_hash_rate(
            AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannel(
                OpenStandardMiningChannelOwned {
                    request_id: 1,
                    user_identity: "mock-downstream".try_into().unwrap(),
                    nominal_hash_rate: 1_000.0,
                    max_target: [0xff; 32].into(),
                },
            )),
            -1.0,
        ))
        .await;
    assert_eq!(
        next_open_mining_channel_error(&downstream)
            .await
            .error_code
            .as_utf8_or_hex(),
        ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_NOMINAL_HASHRATE
    );
    assert_refused_open_stays_local(&jdc).await;

    downstream
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "mock-downstream".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await;
    // JDC asks the Pool for its upstream channel.
    jdc.pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let request = loop {
        match jdc.pool.next_message(0) {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)))) => {
                break request;
            }
            Some(_) => continue,
            None => panic!("OpenExtendedMiningChannel left the Pool's queue before it was read"),
        }
    };
    jdc.pool
        .send(
            0,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: request.request_id,
                    channel_id: 9,
                    target: request.max_target.clone(),
                    extranonce_size: request.min_extranonce_size,
                    extranonce_prefix: vec![0; 4]
                        .try_into()
                        .expect("extranonce prefix must fit into B032"),
                    group_channel_id: 1,
                },
            )),
        )
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert_eq!(
        downstream
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
    );

    jdc.jdc.shutdown().await;
}
