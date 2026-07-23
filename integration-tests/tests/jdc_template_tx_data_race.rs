use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{MockDownstream, MockUpstream, WithSetup},
    utils::get_available_address,
    POOL_COINBASE_REWARD_ADDRESS,
};
use jd_client_sv2::config::ConfigJDCMode;
use std::time::Duration;
use stratum_apps::{
    config_helpers::CoinbaseRewardScript,
    stratum_core::{
        bitcoin::{consensus::serialize, Amount, TxOut},
        common_messages_sv2::Protocol,
        job_declaration_sv2::AllocateMiningJobTokenSuccess,
        mining_sv2::{OpenExtendedMiningChannel, OpenExtendedMiningChannelSuccess},
        parsers_sv2::{AnyMessage, JobDeclaration, Mining, TemplateDistribution},
        template_distribution_sv2::{
            NewTemplate, RequestTransactionDataSuccess, SetNewPrevHash,
            MESSAGE_TYPE_REQUEST_TRANSACTION_DATA,
        },
    },
};

// How long the test waits for a `RequestTransactionData` that must never be sent while the
// upstream channel is closed.
const PREMATURE_REQUEST_WINDOW: Duration = Duration::from_millis(2000);
const TEMPLATE_ID: u64 = 1;

/// Regression coverage for the JDC full-template transaction-data race (issue #626).
///
/// In full-template mode, JDC needs template transaction data to build a `DeclareMiningJob`
/// for the JDS. Building that job requires the upstream extended channel (and job factory),
/// which only exist once a downstream has triggered the upstream channel open. JDC must
/// therefore NOT request transaction data for templates that arrive before the upstream
/// channel is open: the response could not be consumed, and consuming the template state
/// anyway is what caused the original `TemplateNotFound` race.
///
/// This test deliberately creates that ordering:
///
/// 1. `NewTemplate` and `SetNewPrevHash` arrive before the upstream channel exists.
/// 2. JDC must not send `RequestTransactionData` while the channel is closed.
/// 3. When `OpenExtendedMiningChannelSuccess` arrives, JDC requests transaction data for the last
///    future template.
/// 4. That single response must then produce a `DeclareMiningJob`.
#[tokio::test]
async fn jdc_requests_tx_data_only_after_upstream_channel_opens() {
    integration_tests_sv2::start_tracing();

    // Use mock roles for every upstream peer. This keeps the test deterministic and avoids
    // depending on Bitcoin Core timing: each protocol peer only sends the message needed to move
    // JDC to the next step of the scenario.
    //
    // A sniffer is placed between JDC and each mock role. The sniffers let the test observe JDC's
    // outbound messages without changing the protocol flow.
    let mock_tp_addr = get_available_address();
    let mock_tp_sender = MockUpstream::new(
        mock_tp_addr,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
    .await;
    let (tp_sniffer, tp_sniffer_addr) =
        integration_tests_sv2::start_sniffer("jdc-tp", mock_tp_addr, false, vec![], None);

    let mock_pool_addr = get_available_address();
    let mock_pool_sender = MockUpstream::new(
        mock_pool_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    let (pool_sniffer, pool_sniffer_addr) =
        integration_tests_sv2::start_sniffer("jdc-pool", mock_pool_addr, false, vec![], None);

    let mock_jds_addr = get_available_address();
    let mock_jds_sender = MockUpstream::new(
        mock_jds_addr,
        WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
    )
    .start()
    .await;
    let (jds_sniffer, jds_sniffer_addr) =
        integration_tests_sv2::start_sniffer("jdc-jds", mock_jds_addr, false, vec![], None);

    // Start JDC in full-template mode. This mode is required because coinbase-only mode does not
    // send `RequestTransactionData` to the Template Provider.
    let (jdc, jdc_addr, _) = integration_tests_sv2::start_jdc(
        &[(pool_sniffer_addr, jds_sniffer_addr)],
        integration_tests_sv2::sv2_tp_config(tp_sniffer_addr),
        vec![],
        vec![],
        false,
        Some(ConfigJDCMode::FullTemplate),
    );

    // Wait for the two initial token allocations that JDC requests after completing the JDS
    // handshake. Tokens are required later to build a `DeclareMiningJob`, so providing them up
    // front isolates this scenario from token-allocation races unrelated to the template-state
    // lifecycle invariant under test.
    let first_token_request = loop {
        match jds_sniffer.next_message_from_downstream() {
            Some((
                _,
                AnyMessage::JobDeclaration(JobDeclaration::AllocateMiningJobToken(message)),
            )) => break message,
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };

    let second_token_request = loop {
        match jds_sniffer.next_message_from_downstream() {
            Some((
                _,
                AnyMessage::JobDeclaration(JobDeclaration::AllocateMiningJobToken(message)),
            )) => break message,
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };

    // Build the coinbase outputs returned by the mock JDS. The script intentionally differs from
    // JDC's startup reward script so the first token response updates JDC's active coinbase
    // outputs. The output value itself is not important here: JDC replaces it with the template's
    // `coinbase_tx_value_remaining` before constructing the declared job.
    let coinbase_script_pubkey =
        CoinbaseRewardScript::from_descriptor(&format!("addr({POOL_COINBASE_REWARD_ADDRESS})"))
            .expect("pool reward descriptor must be valid")
            .script_pubkey();
    let coinbase_outputs = serialize(&vec![TxOut {
        value: Amount::from_sat(0),
        script_pubkey: coinbase_script_pubkey,
    }]);

    // Return both allocated tokens. After this step, JDC has every JDS-side prerequisite needed
    // to declare a job; only the upstream mining channel is intentionally still missing.
    mock_jds_sender
        .send(AnyMessage::JobDeclaration(
            JobDeclaration::AllocateMiningJobTokenSuccess(AllocateMiningJobTokenSuccess {
                request_id: first_token_request.request_id,
                mining_job_token: 0_u64
                    .to_le_bytes()
                    .try_into()
                    .expect("u64 token must fit into B0255"),
                coinbase_outputs: coinbase_outputs
                    .clone()
                    .try_into()
                    .expect("serialized coinbase outputs must fit into B064K"),
            }),
        ))
        .await
        .expect("mock JDS should send the first token");
    mock_jds_sender
        .send(AnyMessage::JobDeclaration(
            JobDeclaration::AllocateMiningJobTokenSuccess(AllocateMiningJobTokenSuccess {
                request_id: second_token_request.request_id,
                mining_job_token: 1_u64
                    .to_le_bytes()
                    .try_into()
                    .expect("u64 token must fit into B0255"),
                coinbase_outputs: coinbase_outputs
                    .try_into()
                    .expect("serialized coinbase outputs must fit into B064K"),
            }),
        ))
        .await
        .expect("mock JDS should send the second token");

    // Send a valid future template and activate it with a matching `SetNewPrevHash`.
    //
    // The template fields are a known-good test vector also used by the channel job-factory tests.
    // Keeping the transaction list empty makes the scenario focused on JDC's template-state
    // lifecycle rather than on transaction validation.
    mock_tp_sender
        .send(AnyMessage::TemplateDistribution(
            TemplateDistribution::NewTemplate(NewTemplate {
                template_id: TEMPLATE_ID,
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
                    0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113,
                    209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153,
                    98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                ]
                .try_into()
                .expect("coinbase outputs must fit into B064K"),
                coinbase_tx_locktime: 0,
                merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
            }),
        ))
        .await
        .expect("mock TP should send NewTemplate");
    mock_tp_sender
        .send(AnyMessage::TemplateDistribution(
            TemplateDistribution::SetNewPrevHash(SetNewPrevHash {
                template_id: TEMPLATE_ID,
                prev_hash: [0x11; 32].into(),
                header_timestamp: 1_700_000_000,
                n_bits: 0x1d00ffff,
                target: [0xff; 32].into(),
            }),
        ))
        .await
        .expect("mock TP should send SetNewPrevHash");

    // No downstream has asked for a mining channel, so the upstream extended channel is
    // guaranteed not to exist yet. While the upstream channel is not open, JDC must NOT
    // request transaction data: the response could not be turned into a `DeclareMiningJob`
    // and would be discarded. Any `RequestTransactionData` observed here means the
    // premature-consumption race from issue #626 is possible again.
    assert!(
        tp_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_REQUEST_TRANSACTION_DATA,
                PREMATURE_REQUEST_WINDOW,
            )
            .await,
        "JDC must not request transaction data before the upstream channel is open"
    );

    // Connect a downstream mining client and ask JDC for an extended channel. This is what causes
    // JDC to open its single extended channel with the upstream pool.
    let downstream_sender = MockDownstream::new(
        jdc_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    downstream_sender
        .send(AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
            OpenExtendedMiningChannel {
                request_id: 1,
                user_identity: "tx-data-race".try_into().unwrap(),
                nominal_hash_rate: 1_000.0,
                max_target: [0xff; 32].into(),
                min_extranonce_size: 0,
            },
        )))
        .await
        .expect("mock downstream should open an extended channel");

    // Wait for JDC to forward the channel-open request to the mock pool, then complete the
    // upstream channel handshake. Only after this response does JDC have the extranonce and job
    // factory needed to declare the previously received template.
    let open_channel_request = loop {
        match pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(Mining::OpenExtendedMiningChannel(message)))) => {
                break message;
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };
    mock_pool_sender
        .send(AnyMessage::Mining(
            Mining::OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccess {
                request_id: open_channel_request.request_id,
                channel_id: 9,
                target: [0xff; 32].into(),
                extranonce_size: open_channel_request.min_extranonce_size,
                extranonce_prefix: vec![0; 4]
                    .try_into()
                    .expect("extranonce prefix must fit into B032"),
                group_channel_id: 1,
            }),
        ))
        .await
        .expect("mock pool should open the upstream extended channel");

    // Once the upstream channel is open, JDC must request transaction data for the last future
    // template it stored while the channel was closed.
    let tx_data_request = loop {
        match tp_sniffer.next_message_from_downstream() {
            Some((
                _,
                AnyMessage::TemplateDistribution(TemplateDistribution::RequestTransactionData(
                    message,
                )),
            )) => break message,
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };
    assert_eq!(tx_data_request.template_id, TEMPLATE_ID);

    // The response to this request must be enough to declare the job: every prerequisite
    // (upstream channel, job factory, token, prev hash) is now in place.
    mock_tp_sender
        .send(AnyMessage::TemplateDistribution(
            TemplateDistribution::RequestTransactionDataSuccess(RequestTransactionDataSuccess {
                template_id: TEMPLATE_ID,
                excess_data: vec![]
                    .try_into()
                    .expect("empty excess data must fit into B064K"),
                transaction_list: vec![]
                    .try_into()
                    .expect("empty transaction list must be valid"),
            }),
        ))
        .await
        .expect("mock TP should send RequestTransactionDataSuccess");

    // Once the upstream channel is available, JDC must declare the job built from the
    // previously received template. Premature consumption of that template state prevents the
    // declaration.
    let declare_mining_job = loop {
        match jds_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::JobDeclaration(JobDeclaration::DeclareMiningJob(message)))) => {
                break message
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };

    jdc.shutdown().await;

    assert_eq!(declare_mining_job.version, 536_870_912);
}
