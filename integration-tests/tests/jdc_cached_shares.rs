use integration_tests_sv2::{
    interceptor::MessageDirection,
    mocks::{MockServer, ServerSetupConnection},
    template_provider::DifficultyLevel,
    utils::get_available_address,
    *,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::Protocol,
    mining_sv2::*,
    parsers_sv2::{AnyMessageOwned, MiningOwned},
};

// Verifies that the JD client buffers `SubmitSharesExtended` while waiting for
// `SetCustomMiningJob.Success` and relays the cached shares once the success
// message is received from the upstream.
//
// The test simulates a miner submitting shares before the custom job is
// acknowledged by the upstream pool, ensuring that no shares are lost and
// that they are forwarded only after the job becomes valid.
#[tokio::test]
async fn jdc_cached_shares_relayed_on_set_custom_job_success() {
    start_tracing();
    // Use a high difficulty to ensure the miner cannot find a valid block.
    // Otherwise, a discovered block would trigger a chain tip change,
    // causing `SetCustomMiningJob.Success` to fail.
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::High);
    let (_pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let mock_pool_addr = get_available_address();
    let mock_pool = MockServer::new(
        mock_pool_addr,
        Some(ServerSetupConnection::new([(Protocol::MiningProtocol, 0)])),
    )
    .await;

    let (jdc, jdc_addr, _) = start_jdc(
        &[(mock_pool_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;

    let (sv1_sniffer, sv1_sniffer_addr) = start_sv1_sniffer(tproxy_addr);
    let (_minerd, _minerd_addr) = start_minerd(sv1_sniffer_addr, None, None, false).await;

    mock_pool
        .wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL)
        .await;
    let open_channel_msg = loop {
        if let Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(msg)))) =
            mock_pool.next_message(0)
        {
            break msg;
        }
    };

    let channel_id = 9u32;
    let open_channel_success = AnyMessageOwned::Mining(
        MiningOwned::OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccessOwned {
            request_id: open_channel_msg.request_id,
            channel_id,
            // The easiest target a compliant Pool may assign is the `max_target` JDC asked for,
            // and every share the miner finds for its downstream channel meets it, so the first
            // share JDC receives is one it can send upstream.
            target: open_channel_msg.max_target.clone(),
            extranonce_size: open_channel_msg.min_extranonce_size,
            extranonce_prefix: vec![0x00, 0x00, 0x00, 0x00].try_into().unwrap(),
            group_channel_id: 1,
        }),
    );
    mock_pool.send(0, open_channel_success).await;

    mock_pool
        .wait_for_message_type(0, MESSAGE_TYPE_SET_CUSTOM_MINING_JOB)
        .await;
    let set_custom_mining_job = loop {
        if let Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(msg)))) =
            mock_pool.next_message(0)
        {
            break msg;
        }
    };

    sv1_sniffer
        .wait_for_message(&["mining.submit"], MessageDirection::ToUpstream)
        .await;

    let job_id = 99;

    let set_custom_job_success = AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJobSuccess(
        SetCustomMiningJobSuccessOwned {
            channel_id: set_custom_mining_job.channel_id,
            request_id: set_custom_mining_job.request_id,
            job_id,
        },
    ));
    mock_pool.send(0, set_custom_job_success).await;

    mock_pool
        .wait_for_message_type(0, MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED)
        .await;
    let submit_share_extended = loop {
        if let Some((_, AnyMessageOwned::Mining(MiningOwned::SubmitSharesExtended(msg)))) =
            mock_pool.next_message(0)
        {
            break msg;
        }
    };

    assert_eq!(submit_share_extended.job_id, job_id);
    shutdown_all!(translator, jdc);
}
