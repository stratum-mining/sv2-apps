//! BIP-54 (Consensus Cleanup) compliance for blocks mined through the SV2 stack.
//!
//! BIP-54 requires that the coinbase transaction of every block:
//!   * have its `nLockTime` field set to the block height minus 1, and
//!   * have its sole input's `nSequence` field set to a value other than `0xffffffff`.
//!
//! This test drives a block all the way from a mining device, through the translator,
//! JDC, JDS and pool, until it is propagated to the template provider, and then
//! inspects the freshly-accepted block to assert both invariants on its coinbase.
//!
//! See <https://github.com/bitcoin/bips/blob/master/bip-0054.md>.

use integration_tests_sv2::{interceptor::MessageDirection, template_provider::DifficultyLevel, *};
use std::str::FromStr;
use stratum_apps::stratum_core::{
    bitcoin::{BlockHash, Sequence},
    template_distribution_sv2::MESSAGE_TYPE_SUBMIT_SOLUTION,
};

#[tokio::test]
async fn coinbase_of_jdc_mined_block_is_bip54_compliant() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);

    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    // Sniff the JDC -> TP channel so we can wait for the SUBMIT_SOLUTION message
    // and read the chain tip exactly once afterwards. This avoids racing two
    // separate RPCs (`getbestblockhash` + `getblockchaininfo`) against the node.
    let (jdc_tp_sniffer, jdc_tp_sniffer_addr) = start_sniffer("0", tp_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jds_addr)],
        sv2_tp_config(jdc_tp_sniffer_addr),
        vec![],
        vec![],
        false,
        None,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // Wait for the JDC to push a solution to the TP, then read the resulting tip.
    jdc_tp_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION)
        .await;
    let info = tp.get_blockchain_info().unwrap();
    let new_height = info.blocks;
    let block_hash = BlockHash::from_str(&info.best_block_hash).expect("valid block hash");

    // Fetch the newly accepted block and inspect its coinbase transaction.
    let block = tp.bitcoin_core().get_block(block_hash).expect("get_block");
    let coinbase = block
        .txdata
        .first()
        .expect("block must contain at least the coinbase transaction");
    assert!(
        coinbase.is_coinbase(),
        "first transaction in a block must be a coinbase"
    );

    // BIP-54: coinbase nLockTime must equal block_height - 1.
    let expected_locktime = u32::try_from(new_height - 1).expect("height fits in u32");
    let actual_locktime = coinbase.lock_time.to_consensus_u32();
    assert_eq!(
        actual_locktime, expected_locktime,
        "BIP-54 violation: coinbase nLockTime is {actual_locktime}, expected block height - 1 = {expected_locktime}"
    );

    // BIP-54: coinbase input nSequence must not be 0xffffffff.
    // `is_coinbase()` already guarantees exactly one input.
    let coinbase_input = &coinbase.input[0];
    assert_ne!(
        coinbase_input.sequence,
        Sequence::MAX,
        "BIP-54 violation: coinbase input nSequence is 0xffffffff"
    );

    shutdown_all!(translator, jdc, pool);
}
