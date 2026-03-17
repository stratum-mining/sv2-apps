use integration_tests_sv2::{
    interceptor::{IgnoreMessage, MessageDirection},
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::stratum_core::{job_declaration_sv2::*, template_distribution_sv2::*};

// Block propagated from JDS to TP
#[tokio::test]
async fn propagated_from_jds_to_tp() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let current_block_hash = tp.get_best_block_hash().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (_jds, jds_addr) = start_jds(tp.rpc_info());
    let (jdc_jds_sniffer, jdc_jds_sniffer_addr) = start_sniffer("0", jds_addr, false, vec![], None);
    let ignore_submit_solution =
        IgnoreMessage::new(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION);
    let (jdc_tp_sniffer, jdc_tp_sniffer_addr) = start_sniffer(
        "1",
        tp_addr,
        false,
        vec![ignore_submit_solution.into()],
        None,
    );
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jdc_jds_sniffer_addr)],
        sv2_tp_config(jdc_tp_sniffer_addr),
        vec![],
        vec![],
        false,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    jdc_jds_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_PUSH_SOLUTION)
        .await;
    jdc_tp_sniffer
        .assert_message_not_present(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SOLUTION,
            std::time::Duration::from_secs(1),
        )
        .await;
    let new_block_hash = tp.get_best_block_hash().unwrap();
    assert_ne!(current_block_hash, new_block_hash);
    shutdown_all!(translator, jdc, pool);
}
