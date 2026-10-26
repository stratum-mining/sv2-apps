use integration_tests_sv2::{
    mock_roles::{
        MockUpstream, WithSetup, connect_closable_downstream, extended_channel_request,
        next_message_type, request_extensions, send_chain_tip, send_message, send_template,
        template,
    },
    start_jdc_with_user_identities, start_tracing, sv2_tp_config,
    utils::get_available_address,
};
use jd_client_sv2::config::ConfigJDCMode;
use std::time::Duration;
use stratum_apps::stratum_core::{
    common_messages_sv2::Protocol,
    extensions_sv2::{
        EXTENSION_TYPE_WORKER_HASHRATE_TRACKING,
        extensions_negotiation::{
            MESSAGE_TYPE_REQUEST_EXTENSIONS_ERROR, MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS,
        },
    },
    mining_sv2::MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
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
    let tp = MockUpstream::new(
        tp_address,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
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
    send_chain_tip(&tp, 1).await;

    let (receiver, sender) = connect_closable_downstream(jdc_address).await;
    send_message(
        &sender,
        request_extensions(vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING]),
    )
    .await;
    assert_eq!(
        next_message_type(&receiver).await,
        MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS
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
    let tp = MockUpstream::new(
        tp_address,
        WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
    )
    .start()
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
    send_chain_tip(&tp, 1).await;

    let (rejected_receiver, rejected_sender) = connect_closable_downstream(jdc_address).await;
    send_message(&rejected_sender, extended_channel_request([0xff; 32])).await;
    assert_eq!(
        next_message_type(&rejected_receiver).await,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS
    );
    send_message(&rejected_sender, request_extensions(vec![])).await;
    // The channel's job and prevhash come before the refusal.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message_type = next_message_type(&rejected_receiver).await;
            if message_type == MESSAGE_TYPE_REQUEST_EXTENSIONS_ERROR {
                break;
            }
        }
    })
    .await
    .expect("JDC sends its refusal before closing the connection");
    // While JDC waits for the refusal to go out, a new template gives it a job for the refused
    // downstream's channel.
    send_template(&tp, template(2, false)).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while rejected_receiver.recv().await.is_ok() {}
    })
    .await
    .expect("JDC closes the connection of a downstream without a required extension");

    let (receiver, sender) = connect_closable_downstream(jdc_address).await;
    send_message(
        &sender,
        request_extensions(vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING]),
    )
    .await;
    assert_eq!(
        next_message_type(&receiver).await,
        MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS
    );
    send_message(&sender, extended_channel_request([0xff; 32])).await;
    assert_eq!(
        next_message_type(&receiver).await,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS
    );

    jdc.shutdown().await;
}
