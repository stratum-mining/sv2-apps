use stratum_apps::stratum_core::parsers_sv2::{AnyMessageOwned, JobDeclarationOwned, MiningOwned};
// This file contains integration tests for the `JDC/S` module.
use integration_tests_sv2::{
    interceptor::{IgnoreMessage, MessageDirection, ReplaceMessage},
    mock_roles::{MockDownstream, WithSetup},
    sniffer::Sniffer,
    start_jdc_with_user_identities,
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::stratum_core::{
    binary_sv2::{B032Owned, Seq064KOwned, U256Owned},
    common_messages_sv2::*,
    job_declaration_sv2::*,
    mining_sv2::*,
    parsers_sv2::{self},
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
        None,
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
        None,
    );
    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    shutdown_all!(jdc_1, pool);
}

// This test verifies that mode state is isolated per JDC instance.
//
// We start one JDC in solo mining mode (no upstreams) and then start another
// JDC in full template mode (with upstream). The solo instance must not start
// behaving like full-template mode after the second instance activates.
#[tokio::test]
async fn multiple_jdc_sessions() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(Some(1), DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let (solo_tp_sniffer, solo_tp_sniffer_addr) =
        start_sniffer("solo-jdc-tp", tp_addr, false, vec![], None);
    let (solo_jdc, solo_jdc_addr, _) = start_jdc(
        &[],
        sv2_tp_config(solo_tp_sniffer_addr),
        vec![],
        vec![],
        false,
        Some(jd_client_sv2::config::ConfigJDCMode::SoloMining),
    );
    let _solo_downstream = MockDownstream::new(
        solo_jdc_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;

    solo_tp_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    solo_tp_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    solo_tp_sniffer.clean_queue(MessageDirection::ToUpstream);
    solo_tp_sniffer.clean_queue(MessageDirection::ToDownstream);

    let (full_jds_sniffer, full_jds_sniffer_addr) =
        start_sniffer("full-jdc-jds", jds_addr, false, vec![], None);
    let (full_jdc, _full_jdc_addr, _) = start_jdc(
        &[(pool_addr, full_jds_sniffer_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        Some(jd_client_sv2::config::ConfigJDCMode::FullTemplate),
    );

    full_jds_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    full_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // Trigger post-start template updates; using two blocks reduces timing flakiness.
    tp.generate_blocks(1);
    tp.generate_blocks(1);

    // RequestTransactionData is FullTemplate-only. If mode leaked process-wide,
    // the solo JDC would emit this after the full-template JDC activates.
    assert!(
        solo_tp_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_REQUEST_TRANSACTION_DATA,
                std::time::Duration::from_secs(2),
            )
            .await,
        "Solo-mode JDC should not request transaction data after another JDC activates full-template mode"
    );

    shutdown_all!(solo_jdc, full_jdc, pool);
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
        None,
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
        Some((
            _,
            AnyMessageOwned::Common(parsers_sv2::CommonMessagesOwned::SetupConnectionError(msg)),
        )) => msg,
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL,
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
        Some((
            _,
            AnyMessageOwned::Common(parsers_sv2::CommonMessagesOwned::SetupConnectionError(msg)),
        )) => msg,
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_SETUP_CONNECTION_MISSING_DECLARE_TX_DATA_FLAG,
        "SetupConnectionError message error code should be missing-declare-tx-data-flag"
    );

    shutdown_all!(pool);
}

// This test verifies that JDS rejects SetupConnection requesting only protocol version 3,
// since JDS only supports version 2, responding with a SetupConnectionError carrying the
// protocol-version-mismatch error code.
#[tokio::test]
async fn jds_reject_setup_connection_with_unsupported_version() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("mock-jds", jds_addr, false, vec![], None);
    let _mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes(SetupConnectionOwned {
            protocol: Protocol::JobDeclarationProtocol,
            min_version: 3,
            max_version: 3,
            flags: 0,
            endpoint_host: "0.0.0.0".try_into().unwrap(),
            endpoint_port: 0,
            vendor: "integration-test".try_into().unwrap(),
            hardware_version: "".try_into().unwrap(),
            firmware: "".try_into().unwrap(),
            device_id: "".try_into().unwrap(),
        }),
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
        Some((
            _,
            AnyMessageOwned::Common(parsers_sv2::CommonMessagesOwned::SetupConnectionError(msg)),
        )) => msg,
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
        "SetupConnectionError message error code should be protocol-version-mismatch"
    );

    shutdown_all!(pool);
}

// This test verifies that JDC rejects a downstream SetupConnection requesting only protocol
// version 3, since JDC only supports version 2 on its downstream interface, responding with a
// SetupConnectionError carrying the protocol-version-mismatch error code.
#[tokio::test]
async fn jdc_reject_downstream_setup_connection_with_unsupported_version() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );
    let (sniffer, sniffer_addr) = start_sniffer("mock-jdc", jdc_addr, false, vec![], None);
    let _mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes(SetupConnectionOwned {
            protocol: Protocol::MiningProtocol,
            min_version: 3,
            max_version: 3,
            flags: 0,
            endpoint_host: "0.0.0.0".try_into().unwrap(),
            endpoint_port: 0,
            vendor: "integration-test".try_into().unwrap(),
            hardware_version: "".try_into().unwrap(),
            firmware: "".try_into().unwrap(),
            device_id: "".try_into().unwrap(),
        }),
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
        Some((
            _,
            AnyMessageOwned::Common(parsers_sv2::CommonMessagesOwned::SetupConnectionError(msg)),
        )) => msg,
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
        "SetupConnectionError message error code should be protocol-version-mismatch"
    );

    shutdown_all!(jdc, pool);
}

#[tokio::test]
async fn jdc_coinbase_only_mode_rejected_by_jds() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("jdc-jds", jds_addr, false, vec![], None);
    let (jdc, _jdc_addr, _) = start_jdc(
        &[(pool_addr, sniffer_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        Some(jd_client_sv2::config::ConfigJDCMode::CoinbaseOnly),
    );

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
        Some((
            _,
            AnyMessageOwned::Common(parsers_sv2::CommonMessagesOwned::SetupConnectionError(msg)),
        )) => msg,
        msg => panic!("Expected SetupConnectionError message, found: {:?}", msg),
    };
    assert_eq!(
        setup_connection_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_SETUP_CONNECTION_MISSING_DECLARE_TX_DATA_FLAG,
        "SetupConnectionError message error code should be missing-declare-tx-data-flag"
    );

    shutdown_all!(jdc, pool);
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
    let malformed_token_declare = AnyMessageOwned::JobDeclaration(
        parsers_sv2::JobDeclarationOwned::DeclareMiningJob(DeclareMiningJobOwned {
            request_id: 10,
            mining_job_token: vec![0x01].try_into().unwrap(),
            version: 0,
            coinbase_tx_prefix: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_suffix: Vec::<u8>::new().try_into().unwrap(),
            wtxid_list: Seq064KOwned::new(Vec::new()).unwrap(),
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
            AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::DeclareMiningJobError(msg),
            ),
        )) => msg,
        msg => panic!("Expected DeclareMiningJobError message, found: {:?}", msg),
    };
    assert_eq!(malformed_token_error.request_id, 10);
    assert_eq!(
        malformed_token_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
        "DeclareMiningJobError should use invalid-mining-job-token for malformed token"
    );

    sniffer.clean_queue(MessageDirection::ToUpstream);
    sniffer.clean_queue(MessageDirection::ToDownstream);

    // Send a well-formed but never-allocated token to exercise the ownership/allocation
    // validation branch (distinct from malformed token parsing).
    let unallocated_token_declare = AnyMessageOwned::JobDeclaration(
        parsers_sv2::JobDeclarationOwned::DeclareMiningJob(DeclareMiningJobOwned {
            request_id: 11,
            mining_job_token: 42_u64.to_le_bytes().try_into().unwrap(),
            version: 0,
            coinbase_tx_prefix: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_suffix: Vec::<u8>::new().try_into().unwrap(),
            wtxid_list: Seq064KOwned::new(Vec::new()).unwrap(),
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
            AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::DeclareMiningJobError(msg),
            ),
        )) => msg,
        msg => panic!("Expected DeclareMiningJobError message, found: {:?}", msg),
    };
    assert_eq!(unallocated_token_error.request_id, 11);
    assert_eq!(
        unallocated_token_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
        "DeclareMiningJobError should use invalid-mining-job-token for unallocated token"
    );

    shutdown_all!(pool);
}

// Allocated tokens let malformed declarations reach coinbase validation. Each receives a
// controlled `invalid-coinbase-tx` or `invalid-coinbase-tx-input` response, and the connection
// remains usable.
#[tokio::test]
async fn jds_rejects_malformed_declared_coinbase_without_panicking() {
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

    // Allocate a real mining-job token and capture it from the success response. Each declaration
    // consumes its token on error, so we allocate a fresh one per malformed case.
    async fn allocate_token(
        send_to_jds: &async_channel::Sender<AnyMessageOwned>,
        sniffer: &Sniffer<'_>,
        request_id: u32,
    ) -> Vec<u8> {
        send_to_jds
            .send(AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::AllocateMiningJobToken(
                    AllocateMiningJobTokenOwned {
                        request_id,
                        user_identifier: "malformed_coinbase".try_into().unwrap(),
                    },
                ),
            ))
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match sniffer.next_message_from_upstream() {
                    Some((
                        _,
                        AnyMessageOwned::JobDeclaration(
                            parsers_sv2::JobDeclarationOwned::AllocateMiningJobTokenSuccess(msg),
                        ),
                    )) if msg.request_id == request_id => {
                        break msg.mining_job_token.as_ref().to_vec();
                    }
                    Some(_) => continue,
                    None => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("timed out waiting for AllocateMiningJobTokenSuccess")
    }

    // Sends a `DeclareMiningJob` with the given coinbase parts and asserts the JDS responds with
    // `expected_error_code` for `request_id`.
    async fn assert_invalid_coinbase(
        send_to_jds: &async_channel::Sender<AnyMessageOwned>,
        sniffer: &Sniffer<'_>,
        request_id: u32,
        expected_error_code: &str,
        coinbase_tx_prefix: Vec<u8>,
        coinbase_tx_suffix: Vec<u8>,
    ) {
        let token = allocate_token(send_to_jds, sniffer, request_id).await;

        send_to_jds
            .send(AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::DeclareMiningJob(DeclareMiningJobOwned {
                    request_id,
                    mining_job_token: token.try_into().unwrap(),
                    version: 0x20000000,
                    coinbase_tx_prefix: coinbase_tx_prefix.try_into().unwrap(),
                    coinbase_tx_suffix: coinbase_tx_suffix.try_into().unwrap(),
                    wtxid_list: Seq064KOwned::new(Vec::new()).unwrap(),
                    excess_data: Vec::<u8>::new().try_into().unwrap(),
                }),
            ))
            .await
            .unwrap();

        let error = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match sniffer.next_message_from_upstream() {
                    Some((
                        _,
                        AnyMessageOwned::JobDeclaration(
                            parsers_sv2::JobDeclarationOwned::DeclareMiningJobError(msg),
                        ),
                    )) if msg.request_id == request_id => break msg,
                    Some(_) => continue,
                    None => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("timed out waiting for DeclareMiningJobError");

        assert_eq!(
            error.error_code.as_utf8_or_hex(),
            expected_error_code,
            "unexpected DeclareMiningJobError code for request {request_id}"
        );
    }

    // The fixed header is 43 bytes long.
    assert_invalid_coinbase(
        &send_to_jds,
        &sniffer,
        1,
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
        vec![0u8; 10],
        vec![],
    )
    .await;

    // A structurally valid header with a scriptSig length of 0xffff exceeds the consensus bound.
    let mut oversized_prefix = Vec::new();
    oversized_prefix.extend([0x02, 0x00, 0x00, 0x00]); // version
    oversized_prefix.extend([0x00, 0x01]); // segwit marker + flag
    oversized_prefix.push(0x01); // input count
    oversized_prefix.extend([0u8; 32]); // prev txid
    oversized_prefix.extend([0xff, 0xff, 0xff, 0xff]); // prev vout index
    let header = oversized_prefix.clone();
    oversized_prefix.extend([0xfd, 0xff, 0xff]); // VarInt(0xffff) = 65535
    assert_invalid_coinbase(
        &send_to_jds,
        &sniffer,
        2,
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
        oversized_prefix,
        vec![],
    )
    .await;

    let mut huge = header.clone();
    huge.extend([0xff; 9]); // VarInt(u64::MAX)
    assert_invalid_coinbase(
        &send_to_jds,
        &sniffer,
        3,
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
        huge,
        vec![],
    )
    .await;

    let mut truncated = header.clone();
    truncated.extend([0xfd, 0x10]);
    assert_invalid_coinbase(
        &send_to_jds,
        &sniffer,
        4,
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
        truncated,
        vec![],
    )
    .await;

    let mut inconsistent = header.clone();
    inconsistent.extend([2, 1, 2, 3]);
    assert_invalid_coinbase(
        &send_to_jds,
        &sniffer,
        5,
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX,
        inconsistent,
        vec![],
    )
    .await;

    let mut multiple_inputs = header;
    multiple_inputs[6] = 2;
    multiple_inputs.extend([2, 1, 2]); // first scriptSig
    let mut suffix = vec![0xff; 4]; // first sequence
    suffix.extend([0; 32]); // second outpoint txid
    suffix.extend([0xff; 4]); // second outpoint index
    suffix.extend([2, 1, 2]); // second scriptSig
    suffix.extend([0xff; 4]); // second sequence
    suffix.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // one empty output
    suffix.extend([1, 1, 0, 1, 1, 0]); // witness for each input
    suffix.extend([0; 4]); // locktime
    let bytes = [multiple_inputs.as_slice(), suffix.as_slice()].concat();
    let tx: stratum_apps::stratum_core::bitcoin::Transaction =
        stratum_apps::stratum_core::bitcoin::consensus::deserialize(&bytes).unwrap();
    assert_eq!(tx.input.len(), 2);
    assert_invalid_coinbase(
        &send_to_jds,
        &sniffer,
        6,
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_COINBASE_TX_INPUT,
        multiple_inputs,
        suffix,
    )
    .await;

    // The JDS must still be alive and able to serve a fresh token allocation after all malformed
    // declarations.
    let _ = allocate_token(&send_to_jds, &sniffer, 7).await;

    shutdown_all!(pool);
}

// This test verifies that a single JDP connection can allocate tokens under different
// identities without being disconnected.
#[tokio::test]
async fn jds_accepts_multiple_identities_on_one_connection() {
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

    send_to_jds
        .send(AnyMessageOwned::JobDeclaration(
            parsers_sv2::JobDeclarationOwned::AllocateMiningJobToken(AllocateMiningJobTokenOwned {
                request_id: 1,
                user_identifier: "farm_01".try_into().unwrap(),
            }),
        ))
        .await
        .unwrap();
    send_to_jds
        .send(AnyMessageOwned::JobDeclaration(
            parsers_sv2::JobDeclarationOwned::AllocateMiningJobToken(AllocateMiningJobTokenOwned {
                request_id: 2,
                user_identifier: "farm_02".try_into().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let mut allocate_success_request_ids = Vec::new();
    while allocate_success_request_ids.len() < 2 {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessageOwned::JobDeclaration(
                    parsers_sv2::JobDeclarationOwned::AllocateMiningJobTokenSuccess(msg),
                ),
            )) => allocate_success_request_ids.push(msg.request_id),
            Some(_) => continue,
            // Yield so the sniffer's own task gets a chance to run and populate the
            // queue, instead of busy-spinning the executor while nothing has arrived yet.
            None => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    assert_eq!(
        allocate_success_request_ids,
        vec![1, 2],
        "both AllocateMiningJobToken requests (different identities, same connection) \
         should succeed, in order"
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
        None,
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
            Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(msg)))) => break msg,
            _ => continue,
        }
    };

    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        )
        .await;

    // Then, connect a separate mining downstream, open a channel under the same identity JDC
    // used, and replay the same SetCustomMiningJob on that channel. With a known channel and a
    // matching identity, the only reason left to reject it is the already-consumed token.
    // Expected result: JDS rejects it as invalid-mining-job-token.
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

    send_to_pool
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "IT_TEST".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await
        .unwrap();
    mock_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    let mock_channel_id = loop {
        match mock_pool_sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg.channel_id,
            _ => continue,
        }
    };

    let mut replayed = first_set_custom_mining_job.clone();
    replayed.channel_id = mock_channel_id;
    let replayed_set_custom_mining_job =
        AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(replayed));
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
            Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJobError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        set_custom_mining_job_error.request_id,
        first_set_custom_mining_job.request_id
    );
    assert_eq!(
        set_custom_mining_job_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_DECLARE_MINING_JOB_INVALID_MINING_JOB_TOKEN,
        "SetCustomMiningJobError should use invalid-mining-job-token for reused token"
    );

    shutdown_all!(translator, jdc, pool);
}

// This test verifies that Pool/JDS reject a SetCustomMiningJob when the requesting channel's
// user_identity differs from the identity its token was allocated under, even though the token
// itself is still active and the job is valid. Replaying the same job on a channel with the
// matching identity then succeeds, proving the token survived and that the rejection was only
// about identity.
#[tokio::test]
async fn pool_rejects_set_custom_mining_job_with_mismatched_user_identity() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    // JDC reaches the Pool through two chained sniffers: the first records JDC's
    // SetCustomMiningJob, the second drops it, so the Pool never consumes the token.
    let ignore_set_custom_mining_job = IgnoreMessage::new(
        MessageDirection::ToUpstream,
        MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
    );
    let (_jdc_pool_drop_sniffer, jdc_pool_drop_sniffer_addr) = start_sniffer(
        "jdc-pool-drop",
        pool_addr,
        false,
        vec![ignore_set_custom_mining_job.into()],
        None,
    );
    let (jdc_pool_record_sniffer, jdc_pool_record_sniffer_addr) = start_sniffer(
        "jdc-pool-record",
        jdc_pool_drop_sniffer_addr,
        false,
        vec![],
        None,
    );
    let (jdc, jdc_addr, _) = start_jdc(
        &[(jdc_pool_record_sniffer_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    // A mock downstream opening a channel on JDC is enough to trigger job declaration, and
    // unlike a real miner it never finds a block, so the recorded job stays valid.
    let send_to_jdc = MockDownstream::new(
        jdc_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    send_to_jdc
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "IT_TEST".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await
        .unwrap();

    jdc_pool_record_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
        )
        .await;
    let jdc_set_custom_mining_job = loop {
        match jdc_pool_record_sniffer.next_message_from_downstream() {
            Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(msg)))) => break msg,
            _ => continue,
        }
    };

    // Present JDC's still-active token from a separate connection, on a channel opened under
    // a different identity.
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
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    send_to_pool
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 1,
                user_identity: "other_identity".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await
        .unwrap();
    mock_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    let mock_channel_id = loop {
        match mock_pool_sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg.channel_id,
            _ => continue,
        }
    };

    let mut mismatched = jdc_set_custom_mining_job.clone();
    mismatched.channel_id = mock_channel_id;
    send_to_pool
        .send(AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(
            mismatched,
        )))
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
            Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJobError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        set_custom_mining_job_error.request_id,
        jdc_set_custom_mining_job.request_id
    );
    assert_eq!(
        set_custom_mining_job_error.error_code.as_utf8_or_hex(),
        ERROR_CODE_SET_CUSTOM_MINING_JOB_INVALID_MINING_JOB_TOKEN,
        "SetCustomMiningJobError should use invalid-mining-job-token for a mismatched identity"
    );

    // Same token and job, only the channel identity changes.
    send_to_pool
        .send(AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: 2,
                user_identity: "IT_TEST".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        ))
        .await
        .unwrap();
    mock_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    let matching_channel_id = loop {
        match mock_pool_sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg.channel_id,
            _ => continue,
        }
    };

    let mut matching = jdc_set_custom_mining_job.clone();
    matching.channel_id = matching_channel_id;
    send_to_pool
        .send(AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(
            matching,
        )))
        .await
        .unwrap();

    mock_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        )
        .await;
    let set_custom_mining_job_success = loop {
        match mock_pool_sniffer.next_message_from_upstream() {
            Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJobSuccess(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        set_custom_mining_job_success.request_id,
        jdc_set_custom_mining_job.request_id
    );
    assert_eq!(
        set_custom_mining_job_success.channel_id,
        matching_channel_id
    );

    shutdown_all!(jdc, pool);
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

    let prev_hash = U256Owned::from([
        184, 103, 138, 88, 153, 105, 236, 29, 123, 246, 107, 203, 1, 33, 10, 122, 188, 139, 218,
        141, 62, 177, 158, 101, 125, 92, 214, 150, 199, 220, 29, 8,
    ]);
    let extranonce = B032Owned::try_from([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0,
        0, 0,
    ])
    .unwrap();
    let submit_solution_replace = ReplaceMessage::new(
        MessageDirection::ToUpstream,
        MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        AnyMessageOwned::JobDeclaration(parsers_sv2::JobDeclarationOwned::PushSolution(
            PushSolutionOwned {
                ntime: 0,
                nbits: 0,
                nonce: 0,
                version: 0,
                prev_hash,
                extranonce,
            },
        )),
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
        None,
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
        AnyMessageOwned::JobDeclaration(
            parsers_sv2::JobDeclarationOwned::ProvideMissingTransactionsSuccess(
                ProvideMissingTransactionsSuccessOwned {
                    request_id: 1,
                    transaction_list: Seq064KOwned::new(Vec::new()).unwrap(),
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
        None,
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

// A transaction the JDS's node holds when a declaration is first validated can be gone from its
// mempool, replaced here through RBF, by the time the client's answer is validated. The JDS asks
// for it in one more round instead of refusing the declaration, and accepts the declaration once
// it is supplied.
#[tokio::test]
async fn jds_asks_again_for_a_transaction_its_node_lost_between_rounds() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let core = tp.bitcoin_core();
    // One mature coinbase per transaction below, so each spends a confirmed coin of its own and
    // either can be replaced on its own.
    core.fund_wallet().unwrap();
    core.generate_blocks(1);
    let (_, held_txid) = core.create_mempool_transaction().unwrap();
    let (_, unknown_txid) = core.create_mempool_transaction().unwrap();
    let held_tx = core.mempool_transaction(held_txid).unwrap();
    let unknown_tx = core.mempool_transaction(unknown_txid).unwrap();
    // Replaced before the declaration, so the node never holds it: the first round asks for it.
    core.replace_mempool_transaction(unknown_txid).unwrap();

    let (pool, _pool_addr, jds_addr, _) = start_pool_with_jds(core, vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("mock-jdc", jds_addr, false, vec![], None);
    let send_to_jds = connect_mock_jdc(&sniffer, sniffer_addr).await;

    declare(
        core,
        &sniffer,
        &send_to_jds,
        2,
        &[held_tx.clone(), unknown_tx.clone()],
    )
    .await;
    assert_eq!(next_unknown_tx_positions(&sniffer).await, (2, vec![1]));

    // The node loses the transaction it held while the client answers.
    core.replace_mempool_transaction(held_txid).unwrap();
    supply(&sniffer, &send_to_jds, 2, &[unknown_tx]).await;
    assert_eq!(next_unknown_tx_positions(&sniffer).await, (2, vec![0]));

    supply(&sniffer, &send_to_jds, 2, &[held_tx]).await;
    assert_eq!(next_declare_mining_job_success(&sniffer).await, 2);

    shutdown_all!(pool);
}

// A client that moved on to a new chain tip has dropped the declarations it made at the previous
// one and never answers for them. Here that is a second-round request, with the first round's
// transactions already supplied. The JDS refuses the waiting declaration with stale-chain-tip
// once it validates a declaration at the new tip.
#[tokio::test]
async fn jds_refuses_a_declaration_left_waiting_across_a_chain_tip() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let core = tp.bitcoin_core();
    core.fund_wallet().unwrap();
    core.generate_blocks(1);
    let (_, held_txid) = core.create_mempool_transaction().unwrap();
    let (_, unknown_txid) = core.create_mempool_transaction().unwrap();
    let held_tx = core.mempool_transaction(held_txid).unwrap();
    let unknown_tx = core.mempool_transaction(unknown_txid).unwrap();
    core.replace_mempool_transaction(unknown_txid).unwrap();

    let (pool, _pool_addr, jds_addr, _) = start_pool_with_jds(core, vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("mock-jdc", jds_addr, false, vec![], None);
    let send_to_jds = connect_mock_jdc(&sniffer, sniffer_addr).await;

    declare(
        core,
        &sniffer,
        &send_to_jds,
        2,
        &[held_tx, unknown_tx.clone()],
    )
    .await;
    assert_eq!(next_unknown_tx_positions(&sniffer).await, (2, vec![1]));
    core.replace_mempool_transaction(held_txid).unwrap();
    supply(&sniffer, &send_to_jds, 2, &[unknown_tx]).await;
    assert_eq!(next_unknown_tx_positions(&sniffer).await, (2, vec![0]));

    // The chain tip moves while the client is asked again, and the client never answers. The JDS
    // hears of the new tip from its node a moment later, so the declarations made at it until
    // then are answered stale-chain-tip themselves.
    core.generate_blocks(1);
    let (_, fresh_txid) = core.create_mempool_transaction().unwrap();
    let fresh_tx = core.mempool_transaction(fresh_txid).unwrap();
    let mut refused_with = None;
    let mut accepted = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    for request_id in 3.. {
        declare(
            core,
            &sniffer,
            &send_to_jds,
            request_id,
            &[fresh_tx.clone()],
        )
        .await;
        loop {
            match next_jds_answer(&sniffer).await {
                JobDeclarationOwned::DeclareMiningJobSuccess(msg)
                    if msg.request_id == request_id =>
                {
                    accepted = true;
                    break;
                }
                JobDeclarationOwned::DeclareMiningJobError(msg) if msg.request_id == 2 => {
                    refused_with = Some(msg.error_code.as_utf8_or_hex().to_string());
                }
                JobDeclarationOwned::DeclareMiningJobError(msg) if msg.request_id == request_id => {
                    assert_eq!(
                        msg.error_code.as_utf8_or_hex(),
                        ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP
                    );
                    break;
                }
                msg => panic!("unexpected answer from the JDS: {msg:?}"),
            }
        }
        if accepted || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(
        accepted,
        "the JDS never accepted a declaration at the new tip"
    );
    assert_eq!(
        refused_with.as_deref(),
        Some(ERROR_CODE_DECLARE_MINING_JOB_STALE_CHAIN_TIP)
    );

    shutdown_all!(pool);
}

// The next message the JDS sends, waited for up to a minute.
async fn next_jds_answer(sniffer: &Sniffer<'_>) -> JobDeclarationOwned {
    let started = std::time::Instant::now();
    loop {
        if let Some((_, AnyMessageOwned::JobDeclaration(msg))) =
            sniffer.next_message_from_upstream()
        {
            return msg;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "timed out waiting for the JDS's next answer"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// A client of the JDS behind `sniffer`, past its SetupConnection handshake.
async fn connect_mock_jdc(
    sniffer: &Sniffer<'_>,
    sniffer_addr: std::net::SocketAddr,
) -> async_channel::Sender<AnyMessageOwned> {
    let send_to_jds = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0b0001),
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
    send_to_jds
}

// Allocates a token under `request_id` and declares `declared` with it under the same request
// id, returning once the JDS has received the declaration. The coinbase is one Bitcoin Core's
// checkBlock accepts at `core`'s next height: the BIP34 height push, the outputs the token hands
// out, and the witness commitment the declared transactions need since they carry witnesses.
async fn declare(
    core: &integration_tests_sv2::template_provider::BitcoinCore,
    sniffer: &Sniffer<'_>,
    send_to_jds: &async_channel::Sender<AnyMessageOwned>,
    request_id: u32,
    declared: &[stratum_apps::stratum_core::bitcoin::Transaction],
) {
    use stratum_apps::stratum_core::bitcoin::{
        Amount, Block, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
        TxMerkleNode, TxOut, Witness,
        block::{Header, Version as BlockVersion},
        consensus::{deserialize, serialize},
        hashes::Hash,
        script::Builder,
        transaction::Version as TxVersion,
    };

    send_to_jds
        .send(AnyMessageOwned::JobDeclaration(
            parsers_sv2::JobDeclarationOwned::AllocateMiningJobToken(AllocateMiningJobTokenOwned {
                request_id,
                user_identifier: "farm_01".try_into().unwrap(),
            }),
        ))
        .await
        .unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
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
    let token = match sniffer.next_message_from_upstream() {
        Some((
            _,
            AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::AllocateMiningJobTokenSuccess(msg),
            ),
        )) => msg,
        msg => panic!("Expected AllocateMiningJobTokenSuccess message, found: {msg:?}"),
    };

    // The commitment leaves the coinbase itself out, so it is computed before the coinbase is
    // complete.
    let next_height = core.get_blockchain_info().unwrap().blocks + 1;
    let mut coinbase_tx = Transaction {
        version: TxVersion::TWO,
        lock_time: stratum_apps::stratum_core::bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: Builder::new().push_int(next_height).into_script(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[[0u8; 32]]),
        }],
        output: deserialize(token.coinbase_outputs.as_ref()).unwrap(),
    };
    let block = Block {
        header: Header {
            version: BlockVersion::TWO,
            prev_blockhash: Hash::all_zeros(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 0,
            bits: CompactTarget::from_consensus(0),
            nonce: 0,
        },
        txdata: std::iter::once(coinbase_tx.clone())
            .chain(declared.iter().cloned())
            .collect(),
    };
    let commitment = Block::compute_witness_commitment(&block.witness_root().unwrap(), &[0u8; 32]);
    let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    commitment_script.extend_from_slice(commitment.as_ref());
    coinbase_tx.output.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(commitment_script),
    });
    // The JDS reads the scriptSig length at byte 43 of the prefix and fills what the prefix
    // leaves of the scriptSig with the extranonce: none, here.
    let serialized_coinbase = serialize(&coinbase_tx);
    let prefix_len = 43 + 1 + coinbase_tx.input[0].script_sig.len();
    let (coinbase_tx_prefix, coinbase_tx_suffix) = serialized_coinbase.split_at(prefix_len);

    send_to_jds
        .send(AnyMessageOwned::JobDeclaration(
            parsers_sv2::JobDeclarationOwned::DeclareMiningJob(DeclareMiningJobOwned {
                request_id,
                mining_job_token: token.mining_job_token.clone(),
                version: 0x2000_0000,
                coinbase_tx_prefix: coinbase_tx_prefix.to_vec().try_into().unwrap(),
                coinbase_tx_suffix: coinbase_tx_suffix.to_vec().try_into().unwrap(),
                wtxid_list: Seq064KOwned::new(
                    declared
                        .iter()
                        .map(|tx| U256Owned::from(tx.compute_wtxid().to_byte_array()))
                        .collect(),
                )
                .unwrap(),
                excess_data: Vec::<u8>::new().try_into().unwrap(),
            }),
        ))
        .await
        .unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
}

// Answers the JDS's last `ProvideMissingTransactions` for `request_id` with `transactions`,
// returning once the JDS has received the answer.
async fn supply(
    sniffer: &Sniffer<'_>,
    send_to_jds: &async_channel::Sender<AnyMessageOwned>,
    request_id: u32,
    transactions: &[stratum_apps::stratum_core::bitcoin::Transaction],
) {
    use stratum_apps::stratum_core::bitcoin::consensus::serialize;

    send_to_jds
        .send(AnyMessageOwned::JobDeclaration(
            parsers_sv2::JobDeclarationOwned::ProvideMissingTransactionsSuccess(
                ProvideMissingTransactionsSuccessOwned {
                    request_id,
                    transaction_list: Seq064KOwned::new(
                        transactions
                            .iter()
                            .map(|tx| serialize(tx).try_into().unwrap())
                            .collect(),
                    )
                    .unwrap(),
                },
            ),
        ))
        .await
        .unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        )
        .await;
}

// The request id and positions of the next `ProvideMissingTransactions` the JDS sends.
async fn next_unknown_tx_positions(sniffer: &Sniffer<'_>) -> (u32, Vec<u16>) {
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS,
        )
        .await;
    match sniffer.next_message_from_upstream() {
        Some((
            _,
            AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::ProvideMissingTransactions(msg),
            ),
        )) => (msg.request_id, msg.unknown_tx_position_list.into_inner()),
        msg => panic!("Expected ProvideMissingTransactions message, found: {msg:?}"),
    }
}

// The request id of the next `DeclareMiningJob.Success` the JDS sends.
async fn next_declare_mining_job_success(sniffer: &Sniffer<'_>) -> u32 {
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB_SUCCESS,
        )
        .await;
    match sniffer.next_message_from_upstream() {
        Some((
            _,
            AnyMessageOwned::JobDeclaration(
                parsers_sv2::JobDeclarationOwned::DeclareMiningJobSuccess(msg),
            ),
        )) => msg.request_id,
        msg => panic!("Expected DeclareMiningJobSuccess message, found: {msg:?}"),
    }
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
        None,
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
        let open_extended_mining_channel = AnyMessageOwned::Mining(
            MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                request_id: i,
                user_identity: "user_identity".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            }),
        );
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
                Some((
                    _,
                    AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(msg)),
                )) => {
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
        Some((_, AnyMessageOwned::Mining(MiningOwned::NewExtendedMiningJob(msg)))) => msg,
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
        Some((_, AnyMessageOwned::Mining(MiningOwned::SetNewPrevHash(msg)))) => msg,
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
        None,
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
        let open_standard_mining_channel = AnyMessageOwned::Mining(
            MiningOwned::OpenStandardMiningChannel(OpenStandardMiningChannelOwned {
                request_id: i,
                user_identity: "user_identity".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
            }),
        );

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
                Some((
                    _,
                    AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannelSuccess(msg)),
                )) => {
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
        Some((_, AnyMessageOwned::Mining(MiningOwned::NewExtendedMiningJob(msg)))) => msg,
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
        Some((_, AnyMessageOwned::Mining(MiningOwned::SetNewPrevHash(msg)))) => msg,
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
        None,
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
        let open_standard_mining_channel = AnyMessageOwned::Mining(
            MiningOwned::OpenStandardMiningChannel(OpenStandardMiningChannelOwned {
                request_id: i,
                user_identity: "user_identity".try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
            }),
        );

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
                Some((
                    _,
                    AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannelSuccess(msg)),
                )) => {
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
                Some((_, AnyMessageOwned::Mining(MiningOwned::NewMiningJob(msg)))) => {
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
                Some((_, AnyMessageOwned::Mining(MiningOwned::NewMiningJob(msg)))) => {
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
                Some((_, AnyMessageOwned::Mining(MiningOwned::SetNewPrevHash(msg)))) => {
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

// This test verifies that JDC rejects OpenExtendedMiningChannel when
// REQUIRES_STANDARD_JOBS is set during SetupConnection.
#[tokio::test]
async fn jdc_require_standard_jobs_set_rejects_open_extended_mining_channel() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(&bitcoin_core, vec![], vec![], true).await;

    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        ipc_config(
            bitcoin_core.data_dir().clone(),
            bitcoin_core.is_signet(),
            None,
        ),
        vec![],
        vec![],
        false,
        None,
    );

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", jdc_addr, false, vec![], None);
    // SetupConnection flags: 0b0001 == REQUIRES_STANDARD_JOBS.
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

    let open_extended_mining_channel = AnyMessageOwned::Mining(
        MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
            request_id: 100u32,
            user_identity: "user_identity".try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        }),
    );
    send_to_jdc
        .send(open_extended_mining_channel)
        .await
        .unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
        )
        .await;

    let error = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    assert_eq!(
        error.error_code.as_utf8_or_hex(),
        ERROR_CODE_OPEN_MINING_CHANNEL_EXTENDED_CHANNELS_NOT_SUPPORTED_FOR_STANDARD_JOBS
    );

    shutdown_all!(jdc, pool);
}

// Verifies that when the primary pool disconnects, JDC falls back to the next upstream entry
// and sends *that* upstream's `user_identity` in `AllocateMiningJobToken` — not the primary's.
#[tokio::test]
async fn jdc_per_upstream_identity_switches_on_fallback() {
    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (primary_pool, primary_pool_addr, primary_jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (_fallback_pool, fallback_pool_addr, fallback_jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    const PRIMARY_IDENTITY: &str = "bc1qprimary.worker";
    const FALLBACK_IDENTITY: &str = "bc1qfallback.worker";

    let (primary_jds_sniffer, primary_jds_sniffer_addr) =
        start_sniffer("primary-jds", primary_jds_addr, false, vec![], None);
    let (fallback_jds_sniffer, fallback_jds_sniffer_addr) =
        start_sniffer("fallback-jds", fallback_jds_addr, false, vec![], None);

    let (jdc, _jdc_addr, _) = start_jdc_with_user_identities(
        &[
            (
                primary_pool_addr,
                primary_jds_sniffer_addr,
                PRIMARY_IDENTITY.to_string(),
            ),
            (
                fallback_pool_addr,
                fallback_jds_sniffer_addr,
                FALLBACK_IDENTITY.to_string(),
            ),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
        None,
        "JDC",
    );

    primary_jds_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    primary_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        )
        .await;

    primary_pool.shutdown().await;

    fallback_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        )
        .await;

    let allocate_msg = loop {
        match fallback_jds_sniffer.next_message_from_downstream() {
            Some((
                _,
                AnyMessageOwned::JobDeclaration(JobDeclarationOwned::AllocateMiningJobToken(msg)),
            )) => break msg,
            _ => continue,
        }
    };
    let allocate_identity = std::str::from_utf8(allocate_msg.user_identifier.as_ref())
        .expect("user_identifier is not valid UTF-8");
    assert_eq!(
        allocate_identity, FALLBACK_IDENTITY,
        "expected fallback identity in AllocateMiningJobToken '{FALLBACK_IDENTITY}', got '{allocate_identity}'"
    );

    shutdown_all!(jdc);
}

// Verifies that a downstream connection that never starts its handshake is dropped when JDC falls
// back, instead of outliving the generation that accepted it, and that the next generation listens
// on the same address afterwards.
//
// What this observes from outside is the handshake stopping on the fallback token. That the
// connection task counts as pending until then is not visible here; it follows from `track`
// wrapping the task at its spawn, and the coordinator's unit tests cover the counting itself.
#[tokio::test]
async fn jdc_fallback_drops_a_downstream_stalled_in_handshake() {
    use tokio::io::AsyncReadExt as _;

    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (primary_pool, primary_pool_addr, primary_jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (_fallback_pool, fallback_pool_addr, fallback_jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (fallback_jds_sniffer, fallback_jds_sniffer_addr) =
        start_sniffer("fallback-jds", fallback_jds_addr, false, vec![], None);

    let (jdc, jdc_addr, _) = start_jdc(
        &[
            (primary_pool_addr, primary_jds_addr),
            (fallback_pool_addr, fallback_jds_sniffer_addr),
        ],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    // JDC accepts downstreams only once it holds a template: a completed handshake proves the
    // accept loop is running before the stalled connection is opened.
    let (jdc_sniffer, jdc_sniffer_addr) = start_sniffer("jdc", jdc_addr, false, vec![], None);
    let _mock_downstream = MockDownstream::new(
        jdc_sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    jdc_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let connected_at = std::time::Instant::now();
    let mut stalled = tokio::net::TcpStream::connect(jdc_addr)
        .await
        .expect("failed to connect to JDC");
    // Give the accept loop time to hand the connection to its handshake task.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    primary_pool.shutdown().await;
    fallback_jds_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    let read = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stalled.read(&mut [0u8; 1]),
    )
    .await;
    // EOF proves the old generation had accepted the connection and dropped it; a reset would
    // mean it never left the listener's backlog, which would not exercise the handshake task.
    assert!(
        matches!(read, Ok(Ok(0))),
        "stalled connection was not closed cleanly by fallback: {read:?}"
    );
    // Well inside the 10 s noise handshake timeout, so the close was fallback's doing and not
    // the handshake giving up on its own.
    assert!(
        connected_at.elapsed() < std::time::Duration::from_secs(8),
        "stalled connection closed too late to tell fallback from the handshake timeout"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match tokio::net::TcpStream::connect(jdc_addr).await {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => panic!("failed to connect to {jdc_addr}: {e}"),
            }
        }
    })
    .await
    .expect("JDC did not listen on its downstream address again after fallback");

    shutdown_all!(jdc);
}
