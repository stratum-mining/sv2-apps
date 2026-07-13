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
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
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
        None,
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

// Block containing a transaction the JDS node only learned about through
// ProvideMissingTransactions is propagated from JDS to its node.
//
// The JDC and JDS use separate nodes sharing the same chain, but only the JDC's node has
// the transaction in its mempool. The declared job therefore requires a
// ProvideMissingTransactions round before it validates, and the solved block reaches the
// JDS node purely through the JDS (the JDC -> TP SubmitSolution path is blocked).
#[tokio::test]
async fn propagated_from_jds_to_tp_with_missing_transactions() {
    start_tracing();
    let (tp_1, _tp_addr_1) = start_template_provider(None, DifficultyLevel::Low); // JDS node
    let (tp_2, tp_addr_2) = start_template_provider(None, DifficultyLevel::Low); // JDC node

    // Give both nodes the same chain, then add a transaction only to the JDC node's
    // mempool.
    assert!(tp_2.fund_wallet().is_ok());
    tp_1.bitcoin_core()
        .sync_chain_from(tp_2.bitcoin_core())
        .unwrap();
    let (_address, txid) = tp_2.create_mempool_transaction().unwrap();

    let current_block_hash = tp_1.get_best_block_hash().unwrap();
    let current_height = tp_1.get_blockchain_info().unwrap().blocks as u64;
    assert_eq!(current_block_hash, tp_2.get_best_block_hash().unwrap());

    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp_1.bitcoin_core(), vec![], vec![], false).await;
    let (jdc_jds_sniffer, jdc_jds_sniffer_addr) = start_sniffer("0", jds_addr, false, vec![], None);
    let ignore_submit_solution =
        IgnoreMessage::new(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION);
    let (_jdc_tp_sniffer, jdc_tp_sniffer_addr) = start_sniffer(
        "1",
        tp_addr_2,
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
        None,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // The declaration must complete a ProvideMissingTransactions round before it succeeds.
    jdc_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS,
        )
        .await;
    jdc_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_PROVIDE_MISSING_TRANSACTIONS_SUCCESS,
        )
        .await;
    jdc_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB_SUCCESS,
        )
        .await;
    jdc_jds_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_PUSH_SOLUTION)
        .await;

    // PushSolution is fire-and-forget, so poll the JDS node for the new tip.
    let mut new_block_hash = tp_1.get_best_block_hash().unwrap();
    for _ in 0..100 {
        if new_block_hash != current_block_hash {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        new_block_hash = tp_1.get_best_block_hash().unwrap();
    }
    assert_ne!(
        current_block_hash, new_block_hash,
        "JDS node should have accepted the solved block"
    );

    // Check the first block after the old tip in case more than one was found.
    let solved_block_hash = tp_1
        .bitcoin_core()
        .get_block_hash(current_height + 1)
        .unwrap();
    let txids = tp_1
        .bitcoin_core()
        .get_block_txids(&solved_block_hash)
        .unwrap();
    assert!(
        txids.contains(&txid.to_string()),
        "solved block should contain the transaction provided via ProvideMissingTransactions; got {txids:?}"
    );
    shutdown_all!(translator, jdc, pool);
}
