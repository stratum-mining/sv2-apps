use integration_tests_sv2::{interceptor::MessageDirection, template_provider::DifficultyLevel, *};
use stratum_apps::stratum_core::{common_messages_sv2::*, job_declaration_sv2::*};

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
