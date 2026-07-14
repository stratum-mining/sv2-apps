use integration_tests_sv2::{
    interceptor::{IgnoreMessage, MessageDirection},
    mock_roles::{MockDownstream, WithSetup},
    template_provider::DifficultyLevel,
    *,
};
use jd_client_sv2::config::ConfigJDCMode;
use stratum_apps::stratum_core::{
    common_messages_sv2::*,
    job_declaration_sv2::*,
    mining_sv2::*,
    parsers_sv2::{AnyMessage, Mining},
};

// Pool propagates block via IPC
#[tokio::test]
async fn pool_propagates_block_with_bitcoin_core_ipc() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let current_block_hash = bitcoin_core.get_best_block_hash().unwrap();
    let (pool, pool_addr, _) = start_pool(
        ipc_config(
            bitcoin_core.data_dir().clone(),
            bitcoin_core.is_signet(),
            None,
        ),
        vec![],
        vec![],
        false,
    )
    .await;
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[pool_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    let timeout = tokio::time::Duration::from_secs(60);
    let poll_interval = tokio::time::Duration::from_secs(2);
    let start_time = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(poll_interval).await;
        let new_block_hash = bitcoin_core.get_best_block_hash().unwrap();
        if new_block_hash != current_block_hash {
            shutdown_all!(pool, translator);
            return;
        }
        if start_time.elapsed() > timeout {
            panic!(
                "Pool with BitcoinCoreIpc should have propagated a new block within {} seconds",
                timeout.as_secs()
            );
        }
    }
}

// JDC propagates block via IPC (PushSolution blocked to ensure IPC path)
#[tokio::test]
async fn jdc_propagates_block_with_bitcoin_core_ipc() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let current_block_hash = bitcoin_core.get_best_block_hash().unwrap();
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(&bitcoin_core, vec![], vec![], false).await;
    let ignore_push_solution =
        IgnoreMessage::new(MessageDirection::ToUpstream, MESSAGE_TYPE_PUSH_SOLUTION);
    let (sniffer, sniffer_addr) = start_sniffer(
        "0",
        jds_addr,
        false,
        vec![ignore_push_solution.into()],
        None,
    );
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, sniffer_addr)],
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
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
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
    let timeout = tokio::time::Duration::from_secs(60);
    let poll_interval = tokio::time::Duration::from_secs(2);
    let start_time = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(poll_interval).await;
        let new_block_hash = bitcoin_core.get_best_block_hash().unwrap();
        if new_block_hash != current_block_hash {
            sniffer
                .assert_message_not_present(
                    MessageDirection::ToUpstream,
                    MESSAGE_TYPE_PUSH_SOLUTION,
                    std::time::Duration::from_secs(1),
                )
                .await;
            shutdown_all!(pool, jdc, translator);
            return;
        }
        if start_time.elapsed() > timeout {
            panic!(
                "JDC with BitcoinCoreIpc should have propagated a new block within {} seconds",
                timeout.as_secs()
            );
        }
    }
}

// JDC solo mining mode with BitcoinCoreIpc (mode = SOLOMINING, no upstreams)
#[tokio::test]
async fn jdc_solo_mining_with_bitcoin_core_ipc() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let current_block_hash = bitcoin_core.get_best_block_hash().unwrap();

    let (jdc, jdc_addr, _) = start_jdc(
        &[],
        ipc_config(
            bitcoin_core.data_dir().clone(),
            bitcoin_core.is_signet(),
            None,
        ),
        vec![],
        vec![],
        false,
        Some(ConfigJDCMode::SoloMining),
    );

    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd, _) = start_minerd(tproxy_addr, None, None, false).await;

    let timeout = tokio::time::Duration::from_secs(60);
    let poll_interval = tokio::time::Duration::from_secs(2);
    let start_time = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(poll_interval).await;
        let new_block_hash = bitcoin_core.get_best_block_hash().unwrap();
        if new_block_hash != current_block_hash {
            shutdown_all!(jdc, translator);
            return;
        }
        if start_time.elapsed() > timeout {
            panic!(
                "JDC solo mining with BitcoinCoreIpc should have propagated a new block within {} seconds",
                timeout.as_secs()
            );
        }
    }
}

// launch a JDC (with Bitcoin Core IPC) connected to a Pool/JDS and then triggers a fallback to solo
// then it mines a block using solo and verifies the block was propagated
// meant to avoid regressions like https://github.com/stratum-mining/sv2-apps/issues/466
#[tokio::test]
async fn jdc_fallback_to_solo_mines_block_with_bitcoin_core_ipc() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let current_block_hash = bitcoin_core.get_best_block_hash().unwrap();

    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(&bitcoin_core, vec![], vec![], false).await;
    let (jdc_jds_sniffer, jdc_jds_sniffer_addr) = start_sniffer(
        "jdc-fallback-bitcoin-core-jds",
        jds_addr,
        false,
        vec![],
        None,
    );
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jdc_jds_sniffer_addr)],
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

    // assert JDC-JDS connection is established
    {
        jdc_jds_sniffer
            .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
        jdc_jds_sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
            )
            .await;
        jdc_jds_sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
            )
            .await;
        jdc_jds_sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN_SUCCESS,
            )
            .await;
    }

    // trigger JDC fallback
    pool.shutdown().await;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let (tproxy, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    let timeout = tokio::time::Duration::from_secs(60);
    let poll_interval = tokio::time::Duration::from_secs(2);
    let start_time = tokio::time::Instant::now();

    // assert JDC was able to propagate a block while doing solo
    loop {
        tokio::time::sleep(poll_interval).await;
        let new_block_hash = bitcoin_core.get_best_block_hash().unwrap();
        if new_block_hash != current_block_hash {
            shutdown_all!(jdc, tproxy);
            return;
        }
        if start_time.elapsed() > timeout {
            panic!(
                "JDC fallback to solo with BitcoinCoreIpc should have propagated a new block \
                 within {} seconds",
                timeout.as_secs()
            );
        }
    }
}

// This test verifies that Pool rejects OpenExtendedMiningChannel when
// REQUIRES_STANDARD_JOBS is set during SetupConnection.
#[tokio::test]
async fn pool_require_standard_jobs_set_rejects_open_extended_mining_channel() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(
        ipc_config(
            bitcoin_core.data_dir().clone(),
            bitcoin_core.is_signet(),
            None,
        ),
        vec![],
        vec![],
        false,
    )
    .await;

    let (sniffer, sniffer_addr) = start_sniffer("sniffer", pool_addr, false, vec![], None);
    // SetupConnection flags: 0b0001 == REQUIRES_STANDARD_JOBS.
    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0b0001),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let open_extended_mining_channel = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 100u32.into(),
            user_identity: "user_identity".try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool
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
            Some((_, AnyMessage::Mining(Mining::OpenMiningChannelError(msg)))) => break msg,
            _ => continue,
        }
    };

    assert_eq!(
        error.error_code.as_utf8_or_hex(),
        ERROR_CODE_OPEN_MINING_CHANNEL_EXTENDED_CHANNELS_NOT_SUPPORTED_FOR_STANDARD_JOBS
    );

    pool.shutdown().await;
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

    let open_extended_mining_channel = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 100u32.into(),
            user_identity: "user_identity".try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
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
            Some((_, AnyMessage::Mining(Mining::OpenMiningChannelError(msg)))) => break msg,
            _ => continue,
        }
    };

    assert_eq!(
        error.error_code.as_utf8_or_hex(),
        ERROR_CODE_OPEN_MINING_CHANNEL_EXTENDED_CHANNELS_NOT_SUPPORTED_FOR_STANDARD_JOBS
    );

    shutdown_all!(jdc, pool);
}
