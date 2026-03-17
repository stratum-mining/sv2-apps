use integration_tests_sv2::{
    interceptor::{IgnoreMessage, MessageDirection},
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::stratum_core::{common_messages_sv2::*, job_declaration_sv2::*};

// Pool propagates block via IPC
#[tokio::test]
async fn pool_propagates_block_with_bitcoin_core_ipc() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core(DifficultyLevel::Low);
    let current_block_hash = bitcoin_core.get_best_block_hash().unwrap();
    let (pool, pool_addr, _) = start_pool(
        ipc_config(bitcoin_core.data_dir().clone(), bitcoin_core.is_signet()),
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
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let current_block_hash = tp.get_best_block_hash().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (_jds, jds_addr) = start_jds(tp.rpc_info());
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
        ipc_config(tp.data_dir().clone(), tp.is_signet()),
        vec![],
        vec![],
        false,
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
        let new_block_hash = tp.get_best_block_hash().unwrap();
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
