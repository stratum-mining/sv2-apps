use integration_tests_sv2::{interceptor::MessageDirection, template_provider::DifficultyLevel, *};
use stratum_apps::stratum_core::{common_messages_sv2::*, job_declaration_sv2::*};
use tokio::{io::AsyncReadExt, net::TcpStream};

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

// launch a JDC (with Sv2 TP over TCP) connected to a Pool/JDS and then triggers a fallback to solo
// then it mines a block using solo and verifies the block was propagated
// meant to avoid regressions like https://github.com/stratum-mining/sv2-apps/issues/466
//
// currently disabled, blocked by https://github.com/stratum-mining/sv2-tp/issues/99
#[ignore = "https://github.com/stratum-mining/sv2-tp/issues/99"]
#[tokio::test]
async fn jdc_fallback_to_solo_mines_block_with_template_provider() {
    use stratum_apps::stratum_core::template_distribution_sv2::*;

    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let current_block_hash = tp.get_best_block_hash().unwrap();

    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (jdc_jds_sniffer, jdc_jds_sniffer_addr) =
        start_sniffer("jdc-fallback-sv2-tp-jds", jds_addr, false, vec![], None);
    let (jdc_tp_sniffer, jdc_tp_sniffer_addr) =
        start_sniffer("jdc-fallback-sv2-tp-tp", tp_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jdc_jds_sniffer_addr)],
        sv2_tp_config(jdc_tp_sniffer_addr),
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

    // assert JDC was able to propagate a block while doing solo
    {
        jdc_tp_sniffer
            .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION)
            .await;

        let new_block_hash = tp.get_best_block_hash().unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        assert_ne!(current_block_hash, new_block_hash);
    }

    shutdown_all!(jdc, tproxy);
}

// launch a JDC connected to a Pool/JDS and serving a tProxy downstream, open a raw TCP connection
// to the JDC downstream port that never starts the noise handshake, then trigger a fallback.
// the handshake task stops on the fallback token, so fallback drops the stalled connection at
// once instead of leaving it to the noise handshake timeout: the EOF on the raw connection is
// the only thing this test observes. that the task also counts as pending for the fallback
// coordinator is not visible from outside; the coordinator's unit tests cover the counting, and
// the listener registers the task before spawning it, so this is not the regression test for it
#[tokio::test]
async fn jdc_fallback_drops_a_downstream_stalled_in_handshake() {
    start_tracing();
    let bitcoin_core = start_bitcoin_core_latest(DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(&bitcoin_core, vec![], vec![], false).await;
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

    // a downstream that completes SetupConnection proves the JDC accept loop is serving, so the
    // stalled connection below is accepted and handed to a handshake task instead of waiting in
    // the listen backlog
    let (jdc_tproxy_sniffer, jdc_tproxy_sniffer_addr) = start_sniffer(
        "jdc-stalled-handshake-tproxy",
        jdc_addr,
        false,
        vec![],
        None,
    );
    let (tproxy, _, _) = start_sv2_translator(
        &[jdc_tproxy_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;
    jdc_tproxy_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let mut stalled = TcpStream::connect(jdc_addr).await.unwrap();
    // give the accept loop time to hand the connection to its handshake task
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // trigger JDC fallback
    pool.shutdown().await;

    // fallback drops the stalled connection at once, so the read ends with EOF long before the
    // noise handshake timeout would have released it. a reset instead of EOF would mean the
    // listener closed with the connection still in its backlog, never handed to a handshake
    // task, which would prove nothing about that task
    let mut buf = [0u8; 1];
    let read =
        tokio::time::timeout(tokio::time::Duration::from_secs(3), stalled.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0))),
        "JDC fallback should have dropped the downstream stalled in its handshake, got {read:?}"
    );

    shutdown_all!(jdc, tproxy);
}
