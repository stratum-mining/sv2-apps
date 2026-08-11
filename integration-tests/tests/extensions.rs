use stratum_apps::stratum_core::parsers_sv2::{
    AnyMessageOwned, ExtensionsNegotiationOwned, ExtensionsOwned, MiningOwned,
};
// Integration test for translator extension negotiation with extension 0x0002
// (EXTENSION_TYPE_WORKER_HASHRATE_TRACKING) and user_identity TLV validation.
//
// This test validates:
// 1. Pool and translator negotiate extension 0x0002 during SetupConnection
// 2. SV1 miner submits shares through the translator
// 3. Translator forwards SubmitSharesExtended with TLV containing user_identity
// 4. Pool receives and processes the TLV user_identity correctly

use integration_tests_sv2::{interceptor::MessageDirection, template_provider::DifficultyLevel, *};
use stratum_apps::stratum_core::{
    binary_sv2::Seq064KOwned,
    common_messages_sv2::*,
    extensions_sv2::{
        EXTENSION_TYPE_WORKER_HASHRATE_TRACKING, TLV_FIELD_TYPE_USER_IDENTITY,
        extensions_negotiation::MESSAGE_TYPE_REQUEST_EXTENSIONS,
    },
    mining_sv2::*,
};
use tracing::info;

/// Tests that the translator successfully negotiates extension 0x0002 with the pool
/// and sends user_identity TLV in SubmitSharesExtended messages.
#[tokio::test]
async fn test_extension_negotiation_with_tlv_in_submit_shares() {
    start_tracing();
    // Extension 0x0002 for worker hashrate tracking
    let supported_extensions = vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING];
    let required_extensions = vec![EXTENSION_TYPE_WORKER_HASHRATE_TRACKING];
    let sv1_username = "account.SRI-miner";
    let expected_tlv_user_identity = "SRI-miner";

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    // Start pool with extension 0x0002 support
    let (pool, pool_addr, _) = start_pool(
        sv2_tp_config(tp_addr),
        supported_extensions.clone(),
        vec![],
        false,
    )
    .await;
    let (pool_translator_sniffer, pool_translator_sniffer_addr) =
        start_sniffer("pool-translator", pool_addr, false, vec![], None);
    // Start translator with extension 0x0002 support. TLV emission is gated by
    // extension negotiation, not by aggregate_channels.
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        false, // aggregate_channels = false
        supported_extensions.clone(),
        required_extensions,
        None,
        false,
    )
    .await;
    // Start SV1 miner (minerd) with a full SV1 username so the TLV can prove
    // that only the worker name suffix is forwarded.
    let (_minerd_process, _minerd_addr) = start_minerd(
        tproxy_addr,
        Some(sv1_username.to_string()),
        Some("password".to_string()),
        false,
    )
    .await;

    pool_translator_sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    pool_translator_sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // RequestExtensions is sent *after* SetupConnectionSuccess, so wait for it rather than
    // assuming it has already been queued by the time the success message lands.
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_REQUEST_EXTENSIONS,
        )
        .await;

    // Verify RequestExtensions includes extension 0x0002
    let request_extensions_msg = match pool_translator_sniffer.next_message_from_downstream() {
        Some((
            _,
            AnyMessageOwned::Extensions(ExtensionsOwned::ExtensionsNegotiation(
                ExtensionsNegotiationOwned::RequestExtensions(msg),
            )),
        )) => msg,
        msg => panic!("received unexpected message: {msg:?}"),
    };
    assert_eq!(
        request_extensions_msg.requested_extensions,
        Seq064KOwned::new(supported_extensions.clone()).unwrap()
    );

    // Verify RequestExtensionsSuccess acknowledges the extension
    let request_extensions_success_msg = pool_translator_sniffer.next_message_from_upstream();
    match request_extensions_success_msg {
        Some((
            _,
            AnyMessageOwned::Extensions(ExtensionsOwned::ExtensionsNegotiation(
                ExtensionsNegotiationOwned::RequestExtensionsSuccess(msg),
            )),
        )) => {
            assert_eq!(
                msg.supported_extensions,
                Seq064KOwned::new(supported_extensions).unwrap()
            );
        }
        _ => panic!("Expected RequestExtensionsSuccess message"),
    }

    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    // Extract and verify user_identity from OpenExtendedMiningChannel
    let open_channel_msg = pool_translator_sniffer.next_message_from_downstream();
    match open_channel_msg {
        Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(msg)))) => {
            let user_identity = msg.user_identity.as_utf8_or_hex();
            assert_eq!(user_identity, "user_identity.miner1".to_string());
        }
        msg => panic!("received unexpected message: {msg:?}"),
    }

    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;
    // Verify SubmitSharesExtended contains TLV with user_identity
    let submit_shares_msg = pool_translator_sniffer.next_message_from_downstream_with_tlvs();
    match submit_shares_msg {
        Some((_, AnyMessageOwned::Mining(MiningOwned::SubmitSharesExtended(msg)), tlv_fields)) => {
            info!(
                "SubmitSharesExtended received - channel_id: {}, sequence_number: {}, job_id: {}",
                msg.channel_id, msg.sequence_number, msg.job_id
            );
            let tlvs = tlv_fields.unwrap();
            // Find user_identity TLV
            let user_identity_tlv = tlvs.iter().find(|tlv| {
                tlv.r#type.extension_type == EXTENSION_TYPE_WORKER_HASHRATE_TRACKING
                    && tlv.r#type.field_type == TLV_FIELD_TYPE_USER_IDENTITY
            });
            assert!(
                user_identity_tlv.is_some(),
                "user_identity TLV should be present with extension 0x0002"
            );

            // Extract and validate user_identity value
            if let Some(tlv) = user_identity_tlv {
                // Validate TLV structure
                assert_eq!(
                    tlv.r#type.extension_type, EXTENSION_TYPE_WORKER_HASHRATE_TRACKING,
                    "TLV extension_type should be 0x0002"
                );
                assert_eq!(
                    tlv.r#type.field_type, TLV_FIELD_TYPE_USER_IDENTITY,
                    "TLV field_type should be user_identity"
                );
                let payload_len = tlv.value.len();
                assert_eq!(
                    payload_len,
                    expected_tlv_user_identity.len(),
                    "user_identity TLV payload length should match the SV1 worker name"
                );
                let user_identity_str =
                    std::str::from_utf8(&tlv.value).expect("user_identity TLV must be UTF-8");
                assert_eq!(
                    user_identity_str, expected_tlv_user_identity,
                    "user_identity TLV should contain the SV1 worker name suffix"
                );
            }
        }
        _ => panic!("Expected SubmitSharesExtended message with TLV fields"),
    }

    // Wait for SubmitSharesSuccess response from pool
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;
    shutdown_all!(translator, pool);
}
