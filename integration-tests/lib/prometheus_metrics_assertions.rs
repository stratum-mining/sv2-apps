//! Helpers for querying and asserting on Prometheus metrics and JSON API endpoints
//! exposed by SV2 components during integration tests.
//!
//! All network-touching helpers (`fetch_*`, `poll_*`) live on [`MonitoringApi`],
//! a thin typed wrapper around the `SocketAddr` of a component's monitoring
//! server. Pure parsing / assertion helpers on raw metrics text remain as free
//! functions below.

use std::{collections::HashMap, fmt, net::SocketAddr, time::Duration};
use stratum_apps::monitoring::routes;

// `minreq` is a synchronous HTTP client with no async variant, so all fetch
// helpers wrap it in `spawn_blocking` to avoid stalling the tokio runtime.

/// Typed client for the monitoring HTTP API of a single SV2 component.
///
/// Wraps a `SocketAddr` plus the request policy (retry count, per-request
/// timeout) used for every fetch. Construct via [`MonitoringApi::builder`].
#[derive(Debug, Clone, Copy)]
pub struct MonitoringApi {
    addr: SocketAddr,
    retries: usize,
    request_timeout: Duration,
}

/// Default per-request HTTP timeout. Bounds individual attempts so a hung
/// monitoring server cannot stall a test run all the way to its outer
/// deadline; the retry-on-5xx-or-connection-error loop then takes over.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Default retry count for each request.
const DEFAULT_RETRIES: usize = 5;

/// Builder for [`MonitoringApi`]. Created via [`MonitoringApi::builder`].
///
/// Unset knobs fall back to the module-level defaults
/// (`DEFAULT_RETRIES`, `DEFAULT_REQUEST_TIMEOUT`).
#[derive(Debug, Clone, Copy)]
pub struct MonitoringApiBuilder {
    addr: SocketAddr,
    retries: usize,
    request_timeout: Duration,
}

impl MonitoringApiBuilder {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            retries: DEFAULT_RETRIES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Override the retry count. Use `1` in negative-path tests to fail fast.
    pub fn retries(mut self, retries: usize) -> Self {
        self.retries = retries;
        self
    }

    /// Override the per-request timeout applied to each individual attempt.
    pub fn request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Consume the builder and produce an immutable [`MonitoringApi`].
    pub fn build(self) -> MonitoringApi {
        MonitoringApi {
            addr: self.addr,
            retries: self.retries,
            request_timeout: self.request_timeout,
        }
    }
}

impl MonitoringApi {
    /// Start a builder for a client at `addr`. Knobs default to
    /// `DEFAULT_RETRIES` retries and `DEFAULT_REQUEST_TIMEOUT` per request;
    /// see [`MonitoringApiBuilder`] for overrides.
    pub fn builder(addr: SocketAddr) -> MonitoringApiBuilder {
        MonitoringApiBuilder::new(addr)
    }

    /// Underlying socket address (useful for diagnostics or constructing raw URLs).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    // ── Internal HTTP helper ───────────────────────────────────────

    /// Issue a GET against `path` and return the HTTP status + raw bytes.
    /// Centralises URL construction, `spawn_blocking`, the per-request
    /// timeout, and the retry-on-5xx loop for every public fetcher/poller.
    async fn http_get_with_status(&self, path: &str) -> (i32, Vec<u8>) {
        let url = format!("http://{}{}", self.addr, path);
        let retries = self.retries;
        let timeout = self.request_timeout;
        tokio::task::spawn_blocking(move || {
            crate::utils::http::make_get_request_with_status(&url, retries, Some(timeout))
        })
        .await
        .expect("spawn_blocking for http_get_with_status panicked")
    }

    // ── Raw fetches ────────────────────────────────────────────────

    /// Fetch the raw Prometheus text-format metrics from `/metrics`.
    /// Panics on any non-2xx status — `/metrics` should always succeed.
    pub async fn fetch_metrics(&self) -> String {
        let (status, bytes) = self.http_get_with_status(routes::METRICS).await;
        assert!(
            (200..300).contains(&status),
            "GET {} returned non-2xx status {status}",
            routes::METRICS
        );
        String::from_utf8(bytes).expect("metrics response should be valid UTF-8")
    }

    /// Fetch the JSON body from an API endpoint (e.g. `/api/v1/health`).
    /// Panics on any non-2xx status — use [`Self::fetch_with_status`] for error endpoints.
    pub async fn fetch(&self, path: &str) -> String {
        let (status, bytes) = self.http_get_with_status(path).await;
        assert!(
            (200..300).contains(&status),
            "GET {path} returned non-2xx status {status}"
        );
        String::from_utf8(bytes).expect("api response should be valid UTF-8")
    }

    /// Fetch a JSON API endpoint and parse the response into a typed struct.
    pub async fn fetch_typed<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let body = self.fetch(path).await;
        serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!(
                "Failed to parse JSON from {} into {}: {}\nBody: {}",
                path,
                std::any::type_name::<T>(),
                e,
                body
            )
        })
    }

    /// Fetch a JSON API endpoint returning both the HTTP status code and the parsed body
    /// deserialized into the caller-specified type `T`.
    ///
    /// Unlike `fetch_typed`, this does **not** panic on non-2xx responses, so it can be
    /// used to test error endpoints. For 404/error bodies, parametrize `T` with the
    /// production `ErrorResponse` type to keep the assertion fully typed.
    pub async fn fetch_with_status<T: serde::de::DeserializeOwned>(&self, path: &str) -> (i32, T) {
        let (status, bytes) = self.http_get_with_status(path).await;
        let body = String::from_utf8(bytes).expect("api response should be valid UTF-8");
        let value: T = serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!(
                "Failed to parse JSON from {} (status {}) into {}: {}\nBody: {}",
                path,
                status,
                std::any::type_name::<T>(),
                e,
                body
            )
        });
        (status, value)
    }

    // ── Polling helpers ────────────────────────────────────────────

    /// Poll `path` until the response deserialises into `T` and `predicate` returns true.
    ///
    /// Retries every [`crate::utils::POLL_INTERVAL`] until `timeout`. Non-2xx responses and
    /// deserialisation failures are tolerated (the endpoint may not be ready
    /// yet — for example a `/api/v1/clients/{id}` route returns 404 until the
    /// snapshot cache first populates). On timeout, the panic message includes
    /// the path, target type, last status, and last body so CI failures are
    /// debuggable without re-running with extra logging.
    ///
    /// `path` is a plain `&str` so callers can poll dynamic routes such as
    /// `/api/v1/clients/{id}/channels`, whose state settles after the client itself appears.
    pub async fn poll_until<T, F>(&self, path: &str, timeout: Duration, predicate: F) -> T
    where
        T: serde::de::DeserializeOwned,
        F: Fn(&T) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        // Accumulators read only on the timeout path so failures are debuggable.
        #[allow(unused_assignments)]
        let mut last_status: i32 = 0;
        #[allow(unused_assignments)]
        let mut last_body = String::new();
        loop {
            let (status, body) = self.http_get_with_status(path).await;
            last_status = status;
            last_body = String::from_utf8_lossy(&body).into_owned();
            if (200..300).contains(&status) {
                if let Ok(resp) = serde_json::from_str::<T>(&last_body) {
                    if predicate(&resp) {
                        return resp;
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "poll_until: predicate on {} (-> {}) never satisfied within {:?}. \
                     Last status: {}. Last body:\n{}",
                    path,
                    std::any::type_name::<T>(),
                    timeout,
                    last_status,
                    last_body,
                );
            }
            tokio::time::sleep(crate::utils::POLL_INTERVAL).await;
        }
    }

    /// Poll `/metrics` until a line matching `metric` has value >= `min`, or panic
    /// after `timeout`. Polls every 100ms to react quickly while tolerating cache
    /// refresh jitter.
    ///
    /// Returns the full metrics text from the successful scrape so callers can make
    /// additional assertions without a second fetch.
    pub async fn poll_metric_gte<'a, M: Into<Metric<'a>>>(
        &self,
        metric: M,
        min: f64,
        timeout: Duration,
    ) -> String {
        let metric = metric.into();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let metrics = self.fetch_metrics().await;
            if let Some(v) = parse_metric_value(&metrics, metric) {
                if v >= min {
                    return metrics;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "Metric '{metric}' never reached >= {min} within {timeout:?}. Last /metrics response:\n{metrics}"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// A Prometheus metric selector: a metric name plus an optional set of label matchers.
///
/// Label matching is order-independent — the selector matches any exposition line
/// whose label set is a superset of the requested labels. A selector with no labels
/// matches any line for that metric (bare or labeled).
///
/// # Examples
///
/// ```
/// # use integration_tests_sv2::prometheus_metrics_assertions::Metric;
/// // Bare name (implicit via From<&str>):
/// let _: Metric = "sv2_clients_total".into();
///
/// // Specific labeled series:
/// let _ = Metric::with_labels(
///     "sv2_server_shares_accepted_total",
///     &[("channel_id", "1"), ("user_identity", "user1")],
/// );
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Metric<'a> {
    pub name: &'a str,
    pub labels: &'a [(&'a str, &'a str)],
}

impl<'a> Metric<'a> {
    /// Create a selector for a metric by bare name (matches any labels).
    pub const fn new(name: &'a str) -> Self {
        Self { name, labels: &[] }
    }

    /// Create a selector with specific label matchers. Matches lines whose label
    /// set is a superset of `labels`, regardless of label ordering.
    pub const fn with_labels(name: &'a str, labels: &'a [(&'a str, &'a str)]) -> Self {
        Self { name, labels }
    }

    /// Try to match a single Prometheus exposition line. Returns the parsed value
    /// if the line matches this selector, otherwise `None`.
    fn match_line(&self, line: &str) -> Option<f64> {
        let rest = line.strip_prefix(self.name)?;
        // The name must be a complete token: next char is whitespace, '{', or EOL.
        // This prevents e.g. `sv2_clients_total_extra` from matching `sv2_clients_total`.
        let is_labeled = rest.starts_with('{');
        let is_bare = rest.chars().next().is_none_or(|c| c.is_ascii_whitespace());
        if !is_labeled && !is_bare {
            return None;
        }

        // Parse the labels (if any) and the value portion.
        let (line_labels, value_part) = if is_labeled {
            let inner = rest.strip_prefix('{')?;
            let (block, after) = inner.split_once('}')?;
            (parse_label_block(block), after)
        } else {
            (HashMap::new(), rest)
        };

        // Selector labels must all appear on the line with matching values.
        for (k, v) in self.labels {
            if line_labels.get(*k).map(String::as_str) != Some(*v) {
                return None;
            }
        }

        value_part.split_whitespace().next()?.parse().ok()
    }
}

impl<'a> From<&'a str> for Metric<'a> {
    fn from(name: &'a str) -> Self {
        Metric::new(name)
    }
}

impl fmt::Display for Metric<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)?;
        if !self.labels.is_empty() {
            f.write_str("{")?;
            for (i, (k, v)) in self.labels.iter().enumerate() {
                if i > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{k}=\"{v}\"")?;
            }
            f.write_str("}")?;
        }
        Ok(())
    }
}

/// Parse the inside of a Prometheus label block like `k1="v1",k2="v2"` into a map.
/// Supports the subset emitted by the `prometheus` crate: simple `k="v"` pairs with
/// no escape sequences in values (sufficient for our metrics).
fn parse_label_block(block: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let block = block.trim();
    if block.is_empty() {
        return out;
    }
    for pair in block.split(',') {
        let pair = pair.trim();
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_start_matches('"').trim_end_matches('"');
        out.insert(k.trim().to_string(), v.to_string());
    }
    out
}

/// Parse a specific metric value from Prometheus text format.
/// Returns `None` if no line matches the selector.
pub fn parse_metric_value<'a, M: Into<Metric<'a>>>(metrics_text: &str, metric: M) -> Option<f64> {
    let metric = metric.into();
    for line in metrics_text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(v) = metric.match_line(line) {
            return Some(v);
        }
    }
    None
}

/// Assert that a metric is present and its value satisfies the given predicate.
pub(crate) fn assert_metric<'a, M, F>(
    metrics_text: &str,
    metric: M,
    predicate: F,
    description: &str,
) where
    M: Into<Metric<'a>>,
    F: Fn(f64) -> bool,
{
    let metric = metric.into();
    match parse_metric_value(metrics_text, metric) {
        Some(v) => {
            assert!(
                predicate(v),
                "Metric '{metric}' has value {v} but expected: {description}"
            );
        }
        None => {
            panic!("Metric '{metric}' not found in metrics output. Expected: {description}");
        }
    }
}

/// Assert that a metric is present with the exact given value.
pub fn assert_metric_eq<'a, M: Into<Metric<'a>>>(metrics_text: &str, metric: M, expected: f64) {
    assert_metric(
        metrics_text,
        metric,
        |v| (v - expected).abs() < f64::EPSILON,
        &format!("== {expected}"),
    );
}

/// Assert that no exposition line matches the selector.
///
/// For a bare-name selector (`Metric::new("name")` or `"name".into()`), this means
/// the metric name does not appear at all. For a labeled selector, it means no line
/// with matching labels exists — other series for the same metric name are allowed.
pub fn assert_metric_not_present<'a, M: Into<Metric<'a>>>(metrics_text: &str, metric: M) {
    let metric = metric.into();
    for line in metrics_text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if metric.match_line(line).is_some() {
            panic!(
                "Metric '{metric}' was found in metrics output but was expected to be absent. Line: {line}"
            );
        }
    }
}

/// Assert that at least one exposition line matches the selector.
pub fn assert_metric_present<'a, M: Into<Metric<'a>>>(metrics_text: &str, metric: M) {
    let metric = metric.into();
    for line in metrics_text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if metric.match_line(line).is_some() {
            return;
        }
    }
    panic!("Metric '{metric}' was expected to be present but was not found in metrics output");
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
                Metric::with_labels("sv2_server_channels", &[("channel_type", "extended")])
            ),
            Some(1.0)
        );
        assert_eq!(
            parse_metric_value(
                SAMPLE_METRICS,
                Metric::with_labels("sv2_server_channels", &[("channel_type", "standard")])
            ),
            Some(0.0)
        );
    }

    #[test]
    fn test_label_order_independence() {
        // Selector requesting labels in opposite order to the exposition line
        // must still match — the prometheus crate emits in BTreeMap order today,
        // but tests should not silently break if that ever changes.
        assert_eq!(
            parse_metric_value(
                SAMPLE_METRICS,
                Metric::with_labels(
                    "sv2_client_shares_accepted_total",
                    &[("user_identity", "user1"), ("channel_id", "1")],
                )
            ),
            Some(5.0)
        );
    }

    #[test]
    fn test_label_subset_match() {
        // Querying only a subset of labels still matches.
        assert_eq!(
            parse_metric_value(
                SAMPLE_METRICS,
                Metric::with_labels("sv2_client_shares_accepted_total", &[("channel_id", "1")])
            ),
            Some(5.0)
        );
    }

    #[test]
    fn test_label_mismatch_returns_none() {
        assert_eq!(
            parse_metric_value(
                SAMPLE_METRICS,
                Metric::with_labels("sv2_server_channels", &[("channel_type", "nonexistent")])
            ),
            None
        );
    }

    #[test]
    fn test_bare_selector_matches_labeled_line() {
        // A bare-name selector matches any series for that metric (returns the
        // first one found).
        assert_eq!(
            parse_metric_value(SAMPLE_METRICS, "sv2_server_channels"),
            Some(1.0)
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
    fn test_no_false_prefix_match() {
        // sv2_clients_total should not match sv2_clients_total_extra
        let metrics = "sv2_clients_total_extra 99\n";
        assert_metric_not_present(metrics, "sv2_clients_total");
    }
}
