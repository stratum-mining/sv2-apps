use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{MockUpstream, WithSetup},
    template_provider::DifficultyLevel,
    utils::get_available_address,
    *,
};
use std::time::Duration;
use stratum_apps::stratum_core::{
    common_messages_sv2::Protocol,
    mining_sv2::*,
    parsers_sv2::{AnyMessage, Mining},
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
    let (_jds, jds_addr) = start_jds(tp.rpc_info());

    let mock_pool_addr = get_available_address();
    let mock_pool = MockUpstream::new(
        mock_pool_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let mock_pool_sender = mock_pool.start().await;

    let (pool_sniffer, pool_sniffer_addr) =
        start_sniffer("pool", mock_pool_addr, false, vec![], None);

    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_sniffer_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;

    let (sv1_sniffer, sv1_sniffer_addr) = start_sv1_sniffer(tproxy_addr);
    let (_minerd, _minerd_addr) = start_minerd(sv1_sniffer_addr, None, None, false).await;

    let open_channel_msg = loop {
        match pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(Mining::OpenExtendedMiningChannel(msg)))) => break msg,
            _ => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };

    let channel_id = 9u32;
    let open_channel_success = AnyMessage::Mining(Mining::OpenExtendedMiningChannelSuccess(
        OpenExtendedMiningChannelSuccess {
            request_id: open_channel_msg.request_id,
            channel_id,
            // Set the target to the maximum value (lowest difficulty)
            // to guarantee the miner can easily generate valid shares.
            // This helps ensure the first received share is valid.
            target: [0xff_u8; 32].to_vec().try_into().unwrap(),
            extranonce_size: 8,
            extranonce_prefix: vec![0x00, 0x00, 0x00, 0x00].try_into().unwrap(),
            group_channel_id: 1,
        },
    ));
    mock_pool_sender.send(open_channel_success).await.unwrap();

    let set_custom_mining_job = loop {
        match pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(Mining::SetCustomMiningJob(msg)))) => break msg,
            _ => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };

    sv1_sniffer
        .wait_for_message(&["mining.submit"], MessageDirection::ToUpstream)
        .await;

    let job_id = 99;

    let set_custom_job_success = AnyMessage::Mining(Mining::SetCustomMiningJobSuccess(
        SetCustomMiningJobSuccess {
            channel_id: set_custom_mining_job.channel_id,
            request_id: set_custom_mining_job.request_id,
            job_id,
        },
    ));
    mock_pool_sender.send(set_custom_job_success).await.unwrap();

    let submit_share_extended = loop {
        match pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(Mining::SubmitSharesExtended(msg)))) => break msg,
            _ => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };

    assert_eq!(submit_share_extended.job_id, job_id);
    shutdown_all!(translator, jdc);
}
