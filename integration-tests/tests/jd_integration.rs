// This file contains integration tests for the `JDC/S` module.
use integration_tests_sv2::{
    interceptor::{MessageDirection, ReplaceMessage},
    mock_roles::{MockDownstream, WithSetup},
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::stratum_core::{
    binary_sv2::{Seq064K, B032, U256},
    common_messages_sv2::*,
    job_declaration_sv2::{ProvideMissingTransactionsSuccess, PushSolution, *},
    mining_sv2::*,
    parsers_sv2::{self, AnyMessage, Mining},
    template_distribution_sv2::*,
};

// This test verifies that jd-server does not exit when a connected jd-client shuts down.
//
// It is performing the verification by shutding down a jd-client connected to a jd-server and then
// starting a new jd-client that connects to the same jd-server successfully.
#[tokio::test]
async fn jds_should_not_panic_if_jdc_shutsdown() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer_a, sniffer_addr_a) = start_sniffer("0", jds_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, sniffer_addr_a)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );
    sniffer_a
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    jdc.shutdown().await;
    assert!(tokio::net::TcpListener::bind(jdc_addr).await.is_ok());
    let (sniffer, sniffer_addr) = start_sniffer("0", jds_addr, false, vec![], None);
    let (jdc_1, _jdc_addr_1, _) = start_jdc(
        &[(pool_addr, sniffer_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );
    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    shutdown_all!(jdc_1, pool);
}

// This test verifies that jd-client exchange SetupConnection messages with a Template Provider.
//
// Note that jd-client starts to exchange messages with the Template Provider after it has accepted
// a downstream connection.
#[tokio::test]
async fn jdc_tp_success_setup() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (tp_jdc_sniffer, tp_jdc_sniffer_addr) = start_sniffer("0", tp_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(tp_jdc_sniffer_addr),
        vec![],
        vec![],
        false,
    );
    // This is needed because jd-client waits for a downstream connection before it starts
    // exchanging messages with the Template Provider.
    let (translator, _, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    tp_jdc_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    tp_jdc_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    shutdown_all!(translator, jdc, pool);
}

// This test verifies that JDS rejects SetupConnection with a non-JD protocol.
#[tokio::test]
async fn jds_reject_setup_connection_with_non_job_declaration_protocol() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("mock-jds", jds_addr, false, vec![], None);
    let _mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;

    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
        )
        .await;

    let setup_connection_error = sniffer.next_message_from_upstream();
    let setup_connection_error = match setup_connection_error {
        Some((_, AnyMessage::Common(parsers_sv2::CommonMessages::SetupConnectionError(msg)))) => {
            msg
        }
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        "unsupported-protocol",
        "SetupConnectionError message error code should be unsupported-protocol"
    );

    shutdown_all!(pool);
}

// This test verifies that JDS rejects SetupConnection without DECLARE_TX_DATA flag.
#[tokio::test]
async fn jds_reject_setup_connection_without_declare_tx_data_flag() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("mock-jds", jds_addr, false, vec![], None);
    let _mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
    )
    .start()
    .await;

    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
        )
        .await;

    let setup_connection_error = sniffer.next_message_from_upstream();
    let setup_connection_error = match setup_connection_error {
        Some((_, AnyMessage::Common(parsers_sv2::CommonMessages::SetupConnectionError(msg)))) => {
            msg
        }
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        "missing-declare-tx-data-flag",
        "SetupConnectionError message error code should be missing-declare-tx-data-flag"
    );

    shutdown_all!(pool);
}

// This test verifies that JDS rejects DeclareMiningJob when mining_job_token is invalid.
#[tokio::test]
async fn jds_reject_declare_mining_job_with_invalid_mining_job_token() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("mock-jds", jds_addr, false, vec![], None);
    let send_to_jds = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0b0001),
    )
    .start()
    .await;

    // complete SetupConnection handshake
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

    // Deliberately send a malformed token (1 byte instead of 8-byte JdToken) to exercise
    // the decode/parse failure branch before allocation ownership checks.
    let malformed_token_declare = AnyMessage::JobDeclaration(
        parsers_sv2::JobDeclaration::DeclareMiningJob(DeclareMiningJob {
            request_id: 10,
            mining_job_token: vec![0x01].try_into().unwrap(),
            version: 0,
            coinbase_tx_prefix: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_suffix: Vec::<u8>::new().try_into().unwrap(),
            wtxid_list: Seq064K::new(Vec::new()).unwrap(),
            excess_data: Vec::<u8>::new().try_into().unwrap(),
        }),
    );
    send_to_jds.send(malformed_token_declare).await.unwrap();

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB_ERROR,
        )
        .await;

    // Even with an undecodable token, JDS should respond with a protocol-level error
    // (not disconnect or panic) so downstream gets an explicit rejection reason.
    let malformed_token_error = sniffer.next_message_from_upstream();
    let malformed_token_error = match malformed_token_error {
        Some((
            _,
            AnyMessage::JobDeclaration(parsers_sv2::JobDeclaration::DeclareMiningJobError(msg)),
        )) => msg,
        msg => panic!("Expected DeclareMiningJobError message, found: {:?}", msg),
    };
    assert_eq!(malformed_token_error.request_id, 10);
    assert_eq!(
        malformed_token_error.error_code.as_utf8_or_hex(),
        "invalid-mining-job-token",
        "DeclareMiningJobError should use invalid-mining-job-token for malformed token"
    );

    sniffer.clean_queue(MessageDirection::ToUpstream);
    sniffer.clean_queue(MessageDirection::ToDownstream);

    // Send a well-formed but never-allocated token to exercise the ownership/allocation
    // validation branch (distinct from malformed token parsing).
    let unallocated_token_declare = AnyMessage::JobDeclaration(
        parsers_sv2::JobDeclaration::DeclareMiningJob(DeclareMiningJob {
            request_id: 11,
            mining_job_token: 42_u64.to_le_bytes().to_vec().try_into().unwrap(),
            version: 0,
            coinbase_tx_prefix: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_suffix: Vec::<u8>::new().try_into().unwrap(),
            wtxid_list: Seq064K::new(Vec::new()).unwrap(),
            excess_data: Vec::<u8>::new().try_into().unwrap(),
        }),
    );
    send_to_jds.send(unallocated_token_declare).await.unwrap();

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB_ERROR,
        )
        .await;

    let unallocated_token_error = sniffer.next_message_from_upstream();
    let unallocated_token_error = match unallocated_token_error {
        Some((
            _,
            AnyMessage::JobDeclaration(parsers_sv2::JobDeclaration::DeclareMiningJobError(msg)),
        )) => msg,
        msg => panic!("Expected DeclareMiningJobError message, found: {:?}", msg),
    };
    assert_eq!(unallocated_token_error.request_id, 11);
    assert_eq!(
        unallocated_token_error.error_code.as_utf8_or_hex(),
        "invalid-mining-job-token",
        "DeclareMiningJobError should use invalid-mining-job-token for unallocated token"
    );

    shutdown_all!(pool);
}

// This test verifies that a SetCustomMiningJob token cannot be reused after a successful
// SetCustomMiningJob flow has already consumed it.
#[tokio::test]
async fn pool_rejects_reused_set_custom_mining_job_token() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    // First, run the regular JDC flow and capture one valid SetCustomMiningJob.
    let (jdc_pool_sniffer, jdc_pool_sniffer_addr) =
        start_sniffer("jdc-pool", pool_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(jdc_pool_sniffer_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
        )
        .await;

    let first_set_custom_mining_job = loop {
        match jdc_pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(Mining::SetCustomMiningJob(msg)))) => break msg,
            _ => continue,
        }
    };

    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        )
        .await;

    // Then, connect a separate mining downstream and replay the exact same SetCustomMiningJob.
    // Expected result: JDS rejects it as invalid-mining-job-token (token already consumed).
    let (mock_pool_sniffer, mock_pool_sniffer_addr) =
        start_sniffer("mock-pool", pool_addr, false, vec![], None);
    let send_to_pool = MockDownstream::new(
        mock_pool_sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;

    mock_pool_sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;
    mock_pool_sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let replayed_set_custom_mining_job = AnyMessage::Mining(Mining::SetCustomMiningJob(
        first_set_custom_mining_job.clone(),
    ));
    send_to_pool
        .send(replayed_set_custom_mining_job)
        .await
        .unwrap();

    mock_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_ERROR,
        )
        .await;

    let set_custom_mining_job_error = loop {
        match mock_pool_sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(Mining::SetCustomMiningJobError(msg)))) => break msg,
            _ => continue,
        }
    };
    assert_eq!(
        set_custom_mining_job_error.request_id,
        first_set_custom_mining_job.request_id
    );
    assert_eq!(
        set_custom_mining_job_error.error_code.as_utf8_or_hex(),
        "invalid-mining-job-token",
        "SetCustomMiningJobError should use invalid-mining-job-token for reused token"
    );

    shutdown_all!(translator, jdc, pool);
}

// This test verifies that JDS does not exit when it receives a `SubmitSolution`
// while still expecting a `ProvideMissingTransactionsSuccess`.
//
// It is performing the verification by connecting to JDS after the message exchange
// to check whether it remains alive.
#[tokio::test]
async fn jds_receive_solution_while_processing_declared_job_test() {
    start_tracing();
    let (tp_1, _tp_addr_1) = start_template_provider(None, DifficultyLevel::Low);
    let (tp_2, tp_addr_2) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp_1.bitcoin_core(), vec![], vec![], false).await;

    let prev_hash = U256::Owned(vec![
        184, 103, 138, 88, 153, 105, 236, 29, 123, 246, 107, 203, 1, 33, 10, 122, 188, 139, 218,
        141, 62, 177, 158, 101, 125, 92, 214, 150, 199, 220, 29, 8,
    ]);
    let extranonce = B032::Owned(vec![
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0,
        0, 0,
    ]);
    let submit_solution_replace = ReplaceMessage::new(
        MessageDirection::ToUpstream,
        MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        AnyMessage::JobDeclaration(parsers_sv2::JobDeclaration::PushSolution(PushSolution {
            ntime: 0,
            nbits: 0,
            nonce: 0,
            version: 0,
            prev_hash,
            extranonce,
        })),
    );

    // This sniffer sits between `jds` and `jdc`, replacing `ProvideMissingTransactionSuccess`
    // with `SubmitSolution`.
    let (sniffer_a, sniffer_a_addr) = start_sniffer(
        "A",
        jds_addr,
        false,
        vec![submit_solution_replace.into()],
        None,
    );
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, sniffer_a_addr)],
        sv2_tp_config(tp_addr_2),
        vec![],
        vec![],
        false,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    assert!(tp_2.fund_wallet().is_ok());
    assert!(tp_2.create_mempool_transaction().is_ok());
    sniffer_a
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        )
        .await;
    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN_SUCCESS,
        )
        .await;
    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS,
        )
        .await;
    sniffer_a
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_PUSH_SOLUTION)
        .await;
    assert!(tokio::net::TcpListener::bind(jds_addr).await.is_err());
    shutdown_all!(translator, jdc, pool);
}

// This test ensures that JDS does not exit upon receiving a `ProvideMissingTransactionsSuccess`
// message containing a transaction set that differs from the `tx_short_hash_list`
// in the Declare Mining Job.
//
// It is performing the verification by connecting to JDS after the message exchange
// to check whether it remains alive
#[tokio::test]
async fn jds_wont_exit_upon_receiving_unexpected_txids_in_provide_missing_transaction_success() {
    start_tracing();
    let (tp_1, _tp_addr_1) = start_template_provider(None, DifficultyLevel::Low);
    let (tp_2, tp_addr_2) = start_template_provider(None, DifficultyLevel::Low);

    assert!(tp_2.fund_wallet().is_ok());
    assert!(tp_2.create_mempool_transaction().is_ok());

    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp_1.bitcoin_core(), vec![], vec![], false).await;

    let provide_missing_transaction_success_replace = ReplaceMessage::new(
        MessageDirection::ToUpstream,
        MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        AnyMessage::JobDeclaration(
            parsers_sv2::JobDeclaration::ProvideMissingTransactionsSuccess(
                ProvideMissingTransactionsSuccess {
                    request_id: 1,
                    transaction_list: Seq064K::new(Vec::new()).unwrap(),
                },
            ),
        ),
    );

    // This sniffer sits between `jds` and `jdc`, replacing `ProvideMissingTransactionSuccess`
    // with `ProvideMissingTransactionSuccess` with different transaction list.
    let (sniffer, sniffer_addr) = start_sniffer(
        "A",
        jds_addr,
        false,
        vec![provide_missing_transaction_success_replace.into()],
        None,
    );

    let (jdc, jdc_addr_1, _) = start_jdc(
        &[(pool_addr, sniffer_addr)],
        sv2_tp_config(tp_addr_2),
        vec![],
        vec![],
        false,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr_1], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

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
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN_SUCCESS,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        )
        .await;

    assert!(tokio::net::TcpListener::bind(jds_addr).await.is_err());
    shutdown_all!(translator, jdc, pool);
}

// This test launches a JDC and leverages a MockDownstream to test the correct functionalities of
// grouping extended channels.
#[tokio::test]
async fn jdc_group_extended_channels() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", jdc_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_jdc = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    const NUM_EXTENDED_CHANNELS: u32 = 10;
    const EXPECTED_GROUP_CHANNEL_ID: u32 = 1;

    for i in 0..NUM_EXTENDED_CHANNELS {
        let open_extended_mining_channel = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
            OpenExtendedMiningChannel {
                request_id: i,
                user_identity: b"user_identity".to_vec().try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
                min_extranonce_size: 0,
            },
        ));
        send_to_jdc
            .send(open_extended_mining_channel)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            )
            .await;

        // loop until we get the OpenExtendedMiningChannelSuccess message
        // if we get any other message (e.g.: NewExtendedMiningJob), just continue the loop
        let (channel_id, group_channel_id) = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::OpenExtendedMiningChannelSuccess(msg)))) => {
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

    // ok, up until this point, we were just initializing N_EXTENDED_CHANNELS extended channels
    // now, let's see if a mempool change will trigger ONE (and not many) NewExtendedMiningJob
    // message directed to the correct group channel ID

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
        "There should be no extra SetNewPrevHash messages"
    );
    shutdown_all!(jdc, pool);
}

// This test launches a JDC and leverages a MockDownstream to test the correct functionalities of
// grouping standard channels.
#[tokio::test]
async fn jdc_group_standard_channels() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", jdc_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_jdc = mock_downstream.start().await;

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

        send_to_jdc
            .send(open_standard_mining_channel)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            )
            .await;

        // loop until we get the OpenStandardMiningChannelSuccess message
        // if we get any other message (e.g.: NewExtendedMiningJob), just continue the loop
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

    // ok, up until this point, we were just initializing two standard channels
    // now, let's see if a mempool change will trigger ONE (and not many) NewMiningJob
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

    // make sure there's no NewMiningJob message
    assert!(
        sniffer
            .assert_message_not_present(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_MINING_JOB,
                std::time::Duration::from_secs(1)
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
                MESSAGE_TYPE_SET_NEW_PREV_HASH,
                std::time::Duration::from_secs(1),
            )
            .await,
        "There should be no extra SetNewPrevHash messages"
    );
    shutdown_all!(jdc, pool);
}

// This test launches a JDC and leverages a MockDownstream to test the correct functionalities of
// NOT grouping standard channels when REQUIRES_STANDARD_JOBS is set.
#[tokio::test]
async fn jdc_require_standard_jobs_set_does_not_group_standard_channels() {
    start_tracing();
    let sv2_interval = Some(5);
    let (tp, tp_addr) = start_template_provider(sv2_interval, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], true).await;

    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", jdc_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0b0001),
    );
    let send_to_jdc = mock_downstream.start().await;

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

        send_to_jdc
            .send(open_standard_mining_channel)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            )
            .await;

        // loop until we get the OpenStandardMiningChannelSuccess message
        // if we get any other message (e.g.: NewExtendedMiningJob), just continue the loop
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
            "Group channel ID should be correct" /* even though we are not going to use it */
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

    // ok, up until this point, we were just initializing two standard channels
    // now, let's see if a mempool change will trigger NUM_STANDARD_CHANNELS NewMiningJob messages
    // and not ONE NewExtendedMiningJob message

    // send a mempool transaction to trigger a new template
    tp.create_mempool_transaction().unwrap();

    for _i in 0..NUM_STANDARD_CHANNELS {
        sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_MINING_JOB)
            .await;

        let channel_id = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::NewMiningJob(msg)))) => {
                    break msg.channel_id;
                }
                _ => continue,
            };
        };

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

        let channel_id = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::NewMiningJob(msg)))) => {
                    break msg.channel_id;
                }
                _ => continue,
            };
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

        let channel_id = loop {
            match sniffer.next_message_from_upstream() {
                Some((_, AnyMessage::Mining(Mining::SetNewPrevHash(msg)))) => {
                    break msg.channel_id;
                }
                _ => continue,
            };
        };

        assert_ne!(
            channel_id, EXPECTED_GROUP_CHANNEL_ID,
            "Channel ID must be different from the group channel ID"
        );
    }

    shutdown_all!(jdc, pool);
}
