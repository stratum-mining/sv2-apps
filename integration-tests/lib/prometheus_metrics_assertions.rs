//! Helpers for querying and asserting on Prometheus metrics and JSON API endpoints
//! exposed by SV2 components during integration tests.

use std::net::SocketAddr;

/// Fetch the raw Prometheus text-format metrics from a component's `/metrics` endpoint.
/// Uses `spawn_blocking` to avoid blocking the tokio runtime with synchronous HTTP calls.
pub async fn fetch_metrics(monitoring_addr: SocketAddr) -> String {
    let url = format!("http://{}/metrics", monitoring_addr);
    tokio::task::spawn_blocking(move || {
        let bytes = crate::utils::http::make_get_request(&url, 5);
        String::from_utf8(bytes).expect("metrics response should be valid UTF-8")
    })
    .await
    .expect("spawn_blocking for fetch_metrics panicked")
}

/// Fetch the JSON body from a component's API endpoint (e.g. `/api/v1/health`).
/// Uses `spawn_blocking` to avoid blocking the tokio runtime with synchronous HTTP calls.
pub async fn fetch_api(monitoring_addr: SocketAddr, path: &str) -> String {
    let url = format!("http://{}{}", monitoring_addr, path);
    tokio::task::spawn_blocking(move || {
        let bytes = crate::utils::http::make_get_request(&url, 5);
        String::from_utf8(bytes).expect("api response should be valid UTF-8")
    })
    .await
    .expect("spawn_blocking for fetch_api panicked")
}

/// Parse a specific metric value from Prometheus text format.
/// Returns `None` if the metric line is not found.
///
/// For simple gauges/counters (no labels), pass `metric_name` like `"sv2_clients_total"`.
/// For labeled metrics, pass the full label selector like
/// `"sv2_server_channels{channel_type=\"extended\"}"`.
pub(crate) fn parse_metric_value(metrics_text: &str, metric_name: &str) -> Option<f64> {
    for line in metrics_text.lines() {
        if line.starts_with('#') {
            continue;
        }
        // For labeled metrics, match the prefix up to the closing brace
        if let Some(rest) = line.strip_prefix(metric_name) {
            // The value follows a space after the metric name (or after the closing brace)
            let value_str = rest.trim();
            // If there are labels and the name didn't include them, skip
            if value_str.starts_with('{') {
                continue;
            }
            return value_str.parse::<f64>().ok();
        }
    }
    None
}

/// Assert that a metric is present and its value satisfies the given predicate.
pub(crate) fn assert_metric<F: Fn(f64) -> bool>(
    metrics_text: &str,
    metric_name: &str,
    predicate: F,
    description: &str,
) {
    let value = parse_metric_value(metrics_text, metric_name);
    match value {
        Some(v) => {
            assert!(
                predicate(v),
                "Metric '{}' has value {} but expected: {}",
                metric_name,
                v,
                description
            );
        }
        None => {
            panic!(
                "Metric '{}' not found in metrics output. Expected: {}",
                metric_name, description
            );
        }
    }
}

/// Assert that a metric is present with a value >= the given minimum.
pub fn assert_metric_gte(metrics_text: &str, metric_name: &str, min: f64) {
    assert_metric(
        metrics_text,
        metric_name,
        |v| v >= min,
        &format!(">= {}", min),
    );
}

/// Assert that a metric is present with the exact given value.
pub fn assert_metric_eq(metrics_text: &str, metric_name: &str, expected: f64) {
    assert_metric(
        metrics_text,
        metric_name,
        |v| (v - expected).abs() < f64::EPSILON,
        &format!("== {}", expected),
    );
}

/// Assert that a metric name does NOT appear in the metrics output at all.
pub fn assert_metric_not_present(metrics_text: &str, metric_name: &str) {
    for line in metrics_text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(metric_name) {
            // Make sure it's an exact match (not a prefix of another metric name)
            if rest.starts_with(' ') || rest.starts_with('{') {
                panic!(
                    "Metric '{}' was found in metrics output but was expected to be absent. Line: {}",
                    metric_name, line
                );
            }
        }
    }
}

/// Assert that a metric name appears at least once in the metrics output (with any label/value).
pub fn assert_metric_present(metrics_text: &str, metric_name: &str) {
    for line in metrics_text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(metric_name) {
            if rest.starts_with(' ') || rest.starts_with('{') {
                return;
            }
        }
    }
    panic!(
        "Metric '{}' was expected to be present but was not found in metrics output",
        metric_name
    );
}

/// Poll the `/metrics` endpoint until any line matching `metric_name` (with any labels) has a
/// value >= `min`, then return the full metrics text. Panics if the condition is not met within
/// `timeout`.
///
/// Use this instead of a fixed `sleep` for `GaugeVec` metrics (per-channel shares, blocks found)
/// that only appear in Prometheus output after the monitoring snapshot cache has refreshed with
/// observed label combinations. The handler calls `.reset()` on every `/metrics` request before
/// repopulating from the cached snapshot, so a label combination is only present when the
/// snapshot contains a non-default value for it.
pub async fn poll_until_metric_gte(
    monitoring_addr: SocketAddr,
    metric_name: &str,
    min: f64,
    timeout: std::time::Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let metrics = fetch_metrics(monitoring_addr).await;
        let satisfied = metrics.lines().any(|line| {
            if line.starts_with('#') {
                return false;
            }
            if let Some(rest) = line.strip_prefix(metric_name) {
                // Match bare name followed by space, or labeled name followed by '{'
                let value_str = if rest.starts_with(' ') {
                    rest.trim()
                } else if rest.starts_with('{') {
                    // Skip past the closing brace to get the value
                    rest.find('}')
                        .and_then(|i| rest.get(i + 1..))
                        .map(|s| s.trim())
                        .unwrap_or("")
                } else {
                    return false;
                };
                value_str.parse::<f64>().map(|v| v >= min).unwrap_or(false)
            } else {
                false
            }
        });
        if satisfied {
            return metrics;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "Metric '{}' never reached >= {} within {:?}. Last /metrics response:\n{}",
                metric_name, min, timeout, metrics
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Assert that the `/api/v1/health` endpoint returns a response containing `"status":"ok"`.
pub async fn assert_api_health(monitoring_addr: SocketAddr) {
    let body = fetch_api(monitoring_addr, "/api/v1/health").await;
    assert!(
        body.contains("\"status\":\"ok\""),
        "Health endpoint should return ok status, got: {}",
        body
    );
}

/// Assert that the uptime metric is present and positive.
pub fn assert_uptime(metrics_text: &str) {
    assert_metric(
        metrics_text,
        "sv2_uptime_seconds",
        |v| v >= 0.0,
        ">= 0.0 (uptime should be non-negative)",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_METRICS: &str = r#"# HELP sv2_uptime_seconds Server uptime in seconds
# TYPE sv2_uptime_seconds gauge
sv2_uptime_seconds 42
# HELP sv2_clients_total Total number of connected clients
# TYPE sv2_clients_total gauge
sv2_clients_total 3
# HELP sv2_server_channels Number of server channels by type
# TYPE sv2_server_channels gauge
sv2_server_channels{channel_type="extended"} 1
sv2_server_channels{channel_type="standard"} 0
# HELP sv2_client_shares_accepted_total Per-channel accepted shares
# TYPE sv2_client_shares_accepted_total gauge
sv2_client_shares_accepted_total{channel_id="1",user_identity="user1"} 5
"#;

    #[test]
    fn test_parse_simple_metric() {
        assert_eq!(
            parse_metric_value(SAMPLE_METRICS, "sv2_uptime_seconds"),
            Some(42.0)
        );
        assert_eq!(
            parse_metric_value(SAMPLE_METRICS, "sv2_clients_total"),
            Some(3.0)
        );
    }

    #[test]
    fn test_parse_labeled_metric() {
        assert_eq!(
            parse_metric_value(
                SAMPLE_METRICS,
                "sv2_server_channels{channel_type=\"extended\"}"
            ),
            Some(1.0)
        );
        assert_eq!(
            parse_metric_value(
                SAMPLE_METRICS,
                "sv2_server_channels{channel_type=\"standard\"}"
            ),
            Some(0.0)
        );
    }

    #[test]
    fn test_parse_missing_metric() {
        assert_eq!(
            parse_metric_value(SAMPLE_METRICS, "nonexistent_metric"),
            None
        );
    }

    #[test]
    fn test_assert_metric_gte() {
        assert_metric_gte(SAMPLE_METRICS, "sv2_clients_total", 1.0);
        assert_metric_gte(SAMPLE_METRICS, "sv2_clients_total", 3.0);
    }

    #[test]
    fn test_assert_metric_eq() {
        assert_metric_eq(SAMPLE_METRICS, "sv2_uptime_seconds", 42.0);
    }

    #[test]
    fn test_assert_metric_not_present() {
        assert_metric_not_present(SAMPLE_METRICS, "nonexistent_metric");
    }

    #[test]
    #[should_panic(expected = "was found in metrics output")]
    fn test_assert_metric_not_present_panics() {
        assert_metric_not_present(SAMPLE_METRICS, "sv2_clients_total");
    }

    #[test]
    fn test_assert_metric_present() {
        assert_metric_present(SAMPLE_METRICS, "sv2_clients_total");
        assert_metric_present(SAMPLE_METRICS, "sv2_server_channels");
    }

    #[test]
    #[should_panic(expected = "was expected to be present")]
    fn test_assert_metric_present_panics() {
        assert_metric_present(SAMPLE_METRICS, "nonexistent_metric");
    }

    #[test]
    fn test_assert_uptime() {
        assert_uptime(SAMPLE_METRICS);
    }

    #[test]
    fn test_no_false_prefix_match() {
        // sv2_clients_total should not match sv2_clients_total_extra
        let metrics = "sv2_clients_total_extra 99\n";
        assert_metric_not_present(metrics, "sv2_clients_total");
    }
}
