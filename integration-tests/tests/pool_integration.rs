// This file contains integration tests for the `PoolSv2` module.
//
// `PoolSv2` is a module that implements the Pool role in the Stratum V2 protocol.
use integration_tests_sv2::{
    interceptor::{MessageDirection, ReplaceMessage},
    mock_roles::{MockDownstream, WithSetup},
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::stratum_core::{
    binary_sv2::{Seq0255, U256},
    common_messages_sv2::{has_work_selection, Protocol, SetupConnection, *},
    mining_sv2::*,
    parsers_sv2::{self, AnyMessage, CommonMessages, Mining, TemplateDistribution},
    template_distribution_sv2::*,
};

// This test starts a Template Provider and a Pool, and checks if they exchange the correct
// messages upon connection.
// The Sniffer is used as a proxy between the Upstream(Template Provider) and Downstream(Pool). The
// Pool will connect to the Sniffer, and the Sniffer will connect to the Template Provider.
#[tokio::test]
async fn success_pool_template_provider_connection() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (sniffer, sniffer_addr) = start_sniffer("", tp_addr, true, vec![], None);
    let (pool, _, _) = start_pool(sv2_tp_config(sniffer_addr), vec![], vec![], false).await;
    // here we assert that the downstream(pool in this case) have sent `SetupConnection` message
    // with the correct parameters, protocol, flags, min_version and max_version.  Note that the
    // macro can take any number of arguments after the message argument, but the order is
    // important where a property should be followed by its value.
    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    assert_common_message!(
        &sniffer.next_message_from_downstream(),
        SetupConnection,
        protocol,
        Protocol::TemplateDistributionProtocol,
        flags,
        0,
        min_version,
        2,
        max_version,
        2
    );
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    assert_common_message!(
        &sniffer.next_message_from_upstream(),
        SetupConnectionSuccess
    );
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS,
        )
        .await;
    assert_tp_message!(
        &sniffer.next_message_from_downstream(),
        CoinbaseOutputConstraints
    );
    sniffer
        .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_TEMPLATE)
        .await;
    assert_tp_message!(&sniffer.next_message_from_upstream(), NewTemplate);
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_NEW_PREV_HASH,
        )
        .await;
    assert_tp_message!(sniffer.next_message_from_upstream(), SetNewPrevHash);
    pool.shutdown().await;
}

// This test starts a Template Provider, a Pool, and a Translator Proxy, and verifies the
// correctness of the exchanged messages during connection and operation.
//
// Two Sniffers are used:
// - Between the Template Provider and the Pool.
// - Between the Pool and the Translator Proxy.
//
// The test ensures that:
// - The Template Provider sends valid `SetNewPrevHash` and `NewTemplate` messages.
// - The `minntime` field in the second `NewExtendedMiningJob` message sent to the Translator Proxy
//   matches the `header_timestamp` from the `SetNewPrevHash` message, addressing a bug that
//   occurred with non-future jobs.
//
// Related issue: https://github.com/stratum-mining/stratum/issues/1324
#[tokio::test]
async fn header_timestamp_value_assertion_in_new_extended_mining_job() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let tp_pool_sniffer_identifier =
        "header_timestamp_value_assertion_in_new_extended_mining_job tp_pool sniffer";
    let (tp_pool_sniffer, tp_pool_sniffer_addr) =
        start_sniffer(tp_pool_sniffer_identifier, tp_addr, false, vec![], None);
    let (pool, pool_addr, _) =
        start_pool(sv2_tp_config(tp_pool_sniffer_addr), vec![], vec![], false).await;
    let pool_translator_sniffer_identifier =
        "header_timestamp_value_assertion_in_new_extended_mining_job pool_translator sniffer";
    let (pool_translator_sniffer, pool_translator_sniffer_addr) = start_sniffer(
        pool_translator_sniffer_identifier,
        pool_addr,
        false,
        vec![
            // Block SubmitSharesExtended messages to prevent regtest blocks from being mined
            integration_tests_sv2::interceptor::IgnoreMessage::new(
                integration_tests_sv2::interceptor::MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .into(),
        ],
        None,
    );
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    tp_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    assert_common_message!(
        &tp_pool_sniffer.next_message_from_upstream(),
        SetupConnectionSuccess
    );
    // Wait for a NewTemplate message from the Template Provider
    tp_pool_sniffer
        .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_TEMPLATE)
        .await;
    assert_tp_message!(&tp_pool_sniffer.next_message_from_upstream(), NewTemplate);
    // Extract header timestamp from SetNewPrevHash message
    let header_timestamp_to_check = match tp_pool_sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::TemplateDistribution(TemplateDistribution::SetNewPrevHash(msg)))) => {
            msg.header_timestamp
        }
        _ => panic!("SetNewPrevHash not found!"),
    };
    pool_translator_sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    // create a mempool transaction to force TP to send a non-future NewTemplate
    tp.create_mempool_transaction().unwrap();

    // Wait for a second NewExtendedMiningJob message
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    // Extract min_ntime from the second NewExtendedMiningJob message
    let second_job_ntime = match pool_translator_sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(Mining::NewExtendedMiningJob(job)))) => {
            job.min_ntime.into_inner()
        }
        _ => panic!("Second NewExtendedMiningJob not found!"),
    };
    // Assert that min_ntime matches header_timestamp
    assert_eq!(
        second_job_ntime,
        Some(header_timestamp_to_check),
        "The `minntime` field of the second NewExtendedMiningJob does not match the `header_timestamp`!"
    );
    shutdown_all!(translator, pool);
}

// This test starts a Pool, a Sniffer, and a Sv2 Mining Device.  It then checks if the Pool receives
// a share from the Sv2 Mining Device.  While also checking all the messages exchanged between the
// Pool and the Mining Device in between.
#[tokio::test]
async fn pool_standard_channel_receives_share() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("A", pool_addr, false, vec![], None);
    start_mining_device_sv2(sniffer_addr, None, None, None, 1, None, true);
    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL,
        )
        .await;

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;
    sniffer
        .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_MINING_JOB)
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_STANDARD,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;
    pool.shutdown().await;
}

// This test verifies that the Pool does not send SetNewPrevHash and NewExtendedMiningJob (future
// and non-future) messages to JDC.
#[tokio::test]
async fn pool_does_not_send_jobs_to_jdc() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (pool_jdc_sniffer, pool_jdc_sniffer_addr) =
        start_sniffer("pool_jdc", pool_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_jdc_sniffer_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );
    // Block NewExtendedMiningJob and SetNewPrevHash messages between JDC and translator proxy
    let (_tproxy_jdc_sniffer, tproxy_jdc_sniffer_addr) = start_sniffer(
        "tproxy_jdc",
        jdc_addr,
        false,
        vec![
            integration_tests_sv2::interceptor::IgnoreMessage::new(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
            )
            .into(),
            integration_tests_sv2::interceptor::IgnoreMessage::new(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .into(),
        ],
        None,
    );
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[tproxy_jdc_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    // Add SV1 sniffer between translator and miner
    let (_sv1_sniffer, sv1_sniffer_addr) = start_sv1_sniffer(tproxy_addr);
    let (_minerd_process, _minerd_addr) = start_minerd(sv1_sniffer_addr, None, None, false).await;

    pool_jdc_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    // Verify SetupConnection has work_selection flag set (JDC requires custom work)
    let setup_msg = pool_jdc_sniffer.next_message_from_downstream();
    match setup_msg {
        Some((_, AnyMessage::Common(CommonMessages::SetupConnection(msg)))) => {
            assert!(
                has_work_selection(msg.flags),
                "JDC should set work_selection flag in SetupConnection"
            );
        }
        _ => panic!("Expected SetupConnection message from JDC"),
    }

    pool_jdc_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    pool_jdc_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    pool_jdc_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Verify that future NewExtendedMiningJob messages are NOT sent to JDC
    assert!(
        pool_jdc_sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
                std::time::Duration::from_secs(1),
            )
            .await,
        "Pool should NOT send future NewExtendedMiningJob messages to JDC"
    );

    // Verify that SetNewPrevHash is NOT sent to JDC
    assert!(
        pool_jdc_sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
                std::time::Duration::from_secs(1),
            )
            .await,
        "Pool should NOT send SetNewPrevHash messages to JDC"
    );

    // Trigger a new template by creating a mempool transaction
    tp.create_mempool_transaction().unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Verify that non-future NewExtendedMiningJob messages are NOT sent to JDC
    assert!(
        pool_jdc_sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
                std::time::Duration::from_secs(1),
            )
            .await,
        "Pool should NOT send non-future NewExtendedMiningJob messages to JDC"
    );
    shutdown_all!(translator, jdc, pool);
}

// The test runs pool and translator, with translator sending a SetupConnection message
// with a wrong protocol, this test asserts whether pool sends SetupConnection error or
// not to such downstream.
#[tokio::test]
async fn pool_reject_setup_connection_with_non_mining_protocol() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let endpoint_host = "127.0.0.1".to_string().into_bytes().try_into().unwrap();
    let vendor = String::new().try_into().unwrap();
    let hardware_version = String::new().try_into().unwrap();
    let firmware = String::new().try_into().unwrap();
    let device_id = String::new().try_into().unwrap();

    let setup_connection_replace = ReplaceMessage::new(
        MessageDirection::ToUpstream,
        MESSAGE_TYPE_SETUP_CONNECTION,
        AnyMessage::Common(parsers_sv2::CommonMessages::SetupConnection(
            SetupConnection {
                protocol: Protocol::TemplateDistributionProtocol,
                min_version: 2,
                max_version: 2,
                flags: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                endpoint_host,
                endpoint_port: 1212,
                vendor,
                hardware_version,
                firmware,
                device_id,
            },
        )),
    );
    let (pool_translator_sniffer, pool_translator_sniffer_addr) = start_sniffer(
        "0",
        pool_addr,
        false,
        vec![setup_connection_replace.into()],
        None,
    );
    let (translator, _, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    pool_translator_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
        )
        .await;
    let setup_connection_error = pool_translator_sniffer.next_message_from_upstream();
    let setup_connection_error = match setup_connection_error {
        Some((_, AnyMessage::Common(CommonMessages::SetupConnectionError(msg)))) => msg,
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        "unsupported-protocol",
        "SetupConnectionError message error code should be unsupported-protocol"
    );
    shutdown_all!(translator, pool);
}

// This test verifies that pool rejects SetCustomMiningJob when it is started without embedded JDS
// (`start_pool` with no `[jds]` config).
#[tokio::test]
async fn pool_without_jds_rejects_set_custom_mining_job() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);

    // no JDS
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let (sniffer, sniffer_addr) = start_sniffer("mock-pool", pool_addr, false, vec![], None);
    let send_to_pool = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0b0010),
    )
    .start()
    .await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let set_custom_mining_job =
        AnyMessage::Mining(Mining::SetCustomMiningJob(SetCustomMiningJob {
            channel_id: 1,
            request_id: 7,
            token: 42_u64.to_le_bytes().to_vec().try_into().unwrap(),
            version: 0,
            prev_hash: U256::Owned(vec![0_u8; 32]),
            min_ntime: 0,
            nbits: 0,
            coinbase_tx_version: 0,
            coinbase_prefix: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_input_n_sequence: 0,
            coinbase_tx_outputs: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_locktime: 0,
            merkle_path: Seq0255::new(Vec::new()).unwrap(),
        }));
    send_to_pool.send(set_custom_mining_job).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_ERROR,
        )
        .await;

    let set_custom_mining_job_error = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(Mining::SetCustomMiningJobError(msg)))) => break msg,
            _ => continue,
        }
    };
    assert_eq!(set_custom_mining_job_error.request_id, 7);
    assert_eq!(set_custom_mining_job_error.channel_id, 1);
    assert_eq!(
        set_custom_mining_job_error.error_code.as_utf8_or_hex(),
        "jd-not-supported",
        "SetCustomMiningJobError should use jd-not-supported when pool has no embedded JDS"
    );

    shutdown_all!(pool);
}

// This test launches a Pool and leverages a MockDownstream to test the correct functionalities of
// grouping extended channels.
#[tokio::test]
async fn pool_group_extended_channels() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    const NUM_EXTENDED_CHANNELS: u32 = 10;
    const EXPECTED_GROUP_CHANNEL_ID: u32 = 1;

    for i in 0..NUM_EXTENDED_CHANNELS {
        // send OpenExtendedMiningChannel message to the pool
        let open_extended_mining_channel = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
            OpenExtendedMiningChannel {
                request_id: i.into(),
                user_identity: b"user_identity".to_vec().try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
                min_extranonce_size: 0,
            },
        ));
        send_to_pool
            .send(open_extended_mining_channel)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            )
            .await;

        let group_channel_id = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::OpenExtendedMiningChannelSuccess(msg)))) => {
                    break msg.group_channel_id;
                }
                _ => continue,
            };
        };

        assert_eq!(
            group_channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Group channel ID should be correct"
        );

        // also assert the correct message sequence after OpenExtendedMiningChannelSuccess
        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
            )
            .await;
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;
    }

    // ok, up until this point, we were just initializing NUM_EXTENDED_CHANNELS extended channels
    // now, let's see if a mempool change will trigger ONE (and not NUM_EXTENDED_CHANNELS)
    // NewExtendedMiningJob message directed to the correct group channel ID

    // create a mempool transaction to trigger a new template
    tp.create_mempool_transaction().unwrap();

    // wait for a NewExtendedMiningJob message
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // assert that the NewExtendedMiningJob message is directed to the correct group channel ID
    let new_extended_mining_job_msg = sniffer.next_message_from_upstream();
    let new_extended_mining_job_msg = match new_extended_mining_job_msg {
        Some((_, AnyMessage::Mining(Mining::NewExtendedMiningJob(msg)))) => msg,
        msg => panic!("Expected NewExtendedMiningJob message, found: {:?}", msg),
    };

    assert_eq!(
        new_extended_mining_job_msg.channel_id, EXPECTED_GROUP_CHANNEL_ID,
        "NewExtendedMiningJob message should be directed to the correct group channel ID"
    );

    // wait a bit
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // make sure there's no extra NewExtendedMiningJob messages
    assert!(
        sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
                std::time::Duration::from_secs(1),
            )
            .await,
        "There should be no extra NewExtendedMiningJob messages"
    );

    // now let's see if a chain tip update will trigger ONE (and not many) SetNewPrevHashMp message
    // directed to the correct group channel ID

    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    let set_new_prev_hash_msg = sniffer.next_message_from_upstream();
    let set_new_prev_hash_msg = match set_new_prev_hash_msg {
        Some((_, AnyMessage::Mining(Mining::SetNewPrevHash(msg)))) => msg,
        msg => panic!("Expected SetNewPrevHash message, found: {:?}", msg),
    };

    assert_eq!(
        set_new_prev_hash_msg.channel_id, EXPECTED_GROUP_CHANNEL_ID,
        "SetNewPrevHash message should be directed to the correct group channel ID"
    );

    // wait a bit
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // make sure there's no extra SetNewPrevHash messages
    assert!(
        sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SET_NEW_PREV_HASH,
                std::time::Duration::from_secs(1),
            )
            .await,
        "There should be no second SetNewPrevHash message"
    );
    pool.shutdown().await;
}

// This test launches a Pool and leverages a MockDownstream to test the correct functionalities of
// grouping standard channels.
#[tokio::test]
async fn pool_group_standard_channels() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    const NUM_STANDARD_CHANNELS: u32 = 10;
    const EXPECTED_GROUP_CHANNEL_ID: u32 = 1;

    for i in 0..NUM_STANDARD_CHANNELS {
        let open_standard_mining_channel = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
            OpenStandardMiningChannel {
                request_id: i.into(),
                user_identity: b"user_identity".to_vec().try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
            },
        ));

        send_to_pool
            .send(open_standard_mining_channel)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            )
            .await;

        let (channel_id, group_channel_id) = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::OpenStandardMiningChannelSuccess(msg)))) => {
                    break (msg.channel_id, msg.group_channel_id);
                }
                _ => continue,
            };
        };

        assert_ne!(
            channel_id, group_channel_id,
            "Channel ID must be different from the group channel ID"
        );

        assert_eq!(
            group_channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Group channel ID should be correct"
        );

        // also assert the correct message sequence after OpenStandardMiningChannelSuccess
        sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_MINING_JOB)
            .await;
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;
    }

    // ok, up until this point, we were just initializing NUM_STANDARD_CHANNELS standard channels
    // now, let's see if a mempool change will trigger ONE (and not many) NewExtendedMiningJob
    // message directed to the correct group channel ID

    // send a mempool transaction to trigger a new template
    tp.create_mempool_transaction().unwrap();

    // wait for a NewExtendedMiningJob message targeted to the correct group channel ID
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // assert that the NewExtendedMiningJob message is directed to the correct group channel ID
    let new_extended_mining_job_msg = sniffer.next_message_from_upstream();
    let new_extended_mining_job_msg = match new_extended_mining_job_msg {
        Some((_, AnyMessage::Mining(Mining::NewExtendedMiningJob(msg)))) => msg,
        msg => panic!("Expected NewMiningJob message, found: {:?}", msg),
    };

    assert_eq!(
        new_extended_mining_job_msg.channel_id, EXPECTED_GROUP_CHANNEL_ID,
        "NewMiningJob message should be directed to the correct group channel ID"
    );

    // wait a bit
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // make sure there's no extra NewExtendedMiningJob messages
    assert!(
        sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
                std::time::Duration::from_secs(1),
            )
            .await,
        "There should be no second NewMiningJob message"
    );

    // make sure there's no NewMiningJob message
    assert!(
        sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_MINING_JOB,
                std::time::Duration::from_secs(1),
            )
            .await,
        "There should be no NewMiningJob message"
    );

    // now let's see if a chain tip update will trigger ONE (and not many) SetNewPrevHashMp message
    // directed to the correct group channel ID

    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    let set_new_prev_hash_msg = sniffer.next_message_from_upstream();
    let set_new_prev_hash_msg = match set_new_prev_hash_msg {
        Some((_, AnyMessage::Mining(Mining::SetNewPrevHash(msg)))) => msg,
        msg => panic!("Expected SetNewPrevHash message, found: {:?}", msg),
    };

    assert_eq!(
        set_new_prev_hash_msg.channel_id, EXPECTED_GROUP_CHANNEL_ID,
        "SetNewPrevHash message should be directed to the correct group channel ID"
    );

    // wait a bit
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // make sure there's no extra SetNewPrevHash messages
    assert!(
        sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
                std::time::Duration::from_secs(1),
            )
            .await,
        "There should be no extra SetNewPrevHash messages"
    );
    pool.shutdown().await;
}

// This test launches a Pool and leverages a MockDownstream to test the correct functionalities of
// NOT grouping standard channels when REQUIRES_STANDARD_JOBS is set.
#[tokio::test]
async fn pool_require_standard_jobs_set_does_not_group_standard_channels() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", pool_addr, false, vec![], None);

    // Use REQUIRES_STANDARD_JOBS flag (0b0001) to prevent channel grouping
    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0b0001),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    const NUM_STANDARD_CHANNELS: u32 = 10;
    const EXPECTED_GROUP_CHANNEL_ID: u32 = 1;
    let mut channel_ids = Vec::new();

    for i in 0..NUM_STANDARD_CHANNELS {
        let open_standard_mining_channel = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
            OpenStandardMiningChannel {
                request_id: i.into(),
                user_identity: b"user_identity".to_vec().try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
            },
        ));

        send_to_pool
            .send(open_standard_mining_channel)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            )
            .await;

        let (channel_id, group_channel_id) = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::OpenStandardMiningChannelSuccess(msg)))) => {
                    break (msg.channel_id, msg.group_channel_id);
                }
                _ => continue,
            };
        };

        channel_ids.push(channel_id);

        assert_eq!(
            group_channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Group channel ID should be correct" /* even though we are not going to use it */
        );

        assert_ne!(
            channel_id, group_channel_id,
            "Channel ID must be different from the group channel ID"
        );

        // also assert the correct message sequence after OpenStandardMiningChannelSuccess
        sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_MINING_JOB)
            .await;
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;
    }

    // ok, up until this point, we were just initializing NUM_STANDARD_CHANNELS standard channels
    // now, let's see if a mempool change will trigger NUM_STANDARD_CHANNELS NewMiningJob messages
    // and not ONE NewExtendedMiningJob message

    // send a mempool transaction to trigger a new template
    tp.create_mempool_transaction().unwrap();

    for _i in 0..NUM_STANDARD_CHANNELS {
        sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_MINING_JOB)
            .await;

        let new_mining_job_msg = sniffer.next_message_from_upstream();
        let channel_id = match new_mining_job_msg {
            Some((_, AnyMessage::Mining(Mining::NewMiningJob(msg)))) => msg.channel_id,
            msg => panic!("Expected NewMiningJob message, found: {:?}", msg),
        };

        assert!(
            channel_ids.contains(&channel_id),
            "Channel ID should be present in the list of channel IDs"
        );

        assert_ne!(
            channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Channel ID must be different from the group channel ID"
        );
    }

    // now let's see if a chain tip update will trigger NUM_STANDARD_CHANNELS pairs of NewMiningJob
    // message and SetNewPrevHash message

    tp.generate_blocks(1);

    for _i in 0..NUM_STANDARD_CHANNELS {
        sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_MINING_JOB)
            .await;

        let new_mining_job_msg = sniffer.next_message_from_upstream();
        let channel_id = match new_mining_job_msg {
            Some((_, AnyMessage::Mining(Mining::NewMiningJob(msg)))) => msg.channel_id,
            msg => panic!("Expected NewMiningJob message, found: {:?}", msg),
        };

        assert_ne!(
            channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Channel ID must be different from the group channel ID"
        );
    }

    for _i in 0..NUM_STANDARD_CHANNELS {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;

        let set_new_prev_hash_msg = sniffer.next_message_from_upstream();
        let channel_id = match set_new_prev_hash_msg {
            Some((_, AnyMessage::Mining(Mining::SetNewPrevHash(msg)))) => msg.channel_id,
            msg => panic!("Expected SetNewPrevHash message, found: {:?}", msg),
        };

        assert_ne!(
            channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Channel ID must be different from the group channel ID"
        );
    }
    pool.shutdown().await;
}
