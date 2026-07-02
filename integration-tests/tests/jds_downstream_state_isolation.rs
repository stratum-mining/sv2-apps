use integration_tests_sv2::{
    interceptor::MessageDirection,
    mock_roles::{MockDownstream, WithSetup},
    sniffer::Sniffer,
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::Protocol,
    job_declaration_sv2::*,
    mining_sv2::*,
    parsers_sv2::{AnyMessage, JobDeclaration, Mining},
};

#[tokio::test]
// Regression coverage for https://github.com/stratum-mining/sv2-apps/issues/590
//
// Rationale:
// - DeclareMiningJob.request_id is scoped to a single downstream connection.
// - The prior implementation in JDS stored declaration state in a global map keyed only by
//   request_id, so two different downstreams reusing the same request_id could overwrite each
//   other.
// - That cross-downstream state collision could surface as SetCustomMiningJobError in one flow,
//   even though both downstreams followed valid DeclareMiningJob/SetCustomMiningJob sequences.
//
// This test intentionally drives two independent downstreams to collide on request_id and then
// asserts both flows complete without any SetCustomMiningJobError, proving JDS state isolation by
// downstream.
async fn jds_isolates_state_for_colliding_request_ids_across_downstreams() {
    start_tracing();

    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;

    let (jdc1_jds_sniffer, jdc1_jds_sniffer_addr) =
        start_sniffer("jdc1-jds", jds_addr, false, vec![], None);
    let (jdc2_jds_sniffer, jdc2_jds_sniffer_addr) =
        start_sniffer("jdc2-jds", jds_addr, false, vec![], None);
    let (jdc1_pool_sniffer, jdc1_pool_sniffer_addr) =
        start_sniffer("jdc1-pool", pool_addr, false, vec![], None);
    let (jdc2_pool_sniffer, jdc2_pool_sniffer_addr) =
        start_sniffer("jdc2-pool", pool_addr, false, vec![], None);

    let (jdc1, jdc1_addr, _) = start_jdc(
        &[(jdc1_pool_sniffer_addr, jdc1_jds_sniffer_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );
    let (jdc2, jdc2_addr, _) = start_jdc(
        &[(jdc2_pool_sniffer_addr, jdc2_jds_sniffer_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        false,
        None,
    );

    // Attach one mock mining downstream per JDC so both JDCs open mining channels and
    // independently trigger the DeclareMiningJob/SetCustomMiningJob flow against the same JDS.
    let send_to_jdc1 = MockDownstream::new(
        jdc1_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;
    let send_to_jdc2 = MockDownstream::new(
        jdc2_addr,
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
    )
    .start()
    .await;

    // Trigger both JDCs to start job declaration for their mining channel.
    send_to_jdc1
        .send(AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
            OpenExtendedMiningChannel {
                request_id: 1,
                user_identity: b"user_identity".to_vec().try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
                min_extranonce_size: 0,
            },
        )))
        .await
        .unwrap();
    send_to_jdc2
        .send(AnyMessage::Mining(Mining::OpenExtendedMiningChannel(
            OpenExtendedMiningChannel {
                request_id: 1,
                user_identity: b"user_identity".to_vec().try_into().unwrap(),
                nominal_hash_rate: 1000.0,
                max_target: vec![0xff; 32].try_into().unwrap(),
                min_extranonce_size: 0,
            },
        )))
        .await
        .unwrap();

    // Wait until both DeclareMiningJob messages are observed, then assert request_id collision.
    jdc1_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;
    jdc2_jds_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_DECLARE_MINING_JOB,
        )
        .await;

    let jdc1_request_id = next_declare_mining_job_request_id(&jdc1_jds_sniffer);
    let jdc2_request_id = next_declare_mining_job_request_id(&jdc2_jds_sniffer);
    assert_eq!(
        jdc1_request_id, jdc2_request_id,
        "the regression test expects both downstreams to collide on request_id"
    );

    // Both downstream flows must complete without token-crossing failures.
    jdc1_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        )
        .await;
    jdc2_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_CUSTOM_MINING_JOB_SUCCESS,
        )
        .await;

    assert_no_set_custom_mining_job_error(&jdc1_pool_sniffer);
    assert_no_set_custom_mining_job_error(&jdc2_pool_sniffer);

    jdc1.shutdown().await;
    jdc2.shutdown().await;
    pool.shutdown().await;
}

// Returns the first DeclareMiningJob request_id observed from a given sniffer.
fn next_declare_mining_job_request_id(sniffer: &Sniffer<'_>) -> u32 {
    loop {
        if let Some((_, AnyMessage::JobDeclaration(JobDeclaration::DeclareMiningJob(msg)))) =
            sniffer.next_message_from_downstream()
        {
            return msg.request_id;
        }
    }
}

// Scans captured downstream responses and ensures no SetCustomMiningJobError is emitted.
fn assert_no_set_custom_mining_job_error(sniffer: &Sniffer<'_>) {
    while let Some((_, message)) = sniffer.next_message_from_upstream() {
        if let AnyMessage::Mining(Mining::SetCustomMiningJobError(msg)) = message {
            panic!(
                "unexpected SetCustomMiningJobError while validating colliding request_id flow: {}",
                msg.error_code.as_utf8_or_hex()
            );
        }
    }
}
