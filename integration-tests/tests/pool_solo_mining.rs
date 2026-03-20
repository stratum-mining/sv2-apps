//! Tests for pool solo mining mode functionality.
//!
//! These tests validate the pool's handling of different `user_identity` patterns
//! when opening extended mining channels. The user_identity determines how block
//! rewards are distributed between the miner and pool.
//!
//! ## User Identity Patterns
//!
//! | Pattern | Description | Reward Distribution |
//! |---------|-------------|---------------------|
//! | `sri/solo/{payout_addr}/worker.1` | Full solo mining | Miner receives 100% |
//! | `{payout_addr}` (legacy) | Legacy solo mining | Miner receives 100% |
//! | `sri/donate/worker.1` | Full donation to pool | Pool receives 100% |
//! | `sri/donate/{percentage}/{payout_addr}/worker.1` | Partial donation | Pool gets specified %, miner gets rest |
//! | Other patterns | Regular pool mining | Pool receives 100% |
//!
//! ## Test Categories
//!
//! - **Error cases**: Invalid user_identity patterns should return errors
//! - **Solo mining**: Miner specifies payout address, receives full reward
//! - **Donation**: Miner can donate portion or all of reward to pool
//! - **Regular pool**: Default behavior when no solo pattern detected

use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{MockDownstream, WithSetup},
    template_provider::DifficultyLevel,
    POOL_COINBASE_REWARD_ADDRESS, *,
};
use stratum_apps::stratum_core::{
    bitcoin::{consensus::deserialize, params::TESTNET4, Address, Transaction},
    common_messages_sv2::*,
    mining_sv2::*,
    parsers_sv2::{self, AnyMessage, Mining},
};

const MINER_COINBASE_REWARD_ADDR: &str = "tb1qpusf5256yxv50qt0pm0tue8k952fsu5lzsphft";

fn build_coinbase_tx(
    channel_success: &OpenExtendedMiningChannelSuccess,
    new_job: &NewExtendedMiningJob,
) -> Transaction {
    let prefix = new_job.coinbase_tx_prefix.inner_as_ref();
    let suffix = new_job.coinbase_tx_suffix.inner_as_ref();
    let extranonce_prefix = channel_success.extranonce_prefix.inner_as_ref();
    let extranonce_suffix = vec![0; channel_success.extranonce_size as usize];
    let mut coinbase = Vec::new();

    coinbase.extend_from_slice(prefix);
    coinbase.extend_from_slice(extranonce_prefix);
    coinbase.extend_from_slice(&extranonce_suffix);
    coinbase.extend_from_slice(suffix);

    deserialize(&coinbase).expect("coinbase bytes should be valid")
}

struct PayoutInfo {
    addresses: Vec<String>,
    amounts: Vec<u64>,
    total: u64,
}

fn extract_payout_info(coinbase_tx: &Transaction) -> PayoutInfo {
    let payouts: Vec<u64> = coinbase_tx
        .output
        .iter()
        .filter(|o| !o.script_pubkey.is_op_return())
        .map(|o| o.value.to_sat())
        .collect();

    let addresses: Vec<String> = coinbase_tx
        .output
        .iter()
        .filter(|o| !o.script_pubkey.is_op_return())
        .map(|o| {
            Address::from_script(&o.script_pubkey, TESTNET4.clone())
                .expect("scriptPubKey should be valid")
                .to_string()
        })
        .collect();

    let total: u64 = payouts.iter().sum();

    PayoutInfo {
        addresses,
        amounts: payouts,
        total,
    }
}

fn assert_payout_percentage(payout_info: &PayoutInfo, expected_percentages: &[(String, f64)]) {
    for (addr, expected_pct) in expected_percentages {
        let idx = payout_info
            .addresses
            .iter()
            .position(|a| a == addr)
            .expect(&format!("Address {} not found in coinbase", addr));
        let actual_pct = (payout_info.amounts[idx] as f64 / payout_info.total as f64) * 100.0;
        assert!(
            (actual_pct - expected_pct).abs() < 0.1,
            "Address {} should receive ~{}%, got {}%",
            addr,
            expected_pct,
            actual_pct
        );
    }
}

#[tokio::test]
async fn pool_solo_mining_invalid_payout_address() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - invalid payout address ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: "sri/solo/tb1qbalieiro/worker.1"
                .to_string()
                .try_into()
                .unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
        )
        .await;

    let error_ext: OpenMiningChannelError = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenMiningChannelError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        error_ext.error_code.as_utf8_or_hex(),
        "invalid-user-identity"
    );

    // === Standard Channel - invalid payout address ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: "sri/solo/tb1qbalieiro/worker.1"
                .to_string()
                .try_into()
                .unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
        )
        .await;

    let error_std: OpenMiningChannelError = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenMiningChannelError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        error_std.error_code.as_utf8_or_hex(),
        "invalid-user-identity"
    );

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_wrong_user_identity() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - missing keyword ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: "sri/worker.1".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
        )
        .await;

    let error_ext: OpenMiningChannelError = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenMiningChannelError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        error_ext.error_code.as_utf8_or_hex(),
        "invalid-user-identity"
    );

    // === Standard Channel - missing keyword ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: "sri/worker.1".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
        )
        .await;

    let error_std: OpenMiningChannelError = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenMiningChannelError(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };
    assert_eq!(
        error_std.error_code.as_utf8_or_hex(),
        "invalid-user-identity"
    );

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_random_user_identity() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - random user_identity, pool gets 100% ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: "cool_miner/worker.1".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let channel_success_ext: OpenExtendedMiningChannelSuccess = loop {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg,
            _ => continue,
        }
    };

    let new_job_ext: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_ext = build_coinbase_tx(&channel_success_ext, &new_job_ext);
    let payout_info_ext = extract_payout_info(&coinbase_tx_ext);

    assert_eq!(payout_info_ext.addresses.len(), 1);
    assert_eq!(payout_info_ext.addresses[0], POOL_COINBASE_REWARD_ADDRESS);
    assert_payout_percentage(
        &payout_info_ext,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Trigger new template via mempool transaction ===
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for second job (from mempool) and verify payout ===
    let new_job_ext_second: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_second = build_coinbase_tx(&channel_success_ext, &new_job_ext_second);
    let payout_info_second = extract_payout_info(&coinbase_tx_second);

    assert_eq!(
        payout_info_second.addresses.len(),
        1,
        "Second job (mempool) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_second.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Second job (mempool) payout should go to pool address"
    );
    assert_payout_percentage(
        &payout_info_second,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Trigger new template to force pool to send a new job ===
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for third job (from generate blocks) and verify payout ===
    let new_job_ext_third: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_third = build_coinbase_tx(&channel_success_ext, &new_job_ext_third);
    let payout_info_third = extract_payout_info(&coinbase_tx_third);

    assert_eq!(
        payout_info_third.addresses.len(),
        1,
        "Third job (generate blocks) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_third.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Third job (generate blocks) payout should STILL go to pool address"
    );
    assert_payout_percentage(
        &payout_info_third,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Standard Channel - random user_identity ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: "cool_miner/worker.1".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_legacy_pattern() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - legacy pattern, miner gets 100% ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: MINER_COINBASE_REWARD_ADDR.to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let channel_success_ext: OpenExtendedMiningChannelSuccess = loop {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg,
            _ => continue,
        }
    };

    let new_job_ext: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_ext = build_coinbase_tx(&channel_success_ext, &new_job_ext);
    let payout_info_ext = extract_payout_info(&coinbase_tx_ext);

    assert_eq!(payout_info_ext.addresses.len(), 1);
    assert_eq!(payout_info_ext.addresses[0], MINER_COINBASE_REWARD_ADDR);
    assert_payout_percentage(
        &payout_info_ext,
        &[(MINER_COINBASE_REWARD_ADDR.to_string(), 100.0)],
    );

    // === Trigger new template via mempool transaction ===
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for second job (from mempool) and verify payout ===
    let new_job_ext_second: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_second = build_coinbase_tx(&channel_success_ext, &new_job_ext_second);
    let payout_info_second = extract_payout_info(&coinbase_tx_second);

    assert_eq!(
        payout_info_second.addresses.len(),
        1,
        "Second job (mempool) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_second.addresses[0], MINER_COINBASE_REWARD_ADDR,
        "Second job (mempool) payout should go to miner address"
    );
    assert_payout_percentage(
        &payout_info_second,
        &[(MINER_COINBASE_REWARD_ADDR.to_string(), 100.0)],
    );

    // === Trigger new template to force pool to send a new job ===
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for third job (from generate blocks) and verify payout ===
    let new_job_ext_third: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_third = build_coinbase_tx(&channel_success_ext, &new_job_ext_third);
    let payout_info_third = extract_payout_info(&coinbase_tx_third);

    assert_eq!(
        payout_info_third.addresses.len(),
        1,
        "Third job (generate blocks) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_third.addresses[0], MINER_COINBASE_REWARD_ADDR,
        "Third job (generate blocks) payout should STILL go to miner address"
    );
    assert_payout_percentage(
        &payout_info_third,
        &[(MINER_COINBASE_REWARD_ADDR.to_string(), 100.0)],
    );

    // === Standard Channel - legacy pattern ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: MINER_COINBASE_REWARD_ADDR.to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_solo_pattern() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - sri/solo pattern, miner gets 100% ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: format!("sri/solo/{}/worker.1", MINER_COINBASE_REWARD_ADDR)
                .try_into()
                .unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let channel_success_ext: OpenExtendedMiningChannelSuccess = loop {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg,
            _ => continue,
        }
    };

    let new_job_ext: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_ext = build_coinbase_tx(&channel_success_ext, &new_job_ext);
    let payout_info_ext = extract_payout_info(&coinbase_tx_ext);

    assert_eq!(payout_info_ext.addresses.len(), 1);
    assert_eq!(payout_info_ext.addresses[0], MINER_COINBASE_REWARD_ADDR);
    assert_payout_percentage(
        &payout_info_ext,
        &[(MINER_COINBASE_REWARD_ADDR.to_string(), 100.0)],
    );

    // === Trigger new template via mempool transaction ===
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for second job (from mempool) and verify payout ===
    let new_job_ext_second: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_second = build_coinbase_tx(&channel_success_ext, &new_job_ext_second);
    let payout_info_second = extract_payout_info(&coinbase_tx_second);

    assert_eq!(
        payout_info_second.addresses.len(),
        1,
        "Second job (mempool) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_second.addresses[0], MINER_COINBASE_REWARD_ADDR,
        "Second job (mempool) payout should go to miner address"
    );
    assert_payout_percentage(
        &payout_info_second,
        &[(MINER_COINBASE_REWARD_ADDR.to_string(), 100.0)],
    );

    // === Trigger new template to force pool to send a new job ===
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for third job (from generate blocks) and verify payout ===
    let new_job_ext_third: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_third = build_coinbase_tx(&channel_success_ext, &new_job_ext_third);
    let payout_info_third = extract_payout_info(&coinbase_tx_third);

    assert_eq!(
        payout_info_third.addresses.len(),
        1,
        "Third job (generate blocks) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_third.addresses[0], MINER_COINBASE_REWARD_ADDR,
        "Third job (generate blocks) payout should STILL go to miner address"
    );
    assert_payout_percentage(
        &payout_info_third,
        &[(MINER_COINBASE_REWARD_ADDR.to_string(), 100.0)],
    );

    // === Standard Channel - sri/solo pattern ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: format!("sri/solo/{}/worker.1", MINER_COINBASE_REWARD_ADDR)
                .try_into()
                .unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_full_donate() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - sri/donate, pool gets 100% ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: "sri/donate/worker.1".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let channel_success_ext: OpenExtendedMiningChannelSuccess = loop {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg,
            _ => continue,
        }
    };

    let new_job_ext: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_ext = build_coinbase_tx(&channel_success_ext, &new_job_ext);
    let payout_info_ext = extract_payout_info(&coinbase_tx_ext);

    assert_eq!(payout_info_ext.addresses.len(), 1);
    assert_eq!(payout_info_ext.addresses[0], POOL_COINBASE_REWARD_ADDRESS);
    assert_payout_percentage(
        &payout_info_ext,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Trigger new template via mempool transaction ===
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for second job (from mempool) and verify payout ===
    let new_job_ext_second: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_second = build_coinbase_tx(&channel_success_ext, &new_job_ext_second);
    let payout_info_second = extract_payout_info(&coinbase_tx_second);

    assert_eq!(
        payout_info_second.addresses.len(),
        1,
        "Second job (mempool) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_second.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Second job (mempool) payout should go to pool address"
    );
    assert_payout_percentage(
        &payout_info_second,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Trigger new template to force pool to send a new job ===
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for third job (from generate blocks) and verify payout ===
    let new_job_ext_third: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_third = build_coinbase_tx(&channel_success_ext, &new_job_ext_third);
    let payout_info_third = extract_payout_info(&coinbase_tx_third);

    assert_eq!(
        payout_info_third.addresses.len(),
        1,
        "Third job (generate blocks) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_third.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Third job (generate blocks) payout should STILL go to pool address"
    );
    assert_payout_percentage(
        &payout_info_third,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Standard Channel - sri/donate ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: "sri/donate/worker.1".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_full_donate_no_worker_name() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - sri/donate (no worker name), pool gets 100% ===
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: "sri/donate".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let channel_success_ext: OpenExtendedMiningChannelSuccess = loop {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg,
            _ => continue,
        }
    };

    let new_job_ext: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_ext = build_coinbase_tx(&channel_success_ext, &new_job_ext);
    let payout_info_ext = extract_payout_info(&coinbase_tx_ext);

    assert_eq!(payout_info_ext.addresses.len(), 1);
    assert_eq!(payout_info_ext.addresses[0], POOL_COINBASE_REWARD_ADDRESS);
    assert_payout_percentage(
        &payout_info_ext,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Trigger new template via mempool transaction ===
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for second job (from mempool) and verify payout ===
    let new_job_ext_second: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_second = build_coinbase_tx(&channel_success_ext, &new_job_ext_second);
    let payout_info_second = extract_payout_info(&coinbase_tx_second);

    assert_eq!(
        payout_info_second.addresses.len(),
        1,
        "Second job (mempool) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_second.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Second job (mempool) payout should go to pool address"
    );
    assert_payout_percentage(
        &payout_info_second,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Trigger new template to force pool to send a new job ===
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for third job (from generate blocks) and verify payout ===
    let new_job_ext_third: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_third = build_coinbase_tx(&channel_success_ext, &new_job_ext_third);
    let payout_info_third = extract_payout_info(&coinbase_tx_third);

    assert_eq!(
        payout_info_third.addresses.len(),
        1,
        "Third job (generate blocks) should have exactly 1 output"
    );
    assert_eq!(
        payout_info_third.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Third job (generate blocks) payout should STILL go to pool address"
    );
    assert_payout_percentage(
        &payout_info_third,
        &[(POOL_COINBASE_REWARD_ADDRESS.to_string(), 100.0)],
    );

    // === Standard Channel - sri/donate (no worker name) ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: "sri/donate".to_string().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown_all!(pool);
}

#[tokio::test]
async fn pool_solo_mining_partial_donation() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("solo_test", pool_addr, false, vec![], None);

    let mock_downstream = MockDownstream::new(
        sniffer_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let send_to_pool = mock_downstream.start().await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    // === Extended Channel - sri/donate/5%, pool gets 5%, miner gets 95% ===
    let user_identity = format!("sri/donate/5/{}/worker.1", MINER_COINBASE_REWARD_ADDR);
    let open_extended = AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
        OpenExtendedMiningChannel {
            request_id: 0u32,
            user_identity: user_identity.clone().try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
            min_extranonce_size: 8,
        },
    ));
    send_to_pool.send(open_extended).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let channel_success_ext: OpenExtendedMiningChannelSuccess = loop {
        match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => break msg,
            _ => continue,
        }
    };

    let new_job_ext: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_ext = build_coinbase_tx(&channel_success_ext, &new_job_ext);

    assert_eq!(coinbase_tx_ext.output.len(), 3);

    let payout_info_ext = extract_payout_info(&coinbase_tx_ext);

    assert_eq!(payout_info_ext.addresses.len(), 2);
    assert_eq!(payout_info_ext.addresses[0], POOL_COINBASE_REWARD_ADDRESS);
    assert_eq!(payout_info_ext.addresses[1], MINER_COINBASE_REWARD_ADDR);

    assert_payout_percentage(
        &payout_info_ext,
        &[
            (POOL_COINBASE_REWARD_ADDRESS.to_string(), 5.0),
            (MINER_COINBASE_REWARD_ADDR.to_string(), 95.0),
        ],
    );

    // === Trigger new template via mempool transaction ===
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for second job (from mempool) and verify payout ===
    let new_job_ext_second: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_second = build_coinbase_tx(&channel_success_ext, &new_job_ext_second);
    let payout_info_second = extract_payout_info(&coinbase_tx_second);

    assert_eq!(
        payout_info_second.addresses.len(),
        2,
        "Second job (mempool) should have exactly 2 outputs"
    );
    assert_eq!(
        payout_info_second.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Second job (mempool) payout should have pool address first"
    );
    assert_eq!(
        payout_info_second.addresses[1], MINER_COINBASE_REWARD_ADDR,
        "Second job (mempool) payout should have miner address second"
    );
    assert_payout_percentage(
        &payout_info_second,
        &[
            (POOL_COINBASE_REWARD_ADDRESS.to_string(), 5.0),
            (MINER_COINBASE_REWARD_ADDR.to_string(), 95.0),
        ],
    );

    // === Trigger new template to force pool to send a new job ===
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // === Wait for third job (from generate blocks) and verify payout ===
    let new_job_ext_third: NewExtendedMiningJob = loop {
        match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => {
                break msg;
            }
            _ => continue,
        }
    };

    let coinbase_tx_third = build_coinbase_tx(&channel_success_ext, &new_job_ext_third);
    let payout_info_third = extract_payout_info(&coinbase_tx_third);

    assert_eq!(
        payout_info_third.addresses.len(),
        2,
        "Third job (generate blocks) should have exactly 2 outputs"
    );
    assert_eq!(
        payout_info_third.addresses[0], POOL_COINBASE_REWARD_ADDRESS,
        "Third job (generate blocks) payout should STILL have pool address first"
    );
    assert_eq!(
        payout_info_third.addresses[1], MINER_COINBASE_REWARD_ADDR,
        "Third job (generate blocks) payout should STILL have miner address second"
    );
    assert_payout_percentage(
        &payout_info_third,
        &[
            (POOL_COINBASE_REWARD_ADDRESS.to_string(), 5.0),
            (MINER_COINBASE_REWARD_ADDR.to_string(), 95.0),
        ],
    );

    // === Standard Channel - sri/donate/5% ===
    let open_standard = AnyMessage::Mining(Mining::OpenStandardMiningChannel(
        OpenStandardMiningChannel {
            request_id: 0u32.into(),
            user_identity: user_identity.try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: vec![0xff; 32].try_into().unwrap(),
        },
    ));
    send_to_pool.send(open_standard).await.unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown_all!(pool);
}
