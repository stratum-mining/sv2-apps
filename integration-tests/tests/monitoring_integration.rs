// Dedicated integration tests for monitoring/metrics endpoints.
//
// These tests spin up various SV2 topologies with monitoring enabled and validate
// that the correct Prometheus metrics and JSON API endpoints are exposed.

use integration_tests_sv2::{
    interceptor::MessageDirection, prometheus_metrics_assertions::*,
    template_provider::DifficultyLevel, *,
};
use stratum_apps::{
    monitoring::{
        ErrorResponse, GlobalInfo, HealthResponse, RootResponse, ServerChannelsResponse,
        ServerResponse, Sv1ClientInfo, Sv1ClientsResponse, Sv2ClientChannelsResponse,
        Sv2ClientResponse, Sv2ClientsResponse, routes,
    },
    stratum_core::mining_sv2::*,
};

/// Hit `/api/v1/health` and assert `status == "ok"`.
async fn check_health(api: MonitoringApi) {
    let h: HealthResponse = api.fetch_typed(routes::HEALTH).await;
    assert_eq!(
        h.status, "ok",
        "health endpoint should report ok, got {:?}",
        h
    );
}

/// Timeout for polling metric assertions. Generous enough for slow CI.
const METRIC_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 1. Pool + SV2 Mining Device (standard channel) Pool role exposes: client metrics (connections,
//    channels, shares, hashrate) Pool has NO upstream, so server metrics should be absent.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pool_monitoring_with_sv2_mining_device() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, pool_monitoring) =
        start_pool(sv2_tp_config(tp_addr), vec![], vec![], true).await;
    let (sniffer, sniffer_addr) = start_sniffer("A", pool_addr, false, vec![], None);
    // Give the mining device an explicit user_id so its user_identity label on
    // the pool's per-channel metrics is a meaningful value to assert against.
    start_mining_device_sv2(
        sniffer_addr,
        None,
        None,
        Some("test-miner".to_string()),
        1,
        None,
        true,
    );

    // Wait for a share to be accepted so metrics are populated
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_STANDARD,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;

    let pool_mon =
        MonitoringApi::builder(pool_monitoring.expect("pool monitoring should be enabled")).build();

    // Health API
    check_health(pool_mon).await;

    // Poll until the monitoring cache has refreshed with the new share data for
    // the specific (client, channel, user) we expect from the single mining device.
    // The pool reserves channel_id=1 for internal use and assigns 2 to the first
    // downstream-opened channel.
    let pool_metrics = pool_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_client_shares_accepted_total",
                &[
                    ("client_id", "1"),
                    ("channel_id", "2"),
                    ("user_identity", "test-miner"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_present(&pool_metrics, "sv2_uptime_seconds");

    // Pool has no upstream — server metrics should be absent
    assert_metric_not_present(&pool_metrics, "sv2_server_channels");
    assert_metric_not_present(&pool_metrics, "sv2_server_hashrate_total");

    // Pool should see 1 SV2 client (the mining device) with a standard channel
    assert_metric_eq(&pool_metrics, "sv2_clients_total", 1.0);

    shutdown_all!(pool);
}

// Pool + tProxy + SV1 miner: Pool sees 1 SV2 client, tProxy sees 1 SV1 client and 1 upstream
// channel.
#[tokio::test]
async fn pool_and_tproxy_monitoring_with_sv1_miner() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, pool_monitoring) =
        start_pool(sv2_tp_config(tp_addr), vec![], vec![], true).await;
    let (sniffer, sniffer_addr) = start_sniffer("0", pool_addr, false, vec![], None);
    let (tproxy, tproxy_addr, tproxy_monitoring) =
        start_sv2_translator(&[sniffer_addr], false, vec![], vec![], None, true).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // Wait for shares to flow
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    // -- Pool metrics --
    let pool_mon =
        MonitoringApi::builder(pool_monitoring.expect("pool monitoring should be enabled")).build();
    check_health(pool_mon).await;

    // Poll until the pool's cache has refreshed with tProxy's shares under the
    // specific (client, channel, user) this topology produces. tProxy forwards
    // SV1 worker names by suffixing them onto its configured user_identity.
    let pool_metrics = pool_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_client_shares_accepted_total",
                &[
                    ("client_id", "1"),
                    ("channel_id", "2"),
                    ("user_identity", "user_identity.miner1"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_present(&pool_metrics, "sv2_uptime_seconds");
    assert_metric_eq(&pool_metrics, "sv2_clients_total", 1.0);
    // Pool has no upstream
    assert_metric_not_present(&pool_metrics, "sv2_server_channels");

    // -- tProxy metrics --
    let tproxy_mon =
        MonitoringApi::builder(tproxy_monitoring.expect("tproxy monitoring should be enabled"))
            .build();
    check_health(tproxy_mon).await;
    // tProxy has its own monitoring cache, so poll independently for its
    // upstream-channel share metric under the specific labels it reports. The
    // user_identity reflects the SV1 worker name suffixed onto tProxy's config.
    let tproxy_metrics = tproxy_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_server_shares_accepted_total",
                &[
                    ("channel_id", "2"),
                    ("user_identity", "user_identity.miner1"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_present(&tproxy_metrics, "sv2_uptime_seconds");
    // tProxy has 1 upstream extended channel
    assert_metric_eq(
        &tproxy_metrics,
        Metric::with_labels("sv2_server_channels", &[("channel_type", "extended")]),
        1.0,
    );
    // tProxy should see at least 1 SV1 client
    assert_metric_eq(&tproxy_metrics, "sv1_clients_total", 1.0);
    // tProxy has no SV2 downstreams
    assert_metric_not_present(&tproxy_metrics, "sv2_clients_total");

    shutdown_all!(pool, tproxy);
}

// Pool + JDC + tProxy + 2 SV1 miners: aggregated topology with multiple SV1 clients.
#[tokio::test]
async fn jd_aggregated_topology_monitoring() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, pool_monitoring) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], true).await;
    let (jdc_pool_sniffer, jdc_pool_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (jdc, jdc_addr, _jdc_monitoring) = start_jdc(
        &[(jdc_pool_sniffer_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        true,
        None,
    );
    let (_tproxy_jdc_sniffer, tproxy_jdc_sniffer_addr) =
        start_sniffer("1", jdc_addr, false, vec![], None);
    let (tproxy, tproxy_addr, tproxy_monitoring) =
        start_sv2_translator(&[tproxy_jdc_sniffer_addr], true, vec![], vec![], None, true).await;

    // Start two minerd processes
    let (_minerd_process_1, _minerd_addr_1) = start_minerd(tproxy_addr, None, None, false).await;
    let (_minerd_process_2, _minerd_addr_2) = start_minerd(tproxy_addr, None, None, false).await;

    // Wait for shares to flow through the topology
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;

    // -- Pool metrics: sees 1 SV2 client (JDC), shares accepted --
    let pool_mon =
        MonitoringApi::builder(pool_monitoring.expect("pool monitoring should be enabled")).build();
    check_health(pool_mon).await;

    // Poll until the pool's cache has refreshed with JDC's shares under the
    // specific (client, channel, user) this topology produces.
    let pool_metrics = pool_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_client_shares_accepted_total",
                &[
                    ("client_id", "1"),
                    ("channel_id", "2"),
                    ("user_identity", "IT_TEST"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_present(&pool_metrics, "sv2_uptime_seconds");
    assert_metric_eq(&pool_metrics, "sv2_clients_total", 1.0);
    assert_metric_not_present(&pool_metrics, "sv2_server_channels");

    // -- tProxy metrics (aggregated): 2 SV1 clients, 1 upstream extended channel --
    let tproxy_mon =
        MonitoringApi::builder(tproxy_monitoring.expect("tproxy monitoring should be enabled"))
            .build();
    check_health(tproxy_mon).await;
    // tProxy has its own monitoring cache, so poll independently for its
    // upstream-channel share metric under the specific labels it reports. In
    // aggregated mode both SV1 miners share a single upstream channel; the
    // user_identity reflects whichever worker name reaches tProxy first.
    let tproxy_metrics = tproxy_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_server_shares_accepted_total",
                &[
                    ("channel_id", "2"),
                    ("user_identity", "user_identity.translator-proxy"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_present(&tproxy_metrics, "sv2_uptime_seconds");
    assert_metric_eq(
        &tproxy_metrics,
        Metric::with_labels("sv2_server_channels", &[("channel_type", "extended")]),
        1.0,
    );
    assert_metric_eq(&tproxy_metrics, "sv1_clients_total", 2.0);
    assert_metric_not_present(&tproxy_metrics, "sv2_clients_total");

    shutdown_all!(pool, jdc, tproxy);
}

// Block found detection: JDC topology finds regtest blocks, pool metrics reflect it.
#[tokio::test]
async fn block_found_detected_in_pool_metrics() {
    use stratum_apps::stratum_core::template_distribution_sv2::*;

    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, pool_monitoring) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], true).await;

    let (_jdc_jds_sniffer, jdc_jds_sniffer_addr) =
        start_sniffer("0", jds_addr, false, vec![], None);
    let (jdc_tp_sniffer, jdc_tp_sniffer_addr) = start_sniffer("1", tp_addr, false, vec![], None);
    let (jdc, jdc_addr, _) = start_jdc(
        &[(pool_addr, jdc_jds_sniffer_addr)],
        sv2_tp_config(jdc_tp_sniffer_addr),
        vec![],
        vec![],
        false,
        None,
    );
    let (tproxy, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], false, vec![], vec![], None, false).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // Wait for the block to be submitted to TP
    jdc_tp_sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SUBMIT_SOLUTION)
        .await;

    // Poll until the monitoring cache has refreshed with the block found data.
    // sv2_client_blocks_found_total is a scalar gauge (no labels), so bare-name
    // selector is the correct form here.
    let pool_mon =
        MonitoringApi::builder(pool_monitoring.expect("pool monitoring should be enabled")).build();
    let pool_metrics = pool_mon
        .poll_metric_gte("sv2_client_blocks_found_total", 1.0, METRIC_POLL_TIMEOUT)
        .await;
    assert_metric_present(&pool_metrics, "sv2_uptime_seconds");
    assert_metric_eq(&pool_metrics, "sv2_clients_total", 1.0);

    shutdown_all!(pool, jdc, tproxy);
}

// ---------------------------------------------------------------------------
// 5. Pool JSON API endpoints — static (no miner / no activity).
// Covers: root, /api/v1/server (404), /api/v1/server/channels (404),
//         /api/v1/sv1/clients (404).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pool_api_endpoints_static() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, _pool_addr, pool_monitoring) =
        start_pool(sv2_tp_config(tp_addr), vec![], vec![], true).await;
    let pool_mon =
        MonitoringApi::builder(pool_monitoring.expect("pool monitoring should be enabled")).build();

    // Health is always available.
    check_health(pool_mon).await;

    // Root endpoint lists APIs (typed via the production `RootResponse`).
    let root: RootResponse = pool_mon.fetch_typed(routes::ROOT).await;
    assert_eq!(root.service, "SRI Monitoring API");
    assert!(
        root.endpoints.contains_key(routes::HEALTH),
        "root endpoint listing should include {}, got: {:?}",
        routes::HEALTH,
        root.endpoints,
    );

    // Pool has no upstream and no SV1 — these endpoints should return 404 with a typed
    // `ErrorResponse` body.
    for path in [routes::SERVER, routes::SERVER_CHANNELS, routes::SV1_CLIENTS] {
        let (status, err): (i32, ErrorResponse) = pool_mon.fetch_with_status(path).await;
        assert_eq!(
            status, 404,
            "{} should return 404, got {} with body {:?}",
            path, status, err
        );
        assert!(
            !err.error.is_empty(),
            "{} should return a non-empty error message",
            path,
        );
    }

    pool.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6. Pool JSON API endpoints — with active SV2 miner.
// Covers: /api/v1/global, /api/v1/clients (paginated list), /api/v1/clients/{id},
//         /api/v1/clients/{id}/channels, plus 404s for unknown ids.
// Also cross-validates that the JSON API and Prometheus surface report
// consistent share data for the same (client, channel, user).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn pool_api_endpoints_with_miner() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, pool_monitoring) =
        start_pool(sv2_tp_config(tp_addr), vec![], vec![], true).await;
    let (sniffer, sniffer_addr) = start_sniffer("A", pool_addr, false, vec![], None);
    // Explicit user_id so the per-channel Prometheus user_identity label is meaningful.
    start_mining_device_sv2(
        sniffer_addr,
        None,
        None,
        Some("test-miner".to_string()),
        1,
        None,
        true,
    );

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_STANDARD,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        )
        .await;

    let pool_mon =
        MonitoringApi::builder(pool_monitoring.expect("pool monitoring should be enabled")).build();

    // /api/v1/global — pool sees SV2 clients, no upstream server, no SV1.
    let global: GlobalInfo = pool_mon
        .poll_until(routes::GLOBAL, METRIC_POLL_TIMEOUT, |r: &GlobalInfo| {
            r.sv2_clients.as_ref().is_some_and(|c| c.total_clients >= 1)
        })
        .await;
    assert!(
        global.server.is_none(),
        "Pool /api/v1/global should have null server"
    );
    assert_eq!(global.sv2_clients.as_ref().unwrap().total_clients, 1);
    assert!(
        global.sv1_clients.is_none(),
        "Pool /api/v1/global should have null sv1_clients"
    );

    // /api/v1/clients — paginated list.
    let clients: Sv2ClientsResponse = pool_mon
        .poll_until(
            routes::CLIENTS,
            METRIC_POLL_TIMEOUT,
            |r: &Sv2ClientsResponse| r.total >= 1,
        )
        .await;
    assert_eq!(clients.total, 1);
    assert_eq!(clients.items.len(), 1);
    let client_id = clients.items[0].client_id;
    assert!(client_id > 0);

    // /api/v1/clients/{id} — single-client lookup.
    let client: Sv2ClientResponse = pool_mon.fetch_typed(&routes::client_by_id(client_id)).await;
    assert_eq!(client.client_id, client_id);

    // /api/v1/clients/{id}/channels — at least one channel.
    let channels: Sv2ClientChannelsResponse = pool_mon
        .fetch_typed(&routes::client_channels(client_id))
        .await;
    assert_eq!(channels.client_id, client_id);
    assert!(
        channels.total_standard + channels.total_extended >= 1,
        "client should have ≥1 channel, got std={} ext={}",
        channels.total_standard,
        channels.total_extended,
    );

    // 404 paths for unknown client id.
    for path in [routes::client_by_id(99999), routes::client_channels(99999)] {
        let (status, err): (i32, ErrorResponse) = pool_mon.fetch_with_status(&path).await;
        assert_eq!(status, 404, "unknown {} should return 404", path);
        assert!(
            !err.error.is_empty(),
            "unknown {} should have error body",
            path
        );
    }

    // Cross-surface: Prometheus must report at least the same accepted shares the JSON API saw.
    // Pool reserves channel_id=1 internally and assigns 2 to the first downstream-opened channel.
    let metrics = pool_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_client_shares_accepted_total",
                &[
                    ("client_id", "1"),
                    ("channel_id", "2"),
                    ("user_identity", "test-miner"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_eq(&metrics, "sv2_clients_total", 1.0);

    pool.shutdown().await;
}

// ---------------------------------------------------------------------------
// 7. tProxy JSON API endpoints — static (no miner / no activity).
// Covers: root, /api/v1/clients (404 — tProxy has no SV2 downstreams).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn tproxy_api_endpoints_static() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _pool_monitoring) =
        start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (tproxy, _tproxy_addr, tproxy_monitoring) =
        start_sv2_translator(&[pool_addr], false, vec![], vec![], None, true).await;
    let tproxy_mon =
        MonitoringApi::builder(tproxy_monitoring.expect("tproxy monitoring should be enabled"))
            .build();

    check_health(tproxy_mon).await;

    let root: RootResponse = tproxy_mon.fetch_typed(routes::ROOT).await;
    assert_eq!(root.service, "SRI Monitoring API");
    assert!(root.endpoints.contains_key(routes::HEALTH));

    let (status, err): (i32, ErrorResponse) = tproxy_mon.fetch_with_status(routes::CLIENTS).await;
    assert_eq!(
        status,
        404,
        "{} should return 404 for tProxy",
        routes::CLIENTS
    );
    assert!(!err.error.is_empty());

    shutdown_all!(tproxy, pool);
}

// ---------------------------------------------------------------------------
// 8. tProxy JSON API endpoints — with active SV1 miner.
// Covers: /api/v1/global, /api/v1/server, /api/v1/server/channels,
//         /api/v1/sv1/clients (paginated), /api/v1/sv1/clients/{id} (+ 404).
// Also cross-validates that JSON API and Prometheus expose consistent
// upstream-channel share data.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn tproxy_api_endpoints_with_miner() {
    start_tracing();
    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, _pool_monitoring) =
        start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("0", pool_addr, false, vec![], None);
    let (tproxy, tproxy_addr, tproxy_monitoring) =
        start_sv2_translator(&[sniffer_addr], false, vec![], vec![], None, true).await;
    let (_minerd_process, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    let tproxy_mon =
        MonitoringApi::builder(tproxy_monitoring.expect("tproxy monitoring should be enabled"))
            .build();

    // /api/v1/global — tProxy has upstream server + SV1 clients, no SV2 downstreams.
    let global: GlobalInfo = tproxy_mon
        .poll_until(routes::GLOBAL, METRIC_POLL_TIMEOUT, |r: &GlobalInfo| {
            r.sv1_clients.as_ref().is_some_and(|c| c.total_clients >= 1)
        })
        .await;
    let server_summary = global
        .server
        .as_ref()
        .expect("tProxy /api/v1/global should have server");
    assert_eq!(server_summary.extended_channels, 1);
    assert_eq!(global.sv1_clients.as_ref().unwrap().total_clients, 1);
    assert!(
        global.sv2_clients.is_none(),
        "tProxy should have null sv2_clients"
    );

    // /api/v1/server — extended channel count visible.
    let server: ServerResponse = tproxy_mon
        .poll_until(routes::SERVER, METRIC_POLL_TIMEOUT, |r: &ServerResponse| {
            r.extended_channels_count >= 1
        })
        .await;
    assert_eq!(server.extended_channels_count, 1);

    // /api/v1/server/channels — poll until shares_acknowledged is non-zero.
    // A channel appears in the snapshot before shares are processed by the
    // monitoring cache; we need to wait explicitly for shares to arrive.
    // Successful deserialization also guards the JSON shape of shares_rejected
    // and shares_rejected_by_reason fields.
    let server_channels: ServerChannelsResponse = tproxy_mon
        .poll_until(
            routes::SERVER_CHANNELS,
            METRIC_POLL_TIMEOUT,
            |r: &ServerChannelsResponse| {
                r.extended_channels
                    .first()
                    .is_some_and(|ch| ch.shares_acknowledged >= 1)
            },
        )
        .await;
    let ext = &server_channels.extended_channels[0];
    assert!(
        ext.shares_acknowledged > 0,
        "channel should expose non-zero shares_acknowledged, got: {:?}",
        ext
    );

    // /api/v1/sv1/clients — at least one SV1 client.
    let sv1_clients: Sv1ClientsResponse = tproxy_mon
        .poll_until(
            routes::SV1_CLIENTS,
            METRIC_POLL_TIMEOUT,
            |r: &Sv1ClientsResponse| r.total >= 1,
        )
        .await;
    assert_eq!(sv1_clients.total, 1);
    assert!(!sv1_clients.items.is_empty());
    let sv1_client_id = sv1_clients.items[0].client_id;
    assert!(sv1_client_id > 0);

    // /api/v1/sv1/clients/{id} — single-client lookup.
    let client: Sv1ClientInfo = tproxy_mon
        .fetch_typed(&routes::sv1_client_by_id(sv1_client_id))
        .await;
    assert_eq!(client.client_id, sv1_client_id);

    // 404 for unknown SV1 client id.
    let unknown = routes::sv1_client_by_id(99999);
    let (status, err): (i32, ErrorResponse) = tproxy_mon.fetch_with_status(&unknown).await;
    assert_eq!(status, 404);
    assert!(!err.error.is_empty());

    // Cross-surface: Prometheus must report the same upstream-channel accepted shares.
    let metrics = tproxy_mon
        .poll_metric_gte(
            Metric::with_labels(
                "sv2_server_shares_accepted_total",
                &[
                    ("channel_id", "2"),
                    ("user_identity", "user_identity.miner1"),
                ],
            ),
            1.0,
            METRIC_POLL_TIMEOUT,
        )
        .await;
    assert_metric_eq(&metrics, "sv1_clients_total", 1.0);

    shutdown_all!(tproxy, pool);
}

// ---------------------------------------------------------------------------
// 9. JDC JSON API endpoints — with active SV1 miner.
// JDC sits between pool and tProxy: it is an SV2 client to the pool (so
// `server` is populated) and an SV2 server to tProxy (so `sv2_clients` is
// populated). JDC has no SV1 surface.
// Covers: root, /api/v1/global, /api/v1/server, /api/v1/server/channels,
//         /api/v1/clients, /api/v1/clients/{id}, /api/v1/clients/{id}/channels,
//         /api/v1/sv1/clients (404).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn jdc_api_endpoints_with_miner() {
    start_tracing();
    let (tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (pool, pool_addr, jds_addr, _pool_monitoring) =
        start_pool_with_jds(tp.bitcoin_core(), vec![], vec![], false).await;
    let (jdc_pool_sniffer, jdc_pool_sniffer_addr) =
        start_sniffer("0", pool_addr, false, vec![], None);
    let (jdc, jdc_addr, jdc_monitoring) = start_jdc(
        &[(jdc_pool_sniffer_addr, jds_addr)],
        sv2_tp_config(tp_addr),
        vec![],
        vec![],
        true,
        None,
    );
    let (tproxy, tproxy_addr, _) =
        start_sv2_translator(&[jdc_addr], true, vec![], vec![], None, false).await;
    let (_minerd, _minerd_addr) = start_minerd(tproxy_addr, None, None, false).await;

    // Wait until at least one share has flowed all the way to the pool.
    jdc_pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    let jdc_mon =
        MonitoringApi::builder(jdc_monitoring.expect("jdc monitoring should be enabled")).build();
    check_health(jdc_mon).await;

    // Root endpoint lists APIs (typed via the production `RootResponse`).
    let root: RootResponse = jdc_mon.fetch_typed(routes::ROOT).await;
    assert_eq!(root.service, "SRI Monitoring API");
    assert!(root.endpoints.contains_key(routes::HEALTH));

    // /api/v1/global — JDC has both server (pool) and sv2_clients (tproxy), no sv1.
    let global: GlobalInfo = jdc_mon
        .poll_until(routes::GLOBAL, METRIC_POLL_TIMEOUT, |r: &GlobalInfo| {
            r.sv2_clients.as_ref().is_some_and(|c| c.total_clients >= 1)
        })
        .await;
    let server_summary = global
        .server
        .as_ref()
        .expect("JDC /api/v1/global should have server");
    assert_eq!(server_summary.extended_channels, 1);
    assert_eq!(global.sv2_clients.as_ref().unwrap().total_clients, 1);
    assert!(
        global.sv1_clients.is_none(),
        "JDC should have null sv1_clients"
    );

    // /api/v1/server — extended channel from upstream pool.
    let server: ServerResponse = jdc_mon
        .poll_until(routes::SERVER, METRIC_POLL_TIMEOUT, |r: &ServerResponse| {
            r.extended_channels_count >= 1
        })
        .await;
    assert_eq!(server.extended_channels_count, 1);

    // /api/v1/server/channels — poll until shares_acknowledged is non-zero.
    // A channel can appear in the snapshot before any shares are processed,
    // so the predicate waits for both. Successful deserialization also guards
    // the JSON shape of shares_rejected and shares_rejected_by_reason on
    // ServerExtendedChannelInfo.
    let server_channels: ServerChannelsResponse = jdc_mon
        .poll_until(
            routes::SERVER_CHANNELS,
            METRIC_POLL_TIMEOUT,
            |r: &ServerChannelsResponse| {
                r.extended_channels
                    .first()
                    .is_some_and(|ch| ch.shares_acknowledged >= 1)
            },
        )
        .await;
    assert!(!server_channels.extended_channels.is_empty());
    let ext = &server_channels.extended_channels[0];
    assert!(
        ext.shares_acknowledged > 0,
        "channel should expose non-zero shares_acknowledged, got: {:?}",
        ext
    );

    // /api/v1/clients — JDC's downstream is the tProxy (single SV2 client).
    let clients: Sv2ClientsResponse = jdc_mon
        .poll_until(
            routes::CLIENTS,
            METRIC_POLL_TIMEOUT,
            |r: &Sv2ClientsResponse| r.total >= 1,
        )
        .await;
    assert_eq!(clients.total, 1);
    assert_eq!(clients.items.len(), 1);
    let client_id = clients.items[0].client_id;

    // /api/v1/clients/{id} — single client lookup.
    let client: Sv2ClientResponse = jdc_mon.fetch_typed(&routes::client_by_id(client_id)).await;
    assert_eq!(client.client_id, client_id);

    // /api/v1/clients/{id}/channels — tProxy opens an extended channel via JDC.
    let channels: Sv2ClientChannelsResponse = jdc_mon
        .fetch_typed(&routes::client_channels(client_id))
        .await;
    assert_eq!(channels.client_id, client_id);
    assert!(
        channels.total_extended >= 1,
        "tproxy should open an extended channel via JDC, got total_extended={}",
        channels.total_extended,
    );

    // /api/v1/sv1/clients — JDC has no SV1 surface.
    let (status, err): (i32, ErrorResponse) = jdc_mon.fetch_with_status(routes::SV1_CLIENTS).await;
    assert_eq!(status, 404, "JDC {} should return 404", routes::SV1_CLIENTS);
    assert!(!err.error.is_empty());

    shutdown_all!(tproxy, jdc, pool);
}
