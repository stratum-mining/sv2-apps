use integration_tests_sv2::{
    mocks::{ClientSetupConnection, MockClient, MockServer, ServerSetupConnection},
    start_jdc_with_user_identities, start_tracing, sv2_tp_config,
    utils::get_available_address,
};
use jd_client_sv2::config::ConfigJDCMode;
use stratum_apps::stratum_core::{
    binary_sv2::Seq064KOwned,
    common_messages_sv2::{MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol},
    extensions_sv2::{
        EXTENSION_TYPE_WORKER_HASHRATE_TRACKING, RequestExtensionsOwned,
        extensions_negotiation::{
            MESSAGE_TYPE_REQUEST_EXTENSIONS_ERROR, MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS,
        },
    },
    mining_sv2::{
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS, OpenExtendedMiningChannelOwned,
    },
    parsers_sv2::{AnyMessageOwned, MiningOwned, TemplateDistributionOwned},
    template_distribution_sv2::{NewTemplateOwned, SetNewPrevHashOwned},
};

// An extension JDC requires only from its Pool, which no downstream asks for.
const POOL_ONLY_EXTENSION: u16 = 0x0003;

// JDC holds its Pool, not its downstreams, to `required_extensions`: a downstream that asks for an
// extension JDC grants downstreams gets it, although it leaves out the one JDC requires from its
// Pool.
#[tokio::test]
async fn jdc_grants_downstreams_extensions_without_the_ones_it_requires_from_its_pool() {
    start_tracing();
    let tp_address = get_available_address();
    let tp = MockServer::new(
        tp_address,
        Some(ServerSetupConnection::new([(
            Protocol::TemplateDistributionProtocol,
            0,
        )])),
    )
    .await;
    let (jdc, jdc_address, _) = start_jdc_with_user_identities(
        &[],
        sv2_tp_config(tp_address),
        vec![POOL_ONLY_EXTENSION],
        vec![POOL_ONLY_EXTENSION],
        vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING],
        vec![],
        false,
        Some(ConfigJDCMode::SoloMining),
        "JDC",
    );
    tp.send(
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
                    0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113,
                    209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153,
                    98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                ]
                .try_into()
                .expect("coinbase outputs must fit into B064K"),
                coinbase_tx_locktime: 0,
                merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
            },
        )),
    )
    .await;
    tp.send(
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
        jdc_address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    downstream
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;
    downstream
        .send(AnyMessageOwned::Extensions(
            RequestExtensionsOwned {
                request_id: 1,
                requested_extensions: Seq064KOwned::new(vec![
                    EXTENSION_TYPE_WORKER_HASHRATE_TRACKING,
                ])
                .expect("the extensions fit into Seq064K"),
            }
            .into(),
        ))
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS)
        .await;
    assert_eq!(
        downstream
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS)
    );

    jdc.shutdown().await;
}

// A downstream whose `RequestExtensions` leaves out an extension JDC requires of downstreams is
// told which, and disconnected, so it can neither mine nor negotiate again on that connection, even
// when a job for its channel reaches it while JDC waits for the refusal to go out. A downstream
// that asks for the extension still negotiates it and opens its channel.
#[tokio::test]
async fn jdc_disconnects_only_the_downstream_without_a_required_extension() {
    start_tracing();
    let tp_address = get_available_address();
    let tp = MockServer::new(
        tp_address,
        Some(ServerSetupConnection::new([(
            Protocol::TemplateDistributionProtocol,
            0,
        )])),
    )
    .await;
    let (jdc, jdc_address, _) = start_jdc_with_user_identities(
        &[],
        sv2_tp_config(tp_address),
        vec![],
        vec![],
        vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING],
        vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING],
        false,
        Some(ConfigJDCMode::SoloMining),
        "JDC",
    );
    tp.send(
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
                    0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113,
                    209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153,
                    98, 180, 139, 235, 216, 54, 151, 78, 140, 249,
                ]
                .try_into()
                .expect("coinbase outputs must fit into B064K"),
                coinbase_tx_locktime: 0,
                merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
            },
        )),
    )
    .await;
    tp.send(
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

    let rejected = MockClient::new(
        jdc_address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    rejected
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;
    rejected
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
    rejected
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert_eq!(
        rejected
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
    );
    rejected
        .send(AnyMessageOwned::Extensions(
            RequestExtensionsOwned {
                request_id: 1,
                requested_extensions: Seq064KOwned::new(vec![])
                    .expect("the extensions fit into Seq064K"),
            }
            .into(),
        ))
        .await;
    // The channel's job and prevhash come before the refusal.
    rejected
        .wait_for_message_type(MESSAGE_TYPE_REQUEST_EXTENSIONS_ERROR)
        .await;
    // While JDC waits for the refusal to go out, a new template gives it a job for the refused
    // downstream's channel.
    tp.send(
        0,
        AnyMessageOwned::TemplateDistribution(TemplateDistributionOwned::NewTemplate(
            NewTemplateOwned {
                template_id: 2,
                future_template: false,
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
            },
        )),
    )
    .await;
    // JDC closes the connection of a downstream without a required extension.
    rejected.wait_for_close().await;

    let downstream = MockClient::new(
        jdc_address,
        Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
    )
    .await;
    downstream
        .wait_for_message_type_and_clean_queue(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
        .await;
    downstream
        .send(AnyMessageOwned::Extensions(
            RequestExtensionsOwned {
                request_id: 1,
                requested_extensions: Seq064KOwned::new(vec![
                    EXTENSION_TYPE_WORKER_HASHRATE_TRACKING,
                ])
                .expect("the extensions fit into Seq064K"),
            }
            .into(),
        ))
        .await;
    downstream
        .wait_for_message_type(MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS)
        .await;
    assert_eq!(
        downstream
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS)
    );
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
    downstream
        .wait_for_message_type(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert_eq!(
        downstream
            .next_message()
            .map(|(message_type, _)| message_type),
        Some(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
    );

    jdc.shutdown().await;
}
