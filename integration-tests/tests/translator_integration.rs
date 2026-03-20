// This file contains integration tests for the `TranslatorSv2` module.
use integration_tests_sv2::{
    interceptor::{IgnoreMessage, MessageDirection, ReplaceMessage},
    mock_roles::{MockUpstream, WithSetup},
    sv1_sniffer::SV1MessageFilter,
    template_provider::DifficultyLevel,
    utils::get_available_address,
    *,
};
use stratum_apps::stratum_core::mining_sv2::*;
use tokio::net::{TcpListener, TcpStream};

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use stratum_apps::stratum_core::{
    binary_sv2::{Seq0255, Sv2Option},
    common_messages_sv2::{
        Protocol, SetupConnectionError, SetupConnectionSuccess, MESSAGE_TYPE_SETUP_CONNECTION,
        MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
    },
    mining_sv2::{
        CloseChannel, OpenMiningChannelError, MESSAGE_TYPE_CLOSE_CHANNEL,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
    },
    parsers_sv2::{self, AnyMessage, CommonMessages},
    sv1_api,
    template_distribution_sv2::MESSAGE_TYPE_SUBMIT_SOLUTION,
};

// This test runs an sv2 translator between an sv1 mining device and a pool. the connection between
// the translator and the pool is intercepted by a sniffer. The test checks if the translator and
// the pool exchange the correct messages upon connection. And that the miner is able to submit
// shares.
#[tokio::test]
async fn translate_sv1_to_sv2_successfully() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (pool_translator_sniffer, pool_translator_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    pool_translator_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;
    shutdown_all!(translator, pool);
}

// Demonstrates the scenario where TProxy falls back to the secondary pool
// after the primary pool returns a `SetupConnection.Error`.
#[tokio::test]
async fn test_translator_fallback_on_setup_connection_error() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool_1, pool_addr_1, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (pool_2, pool_addr_2, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let random_error_code = "Something went wrong".to_string();

    let setup_connection_success_replace = ReplaceMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        AnyMessage::Common(parsers_sv2::CommonMessages::SetupConnectionError(
            SetupConnectionError {
                flags: 0,
                error_code: random_error_code.try_into().unwrap(),
            },
        )),
    );

    let (pool_translator_sniffer_1, pool_translator_sniffer_addr_1) = start_sniffer(
        "A",
        pool_addr_1,
        false,
        vec![setup_connection_success_replace.into()],
        None,
    );

    let (pool_translator_sniffer_2, pool_translator_sniffer_addr_2) =
        start_sniffer("B", pool_addr_2, false, vec![], None);

    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[
            pool_translator_sniffer_addr_1,
            pool_translator_sniffer_addr_2,
        ],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    pool_translator_sniffer_1
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    pool_translator_sniffer_1
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
        )
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    pool_translator_sniffer_2
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    shutdown_all!(translator, pool_2, pool_1);
}

// Demonstrates the scenario where the primary pool returns an `OpenMiningChannel.Error`,
// causing TProxy to fall back to the secondary pool.
#[tokio::test]
async fn test_translator_fallback_on_open_mining_message_error() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool_1, pool_addr_1, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (pool_2, pool_addr_2, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let random_error_code = "Something went wrong".to_string();

    let open_mining_channel_success_replace = ReplaceMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        AnyMessage::Mining(parsers_sv2::Mining::OpenMiningChannelError(
            OpenMiningChannelError {
                request_id: 0,
                error_code: random_error_code.try_into().unwrap(),
            },
        )),
    );

    let (pool_translator_sniffer_1, pool_translator_sniffer_addr_1) = start_sniffer(
        "A",
        pool_addr_1,
        false,
        vec![open_mining_channel_success_replace.into()],
        None,
    );

    let (pool_translator_sniffer_2, pool_translator_sniffer_addr_2) =
        start_sniffer("B", pool_addr_2, false, vec![], None);

    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[
            pool_translator_sniffer_addr_1,
            pool_translator_sniffer_addr_2,
        ],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    pool_translator_sniffer_1
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    pool_translator_sniffer_1
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    pool_translator_sniffer_1
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    pool_translator_sniffer_2
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    shutdown_all!(translator, pool_2, pool_1);
}

// This test verifies that the translator sends keepalive jobs to downstream miners when no new
// jobs are received from upstream, and that shares submitted for keepalive jobs are properly
// received by the pool. Keepalive job_id(s) use the format `{original_job_id}#{counter}`.
#[tokio::test]
async fn test_translator_keepalive_job_sent_and_share_received_by_pool() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::High);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (pool_translator_sniffer, pool_translator_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);

    // Start translator with a short keepalive interval (5 seconds)
    let keepalive_interval_secs = 5_u16;
    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        false,
        vec![],
        vec![],
        Some(keepalive_interval_secs),
        false,
    )
    .await;
    let (sv1_sniffer, sv1_sniffer_addr) = start_sv1_sniffer(tproxy_addr);
    let (_minerd_process, _minerd_addr) = start_minerd(sv1_sniffer_addr, None, None, false).await;

    sv1_sniffer
        .wait_for_message(&["mining.notify"], MessageDirection::ToDownstream)
        .await;

    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    // Wait for keepalive interval plus some buffer time
    tokio::time::sleep(std::time::Duration::from_secs(
        keepalive_interval_secs as u64 + 3,
    ))
    .await;

    // Wait for a keepalive mining.notify message (job_id contains '#' delimiter)
    sv1_sniffer
        .wait_for_keepalive_notify(MessageDirection::ToDownstream)
        .await;

    // Wait for the share submission success message
    // This proves the keepalive job was valid and the share was properly mapped
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;
    shutdown_all!(translator, pool);
}

// This test launches a tProxy in aggregated mode and leverages a MockUpstream to test the correct
// functionalities of grouping extended channels.
#[tokio::test]
async fn aggregated_translator_correctly_deals_with_group_channels() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();

    // block SubmitSolution messages from arriving to TP
    // so we avoid shares triggering chain tip updates
    // which we want to do explicitly via generate_blocks()
    let ignore_submit_solution =
        IgnoreMessage::new(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION);
    let (_sniffer_pool_tp, sniffer_pool_tp_addr) = start_sniffer(
        "0",
        tp_addr,
        false,
        vec![ignore_submit_solution.into()],
        None,
    );

    let (pool, pool_addr, _) =
        start_pool(sv2_tp_config(sniffer_pool_tp_addr), vec![], vec![], false).await;

    // ignore SubmitSharesSuccess messages, so we can keep the assertion flow simple
    let ignore_submit_shares_success = IgnoreMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
    );
    let (sniffer, sniffer_addr) = start_sniffer(
        "0",
        pool_addr,
        false,
        vec![ignore_submit_shares_success.into()],
        None,
    );

    // aggregated tProxy
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[sniffer_addr], true, vec![], vec![], None, false).await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let mut minerd_vec = Vec::new();

    // start the first minerd process, to trigger the first OpenExtendedMiningChannel message
    let (minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    minerd_vec.push(minerd_process);

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    // save the aggregated and group channel IDs
    let (aggregated_channel_id, group_channel_id) = match sniffer.next_message_from_upstream() {
        Some((
            _,
            AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
        )) => (msg.channel_id, msg.group_channel_id),
        msg => panic!(
            "Expected OpenExtendedMiningChannelSuccess message, found: {:?}",
            msg
        ),
    };

    // wait for the expected NewExtendedMiningJob and SetNewPrevHash messages
    // and clean the queue
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    // open a few more extended channels to be aggregated with the first one
    const N_MINERDS: u32 = 5;
    for _i in 0..N_MINERDS {
        let (minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
        minerd_vec.push(minerd_process);

        // wait a bit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // assert no furter OpenExtendedMiningChannel messages are sent
        sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                std::time::Duration::from_secs(1),
            )
            .await;
    }

    // wait for a SubmitSharesExtended message
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    let share_channel_id = match sniffer.next_message_from_downstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => {
            msg.channel_id
        }
        msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
    };

    assert_eq!(
        aggregated_channel_id, share_channel_id,
        "Share submitted to the correct channel ID"
    );
    assert_ne!(
        share_channel_id, group_channel_id,
        "Share NOT submitted to the group channel ID"
    );

    // wait for another share, so we can clean the queue
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    // now let's force a mempool update, so we trigger a NewExtendedMiningJob message
    // it's actually directed to the group channel Id, not the aggregated channel Id
    // nevertheless, tProxy should still submit the share to the aggregated channel Id
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    let new_extended_mining_job = match sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => msg,
        msg => panic!("Expected NewExtendedMiningJob message, found: {:?}", msg),
    };

    // here we're actually asserting pool behavior, not tProxy
    // but still good to have, to ensure the global sanity of the test
    assert_ne!(new_extended_mining_job.channel_id, aggregated_channel_id);
    assert_eq!(new_extended_mining_job.channel_id, group_channel_id);

    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        // assert the share is submitted to the aggregated channel Id
        assert_eq!(submit_shares_extended.channel_id, aggregated_channel_id);
        assert_ne!(submit_shares_extended.channel_id, group_channel_id);

        if submit_shares_extended.job_id == 2 {
            break;
        }
    }

    // now let's force a chain tip update, so we trigger a SetNewPrevHash + NewExtendedMiningJob
    // message pair
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    let new_extended_mining_job = match sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => msg,
        msg => panic!("Expected NewExtendedMiningJob message, found: {:?}", msg),
    };

    // again, asserting pool behavior, not tProxy
    // just to ensure the global sanity of the test
    assert_ne!(new_extended_mining_job.channel_id, aggregated_channel_id);
    assert_eq!(new_extended_mining_job.channel_id, group_channel_id);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;
    let set_new_prev_hash = match sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(msg)))) => msg,
        msg => panic!("Expected SetNewPrevHash message, found: {:?}", msg),
    };

    // again, asserting pool behavior, not tProxy
    // just to ensure the global sanity of the test
    assert_eq!(set_new_prev_hash.channel_id, group_channel_id);
    assert_ne!(set_new_prev_hash.channel_id, aggregated_channel_id);

    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        // assert the share is submitted to the aggregated channel Id
        assert_eq!(submit_shares_extended.channel_id, aggregated_channel_id);
        assert_ne!(submit_shares_extended.channel_id, group_channel_id);

        if submit_shares_extended.job_id == 3 {
            break;
        }
    }
    shutdown_all!(translator, pool);
}

// This test launches a tProxy in non-aggregated mode and leverages a MockUpstream to test the
// correct functionalities of grouping extended channels.
#[tokio::test]
async fn non_aggregated_translator_correctly_deals_with_group_channels() {
    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().unwrap();

    // block SubmitSolution messages from arriving to TP
    // so we avoid shares triggering chain tip updates
    // which we want to do explicitly via generate_blocks()
    let ignore_submit_solution =
        IgnoreMessage::new(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION);
    let (_sniffer_pool_tp, sniffer_pool_tp_addr) = start_sniffer(
        "0",
        tp_addr,
        false,
        vec![ignore_submit_solution.into()],
        None,
    );

    let (pool, pool_addr, _) =
        start_pool(sv2_tp_config(sniffer_pool_tp_addr), vec![], vec![], false).await;

    // ignore SubmitSharesSuccess messages, so we can keep the assertion flow simple
    let ignore_submit_shares_success = IgnoreMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
    );
    let (sniffer, sniffer_addr) = start_sniffer(
        "0",
        pool_addr,
        false,
        vec![ignore_submit_shares_success.into()],
        None,
    );
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[sniffer_addr], false, vec![], vec![], None, false).await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    const N_EXTENDED_CHANNELS: u32 = 5;
    const EXPECTED_GROUP_CHANNEL_ID: u32 = 1;
    let mut minerd_vec = Vec::new();
    let mut sv1_sniffers = Vec::new();
    let mut channel_ids = Vec::new();

    for _i in 0..N_EXTENDED_CHANNELS {
        let (sv1_sniffer, sv1_sniffer_addr) = start_sv1_sniffer(tproxy_addr);
        sv1_sniffers.push(sv1_sniffer);
        let (minerd, _minerd_addr) = start_minerd(sv1_sniffer_addr, None, None, false).await;
        minerd_vec.push(minerd);
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            )
            .await;
        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            )
            .await;
        let open_extended_mining_channel_success = match sniffer.next_message_from_upstream() {
            Some((
                _,
                AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(msg)),
            )) => msg,
            msg => panic!(
                "Expected OpenExtendedMiningChannelSuccess message, found: {:?}",
                msg
            ),
        };
        let channel_id = open_extended_mining_channel_success.channel_id;
        channel_ids.push(channel_id);

        // we expect this initial NewExtendedMiningJob message to be directed to the newly created
        // channel ID, not the group channel ID this is actually asserting pool behavior,
        // not tProxy but still good to have, to ensure the global sanity of the test
        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
            )
            .await;
        let new_extended_mining_job = match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => msg,
            msg => panic!("Expected NewExtendedMiningJob message, found: {:?}", msg),
        };
        assert_eq!(new_extended_mining_job.channel_id, channel_id);
        assert_ne!(
            new_extended_mining_job.channel_id,
            EXPECTED_GROUP_CHANNEL_ID
        );

        // we expect this initial SetNewPrevHash message to be directed to the newly created channel
        // ID, not the group channel ID this is actually asserting pool behavior, not tProxy
        // but still good to have, to ensure the global sanity of the test
        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;
        let set_new_prev_hash = match sniffer.next_message_from_upstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(msg)))) => msg,
            msg => panic!("Expected SetNewPrevHash message, found: {:?}", msg),
        };
        assert_eq!(set_new_prev_hash.channel_id, channel_id);
        assert_ne!(set_new_prev_hash.channel_id, EXPECTED_GROUP_CHANNEL_ID);
    }

    // all channels must submit at least one share with job_id = 1
    let mut channel_submitted_to: HashSet<u32> = channel_ids.clone().into_iter().collect();
    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        if submit_shares_extended.job_id != 1 {
            continue;
        }

        assert_ne!(submit_shares_extended.channel_id, EXPECTED_GROUP_CHANNEL_ID);

        channel_submitted_to.remove(&submit_shares_extended.channel_id);
        if channel_submitted_to.is_empty() {
            break;
        }
    }

    // now let's force a mempool update, so we trigger a NewExtendedMiningJob message
    // that's actually directed to the group channel ID, and not each individual channel
    tp.create_mempool_transaction().unwrap();

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    let new_extended_mining_job = match sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => msg,
        msg => panic!("Expected NewExtendedMiningJob message, found: {:?}", msg),
    };
    assert_eq!(
        new_extended_mining_job.channel_id,
        EXPECTED_GROUP_CHANNEL_ID
    );

    // all channels must submit at least one share with job_id = 2
    let mut channel_submitted_to: HashSet<u32> = channel_ids.clone().into_iter().collect();
    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        if submit_shares_extended.job_id != 2 {
            continue;
        }

        assert_ne!(submit_shares_extended.channel_id, EXPECTED_GROUP_CHANNEL_ID);

        channel_submitted_to.remove(&submit_shares_extended.channel_id);
        if channel_submitted_to.is_empty() {
            break;
        }
    }

    // take the mining.notify prevhash from the first miner
    let prevhash_before_chain_tip_update = {
        let mut prevhash_before_chain_tip_update = None;
        sv1_sniffers[0]
            .wait_and_assert(
                SV1MessageFilter::WithMessageName("mining.notify"),
                MessageDirection::ToDownstream,
                |msg| match msg {
                    sv1_api::Message::Notification(notif) => {
                        let notify = sv1_api::server_to_client::Notify::try_from(notif.clone())
                            .expect("Failed to parse mining.notify");
                        prevhash_before_chain_tip_update = Some(notify.prev_hash.clone());
                    }
                    _ => panic!("Expected Notification for mining.notify"),
                },
            )
            .await;
        prevhash_before_chain_tip_update
            .expect("Failed to capture prevhash before chain tip update")
    };

    // now let's force a chain tip update, so we trigger a NewExtendedMiningJob + SetNewPrevHash
    // message pair
    tp.generate_blocks(1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    let new_extended_mining_job = match sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(msg)))) => msg,
        msg => panic!("Expected NewExtendedMiningJob message, found: {:?}", msg),
    };
    assert_eq!(
        new_extended_mining_job.channel_id,
        EXPECTED_GROUP_CHANNEL_ID
    );

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;
    let set_new_prev_hash = match sniffer.next_message_from_upstream() {
        Some((_, AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(msg)))) => msg,
        msg => panic!("Expected SetNewPrevHash message, found: {:?}", msg),
    };
    assert_eq!(set_new_prev_hash.channel_id, EXPECTED_GROUP_CHANNEL_ID);

    // capture prevhash from SV1 mining.notify after chain tip update and assert it changed
    // check ALL miners to ensure they all received the updated prevhash
    for (i, sv1_sniffer) in sv1_sniffers.iter().enumerate() {
        sv1_sniffer
            .wait_and_assert(
                SV1MessageFilter::WithMessageName("mining.notify"),
                MessageDirection::ToDownstream,
                |msg| match msg {
                    sv1_api::Message::Notification(notif) => {
                        let notify = sv1_api::server_to_client::Notify::try_from(notif.clone())
                            .expect("Failed to parse mining.notify");
                        let prevhash_after_chain_tip_update = notify.prev_hash.clone();

                        // assert that the prevhash changed after the chain tip update
                        assert_ne!(
                            prevhash_before_chain_tip_update, prevhash_after_chain_tip_update,
                            "Miner {} mining.notify prevhash should change after chain tip update. Before: {}, After: {}",
                            i, prevhash_before_chain_tip_update, prevhash_after_chain_tip_update
                        );

                        println!("Miner {} mining.notify prevhash changed after chain tip update. Before: {}, After: {}", i, prevhash_before_chain_tip_update, prevhash_after_chain_tip_update);
                    }
                    _ => panic!("Expected Notification for mining.notify"),
                },
            )
            .await;
    }

    // all channels must submit at least one share with job_id = 3
    let mut channel_submitted_to: HashSet<u32> = channel_ids.clone().into_iter().collect();
    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        if submit_shares_extended.job_id != 3 {
            continue;
        }

        assert_ne!(submit_shares_extended.channel_id, EXPECTED_GROUP_CHANNEL_ID);

        channel_submitted_to.remove(&submit_shares_extended.channel_id);
        if channel_submitted_to.is_empty() {
            break;
        }
    }
    shutdown_all!(translator, pool);
}

/// This test launches a tProxy in non-aggregated mode and leverages a MockUpstream to test the
/// correct behavior of handling SetGroupChannel messages.
///
/// We first send a SetGroupChannel message to set a group channel ID A and B, and then we send a
/// NewExtendedMiningJob + SetNewPrevHash message pair to group channel ID A.
///
/// We then assert that all channels in group channel ID A must submit at least one share with
/// job_id = 2, and channels in group channel ID B must NOT submit any shares with job_id = 2.
#[tokio::test]
async fn non_aggregated_translator_handles_set_group_channel_message() {
    start_tracing();

    let mock_upstream_addr = get_available_address();
    let mock_upstream = MockUpstream::new(mock_upstream_addr, WithSetup::no());
    let send_to_tproxy = mock_upstream.start().await;

    let (sniffer, sniffer_addr) = start_sniffer("", mock_upstream_addr, false, vec![], None);

    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[sniffer_addr], false, vec![], vec![], None, false).await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    let setup_connection_success = AnyMessage::Common(CommonMessages::SetupConnectionSuccess(
        SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        },
    ));
    send_to_tproxy.send(setup_connection_success).await.unwrap();

    const N_EXTENDED_CHANNELS: u32 = 6;
    const GROUP_CHANNEL_ID_A: u32 = 100;
    const GROUP_CHANNEL_ID_B: u32 = 200;

    // we need to keep references to each minerd
    // otherwise they would be dropped
    let mut minerd_vec = Vec::new();

    // spawn minerd processes to force opening N_EXTENDED_CHANNELS extended channels
    for i in 0..N_EXTENDED_CHANNELS {
        let (minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
        minerd_vec.push(minerd_process);

        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            )
            .await;
        let open_extended_mining_channel: OpenExtendedMiningChannel = loop {
            match sniffer.next_message_from_downstream() {
                Some((
                    _,
                    AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannel(msg)),
                )) => {
                    break msg;
                }
                _ => continue,
            };
        };

        let open_extended_mining_channel_success =
            AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccess {
                    request_id: open_extended_mining_channel.request_id,
                    channel_id: i,
                    target: hex::decode(
                        "0000137c578190689425e3ecf8449a1af39db0aed305d9206f45ac32fe8330fc",
                    )
                    .unwrap()
                    .try_into()
                    .unwrap(),
                    // full extranonce has a total of 8 bytes
                    extranonce_size: 4,
                    extranonce_prefix: vec![0x00, 0x01, 0x00, i as u8].try_into().unwrap(),
                    group_channel_id: GROUP_CHANNEL_ID_A,
                },
            ));
        send_to_tproxy
            .send(open_extended_mining_channel_success)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            )
            .await;

        let new_extended_mining_job = AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(NewExtendedMiningJob {
            channel_id: i,
            job_id: 1,
            min_ntime: Sv2Option::new(None),
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255::new(vec![]).unwrap(),
            // scriptSig for a total of 8 bytes of extranonce
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff225200162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
        }));

        send_to_tproxy.send(new_extended_mining_job).await.unwrap();
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
            )
            .await;

        let set_new_prev_hash =
            AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(SetNewPrevHash {
                channel_id: i,
                job_id: 1,
                prev_hash: hex::decode(
                    "3ab7089cd2cd30f133552cfde82c4cb239cd3c2310306f9d825e088a1772cc39",
                )
                .unwrap()
                .try_into()
                .unwrap(),
                min_ntime: 1766782170,
                nbits: 0x207fffff,
            }));

        send_to_tproxy.send(set_new_prev_hash).await.unwrap();
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;
    }

    // half of the channels belong to GROUP_CHANNEL_ID_A
    let group_channel_a_ids = (0..N_EXTENDED_CHANNELS)
        .filter(|i| i % 2 != 0)
        .collect::<Vec<_>>();

    // half of the channels belong to GROUP_CHANNEL_ID_B
    let group_channel_b_ids = (0..N_EXTENDED_CHANNELS)
        .filter(|i| i % 2 == 0)
        .collect::<Vec<_>>();

    // send a SetGroupChannel message to set GROUP_CHANNEL_ID_B
    let set_group_channel =
        AnyMessage::Mining(parsers_sv2::Mining::SetGroupChannel(SetGroupChannel {
            channel_ids: group_channel_b_ids.clone().into(),
            group_channel_id: GROUP_CHANNEL_ID_B,
        }));
    send_to_tproxy.send(set_group_channel).await.unwrap();

    // send a NewExtendedMiningJob + SetNewPrevHash message pair ONLY to GROUP_CHANNEL_ID_B
    let new_extended_mining_job = AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(NewExtendedMiningJob {
        channel_id: GROUP_CHANNEL_ID_B,
        job_id: 2,
        min_ntime: Sv2Option::new(None),
        version: 0x20000000,
        version_rolling_allowed: true,
        merkle_path: Seq0255::new(vec![]).unwrap(),
        // scriptSig for a total of 8 bytes of extranonce
        coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff225300162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
        coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
    }));

    send_to_tproxy.send(new_extended_mining_job).await.unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    let set_new_prev_hash =
        AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(SetNewPrevHash {
            channel_id: GROUP_CHANNEL_ID_B,
            job_id: 2,
            prev_hash: hex::decode(
                "2089973501ad229333ae0e9c52fa160f95616890db364a71ccfb77773a8b54cb",
            )
            .unwrap()
            .try_into()
            .unwrap(),
            min_ntime: 1766782171,
            nbits: 0x207fffff,
        }));
    send_to_tproxy.send(set_new_prev_hash).await.unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    // all channels in GROUP_CHANNEL_ID_B must submit at least one share with job_id = 2
    // channels in GROUP_CHANNEL_ID_A must NOT submit any shares with job_id = 2
    let mut channels_submitted_to: HashSet<u32> = group_channel_b_ids.clone().into_iter().collect();
    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        if submit_shares_extended.job_id != 2 {
            continue;
        }

        if group_channel_a_ids.contains(&submit_shares_extended.channel_id) {
            panic!(
                "Channel {} should not have submitted a share with job_id = 2",
                submit_shares_extended.channel_id
            );
        }

        channels_submitted_to.remove(&submit_shares_extended.channel_id);
        if channels_submitted_to.is_empty() {
            break;
        }
    }
    translator.shutdown().await;
}

/// This test launches a tProxy in non-aggregated mode and leverages a MockUpstream to test the
/// correct behavior of handling CloseChannel messages.
///
/// First we close a single channel, and assert that no shares are submitted from it.
/// Then we close the group channel, and assert that no shares are submitted from any channel.
#[tokio::test]
async fn non_aggregated_translator_correctly_deals_with_close_channel_message() {
    start_tracing();

    let mock_upstream_addr = get_available_address();
    let mock_upstream = MockUpstream::new(mock_upstream_addr, WithSetup::no());
    let send_to_tproxy = mock_upstream.start().await;

    let (sniffer, sniffer_addr) = start_sniffer("", mock_upstream_addr, false, vec![], None);

    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[sniffer_addr], false, vec![], vec![], None, false).await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    let setup_connection_success = AnyMessage::Common(CommonMessages::SetupConnectionSuccess(
        SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        },
    ));
    send_to_tproxy.send(setup_connection_success).await.unwrap();

    const N_EXTENDED_CHANNELS: u32 = 3;
    const GROUP_CHANNEL_ID: u32 = 100;

    // we need to keep references to each minerd
    // otherwise they would be dropped
    let mut minerd_vec = Vec::new();

    // spawn minerd processes to force opening N_EXTENDED_CHANNELS extended channels
    for i in 0..N_EXTENDED_CHANNELS {
        let (minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
        minerd_vec.push(minerd_process);

        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            )
            .await;
        let open_extended_mining_channel: OpenExtendedMiningChannel = loop {
            match sniffer.next_message_from_downstream() {
                Some((
                    _,
                    AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannel(msg)),
                )) => {
                    break msg;
                }
                _ => continue,
            };
        };

        let open_extended_mining_channel_success =
            AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccess {
                    request_id: open_extended_mining_channel.request_id,
                    channel_id: i,
                    target: hex::decode(
                        "0000137c578190689425e3ecf8449a1af39db0aed305d9206f45ac32fe8330fc",
                    )
                    .unwrap()
                    .try_into()
                    .unwrap(),
                    // full extranonce has a total of 8 bytes
                    extranonce_size: 4,
                    extranonce_prefix: vec![0x00, 0x01, 0x00, i as u8].try_into().unwrap(),
                    group_channel_id: GROUP_CHANNEL_ID,
                },
            ));
        send_to_tproxy
            .send(open_extended_mining_channel_success)
            .await
            .unwrap();

        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            )
            .await;

        let new_extended_mining_job = AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(NewExtendedMiningJob {
            channel_id: i,
            job_id: 1,
            min_ntime: Sv2Option::new(None),
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255::new(vec![]).unwrap(),
            // scriptSig for a total of 8 bytes of extranonce
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff225200162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
        }));

        send_to_tproxy.send(new_extended_mining_job).await.unwrap();
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
            )
            .await;

        let set_new_prev_hash =
            AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(SetNewPrevHash {
                channel_id: i,
                job_id: 1,
                prev_hash: hex::decode(
                    "3ab7089cd2cd30f133552cfde82c4cb239cd3c2310306f9d825e088a1772cc39",
                )
                .unwrap()
                .try_into()
                .unwrap(),
                min_ntime: 1766782170,
                nbits: 0x207fffff,
            }));

        send_to_tproxy.send(set_new_prev_hash).await.unwrap();
        sniffer
            .wait_for_message_type_and_clean_queue(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
            )
            .await;
    }

    // let's wait until all channels send at least one share
    let mut channels_submitted_to: HashSet<u32> = (0..N_EXTENDED_CHANNELS).into_iter().collect();
    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        channels_submitted_to.remove(&submit_shares_extended.channel_id);
        if channels_submitted_to.is_empty() {
            break;
        }
    }

    // let's close one of the channels
    const CLOSED_CHANNEL_ID: u32 = 0;
    let close_channel = AnyMessage::Mining(parsers_sv2::Mining::CloseChannel(CloseChannel {
        channel_id: CLOSED_CHANNEL_ID,
        reason_code: "".to_string().try_into().unwrap(),
    }));
    send_to_tproxy.send(close_channel).await.unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_CLOSE_CHANNEL,
        )
        .await;

    // Drain all pending messages from the sniffer queue
    while sniffer.next_message_from_downstream().is_some() {
        // Keep draining until queue is empty
    }

    // Small delay to let any in-flight messages arrive
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // let's wait until all open channels send at least 5 shares
    // if the closed channel sends a share, the test fails
    let mut share_submission_count = HashMap::new();
    loop {
        sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            )
            .await;
        let submit_shares_extended = match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::SubmitSharesExtended(msg)))) => msg,
            msg => panic!("Expected SubmitSharesExtended message, found: {:?}", msg),
        };

        if submit_shares_extended.channel_id == CLOSED_CHANNEL_ID {
            panic!("Closed channel should not have submitted a share");
        }

        // update the share submission count for the channel
        if let Some(count) = share_submission_count.get_mut(&submit_shares_extended.channel_id) {
            *count += 1;
        } else {
            share_submission_count.insert(submit_shares_extended.channel_id, 1);
        }

        // have all open channels submitted shares?
        if share_submission_count.len() == (N_EXTENDED_CHANNELS - 1) as usize {
            // check if all open channels submitted at least 5 shares
            let all_open_channels_have_enough_shares =
                share_submission_count.values().all(|count| *count >= 5);

            if all_open_channels_have_enough_shares {
                // all open channels submitted at least 5 shares
                break;
            }
        }
    }

    // now let's send a CloseChannel for the group channel
    let close_channel = AnyMessage::Mining(parsers_sv2::Mining::CloseChannel(CloseChannel {
        channel_id: GROUP_CHANNEL_ID,
        reason_code: "".to_string().try_into().unwrap(),
    }));
    send_to_tproxy.send(close_channel).await.unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_CLOSE_CHANNEL,
        )
        .await;

    // wait enough time for any channels to submit some share (which they shouldn't)
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // no shares should arrive after the group channel is closed
    sniffer
        .assert_message_not_present(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            std::time::Duration::from_secs(1),
        )
        .await;
    translator.shutdown().await;
}

/// This test launches a tProxy in aggregated mode and leverages two MockUpstreams to test the
/// correct behavior of handling CloseChannel messages.
///
/// We first send a CloseChannel message to a single channel, and assert that no shares are
/// submitted from it. Then we send a CloseChannel message to the group channel, and assert that no
/// shares are submitted from any channel.
#[tokio::test]
async fn aggregated_translator_triggers_fallback_on_close_channel_message() {
    start_tracing();

    // first upstream server mock
    let mock_upstream_addr_a = get_available_address();
    let mock_upstream_a = MockUpstream::new(mock_upstream_addr_a, WithSetup::no());
    let send_to_tproxy_a = mock_upstream_a.start().await;
    let (sniffer_a, sniffer_addr_a) = start_sniffer("", mock_upstream_addr_a, false, vec![], None);

    // fallback upstream server mock
    let mock_upstream_addr_b = get_available_address();
    let mock_upstream_b = MockUpstream::new(
        mock_upstream_addr_b,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    );
    let _send_to_tproxy_b = mock_upstream_b.start().await;
    let (sniffer_b, sniffer_addr_b) = start_sniffer("", mock_upstream_addr_b, false, vec![], None);

    let (translator, tproxy_addr, _) = start_sv2_translator(
        &[sniffer_addr_a, sniffer_addr_b],
        true,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    let setup_connection_success = AnyMessage::Common(CommonMessages::SetupConnectionSuccess(
        SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        },
    ));
    send_to_tproxy_a
        .send(setup_connection_success)
        .await
        .unwrap();

    // we need to keep references to each minerd
    // otherwise they would be dropped
    let mut minerd_vec = Vec::new();

    let (minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
    minerd_vec.push(minerd_process);

    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    let open_extended_mining_channel: OpenExtendedMiningChannel = loop {
        match sniffer_a.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannel(msg)))) => {
                break msg;
            }
            _ => continue,
        };
    };

    let open_extended_mining_channel_success = AnyMessage::Mining(
        parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccess {
            request_id: open_extended_mining_channel.request_id,
            channel_id: 0,
            target: hex::decode("0000137c578190689425e3ecf8449a1af39db0aed305d9206f45ac32fe8330fc")
                .unwrap()
                .try_into()
                .unwrap(),
            // full extranonce has a total of 12 bytes
            extranonce_size: 8,
            extranonce_prefix: vec![0x00, 0x01, 0x00, 0x00].try_into().unwrap(),
            group_channel_id: 100,
        }),
    );
    send_to_tproxy_a
        .send(open_extended_mining_channel_success)
        .await
        .unwrap();

    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let new_extended_mining_job = AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(NewExtendedMiningJob {
            channel_id: 0,
            job_id: 1,
            min_ntime: Sv2Option::new(None),
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255::new(vec![]).unwrap(),
            // scriptSig for a total of 8 bytes of extranonce
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
        }));

    send_to_tproxy_a
        .send(new_extended_mining_job)
        .await
        .unwrap();
    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    let set_new_prev_hash =
        AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(SetNewPrevHash {
            channel_id: 0,
            job_id: 1,
            prev_hash: hex::decode(
                "3ab7089cd2cd30f133552cfde82c4cb239cd3c2310306f9d825e088a1772cc39",
            )
            .unwrap()
            .try_into()
            .unwrap(),
            min_ntime: 1766782170,
            nbits: 0x207fffff,
        }));

    send_to_tproxy_a.send(set_new_prev_hash).await.unwrap();
    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    // up until now, we have done the usual channel initialization process
    // now, lets send a CloseChannel message for the channel
    let close_channel = AnyMessage::Mining(parsers_sv2::Mining::CloseChannel(CloseChannel {
        channel_id: 0,
        reason_code: "".to_string().try_into().unwrap(),
    }));
    send_to_tproxy_a.send(close_channel).await.unwrap();

    // this should trigger fallback
    sniffer_b
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    translator.shutdown().await;
}

// Verify's that the non-aggregated mode translator does not shut down if an
// upstream message references a channel ID that is not associated with any
// downstream in the tproxy.
// See: https://github.com/stratum-mining/sv2-apps/issues/216
#[tokio::test]
async fn translator_does_not_shutdown_on_missing_downstream_channel() {
    start_tracing();

    // upstream server mock
    let mock_upstream_addr_a = get_available_address();
    let mock_upstream_a = MockUpstream::new(mock_upstream_addr_a, WithSetup::no());
    let send_to_tproxy_a = mock_upstream_a.start().await;
    let (sniffer_a, sniffer_addr_a) = start_sniffer("", mock_upstream_addr_a, false, vec![], None);

    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[sniffer_addr_a], false, vec![], vec![], None, false).await;

    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    let setup_connection_success = AnyMessage::Common(CommonMessages::SetupConnectionSuccess(
        SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        },
    ));
    send_to_tproxy_a
        .send(setup_connection_success)
        .await
        .unwrap();

    let (_minerd_process, _minerd_addr) =
        start_minerd(tproxy_addr.clone(), None, None, false).await;

    sniffer_a
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    let open_extended_mining_channel: OpenExtendedMiningChannel = loop {
        match sniffer_a.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannel(msg)))) => {
                break msg;
            }
            _ => continue,
        };
    };

    let open_extended_mining_channel_success = AnyMessage::Mining(
        parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccess {
            request_id: open_extended_mining_channel.request_id,
            channel_id: 0,
            target: hex::decode("0000137c578190689425e3ecf8449a1af39db0aed305d9206f45ac32fe8330fc")
                .unwrap()
                .try_into()
                .unwrap(),
            // full extranonce has a total of 12 bytes
            extranonce_size: 8,
            extranonce_prefix: vec![0x00, 0x01, 0x00, 0x00].try_into().unwrap(),
            group_channel_id: 100,
        }),
    );
    send_to_tproxy_a
        .send(open_extended_mining_channel_success)
        .await
        .unwrap();

    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    let new_extended_mining_job = AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(NewExtendedMiningJob {
            channel_id: 0,
            job_id: 1,
            min_ntime: Sv2Option::new(None),
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255::new(vec![]).unwrap(),
            // scriptSig for a total of 8 bytes of extranonce
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08").unwrap().try_into().unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
        }));

    send_to_tproxy_a
        .send(new_extended_mining_job)
        .await
        .unwrap();
    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    let set_new_prev_hash =
        AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(SetNewPrevHash {
            channel_id: 0,
            job_id: 1,
            prev_hash: hex::decode(
                "3ab7089cd2cd30f133552cfde82c4cb239cd3c2310306f9d825e088a1772cc39",
            )
            .unwrap()
            .try_into()
            .unwrap(),
            min_ntime: 1766782170,
            nbits: 0x207fffff,
        }));

    send_to_tproxy_a.send(set_new_prev_hash).await.unwrap();
    sniffer_a
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    // SetTarget message with channel id not present in downstream
    let set_target = AnyMessage::Mining(parsers_sv2::Mining::SetTarget(SetTarget {
        channel_id: 5,
        maximum_target: [0; 32].into(),
    }));
    send_to_tproxy_a.send(set_target).await.unwrap();

    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(TcpListener::bind(tproxy_addr).await.is_err());
    translator.shutdown().await;
}

/// This test verifies that in aggregated mode, a new downstream connection that arrives
/// between a future NewExtendedMiningJob and its corresponding SetNewPrevHash will correctly
/// receive the future job and be able to submit shares after SetNewPrevHash activates the job.
///
/// This is a regression test for the "Failed to set new prev hash: JobIdNotFound" error
/// that occurred when new downstream channels were created while a future job was pending.
///
/// See: https://github.com/stratum-mining/sv2-apps/issues/223
#[tokio::test]
async fn aggregated_translator_handles_downstream_connecting_during_future_job() {
    start_tracing();

    let mock_upstream_addr = get_available_address();
    let mock_upstream = MockUpstream::new(mock_upstream_addr, WithSetup::no());
    let send_to_tproxy = mock_upstream.start().await;

    // ignore SubmitSharesSuccess messages to simplify the test flow
    let ignore_submit_shares_success = IgnoreMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
    );
    let (sniffer, sniffer_addr) = start_sniffer(
        "future_job_test",
        mock_upstream_addr,
        false,
        vec![ignore_submit_shares_success.into()],
        None,
    );

    // Start translator in aggregated mode
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[sniffer_addr], true, vec![], vec![], None, false).await;

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    let setup_connection_success = AnyMessage::Common(CommonMessages::SetupConnectionSuccess(
        SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        },
    ));
    send_to_tproxy.send(setup_connection_success).await.unwrap();

    // Keep references to minerd processes and SV1 sniffers so they don't get dropped
    let mut minerd_vec = Vec::new();

    // Start SV1 sniffer for the first miner
    let (sv1_sniffer_1, sv1_sniffer_addr_1) = start_sv1_sniffer(tproxy_addr);

    // Start the first minerd (through SV1 sniffer) to trigger OpenExtendedMiningChannel
    let (minerd_process_1, _minerd_addr_1) =
        start_minerd(sv1_sniffer_addr_1, None, None, false).await;
    minerd_vec.push(minerd_process_1);

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    let open_extended_mining_channel: OpenExtendedMiningChannel = loop {
        match sniffer.next_message_from_downstream() {
            Some((_, AnyMessage::Mining(parsers_sv2::Mining::OpenExtendedMiningChannel(msg)))) => {
                break msg;
            }
            _ => continue,
        };
    };

    // Send OpenExtendedMiningChannelSuccess for the aggregated channel
    let open_extended_mining_channel_success = AnyMessage::Mining(
        parsers_sv2::Mining::OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccess {
            request_id: open_extended_mining_channel.request_id,
            channel_id: 2, // aggregated channel ID
            target: hex::decode("0000137c578190689425e3ecf8449a1af39db0aed305d9206f45ac32fe8330fc")
                .unwrap()
                .try_into()
                .unwrap(),
            // full extranonce has a total of 12 bytes
            extranonce_size: 8,
            extranonce_prefix: vec![0x00, 0x01, 0x00, 0x00].try_into().unwrap(),
            group_channel_id: 1,
        }),
    );
    send_to_tproxy
        .send(open_extended_mining_channel_success)
        .await
        .unwrap();

    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    // Send a FUTURE job (min_ntime: None) - this job is not active yet!
    let future_job = AnyMessage::Mining(parsers_sv2::Mining::NewExtendedMiningJob(
        NewExtendedMiningJob {
            channel_id: 2,
            job_id: 1,
            min_ntime: Sv2Option::new(None), // This makes it a future job!
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255::new(vec![]).unwrap(),
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f0c").unwrap().try_into().unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000").unwrap().try_into().unwrap(),
        },
    ));

    send_to_tproxy.send(future_job).await.unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;

    // CRITICAL: Start a SECOND minerd BEFORE sending SetNewPrevHash
    // This is the race condition we're testing - the new downstream connects
    // while a future job is pending but not yet activated

    // Start SV1 sniffer for the second miner
    let (sv1_sniffer_2, sv1_sniffer_addr_2) = start_sv1_sniffer(tproxy_addr);

    let (minerd_process_2, _minerd_addr_2) =
        start_minerd(sv1_sniffer_addr_2, None, None, false).await;
    minerd_vec.push(minerd_process_2);

    // Give time for the second minerd to connect and the channel to be created
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Now send SetNewPrevHash to activate the future job
    // Without the fix, this would cause "Failed to set new prev hash: JobIdNotFound"
    // because the second downstream's channel wouldn't have the future job
    let set_new_prev_hash =
        AnyMessage::Mining(parsers_sv2::Mining::SetNewPrevHash(SetNewPrevHash {
            channel_id: 2,
            job_id: 1,
            prev_hash: hex::decode(
                "3ab7089cd2cd30f133552cfde82c4cb239cd3c2310306f9d825e088a1772cc39",
            )
            .unwrap()
            .try_into()
            .unwrap(),
            min_ntime: 1766782170,
            nbits: 0x207fffff,
        }));

    send_to_tproxy.send(set_new_prev_hash).await.unwrap();
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    // Verify BOTH miners receive the mining.notify message
    sv1_sniffer_1
        .wait_for_message(&["mining.notify"], MessageDirection::ToDownstream)
        .await;
    sv1_sniffer_2
        .wait_for_message(&["mining.notify"], MessageDirection::ToDownstream)
        .await;

    // Verify BOTH miners submit shares (mining.submit)
    // This proves both miners are working correctly after the future job was activated
    sv1_sniffer_1
        .wait_for_message(&["mining.submit"], MessageDirection::ToUpstream)
        .await;
    sv1_sniffer_2
        .wait_for_message(&["mining.submit"], MessageDirection::ToUpstream)
        .await;
    translator.shutdown().await;
}

// This test verifies that the pool server continues accepting new connection
// requests while performing handshakes with other clients. It also checks the
// scenario where a downstream client connects but never completes the handshake.
//
// The goal is to ensure such incomplete handshakes do not block the server or
// render it unresponsive.
//
// For more context see:
// https://github.com/stratum-mining/sv2-apps/issues/241
#[tokio::test]
async fn pool_does_not_hang_on_no_handshake() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let ephemeral_stream = TcpStream::connect(pool_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (pool_translator_sniffer, pool_translator_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (translator, _, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        false,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    pool_translator_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    // Sleep for time just more than `NOISE_HANDSHAKE_TIMEOUT`
    tokio::time::sleep(Duration::from_secs(10)).await;
    let buf = vec![1];

    // the OS may not immediately detect that the connection was closed after the handshake
    // timeout. On some systems (macOS triggered this), try_write can still succeed for a short time
    // after the remote end has closed the socket. We retry until the write fails or we hit
    // the max retries to avoid flaky test failures.
    let mut retries = 0;
    let mut value;
    loop {
        value = ephemeral_stream.try_write(&buf);
        if value.is_err() || retries >= 100 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        retries += 1;
    }
    assert!(value.is_err());
    shutdown_all!(translator, pool);
}

// This test verifies that when multiple downstream miners connect very quickly
// to the tproxy (in aggregated mode), it does NOT forward multiple
// `OpenExtendedMiningChannel` messages upstream.
//
// In aggregated mode, all downstream miners should share a single upstream
// extended channel. Therefore, even under rapid concurrent connections,
// only one `OpenExtendedMiningChannel` must be sent upstream.
//
// More info can be found here: https://github.com/stratum-mining/sv2-apps/issues/157
#[tokio::test]
async fn tproxy_sends_single_open_extended_mining_channel_in_aggregated_mode() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::High);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    let (pool_translator_sniffer, pool_translator_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (tproxy, tproxy_addr, _) = start_sv2_translator(
        &[pool_translator_sniffer_addr],
        true,
        vec![],
        vec![],
        None,
        false,
    )
    .await;

    pool_translator_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    pool_translator_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    let mut minerd_vec = Vec::new();
    // connect several Sv1 miners to tProxy
    const N_MINERDS: u32 = 10;
    for _i in 0..N_MINERDS {
        let (minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;
        minerd_vec.push(minerd_process);
    }

    pool_translator_sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    assert!(
        pool_translator_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                Duration::from_secs(10)
            )
            .await
    );

    shutdown_all!(pool, tproxy);
}
