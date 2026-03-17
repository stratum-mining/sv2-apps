use integration_tests_sv2::{template_provider::DifficultyLevel, *};
use interceptor::MessageDirection;
use stratum_apps::stratum_core::sv1_api::{self, server_to_client};
use sv1_sniffer::SV1MessageFilter;

#[tokio::test]
async fn test_basic_sv1() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[pool_addr], false, vec![], vec![], None, false).await;
    let (sniffer_sv1, sniffer_sv1_addr) = start_sv1_sniffer(tproxy_addr);
    let (_minerd_process, _minerd_addr) = start_minerd(sniffer_sv1_addr, None, None, false).await;
    sniffer_sv1
        .wait_for_message(&["mining.subscribe"], MessageDirection::ToUpstream)
        .await;
    sniffer_sv1
        .wait_for_message(&["mining.authorize"], MessageDirection::ToUpstream)
        .await;
    sniffer_sv1
        .wait_for_message(&["mining.set_difficulty"], MessageDirection::ToDownstream)
        .await;
    sniffer_sv1
        .wait_for_message(&["mining.notify"], MessageDirection::ToDownstream)
        .await;
    shutdown_all!(translator, pool);
}

/// This test demonstrates the `SnifferSV1::wait_and_assert` feature, which allows you to:
/// 1. Wait for a specific SV1 message to arrive
/// 2. Execute custom assertions on the message content
///
/// This is useful when you need to verify not just that a message was received, but also that
/// it contains the expected data.
///
/// # Example Usage
///
/// The test shows two ways to filter messages:
/// - `SV1MessageFilter::WithMessageName`: Filter by method name (e.g., "mining.notify")
/// - `SV1MessageFilter::WithMessageId`: Filter by message ID for responses
///
/// The assertion closure receives the full `sv1_api::Message` and can perform any validation
/// needed on the message fields.
#[tokio::test]
async fn test_sniffer_sv1_wait_and_assert() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    tp.fund_wallet().expect("Failed to fund wallet");
    let (pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (translator, tproxy_addr, _) =
        start_sv2_translator(&[pool_addr], false, vec![], vec![], None, false).await;
    let (sniffer_sv1, sniffer_sv1_addr) = start_sv1_sniffer(tproxy_addr);
    let (_minerd_process, _minerd_addr) = start_minerd(sniffer_sv1_addr, None, None, false).await;

    // Example 1: Wait for a mining.subscribe request, extract its ID, and verify the response
    // This demonstrates the complete request-response flow and both filter types
    let subscribe_id = {
        let mut extracted_id = None;
        sniffer_sv1
            .wait_and_assert(
                SV1MessageFilter::WithMessageName("mining.subscribe"),
                MessageDirection::ToUpstream,
                |msg| {
                    // The message should be a StandardRequest with the mining.subscribe method
                    match msg {
                        sv1_api::Message::StandardRequest(req) => {
                            assert_eq!(req.method, "mining.subscribe");
                            extracted_id = Some(req.id);
                            // mining.subscribe typically has a user agent parameter
                            // params is a serde_json::Value, check if it's an array with elements
                            if let Some(params_array) = req.params.as_array() {
                                assert!(
                                    !params_array.is_empty(),
                                    "subscribe should have parameters"
                                );
                            }
                        }
                        _ => panic!("Expected StandardRequest for mining.subscribe"),
                    }
                },
            )
            .await;
        extracted_id.expect("Failed to extract subscribe ID")
    };

    // Now wait for the response to that specific message ID
    sniffer_sv1
        .wait_and_assert(
            SV1MessageFilter::WithMessageId(subscribe_id),
            MessageDirection::ToDownstream,
            |msg| {
                match msg {
                    sv1_api::Message::OkResponse(res) => {
                        assert_eq!(res.id, subscribe_id);
                        // Verify the response has the expected subscription data
                        assert!(
                            !res.result.is_null(),
                            "subscribe response should contain subscription details"
                        );
                    }
                    sv1_api::Message::ErrorResponse(err) => {
                        panic!("Expected success response but got error: {:?}", err.error);
                    }
                    _ => panic!("Expected OkResponse or ErrorResponse for subscribe"),
                }
            },
        )
        .await;

    // Example 2: Wait for a mining.notify notification and validate job parameters
    // This demonstrates parsing into a properly typed struct
    sniffer_sv1
        .wait_and_assert(
            SV1MessageFilter::WithMessageName("mining.notify"),
            MessageDirection::ToDownstream,
            |msg| {
                match msg {
                    sv1_api::Message::Notification(notif) => {
                        assert_eq!(notif.method, "mining.notify");
                        // Parse the notification into a properly typed Notify struct
                        let notify = server_to_client::Notify::try_from(notif.clone())
                            .expect("Failed to parse mining.notify");

                        // Verify job parameters are present
                        assert!(!notify.job_id.is_empty(), "job_id should not be empty");

                        // Verify that the merkle_branch is empty
                        assert!(
                            notify.merkle_branch.is_empty(),
                            "merkle_branch should be empty"
                        );
                    }
                    _ => panic!("Expected Notification for mining.notify"),
                }
            },
        )
        .await;
    shutdown_all!(translator, pool);
}
