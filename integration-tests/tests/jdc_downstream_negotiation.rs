use integration_tests_sv2::{
    mock_roles::{
        MockUpstream, WithSetup, connect_closable_downstream, next_message_type,
        request_extensions, send_chain_tip, send_message,
    },
    start_jdc_with_user_identities, start_tracing, sv2_tp_config,
    utils::get_available_address,
};
use jd_client_sv2::config::ConfigJDCMode;
use stratum_apps::stratum_core::{
    common_messages_sv2::Protocol,
    extensions_sv2::{
        EXTENSION_TYPE_WORKER_HASHRATE_TRACKING,
        extensions_negotiation::MESSAGE_TYPE_REQUEST_EXTENSIONS_SUCCESS,
    },
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
