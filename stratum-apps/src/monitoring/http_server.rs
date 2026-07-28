//! HTTP server for exposing monitoring data using Axum

#[cfg(feature = "asic-rs-telemetry")]
use super::miner_telemetry::{MinerTelemetry, MinerTelemetryStatus};
use super::{
    GlobalInfo,
    client::{
        ExtendedChannelInfo, StandardChannelInfo, Sv2ClientInfo, Sv2ClientKind, Sv2ClientMetadata,
        Sv2ClientsMonitoring, Sv2ClientsSummary,
    },
    prometheus_metrics::PrometheusMetrics,
    routes,
    server::{
        ServerExtendedChannelInfo, ServerMonitoring, ServerStandardChannelInfo, ServerSummary,
    },
    snapshot_cache::SnapshotCache,
    sv1::{Sv1ClientInfo, Sv1ClientsMonitoring, Sv1ClientsSummary},
};
use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::get,
};
use prometheus::{Encoder, TextEncoder};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;
use tracing::info;
use utoipa::{IntoParams, OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

#[derive(OpenApi)]
#[cfg_attr(
    feature = "asic-rs-telemetry",
    openapi(components(schemas(MinerTelemetry, MinerTelemetryStatus)))
)]
#[openapi(
    info(
        // This `info` block is the single source of truth for the API title
        // and version. `handle_root` reads them back from `ApiDoc::openapi()`
        // at runtime instead of duplicating the literals.
        title = "SRI Monitoring API",
        version = "0.1.0",
        description = "HTTP JSON API for monitoring SV2 applications"
    ),
    paths(
        handle_health,
        handle_global,
        handle_server,
        handle_server_channels,
        handle_clients,
        handle_client_by_id,
        handle_client_channels,
        handle_sv1_clients,
        handle_sv1_client_by_id,
    ),
    components(schemas(
        GlobalInfo,
        ServerSummary,
        Sv2ClientsSummary,
        ServerExtendedChannelInfo,
        ServerStandardChannelInfo,
        Sv2ClientKind,
        Sv2ClientInfo,
        Sv2ClientMetadata,
        ExtendedChannelInfo,
        StandardChannelInfo,
        Sv1ClientInfo,
        Sv1ClientsSummary,
        HealthResponse,
        ErrorResponse,
        ServerResponse,
        ServerChannelsResponse,
        Sv2ClientsResponse,
        Sv2ClientResponse,
        Sv2ClientChannelsResponse,
        Sv1ClientsResponse,
    )),
    tags(
        (name = "health", description = "Health check endpoints"),
        (name = "global", description = "Global statistics"),
        (name = "server", description = "Server (upstream) monitoring"),
        (name = "clients", description = "Clients (downstream) monitoring"),
        (name = "sv1", description = "Sv1 clients monitoring (Translator Proxy only)")
    )
)]
pub struct ApiDoc;

/// Shared state for all HTTP handlers
#[derive(Clone)]
struct ServerState {
    cache: Arc<SnapshotCache>,
    start_time: u64,
    metrics: PrometheusMetrics,
}

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;

#[derive(Deserialize, IntoParams)]
struct Pagination {
    /// Offset for pagination (default: 0)
    #[serde(default)]
    offset: usize,
    /// Limit for pagination (default: 25, max: 100)
    #[serde(default)]
    limit: Option<usize>,
}

impl Pagination {
    fn effective_limit(&self) -> usize {
        self.limit
            .map(|l| l.min(MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT)
    }
}

fn paginate<T: Clone>(items: &[T], params: &Pagination) -> (usize, Vec<T>) {
    let total = items.len();
    let limit = params.effective_limit();
    let offset = params.offset.min(total);
    let sliced = items
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    (total, sliced)
}

/// HTTP server that exposes monitoring data as JSON
pub struct MonitoringServer {
    bind_address: SocketAddr,
    state: ServerState,
    refresh_interval: Duration,
}

impl MonitoringServer {
    /// Create a new monitoring server with automatic cache refresh.
    ///
    /// This constructor creates a snapshot cache that decouples monitoring API
    /// requests from business logic locks, eliminating the DoS vulnerability where
    /// rapid API requests could cause lock contention with share validation and
    /// job distribution.
    ///
    /// The cache is automatically refreshed in the background at the specified interval.
    ///
    /// # Arguments
    ///
    /// * `bind_address` - Address to bind the HTTP server to
    /// * `server_monitoring` - Optional server (upstream) monitoring trait object
    /// * `sv2_clients_monitoring` - Optional Sv2 clients (downstream) monitoring trait object
    /// * `refresh_interval` - How often to refresh the cache (e.g., Duration::from_secs(15))
    pub fn new(
        bind_address: SocketAddr,
        server_monitoring: Option<Arc<dyn ServerMonitoring + Send + Sync + 'static>>,
        sv2_clients_monitoring: Option<Arc<dyn Sv2ClientsMonitoring + Send + Sync + 'static>>,
        refresh_interval: Duration,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let has_server = server_monitoring.is_some();
        let has_sv2_clients = sv2_clients_monitoring.is_some();

        let metrics = PrometheusMetrics::new(has_server, has_sv2_clients, false)?;

        // Create the snapshot cache with metrics attached so refresh()
        // updates Prometheus gauges atomically alongside the snapshot data.
        let cache = Arc::new(
            SnapshotCache::new(refresh_interval, server_monitoring, sv2_clients_monitoring)
                .with_metrics(metrics.clone()),
        );

        // Do initial refresh (populates both snapshot and Prometheus gauges)
        cache.refresh();

        Ok(Self {
            bind_address,
            refresh_interval,
            state: ServerState {
                cache,
                start_time,
                metrics,
            },
        })
    }

    /// Add Sv1 clients monitoring (optional, for Translator Proxy only)
    ///
    /// This must be called before `run()` if you want SV1 monitoring.
    pub fn with_sv1_monitoring(
        mut self,
        sv1_monitoring: Arc<dyn Sv1ClientsMonitoring + Send + Sync + 'static>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Determine what sources the cache already has
        let snapshot = self.state.cache.get_snapshot();
        let has_server = snapshot.server_info.is_some();
        let has_sv2_clients = snapshot.sv2_clients_summary.is_some();

        // Create metrics with SV1 monitoring enabled
        let metrics = PrometheusMetrics::new(has_server, has_sv2_clients, true)?;

        // Add Sv1 clients source and attach new metrics to the cache
        let cache = Arc::new(
            Arc::try_unwrap(self.state.cache)
                .unwrap_or_else(|arc| (*arc).clone())
                .with_sv1_clients_source(sv1_monitoring)
                .with_metrics(metrics.clone()),
        );

        // Refresh cache with new SV1 data (also updates Prometheus gauges)
        cache.refresh();

        self.state.metrics = metrics;
        self.state.cache = cache;

        Ok(self)
    }

    /// Run the monitoring server until the shutdown signal completes
    ///
    /// Starts an HTTP server that exposes monitoring data as JSON.
    /// Also starts a background task that refreshes the snapshot cache periodically.
    /// Both tasks shut down gracefully when `shutdown_signal` completes.
    ///
    /// Automatically exposes:
    /// - Swagger UI at `/swagger-ui`
    /// - OpenAPI spec at `/api-docs/openapi.json`
    /// - Prometheus metrics at `/metrics`
    pub async fn run(
        self,
        shutdown_signal: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Starting monitoring server on http://{}", self.bind_address);
        info!("Cache refresh interval: {:?}", self.refresh_interval);

        let listener = TcpListener::bind(self.bind_address).await?;

        // Spawn background task to refresh cache periodically
        let cache_for_refresh = self.state.cache.clone();
        let refresh_interval = self.refresh_interval;
        let refresh_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(refresh_interval);
            loop {
                interval.tick().await;
                cache_for_refresh.refresh();
            }
        });

        // Versioned JSON API under /api/v1
        let api_v1 = Router::new()
            .route(routes::segments::HEALTH, get(handle_health))
            .route(routes::segments::GLOBAL, get(handle_global))
            .route(routes::segments::SERVER, get(handle_server))
            .route(
                routes::segments::SERVER_CHANNELS,
                get(handle_server_channels),
            )
            .route(routes::segments::CLIENTS, get(handle_clients))
            .route(routes::segments::CLIENT_BY_ID, get(handle_client_by_id))
            .route(
                routes::segments::CLIENT_CHANNELS,
                get(handle_client_channels),
            )
            .route(routes::segments::SV1_CLIENTS, get(handle_sv1_clients))
            .route(
                routes::segments::SV1_CLIENT_BY_ID,
                get(handle_sv1_client_by_id),
            );

        let app = Router::new()
            .route(routes::ROOT, get(handle_root))
            .merge(SwaggerUi::new(routes::SWAGGER_UI).url(routes::OPENAPI_SPEC, ApiDoc::openapi()))
            .nest(routes::API_V1_PREFIX, api_v1)
            .route(routes::METRICS, get(handle_prometheus_metrics))
            .with_state(self.state);

        info!(
            "Swagger UI available at http://{}/swagger-ui",
            self.bind_address
        );
        info!(
            "Prometheus metrics available at http://{}/metrics",
            self.bind_address
        );

        let server_handle = axum::serve(listener, app).with_graceful_shutdown(async move {
            shutdown_signal.await;
            info!("Monitoring server received shutdown signal, stopping...");
        });

        // Run server and wait for shutdown
        let result = server_handle.await;

        // Stop the refresh task
        refresh_handle.abort();

        info!("Monitoring server stopped");
        result.map_err(|e| e.into())
    }
}

// Response types — used for both actual responses, OpenAPI documentation,
// and as the canonical types for JSON deserialization in tests (both
// in-crate unit tests and downstream integration tests).
#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub timestamp: u64,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct ServerResponse {
    pub extended_channels_count: usize,
    pub standard_channels_count: usize,
    pub total_hashrate: f32,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct ServerChannelsResponse {
    pub offset: usize,
    pub limit: usize,
    pub total_extended: usize,
    pub total_standard: usize,
    pub extended_channels: Vec<ServerExtendedChannelInfo>,
    pub standard_channels: Vec<ServerStandardChannelInfo>,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Sv2ClientsResponse {
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub items: Vec<Sv2ClientMetadata>,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Sv2ClientResponse {
    pub client_id: usize,
    pub client_kind: Sv2ClientKind,
    pub extended_channels_count: usize,
    pub standard_channels_count: usize,
    pub total_hashrate: f32,
    #[cfg(feature = "asic-rs-telemetry")]
    #[schema(value_type = Option<String>)]
    pub management_ip: Option<std::net::IpAddr>,
    #[cfg(feature = "asic-rs-telemetry")]
    pub miner_telemetry: Option<MinerTelemetry>,
    #[cfg(feature = "asic-rs-telemetry")]
    pub miner_telemetry_status: Option<MinerTelemetryStatus>,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Sv2ClientChannelsResponse {
    pub client_id: usize,
    pub offset: usize,
    pub limit: usize,
    pub total_extended: usize,
    pub total_standard: usize,
    pub extended_channels: Vec<ExtendedChannelInfo>,
    pub standard_channels: Vec<StandardChannelInfo>,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Sv1ClientsResponse {
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub items: Vec<Sv1ClientInfo>,
}

/// Response shape for the root `/` endpoint: a service banner plus a map of
/// available endpoints to their human-readable descriptions.
#[derive(Serialize, Deserialize, Debug)]
pub struct RootResponse {
    pub service: String,
    pub version: String,
    pub endpoints: std::collections::BTreeMap<String, String>,
}

/// Root endpoint - lists all available APIs
async fn handle_root() -> Json<RootResponse> {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        routes::ROOT.to_string(),
        "This endpoint - API listing".to_string(),
    );
    endpoints.insert(
        routes::SWAGGER_UI.to_string(),
        "Swagger UI (interactive API documentation)".to_string(),
    );
    endpoints.insert(
        routes::OPENAPI_SPEC.to_string(),
        "OpenAPI specification".to_string(),
    );
    endpoints.insert(routes::HEALTH.to_string(), "Health check".to_string());
    endpoints.insert(routes::GLOBAL.to_string(), "Global statistics".to_string());
    endpoints.insert(routes::SERVER.to_string(), "Server metadata".to_string());
    endpoints.insert(
        routes::SERVER_CHANNELS.to_string(),
        "Server channels (paginated)".to_string(),
    );
    endpoints.insert(
        routes::CLIENTS.to_string(),
        "All Sv2 clients metadata (paginated)".to_string(),
    );
    endpoints.insert(
        routes::CLIENT_BY_ID_PATTERN.to_string(),
        "Single Sv2 client metadata".to_string(),
    );
    endpoints.insert(
        routes::CLIENT_CHANNELS_PATTERN.to_string(),
        "Sv2 client channels (paginated)".to_string(),
    );
    endpoints.insert(
        routes::SV1_CLIENTS.to_string(),
        "Sv1 clients (Translator Proxy only, paginated)".to_string(),
    );
    endpoints.insert(
        routes::SV1_CLIENT_BY_ID_PATTERN.to_string(),
        "Single Sv1 client (Translator Proxy only)".to_string(),
    );
    endpoints.insert(
        routes::METRICS.to_string(),
        "Prometheus metrics".to_string(),
    );

    // Pull title/version from the OpenAPI spec so the `/` listing, the
    // OpenAPI document, and Swagger UI always agree.
    let info = ApiDoc::openapi().info;
    Json(RootResponse {
        service: info.title,
        version: info.version,
        endpoints,
    })
}

// Note: the `path = "..."` arguments to `#[utoipa::path(...)]` below must be
// string literals — utoipa parses them at macro-expansion time and does not
// accept `const` references. They must be kept in sync with `routes::*`.

/// Health check endpoint
#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "Service is healthy", body = HealthResponse)
    )
)]
async fn handle_health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    })
}

/// Get global statistics
///
/// Returns aggregated statistics for the server (upstream) and clients (downstream).
/// Fields are omitted from the response if that type of monitoring is not enabled.
///
/// **Typical responses:**
/// - **Pool/JDC**: `server` + `clients` (Sv2 downstream)
/// - **tProxy**: `server` + `sv1_clients` (Sv1 miners)
#[utoipa::path(
    get,
    path = "/api/v1/global",
    tag = "global",
    responses(
        (status = 200, description = "Global statistics", body = GlobalInfo)
    )
)]
async fn handle_global(State(state): State<ServerState>) -> Json<GlobalInfo> {
    let uptime_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        - state.start_time;

    let snapshot = state.cache.get_snapshot();

    Json(GlobalInfo {
        server: snapshot.server_summary,
        sv2_clients: snapshot.sv2_clients_summary,
        sv1_clients: snapshot.sv1_clients_summary,
        uptime_secs,
    })
}

/// Get server (upstream) metadata - use /server/channels for channel details
#[utoipa::path(
    get,
    path = "/api/v1/server",
    tag = "server",
    responses(
        (status = 200, description = "Server metadata", body = ServerResponse),
        (status = 404, description = "Server monitoring not available", body = ErrorResponse)
    )
)]
async fn handle_server(State(state): State<ServerState>) -> Response {
    let snapshot = state.cache.get_snapshot();

    match snapshot.server_summary {
        Some(summary) => Json(ServerResponse {
            extended_channels_count: summary.extended_channels,
            standard_channels_count: summary.standard_channels,
            total_hashrate: summary.total_hashrate,
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Server monitoring not available".to_string(),
            }),
        )
            .into_response(),
    }
}

/// Get server channels (paginated)
#[utoipa::path(
    get,
    path = "/api/v1/server/channels",
    tag = "server",
    params(Pagination),
    responses(
        (status = 200, description = "Server channels (paginated)", body = ServerChannelsResponse),
        (status = 404, description = "Server monitoring not available", body = ErrorResponse)
    )
)]
async fn handle_server_channels(
    Query(params): Query<Pagination>,
    State(state): State<ServerState>,
) -> Response {
    let snapshot = state.cache.get_snapshot();

    match snapshot.server_info {
        Some(server) => {
            let (total_extended, extended_channels) = paginate(&server.extended_channels, &params);
            let (total_standard, standard_channels) = paginate(&server.standard_channels, &params);

            Json(ServerChannelsResponse {
                offset: params.offset,
                limit: params.effective_limit(),
                total_extended,
                total_standard,
                extended_channels,
                standard_channels,
            })
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Server monitoring not available".to_string(),
            }),
        )
            .into_response(),
    }
}

/// Get all Sv2 clients (downstream) - returns metadata only, use /clients/{id}/channels for
/// channels
#[utoipa::path(
    get,
    path = "/api/v1/clients",
    tag = "clients",
    params(Pagination),
    responses(
        (status = 200, description = "List of Sv2 clients (metadata only)", body = Sv2ClientsResponse),
        (status = 404, description = "Sv2 clients monitoring not available", body = ErrorResponse)
    )
)]
async fn handle_clients(
    Query(params): Query<Pagination>,
    State(state): State<ServerState>,
) -> Response {
    let snapshot = state.cache.get_snapshot();

    match snapshot.sv2_clients {
        Some(ref sv2_clients) => {
            let metadata: Vec<Sv2ClientMetadata> =
                sv2_clients.iter().map(|c| c.to_metadata()).collect();
            let (total, items) = paginate(&metadata, &params);

            Json(Sv2ClientsResponse {
                offset: params.offset,
                limit: params.effective_limit(),
                total,
                items,
            })
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Sv2 clients monitoring not available".to_string(),
            }),
        )
            .into_response(),
    }
}

/// Get a single Sv2 client by ID - returns metadata only, use /clients/{id}/channels for channels
#[utoipa::path(
    get,
    path = "/api/v1/clients/{client_id}",
    tag = "clients",
    params(
        ("client_id" = usize, Path, description = "Sv2 Client ID")
    ),
    responses(
        (status = 200, description = "Sv2 client metadata", body = Sv2ClientResponse),
        (status = 404, description = "Sv2 client not found", body = ErrorResponse)
    )
)]
async fn handle_client_by_id(
    Path(client_id): Path<usize>,
    State(state): State<ServerState>,
) -> Response {
    let snapshot = state.cache.get_snapshot();

    let sv2_clients = match snapshot.sv2_clients {
        Some(ref clients) => clients,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "Sv2 clients monitoring not available".to_string(),
                }),
            )
                .into_response();
        }
    };

    match sv2_clients.iter().find(|c| c.client_id == client_id) {
        Some(client) => Json(Sv2ClientResponse {
            client_id,
            client_kind: client.client_kind,
            extended_channels_count: client.extended_channels.len(),
            standard_channels_count: client.standard_channels.len(),
            total_hashrate: client.total_hashrate(),
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: client.management_ip,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: client.miner_telemetry.clone(),
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: client.miner_telemetry_status,
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Sv2 client {client_id} not found"),
            }),
        )
            .into_response(),
    }
}

/// Get channels for a specific Sv2 client (paginated)
#[utoipa::path(
    get,
    path = "/api/v1/clients/{client_id}/channels",
    tag = "clients",
    params(
        ("client_id" = usize, Path, description = "Sv2 Client ID"),
        Pagination
    ),
    responses(
        (status = 200, description = "Sv2 client channels (paginated)", body = Sv2ClientChannelsResponse),
        (status = 404, description = "Sv2 client not found", body = ErrorResponse)
    )
)]
async fn handle_client_channels(
    Path(client_id): Path<usize>,
    Query(params): Query<Pagination>,
    State(state): State<ServerState>,
) -> Response {
    let snapshot = state.cache.get_snapshot();

    let sv2_clients = match snapshot.sv2_clients {
        Some(ref clients) => clients,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "Sv2 clients monitoring not available".to_string(),
                }),
            )
                .into_response();
        }
    };

    match sv2_clients.iter().find(|c| c.client_id == client_id) {
        Some(client) => {
            let (total_extended, extended_channels) = paginate(&client.extended_channels, &params);
            let (total_standard, standard_channels) = paginate(&client.standard_channels, &params);

            Json(Sv2ClientChannelsResponse {
                client_id,
                offset: params.offset,
                limit: params.effective_limit(),
                total_extended,
                total_standard,
                extended_channels,
                standard_channels,
            })
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Sv2 client {client_id} not found"),
            }),
        )
            .into_response(),
    }
}

/// Get Sv1 clients (Translator Proxy only)
#[utoipa::path(
    get,
    path = "/api/v1/sv1/clients",
    tag = "sv1",
    params(Pagination),
    responses(
        (status = 200, description = "List of Sv1 clients", body = Sv1ClientsResponse),
        (status = 404, description = "Sv1 monitoring not available", body = ErrorResponse)
    )
)]
async fn handle_sv1_clients(
    Query(params): Query<Pagination>,
    State(state): State<ServerState>,
) -> Response {
    let snapshot = state.cache.get_snapshot();

    match snapshot.sv1_clients {
        Some(ref sv1_clients) => {
            let (total, items) = paginate(sv1_clients, &params);

            Json(Sv1ClientsResponse {
                offset: params.offset,
                limit: params.effective_limit(),
                total,
                items,
            })
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Sv1 client monitoring not available".to_string(),
            }),
        )
            .into_response(),
    }
}

/// Get a single Sv1 client by ID
#[utoipa::path(
    get,
    path = "/api/v1/sv1/clients/{client_id}",
    tag = "sv1",
    params(
        ("client_id" = usize, Path, description = "Sv1 client ID")
    ),
    responses(
        (status = 200, description = "Sv1 client details", body = Sv1ClientInfo),
        (status = 404, description = "Sv1 client not found", body = ErrorResponse)
    )
)]
async fn handle_sv1_client_by_id(
    Path(client_id): Path<usize>,
    State(state): State<ServerState>,
) -> Response {
    let snapshot = state.cache.get_snapshot();

    let sv1_clients = match snapshot.sv1_clients {
        Some(ref clients) => clients,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "Sv1 client monitoring not available".to_string(),
                }),
            )
                .into_response();
        }
    };

    match sv1_clients.iter().find(|c| c.client_id == client_id) {
        Some(client) => Json(client.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Sv1 client {client_id} not found"),
            }),
        )
            .into_response(),
    }
}

/// Handler for Prometheus metrics endpoint.
///
/// All GaugeVec metric values are updated atomically by the background cache refresh
/// task in `SnapshotCache::refresh()`. This handler only needs to:
/// 1. Set the uptime gauge (requires wall-clock time at scrape time)
/// 2. Gather and encode all registered metrics
///
/// Because metric values are always kept in sync with the snapshot data, there is
/// never a gap where label series momentarily disappear. Tests can assert on metrics
/// directly after a cache refresh without polling for transient states.
async fn handle_prometheus_metrics(State(state): State<ServerState>) -> Response {
    // Uptime is the only metric set at scrape time (needs current wall clock)
    let uptime_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        - state.start_time;
    state.metrics.sv2_uptime_seconds.set(uptime_secs as f64);

    // Gather and encode — all other metrics were set by the last cache refresh
    let encoder = TextEncoder::new();
    let metric_families = state.metrics.registry.gather();
    let mut buffer = Vec::new();

    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => match String::from_utf8(buffer) {
            Ok(metrics_text) => (StatusCode::OK, metrics_text).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("UTF-8 error: {e}"),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Encoding error: {e}"),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitoring::server::ServerInfo;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::{collections::HashMap, sync::Mutex};
    use stratum_core::mining_sv2::ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE;
    use tower::ServiceExt;

    // ── helpers ──────────────────────────────────────────────────────

    fn create_extended_channel_info(channel_id: u32, hashrate: f32) -> ExtendedChannelInfo {
        ExtendedChannelInfo {
            channel_id,
            user_identity: format!("user-ext-{}", channel_id),
            nominal_hashrate: hashrate,
            stable_hashrate: false,
            target_hex: "00ff".into(),
            requested_max_target_hex: "00ff".into(),
            extranonce_prefix_hex: "aa".into(),
            full_extranonce_size: 16,
            rollable_extranonce_size: 4,
            expected_shares_per_minute: 1.0,
            shares_accepted: 10,
            shares_rejected: 0,
            shares_rejected_by_reason: HashMap::new(),
            share_work_sum: 100.0,
            last_share_sequence_number: 5,
            best_diff: 50.0,
            last_batch_accepted: 3,
            last_batch_work_sum: 30,
            share_batch_size: 10,
            blocks_found: 0,
        }
    }

    fn create_standard_channel_info(channel_id: u32, hashrate: f32) -> StandardChannelInfo {
        StandardChannelInfo {
            channel_id,
            user_identity: format!("user-std-{}", channel_id),
            nominal_hashrate: hashrate,
            stable_hashrate: false,
            target_hex: "00ff".into(),
            requested_max_target_hex: "00ff".into(),
            extranonce_prefix_hex: "bb".into(),
            expected_shares_per_minute: 2.0,
            shares_accepted: 20,
            shares_rejected: 1,
            shares_rejected_by_reason: HashMap::from([(
                ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE.to_string(),
                1,
            )]),
            share_work_sum: 200.0,
            last_share_sequence_number: 8,
            best_diff: 80.0,
            last_batch_accepted: 5,
            last_batch_work_sum: 50,
            share_batch_size: 20,
            blocks_found: 0,
        }
    }

    fn create_server_extended_channel_info(
        channel_id: u32,
        hashrate: Option<f32>,
    ) -> ServerExtendedChannelInfo {
        ServerExtendedChannelInfo {
            channel_id,
            user_identity: format!("pool-ext-{}", channel_id),
            nominal_hashrate: hashrate,
            target_hex: "00ff".into(),
            extranonce_prefix_hex: "aa".into(),
            full_extranonce_size: 16,
            rollable_extranonce_size: 4,
            version_rolling: true,
            shares_acknowledged: 10,
            shares_rejected: 0,
            shares_rejected_by_reason: HashMap::new(),
            acknowledged_work_sum: 100,
            validated_work_sum: 100.0,
            shares_submitted: 12,
            best_diff: 50.0,
            blocks_found: 0,
        }
    }

    fn create_server_standard_channel_info(
        channel_id: u32,
        hashrate: Option<f32>,
    ) -> ServerStandardChannelInfo {
        ServerStandardChannelInfo {
            channel_id,
            user_identity: format!("pool-std-{}", channel_id),
            nominal_hashrate: hashrate,
            target_hex: "00ff".into(),
            extranonce_prefix_hex: "bb".into(),
            shares_acknowledged: 20,
            shares_submitted: 22,
            shares_rejected: 1,
            shares_rejected_by_reason: HashMap::from([(
                ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE.to_string(),
                1,
            )]),
            acknowledged_work_sum: 200,
            validated_work_sum: 200.0,
            best_diff: 80.0,
            blocks_found: 0,
        }
    }

    #[cfg(feature = "asic-rs-telemetry")]
    fn create_miner_telemetry() -> MinerTelemetry {
        MinerTelemetry {
            make: Some("Acme".into()),
            model: Some("HashBox".into()),
            firmware_version: Some("1.2.3".into()),
            reported_hashrate_hs: Some(100_000_000_000_000.0),
            power_consumption_w: Some(3200.0),
            efficiency_j_per_th: Some(32.0),
            average_temperature_c: Some(68.0),
            uptime_secs: Some(3600),
            is_mining: Some(true),
        }
    }

    fn create_sv1_client_info(id: usize, hashrate: Option<f32>) -> Sv1ClientInfo {
        Sv1ClientInfo {
            client_id: id,
            channel_id: Some(id as u32),
            connection_ip: format!("192.0.2.{}", id)
                .parse()
                .expect("test IP address must be valid"),
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            sv1_username: format!("account.worker-{}", id),
            sv1_worker_name: format!("worker-{}", id),
            target_hex: "00ff".into(),
            hashrate,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
            stable_hashrate: false,
            extranonce1_hex: "aabb".into(),
            extranonce2_len: 8,
            version_rolling_mask: Some("ffffffff".into()),
            version_rolling_min_bit: Some("00000000".into()),
        }
    }

    struct MockServer(ServerInfo);
    impl ServerMonitoring for MockServer {
        fn get_server(&self) -> ServerInfo {
            self.0.clone()
        }
    }

    struct MockClients(Vec<Sv2ClientInfo>);
    impl Sv2ClientsMonitoring for MockClients {
        fn get_sv2_clients(&self) -> Vec<Sv2ClientInfo> {
            self.0.clone()
        }
    }

    struct MockSv1Clients(Vec<Sv1ClientInfo>);
    impl Sv1ClientsMonitoring for MockSv1Clients {
        fn get_sv1_clients(&self) -> Vec<Sv1ClientInfo> {
            self.0.clone()
        }
    }

    /// Build a full Router with mock data for integration testing.
    fn build_test_app(
        server: Option<Arc<dyn ServerMonitoring + Send + Sync>>,
        clients: Option<Arc<dyn Sv2ClientsMonitoring + Send + Sync>>,
        sv1: Option<Arc<dyn Sv1ClientsMonitoring + Send + Sync>>,
    ) -> Router {
        let has_server = server.is_some();
        let has_clients = clients.is_some();
        let has_sv1 = sv1.is_some();

        let metrics = PrometheusMetrics::new(has_server, has_clients, has_sv1).unwrap();

        let cache = Arc::new(
            SnapshotCache::new(Duration::from_secs(60), server, clients)
                .with_metrics(metrics.clone()),
        );

        let cache = if let Some(sv1_source) = sv1 {
            Arc::new(
                Arc::try_unwrap(cache)
                    .unwrap_or_else(|arc| (*arc).clone())
                    .with_sv1_clients_source(sv1_source),
            )
        } else {
            cache
        };

        cache.refresh();

        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let state = ServerState {
            cache,
            start_time,
            metrics,
        };

        let api_v1 = Router::new()
            .route(routes::segments::HEALTH, get(handle_health))
            .route(routes::segments::GLOBAL, get(handle_global))
            .route(routes::segments::SERVER, get(handle_server))
            .route(
                routes::segments::SERVER_CHANNELS,
                get(handle_server_channels),
            )
            .route(routes::segments::CLIENTS, get(handle_clients))
            .route(routes::segments::CLIENT_BY_ID, get(handle_client_by_id))
            .route(
                routes::segments::CLIENT_CHANNELS,
                get(handle_client_channels),
            )
            .route(routes::segments::SV1_CLIENTS, get(handle_sv1_clients))
            .route(
                routes::segments::SV1_CLIENT_BY_ID,
                get(handle_sv1_client_by_id),
            );

        Router::new()
            .route(routes::ROOT, get(handle_root))
            .nest(routes::API_V1_PREFIX, api_v1)
            .route(routes::METRICS, get(handle_prometheus_metrics))
            .with_state(state)
    }

    async fn get_body(response: axum::response::Response) -> String {
        let body = response.into_body();
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn make_request(uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    // ── Pagination unit tests ───────────────────────────────────────

    #[test]
    fn pagination_effective_limit_default() {
        let p = Pagination {
            offset: 0,
            limit: None,
        };
        assert_eq!(p.effective_limit(), DEFAULT_LIMIT);
    }

    #[test]
    fn pagination_effective_limit_capped_at_max() {
        let p = Pagination {
            offset: 0,
            limit: Some(500),
        };
        assert_eq!(p.effective_limit(), MAX_LIMIT);
    }

    #[test]
    fn pagination_effective_limit_respects_small_value() {
        let p = Pagination {
            offset: 0,
            limit: Some(5),
        };
        assert_eq!(p.effective_limit(), 5);
    }

    #[test]
    fn paginate_empty_slice() {
        let items: Vec<i32> = vec![];
        let params = Pagination {
            offset: 0,
            limit: Some(10),
        };
        let (total, result) = paginate(&items, &params);
        assert_eq!(total, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn paginate_basic() {
        let items: Vec<i32> = (0..50).collect();
        let params = Pagination {
            offset: 10,
            limit: Some(5),
        };
        let (total, result) = paginate(&items, &params);
        assert_eq!(total, 50);
        assert_eq!(result, vec![10, 11, 12, 13, 14]);
    }

    #[test]
    fn paginate_offset_beyond_length() {
        let items: Vec<i32> = vec![1, 2, 3];
        let params = Pagination {
            offset: 100,
            limit: Some(10),
        };
        let (total, result) = paginate(&items, &params);
        assert_eq!(total, 3);
        assert!(result.is_empty());
    }

    #[test]
    fn paginate_limit_exceeds_remaining() {
        let items: Vec<i32> = vec![1, 2, 3, 4, 5];
        let params = Pagination {
            offset: 3,
            limit: Some(10),
        };
        let (total, result) = paginate(&items, &params);
        assert_eq!(total, 5);
        assert_eq!(result, vec![4, 5]);
    }

    // ── HTTP endpoint integration tests ─────────────────────────────

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let app = build_test_app(None, None, None);
        let response = app.oneshot(make_request(routes::HEALTH)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: HealthResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.status, "ok");
    }

    #[tokio::test]
    async fn root_endpoint_lists_endpoints() {
        let app = build_test_app(None, None, None);
        let response = app.oneshot(make_request(routes::ROOT)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: RootResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.service, ApiDoc::openapi().info.title);
        assert!(resp.endpoints.contains_key(routes::HEALTH));
    }

    #[tokio::test]
    async fn run_returns_error_when_bind_address_is_in_use() {
        let occupied_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_addr = occupied_listener.local_addr().unwrap();
        let server =
            MonitoringServer::new(occupied_addr, None, None, Duration::from_millis(10)).unwrap();

        let err = server.run(std::future::pending()).await.unwrap_err();

        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::AddrInUse)
        );
    }

    #[tokio::test]
    async fn global_endpoint_with_no_sources() {
        let app = build_test_app(None, None, None);
        let response = app.oneshot(make_request(routes::GLOBAL)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: GlobalInfo = serde_json::from_str(&body).unwrap();
        assert!(resp.server.is_none());
        assert!(resp.sv2_clients.is_none());
    }

    #[tokio::test]
    async fn global_endpoint_with_data() {
        let server = Arc::new(MockServer(ServerInfo {
            extended_channels: vec![create_server_extended_channel_info(1, Some(100.0))],
            standard_channels: vec![],
        }));
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![create_extended_channel_info(1, 50.0)],
            standard_channels: vec![],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }]));

        let app = build_test_app(
            Some(server as Arc<dyn ServerMonitoring + Send + Sync>),
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app.oneshot(make_request(routes::GLOBAL)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: GlobalInfo = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.server.as_ref().unwrap().extended_channels, 1);
        assert_eq!(resp.sv2_clients.as_ref().unwrap().total_clients, 1);
    }

    #[tokio::test]
    async fn server_endpoint_not_available() {
        let app = build_test_app(None, None, None);
        let response = app.oneshot(make_request(routes::SERVER)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn server_endpoint_with_data() {
        let server = Arc::new(MockServer(ServerInfo {
            extended_channels: vec![create_server_extended_channel_info(1, Some(100.0))],
            standard_channels: vec![create_server_standard_channel_info(2, Some(50.0))],
        }));

        let app = build_test_app(
            Some(server as Arc<dyn ServerMonitoring + Send + Sync>),
            None,
            None,
        );
        let response = app.oneshot(make_request(routes::SERVER)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: ServerResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.extended_channels_count, 1);
        assert_eq!(resp.standard_channels_count, 1);
    }

    #[tokio::test]
    async fn server_channels_endpoint_with_pagination() {
        let server = Arc::new(MockServer(ServerInfo {
            extended_channels: vec![
                create_server_extended_channel_info(1, Some(100.0)),
                create_server_extended_channel_info(2, Some(200.0)),
                create_server_extended_channel_info(3, Some(300.0)),
            ],
            standard_channels: vec![],
        }));

        let app = build_test_app(
            Some(server as Arc<dyn ServerMonitoring + Send + Sync>),
            None,
            None,
        );
        let response = app
            .oneshot(make_request(&format!(
                "{}?offset=1&limit=1",
                routes::SERVER_CHANNELS
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: ServerChannelsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.total_extended, 3);
        assert_eq!(resp.offset, 1);
        assert_eq!(resp.limit, 1);
        assert_eq!(resp.extended_channels.len(), 1);
    }

    #[tokio::test]
    async fn server_channels_endpoint_keeps_rejected_shares_total_compatible() {
        let server = Arc::new(MockServer(ServerInfo {
            extended_channels: vec![],
            standard_channels: vec![create_server_standard_channel_info(1, Some(50.0))],
        }));

        let app = build_test_app(
            Some(server as Arc<dyn ServerMonitoring + Send + Sync>),
            None,
            None,
        );
        let response = app
            .oneshot(make_request(routes::SERVER_CHANNELS))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let channel = &json["standard_channels"][0];

        assert_eq!(channel["shares_rejected"], 1);
        assert_eq!(
            channel["shares_rejected_by_reason"][ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE],
            1
        );
    }

    #[tokio::test]
    async fn clients_endpoint_not_available() {
        let app = build_test_app(None, None, None);
        let response = app.oneshot(make_request(routes::CLIENTS)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn clients_endpoint_returns_metadata() {
        let clients = Arc::new(MockClients(vec![
            Sv2ClientInfo {
                client_id: 1,
                client_kind: Sv2ClientKind::Miner,
                extended_channels: vec![create_extended_channel_info(1, 100.0)],
                standard_channels: vec![],
                #[cfg(feature = "asic-rs-telemetry")]
                management_ip: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry: Some(create_miner_telemetry()),
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry_status: Some(MinerTelemetryStatus::Matched),
            },
            Sv2ClientInfo {
                client_id: 2,
                client_kind: Sv2ClientKind::Miner,
                extended_channels: vec![],
                standard_channels: vec![create_standard_channel_info(1, 50.0)],
                #[cfg(feature = "asic-rs-telemetry")]
                management_ip: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry_status: None,
            },
        ]));

        let app = build_test_app(
            None,
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app.oneshot(make_request(routes::CLIENTS)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv2ClientsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.total, 2);
        assert_eq!(resp.items.len(), 2);
        assert_eq!(resp.items[0].client_id, 1);
        assert_eq!(resp.items[0].client_kind, Sv2ClientKind::Miner);
        #[cfg(feature = "asic-rs-telemetry")]
        assert_eq!(
            resp.items[0]
                .miner_telemetry
                .as_ref()
                .and_then(|telemetry| telemetry.model.as_deref()),
            Some("HashBox")
        );
    }

    #[tokio::test]
    async fn client_by_id_found() {
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 42,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![create_extended_channel_info(1, 100.0)],
            standard_channels: vec![create_standard_channel_info(2, 50.0)],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: Some(create_miner_telemetry()),
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: Some(MinerTelemetryStatus::Matched),
        }]));

        let app = build_test_app(
            None,
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app
            .oneshot(make_request(&routes::client_by_id(42)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv2ClientResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.client_id, 42);
        assert_eq!(resp.client_kind, Sv2ClientKind::Miner);
        assert_eq!(resp.extended_channels_count, 1);
        assert_eq!(resp.standard_channels_count, 1);
        #[cfg(feature = "asic-rs-telemetry")]
        assert_eq!(
            resp.miner_telemetry
                .as_ref()
                .and_then(|telemetry| telemetry.model.as_deref()),
            Some("HashBox")
        );
    }

    #[tokio::test]
    async fn client_by_id_not_found() {
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![],
            standard_channels: vec![],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }]));

        let app = build_test_app(
            None,
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app
            .oneshot(make_request(&routes::client_by_id(999)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn client_channels_with_pagination() {
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![
                create_extended_channel_info(10, 100.0),
                create_extended_channel_info(11, 200.0),
                create_extended_channel_info(12, 300.0),
            ],
            standard_channels: vec![create_standard_channel_info(20, 50.0)],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }]));

        let app = build_test_app(
            None,
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app
            .oneshot(make_request(&format!(
                "{}?offset=1&limit=2",
                routes::client_channels(1)
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv2ClientChannelsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.client_id, 1);
        assert_eq!(resp.total_extended, 3);
        assert_eq!(resp.total_standard, 1);
        assert_eq!(resp.extended_channels.len(), 2);
    }

    #[tokio::test]
    async fn sv1_clients_not_available() {
        let app = build_test_app(None, None, None);
        let response = app
            .oneshot(make_request(routes::SV1_CLIENTS))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sv1_clients_with_data() {
        let sv1 = Arc::new(MockSv1Clients(vec![
            create_sv1_client_info(1, Some(100.0)),
            create_sv1_client_info(2, Some(200.0)),
        ]));

        let app = build_test_app(
            None,
            None,
            Some(sv1 as Arc<dyn Sv1ClientsMonitoring + Send + Sync>),
        );
        let response = app
            .oneshot(make_request(routes::SV1_CLIENTS))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv1ClientsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.total, 2);
        assert_eq!(resp.items.len(), 2);
    }

    #[tokio::test]
    async fn sv1_client_by_id_found() {
        let sv1 = Arc::new(MockSv1Clients(vec![create_sv1_client_info(7, Some(500.0))]));

        let app = build_test_app(
            None,
            None,
            Some(sv1 as Arc<dyn Sv1ClientsMonitoring + Send + Sync>),
        );
        let response = app
            .oneshot(make_request(&routes::sv1_client_by_id(7)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv1ClientInfo = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.client_id, 7);
    }

    #[tokio::test]
    async fn sv1_client_by_id_not_found() {
        let sv1 = Arc::new(MockSv1Clients(vec![create_sv1_client_info(1, Some(100.0))]));

        let app = build_test_app(
            None,
            None,
            Some(sv1 as Arc<dyn Sv1ClientsMonitoring + Send + Sync>),
        );
        let response = app
            .oneshot(make_request(&routes::sv1_client_by_id(999)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_format() {
        let server = Arc::new(MockServer(ServerInfo {
            extended_channels: vec![create_server_extended_channel_info(1, Some(100.0))],
            standard_channels: vec![],
        }));
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![create_extended_channel_info(1, 50.0)],
            standard_channels: vec![],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }]));

        let app = build_test_app(
            Some(server as Arc<dyn ServerMonitoring + Send + Sync>),
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app.oneshot(make_request(routes::METRICS)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        assert!(body.contains("sv2_uptime_seconds"));
        assert!(body.contains("sv2_server_channels"));
        assert!(body.contains("sv2_clients_total"));
    }

    #[tokio::test]
    async fn metrics_endpoint_with_no_sources() {
        let app = build_test_app(None, None, None);
        let response = app.oneshot(make_request(routes::METRICS)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        // Uptime is always present
        assert!(body.contains("sv2_uptime_seconds"));
        // Server/client metrics should NOT be present when sources are None
        assert!(!body.contains("sv2_server_channels"));
        assert!(!body.contains("sv2_clients_total"));
    }

    // Mutable mock that allows changing data between requests
    struct MutableMockClients(Mutex<Vec<Sv2ClientInfo>>);
    impl Sv2ClientsMonitoring for MutableMockClients {
        fn get_sv2_clients(&self) -> Vec<Sv2ClientInfo> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Verify that stale channel labels are removed without a reset gap.
    ///
    /// Scenario: First scrape has client with channel 1 and channel 2.
    /// Second scrape: channel 2 is gone. The test verifies that:
    /// - Channel 1 metrics are still present (no gap)
    /// - Channel 2 metrics are removed (stale cleanup)
    #[tokio::test]
    async fn metrics_stale_labels_removed_without_reset_gap() {
        let mut channel_2 = create_extended_channel_info(2, 200.0);
        channel_2.shares_rejected = 1;
        channel_2.shares_rejected_by_reason =
            HashMap::from([(ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE.to_string(), 1)]);

        let initial_clients = vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![create_extended_channel_info(1, 100.0), channel_2],
            standard_channels: vec![],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }];

        let mock_clients = Arc::new(MutableMockClients(Mutex::new(initial_clients)));
        let metrics = PrometheusMetrics::new(false, true, false).unwrap();
        let cache = Arc::new(
            SnapshotCache::new(
                Duration::from_secs(60),
                None,
                Some(mock_clients.clone() as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            )
            .with_metrics(metrics.clone()),
        );
        cache.refresh();

        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let state = ServerState {
            cache: cache.clone(),
            start_time,
            metrics,
        };

        let app = Router::new()
            .route(routes::METRICS, get(handle_prometheus_metrics))
            .with_state(state);

        // First scrape — both channels present
        let response = app
            .clone()
            .oneshot(make_request(routes::METRICS))
            .await
            .unwrap();
        let body = get_body(response).await;
        // Prometheus sorts label keys alphabetically: channel_id, client_id, user_identity
        assert!(
            body.contains("sv2_client_shares_accepted_total{channel_id=\"1\",client_id=\"1\""),
            "Channel 1 should be present on first scrape"
        );
        assert!(
            body.contains("sv2_client_shares_accepted_total{channel_id=\"2\",client_id=\"1\""),
            "Channel 2 should be present on first scrape"
        );
        assert!(
            body.contains("sv2_client_shares_rejected_total{channel_id=\"2\",client_id=\"1\",error_code=\"duplicate-share\""),
            "Channel 2 rejected-share metric should be present on first scrape"
        );

        // Remove channel 2 from mock data and refresh cache
        {
            let mut clients = mock_clients.0.lock().unwrap();
            clients[0].extended_channels.retain(|c| c.channel_id == 1);
        }
        cache.refresh();

        // Second scrape — channel 2 should be removed, channel 1 still present
        let response = app
            .clone()
            .oneshot(make_request(routes::METRICS))
            .await
            .unwrap();
        let body = get_body(response).await;
        assert!(
            body.contains("sv2_client_shares_accepted_total{channel_id=\"1\",client_id=\"1\""),
            "Channel 1 should still be present after stale removal"
        );
        assert!(
            !body.contains("sv2_client_shares_accepted_total{channel_id=\"2\",client_id=\"1\""),
            "Channel 2 should be removed as stale"
        );
        assert!(
            !body.contains("sv2_client_shares_rejected_total{channel_id=\"2\",client_id=\"1\",error_code=\"duplicate-share\""),
            "Channel 2 rejected-share metric should be removed as stale"
        );
    }

    /// Regression test for lazy-loading of `sv2_*_shares_rejected_total`.
    ///
    /// A `GaugeVec` only emits a series after `with_label_values(...).set(...)` runs at
    /// least once. With the rejection metric populated only inside a loop over
    /// `channel.shares_rejected_by_reason`, a channel with zero rejections produces no
    /// series at all in `/metrics` — Grafana panels fail to load and alerting rules
    /// silently never fire.
    ///
    /// Pre-seeding the spec-defined error codes from `mining_sv2::ERROR_CODE_SUBMIT_SHARES_*`
    /// to `0` on every refresh fixes this. This test asserts the metric is emitted with
    /// zero rejections.
    #[tokio::test]
    async fn shares_rejected_metric_emitted_with_zero_rejections() {
        // Server channel with zero rejections.
        // Helper defaults for standard channel set shares_rejected=1; override to 0.
        let mut server_info = super::super::server::ServerInfo {
            extended_channels: vec![create_server_extended_channel_info(1, Some(100.0))],
            standard_channels: vec![create_server_standard_channel_info(2, Some(50.0))],
        };
        server_info.standard_channels[0].shares_rejected = 0;
        server_info.standard_channels[0].shares_rejected_by_reason = HashMap::new();

        let server = Arc::new(MockServer(server_info));

        // Client channel with zero rejections
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![create_extended_channel_info(1, 100.0)],
            standard_channels: vec![],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }]));

        let app = build_test_app(
            Some(server as Arc<dyn ServerMonitoring + Send + Sync>),
            Some(clients as Arc<dyn super::super::client::Sv2ClientsMonitoring + Send + Sync>),
            None,
        );

        let response = app.oneshot(make_request(routes::METRICS)).await.unwrap();
        let body = get_body(response).await;

        // Both metrics MUST appear with the spec-defined error_code labels pre-seeded to 0.
        assert!(
            body.contains(
                "sv2_server_shares_rejected_total{channel_id=\"1\",error_code=\"stale-share\""
            ),
            "sv2_server_shares_rejected_total stale-share label must be pre-seeded to 0; got:\n{body}"
        );
        assert!(
            body.contains(
                "sv2_server_shares_rejected_total{channel_id=\"1\",error_code=\"duplicate-share\""
            ),
            "sv2_server_shares_rejected_total duplicate-share label must be pre-seeded to 0; got:\n{body}"
        );
        assert!(
            body.contains("sv2_client_shares_rejected_total{channel_id=\"1\",client_id=\"1\",error_code=\"stale-share\""),
            "sv2_client_shares_rejected_total stale-share label must be pre-seeded to 0; got:\n{body}"
        );
    }

    // ── Edge-case unit tests (pagination, missing data, invalid params) ──

    #[test]
    fn paginate_with_limit_zero() {
        // effective_limit(Some(0)) = 0.min(MAX_LIMIT) = 0, so take(0) returns nothing
        let items: Vec<i32> = (0..50).collect();
        let params = Pagination {
            offset: 0,
            limit: Some(0),
        };
        let (total, result) = paginate(&items, &params);
        assert_eq!(total, 50);
        assert!(result.is_empty(), "limit=0 should return no items");
    }

    #[tokio::test]
    async fn server_channels_not_available() {
        let app = build_test_app(None, None, None);
        let response = app
            .oneshot(make_request(routes::SERVER_CHANNELS))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = get_body(response).await;
        let resp: ErrorResponse = serde_json::from_str(&body).unwrap();
        assert!(!resp.error.is_empty());
    }

    #[tokio::test]
    async fn client_by_id_no_monitoring() {
        // When client monitoring is not available at all, any client_id returns 404
        let app = build_test_app(None, None, None);
        let response = app
            .oneshot(make_request(&routes::client_by_id(1)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = get_body(response).await;
        let resp: ErrorResponse = serde_json::from_str(&body).unwrap();
        assert!(!resp.error.is_empty());
    }

    #[tokio::test]
    async fn client_channels_client_not_found() {
        // Client monitoring is available but the specific client_id does not exist
        let clients = Arc::new(MockClients(vec![Sv2ClientInfo {
            client_id: 1,
            client_kind: Sv2ClientKind::Miner,
            extended_channels: vec![],
            standard_channels: vec![],
            #[cfg(feature = "asic-rs-telemetry")]
            management_ip: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry: None,
            #[cfg(feature = "asic-rs-telemetry")]
            miner_telemetry_status: None,
        }]));

        let app = build_test_app(
            None,
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app
            .oneshot(make_request(&routes::client_channels(999)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = get_body(response).await;
        let resp: ErrorResponse = serde_json::from_str(&body).unwrap();
        assert!(resp.error.contains("999"));
    }

    #[tokio::test]
    async fn client_channels_no_monitoring() {
        // When client monitoring is not available at all
        let app = build_test_app(None, None, None);
        let response = app
            .oneshot(make_request(&routes::client_channels(1)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sv1_client_by_id_no_monitoring() {
        // When SV1 monitoring is not available at all
        let app = build_test_app(None, None, None);
        let response = app
            .oneshot(make_request(&routes::sv1_client_by_id(1)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = get_body(response).await;
        let resp: ErrorResponse = serde_json::from_str(&body).unwrap();
        assert!(!resp.error.is_empty());
    }

    #[tokio::test]
    async fn clients_pagination_offset_and_limit() {
        let clients = Arc::new(MockClients(vec![
            Sv2ClientInfo {
                client_id: 1,
                client_kind: Sv2ClientKind::Miner,
                extended_channels: vec![create_extended_channel_info(1, 100.0)],
                standard_channels: vec![],
                #[cfg(feature = "asic-rs-telemetry")]
                management_ip: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry_status: None,
            },
            Sv2ClientInfo {
                client_id: 2,
                client_kind: Sv2ClientKind::Miner,
                extended_channels: vec![],
                standard_channels: vec![create_standard_channel_info(1, 50.0)],
                #[cfg(feature = "asic-rs-telemetry")]
                management_ip: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry_status: None,
            },
            Sv2ClientInfo {
                client_id: 3,
                client_kind: Sv2ClientKind::Miner,
                extended_channels: vec![create_extended_channel_info(2, 200.0)],
                standard_channels: vec![],
                #[cfg(feature = "asic-rs-telemetry")]
                management_ip: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry: None,
                #[cfg(feature = "asic-rs-telemetry")]
                miner_telemetry_status: None,
            },
        ]));

        let app = build_test_app(
            None,
            Some(clients as Arc<dyn Sv2ClientsMonitoring + Send + Sync>),
            None,
        );
        let response = app
            .oneshot(make_request(&format!(
                "{}?offset=1&limit=1",
                routes::CLIENTS
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv2ClientsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.total, 3);
        assert_eq!(resp.offset, 1);
        assert_eq!(resp.limit, 1);
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].client_id, 2);
    }

    #[tokio::test]
    async fn sv1_clients_pagination() {
        let sv1 = Arc::new(MockSv1Clients(vec![
            create_sv1_client_info(1, Some(100.0)),
            create_sv1_client_info(2, Some(200.0)),
            create_sv1_client_info(3, Some(300.0)),
        ]));

        let app = build_test_app(
            None,
            None,
            Some(sv1 as Arc<dyn Sv1ClientsMonitoring + Send + Sync>),
        );
        let response = app
            .oneshot(make_request(&format!(
                "{}?offset=2&limit=10",
                routes::SV1_CLIENTS
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: Sv1ClientsResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(resp.total, 3);
        assert_eq!(resp.offset, 2);
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].client_id, 3);
    }

    #[tokio::test]
    async fn global_endpoint_with_sv1_data() {
        let sv1 = Arc::new(MockSv1Clients(vec![create_sv1_client_info(1, Some(100.0))]));

        let app = build_test_app(
            None,
            None,
            Some(sv1 as Arc<dyn Sv1ClientsMonitoring + Send + Sync>),
        );
        let response = app.oneshot(make_request(routes::GLOBAL)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = get_body(response).await;
        let resp: GlobalInfo = serde_json::from_str(&body).unwrap();
        // Server and SV2 clients should be None
        assert!(resp.server.is_none());
        assert!(resp.sv2_clients.is_none());
        // SV1 clients should be present
        assert_eq!(resp.sv1_clients.as_ref().unwrap().total_clients, 1);
    }
}
