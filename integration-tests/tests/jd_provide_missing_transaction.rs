use integration_tests_sv2::{interceptor::MessageDirection, template_provider::DifficultyLevel, *};
use stratum_apps::stratum_core::job_declaration_sv2::*;

#[tokio::test]
async fn jds_ask_for_missing_transactions() {
    start_tracing();
    let (tp_1, _tp_addr_1) = start_template_provider(None, DifficultyLevel::Low);
    let (tp_2, tp_addr_2) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp_1.bitcoin_core(), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("A", jds_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, sniffer_addr)],
        sv2_tp_config(tp_addr_2),
        vec![],
        vec![],
        false,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    assert!(tp_2.fund_wallet().is_ok());
    assert!(tp_2.create_mempool_transaction().is_ok());
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        )
        .await;
    // we're using two separate TPs, which are disconnected
    // we only funding wallet on tp_2, so utxo set on tp_1 is not compatible
    // with the mempool tx and the job declaration is expected to fail
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB_ERROR,
        )
        .await;
    shutdown_all!(translator, jdc, pool);
}
