use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{MockUpstream, WithSetup},
    start_jdc_with_user_identities,
    template_provider::DifficultyLevel,
    utils::get_available_address,
    *,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::*,
    mining_sv2::*,
    parsers_sv2::{AnyMessageOwned, CommonMessagesOwned, MiningOwned},
};

#[tokio::test]
async fn jd_non_aggregated_tproxy_integration() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (jdc_pool_sniffer, jdc_pool_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(jdc_pool_sniffer_addr, jds_addr)],
        ipc_config(
            tp.bitcoin_core().data_dir().clone(),
            tp.bitcoin_core().is_signet(),
            None,
        ),
        vec![],
        vec![],
        false,
        None,
    );
    let (tproxy_jdc_sniffer, tproxy_jdc_sniffer_addr) =
        start_sniffer("1", jdc_addr, false, vec![], None);
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[tproxy_jdc_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    // start two minerd processes
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // assert that two OpenExtendedMiningChannel messages are present in the queue
    // because two minerd processes are started
    {
        tproxy_jdc_sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            )
            .await;
        tproxy_jdc_sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            )
            .await;
    }

    jdc_pool_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;
    shutdown_all!(translator, jdc, pool);
}

#[tokio::test]
async fn jd_aggregated_tproxy_integration() {
    start_tracing();
    let (tp, _tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (jdc_pool_sniffer, jdc_pool_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(jdc_pool_sniffer_addr, jds_addr)],
        ipc_config(
            tp.bitcoin_core().data_dir().clone(),
            tp.bitcoin_core().is_signet(),
            None,
        ),
        vec![],
        vec![],
        false,
        None,
    );
    let (tproxy_jdc_sniffer, tproxy_jdc_sniffer_addr) =
        start_sniffer("1", jdc_addr, false, vec![], None);
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[tproxy_jdc_sniffer_addr],
        true,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    // start two minerd processes
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // assert that only one OpenExtendedMiningChannel message is present in the queue
    {
        tproxy_jdc_sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            )
            .await;
        assert!(
            tproxy_jdc_sniffer
                .assert_message_not_present(
                    MessageDirection::ToUpstream,
                    MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                    std::time::Duration::from_secs(2),
                )
                .await,
            "Expected only one OpenExtendedMiningChannel but found another one."
        );
    }

    jdc_pool_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;
    shutdown_all!(translator, jdc, pool);
}

// Verifies that when a per-upstream `user_identity` is set on a JDC upstream entry,
// that identity is sent as-is in `OpenExtendedMiningChannel`.
#[tokio::test]
async fn jdc_sends_per_upstream_identity() {
    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, _pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let mock_pool_addr = get_available_address();
    let mock_pool = MockUpstream::new(mock_pool_addr, WithSetup::no());
    let pool_sender = mock_pool.start().await;

    let (pool_sniffer, pool_sniffer_addr) =
        start_sniffer("pool", mock_pool_addr, false, vec![], None);

    const PER_UPSTREAM_IDENTITY: &str = "bc1qtest.worker";

    let (jdc, jdc_addr, _) = start_jdc_with_user_identities(
        &[(
            pool_sniffer_addr,
            jds_addr,
            PER_UPSTREAM_IDENTITY.to_string(),
        )],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    pool_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    // Respond with success so JDC proceeds to connect to JDS and open channels.
    pool_sender
        .send(AnyMessageOwned::Common(
            CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
                used_version: 2,
                flags: 0,
            }),
        ))
        .await
        .unwrap();

    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd, _) = start_minerd(tproxy_addr, None, None, false).await;

    pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    let oemc = loop {
        match pool_sniffer.next_message_from_downstream() {
            Some((_, AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let identity_str =
        std::str::from_utf8(oemc.user_identity.as_ref()).expect("user_identity is not valid UTF-8");
    assert_eq!(
        identity_str, PER_UPSTREAM_IDENTITY,
        "expected per-upstream identity '{PER_UPSTREAM_IDENTITY}', got '{identity_str}'"
    );

    shutdown_all!(translator, jdc);
}
