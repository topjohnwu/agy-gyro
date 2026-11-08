// SPDX-License-Identifier: MIT

use crate::config::{Config, DEFAULT_GATEWAY_MODELS, EXCLUDED_MODELS, MODEL_EFFORT_LEVELS};
use crate::retry::{
    calculate_backoff, is_retriable_error, is_retriable_status, parse_in_stream_error,
    parse_retry_after,
};
use crate::usage::{
    PricingTable, StreamUsageExtractor, UsageLogLayer, UsageSnapshot, UsageTracker,
    extract_model_from_path, extract_session_from_headers, extract_usage_from_chunks,
    format_session_api_key,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, OriginalUri, Path, Query, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{any, delete, get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelsResponse {
    #[serde(default)]
    pub models: Vec<GeminiModelInfo>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelInfo {
    pub name: String,
    #[serde(default)]
    pub supported_generation_methods: Vec<String>,
    #[serde(default)]
    pub thinking: bool,
}

/// Filters a list of `/v1beta/models` entries to retain only Core Gemini Text / Reasoning
/// models, excluding [`EXCLUDED_MODELS`], ordered newest-first.
pub fn filter_core_gemini_models(models: &[GeminiModelInfo]) -> Vec<String> {
    const NON_TEXT_KEYWORDS: &[&str] = &[
        "-image",
        "-banana",
        "-tts",
        "-audio",
        "-live",
        "-transcribe",
        "-omni",
        "-robotics",
        "-computer-use",
        "-embedding",
    ];

    let mut filtered: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let id = m.name.strip_prefix("models/").unwrap_or(&m.name);
            if !id.starts_with("gemini-") {
                return None;
            }
            if !m.thinking {
                return None;
            }
            if !m
                .supported_generation_methods
                .iter()
                .any(|method| method == "generateContent")
            {
                return None;
            }
            if NON_TEXT_KEYWORDS.iter().any(|kw| id.contains(kw)) {
                return None;
            }
            if EXCLUDED_MODELS.contains(&id) {
                return None;
            }
            Some(id.to_string())
        })
        .collect();

    // The Gemini API returns models oldest-first; reverse so newest models appear first.
    filtered.reverse();
    filtered
}

/// Expands a list of base Gemini model names into a comma-separated list of
/// effort-suffixed model IDs (`<model>-low`, `<model>-medium`, `<model>-high`).
pub fn expand_gateway_models<S: AsRef<str>>(models: &[S]) -> String {
    models
        .iter()
        .flat_map(|model| {
            let base = model.as_ref();
            MODEL_EFFORT_LEVELS
                .iter()
                .map(move |effort| format!("{}-{}", base, effort))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Queries `GET {upstream}/v1beta/models` and returns the comma-separated list of
/// active Core Gemini Text / Reasoning models expanded with effort suffixes
/// (falling back to [`DEFAULT_GATEWAY_MODELS`] if the request fails).
pub async fn resolve_gateway_models(client: &reqwest::Client, config: &Config) -> String {
    let api_key = config.api_key.trim();
    let upstream_base = config.upstream.trim_end_matches('/');
    let mut all_models = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut url = format!("{}/v1beta/models?pageSize=100", upstream_base);
        if let Some(ref token) = page_token {
            url.push_str("&pageToken=");
            url.push_str(token);
        }

        let res = match client
            .get(&url)
            .header("x-goog-api-key", api_key)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => res,
            Ok(res) => {
                warn!(
                    "Failed to fetch models from {} (status {}); falling back to default model list",
                    url,
                    res.status()
                );
                return expand_gateway_models(DEFAULT_GATEWAY_MODELS);
            }
            Err(err) => {
                warn!(
                    "Error fetching models from {}: {}; falling back to default model list",
                    url, err
                );
                return expand_gateway_models(DEFAULT_GATEWAY_MODELS);
            }
        };

        match res.json::<GeminiModelsResponse>().await {
            Ok(parsed) => {
                all_models.extend(parsed.models);
                if let Some(next) = parsed.next_page_token.filter(|t| !t.is_empty()) {
                    page_token = Some(next);
                } else {
                    break;
                }
            }
            Err(err) => {
                warn!(
                    "Failed to parse /v1beta/models JSON response: {}; falling back to default model list",
                    err
                );
                return expand_gateway_models(DEFAULT_GATEWAY_MODELS);
            }
        }
    }

    let models = filter_core_gemini_models(&all_models);
    if models.is_empty() {
        warn!("No core Gemini models matched filter; falling back to default model list");
        expand_gateway_models(DEFAULT_GATEWAY_MODELS)
    } else {
        info!("Discovered available Gemini models: {}", models.join(","));
        expand_gateway_models(&models)
    }
}

/// Default interval before committing early 200 OK headers and between keep-alive heartbeats.
/// Set to 15 seconds to stay well below agy's 60-second ResponseHeaderTimeout.
pub const DEFAULT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

pub struct ProxyState {
    pub config: Config,
    pub client: reqwest::Client,
    pub upstream_base: String,
    pub keep_alive_interval: Duration,
    pub usage: Arc<UsageTracker>,
    pub gateway_models: RwLock<String>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl ProxyState {
    pub fn new(config: Config, client: reqwest::Client) -> Self {
        use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

        let _ = tracing_subscriber::registry()
            .with(EnvFilter::new("info,agy_gyro=debug"))
            .with(UsageLogLayer)
            .try_init();

        let upstream_base = config.upstream.trim_end_matches('/').to_string();
        let usage = Arc::new(UsageTracker::default());
        usage.register_global_logger();
        usage.spawn_process_monitor();
        let default_models = expand_gateway_models(DEFAULT_GATEWAY_MODELS);
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Self {
            config,
            client,
            upstream_base,
            keep_alive_interval: DEFAULT_KEEP_ALIVE_INTERVAL,
            usage,
            gateway_models: RwLock::new(default_models),
            shutdown_tx,
        }
    }

    pub fn with_keep_alive_interval(mut self, interval: Duration) -> Self {
        self.keep_alive_interval = interval;
        self
    }

    pub fn with_pricing(self, pricing: PricingTable) -> Self {
        self.usage.set_pricing(pricing);
        self
    }

    pub fn set_gateway_models(&self, models: impl Into<String>) {
        if let Ok(mut guard) = self.gateway_models.write() {
            *guard = models.into();
        }
    }

    pub fn gateway_models(&self) -> String {
        self.gateway_models
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| expand_gateway_models(DEFAULT_GATEWAY_MODELS))
    }

    pub fn subscribe_shutdown(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Builds a tuned reqwest HTTP client from the proxy configuration.
pub fn build_http_client(config: &Config) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(config.request_timeout_secs))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(60))
        .http2_keep_alive_interval(Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
        .tcp_nodelay(true)
        .build()
}

async fn usage_handler(
    State(state): State<Arc<ProxyState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let tracker = state.usage.clone();
    let rx = tracker.subscribe();
    let shutdown_rx = state.subscribe_shutdown();
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let stream = futures_util::stream::unfold(
        (tracker, rx, shutdown_rx, ticker, true),
        |(tracker, mut rx, mut shutdown_rx, mut ticker, is_first)| async move {
            if *shutdown_rx.borrow() {
                return None;
            }
            if !is_first {
                tokio::select! {
                    res = rx.changed() => {
                        if res.is_err() {
                            return None;
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return None;
                        }
                    }
                    _ = ticker.tick() => {}
                }
            }
            let _ = rx.borrow_and_update();
            let snap = tracker.snapshot();
            let json = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".to_string());
            Some((
                Ok(Event::default().data(json)),
                (tracker, rx, shutdown_rx, ticker, false),
            ))
        },
    );

    Sse::new(stream)
}

#[derive(Debug, Default, Deserialize)]
struct ResetUsageQuery {
    session: Option<String>,
}

async fn reset_usage_handler(
    State(state): State<Arc<ProxyState>>,
    Query(query): Query<ResetUsageQuery>,
) -> Json<UsageSnapshot> {
    if let Some(ref session_id) = query.session {
        state.usage.reset_session(session_id);
    } else {
        state.usage.reset();
    }
    Json(state.usage.snapshot())
}

/// Request payload for `POST /sessions` when a wrapper registers an `agy` instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterSessionRequest {
    pub pid: Option<u32>,
    pub label: String,
}

/// Response payload from `POST /sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterSessionResponse {
    pub session_id: String,
    pub api_key: String,
    pub gateway_models: String,
}

/// Response payload from `GET /sessions` for checking daemon liveness and active model list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatusResponse {
    pub pid: u32,
    pub gateway_models: String,
}

async fn daemon_status_handler(State(state): State<Arc<ProxyState>>) -> Json<DaemonStatusResponse> {
    Json(DaemonStatusResponse {
        pid: std::process::id(),
        gateway_models: state.gateway_models(),
    })
}

async fn register_session_handler(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<RegisterSessionRequest>,
) -> Json<RegisterSessionResponse> {
    let meta = state.usage.register_session(req.pid, &req.label);
    let api_key = format_session_api_key(&meta.id, meta.pid, &meta.label);
    let gateway_models = state.gateway_models();
    Json(RegisterSessionResponse {
        session_id: meta.id,
        api_key,
        gateway_models,
    })
}

async fn remove_session_handler(
    State(state): State<Arc<ProxyState>>,
    Path(id): Path<String>,
) -> StatusCode {
    state.usage.remove_session(&id);
    StatusCode::NO_CONTENT
}

/// Response payload from `POST /shutdown`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownResponse {
    pub status: String,
    pub pid: u32,
}

async fn shutdown_handler(State(state): State<Arc<ProxyState>>) -> Json<ShutdownResponse> {
    let pid = std::process::id();
    info!("Shutdown requested via /shutdown endpoint (pid={})", pid);
    state.trigger_shutdown();
    Json(ShutdownResponse {
        status: "stopping".to_string(),
        pid,
    })
}

async fn logs_handler(
    State(state): State<Arc<ProxyState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let tracker = state.usage.clone();
    let rx = tracker.subscribe_logs();
    let shutdown_rx = state.subscribe_shutdown();

    let stream = futures_util::stream::unfold(
        (tracker, rx, shutdown_rx, true),
        |(tracker, mut rx, mut shutdown_rx, is_first)| async move {
            if *shutdown_rx.borrow() {
                return None;
            }
            if !is_first {
                tokio::select! {
                    res = rx.changed() => {
                        if res.is_err() {
                            return None;
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return None;
                        }
                    }
                }
            }
            let _ = rx.borrow_and_update();
            let logs = tracker.logs_snapshot();
            let json = serde_json::to_string(&logs).unwrap_or_else(|_| "[]".to_string());
            Some((
                Ok(Event::default().data(json)),
                (tracker, rx, shutdown_rx, false),
            ))
        },
    );

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
}

pub fn create_router(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/usage", get(usage_handler).delete(reset_usage_handler))
        .route("/logs", get(logs_handler))
        .route(
            "/sessions",
            get(daemon_status_handler).post(register_session_handler),
        )
        .route("/sessions/{id}", delete(remove_session_handler))
        .route("/shutdown", post(shutdown_handler))
        .route("/", any(proxy_handler))
        .route("/{*path}", any(proxy_handler))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

fn is_hop_by_hop(header_name: &HeaderName) -> bool {
    matches!(
        header_name.as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

fn is_sse_request(path_and_query: &str, headers: &HeaderMap) -> bool {
    path_and_query.contains(":streamGenerateContent")
        || path_and_query.contains("alt=sse")
        || headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"))
}

fn status_to_gemini_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
        StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "DEADLINE_EXCEEDED",
        StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY => "UNAVAILABLE",
        _ => "INTERNAL",
    }
}

fn format_error_json_value(status: StatusCode, body_bytes: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(body_bytes);
    let trimmed = text.trim();
    let json_candidate = trimmed
        .strip_prefix("data:")
        .map(str::trim)
        .unwrap_or(trimmed);

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_candidate) {
        if val.get("error").is_some() {
            return val;
        }
        if let Some(first) = val.as_array().and_then(|arr| arr.first())
            && first.get("error").is_some()
        {
            return first.clone();
        }
    }

    serde_json::json!({
        "error": {
            "code": status.as_u16(),
            "message": if trimmed.is_empty() { status.to_string() } else { trimmed.to_string() },
            "status": status_to_gemini_status(status),
        }
    })
}

fn format_sse_error_chunk(status: StatusCode, body_bytes: &[u8]) -> Bytes {
    let val = format_error_json_value(status, body_bytes);
    Bytes::from(format!("data: {}\n\n", val))
}

fn format_json_error_chunk(status: StatusCode, body_bytes: &[u8]) -> Bytes {
    let val = format_error_json_value(status, body_bytes);
    Bytes::from(val.to_string())
}

type BoxByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

enum UpstreamBody {
    Buffered(Vec<Bytes>),
    Passthrough(BoxByteStream),
    Empty,
}

struct UpstreamOutcome {
    status: StatusCode,
    headers: HeaderMap,
    body: UpstreamBody,
}

impl UpstreamOutcome {
    fn bad_gateway(msg: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            headers: HeaderMap::new(),
            body: UpstreamBody::Buffered(vec![Bytes::from(msg)]),
        }
    }

    fn into_fast_response(self) -> Response {
        let mut client_res_builder = Response::builder().status(self.status.as_u16());
        for (name, val) in &self.headers {
            if !is_hop_by_hop(name) {
                client_res_builder = client_res_builder.header(name, val);
            }
        }

        let body = match self.body {
            UpstreamBody::Buffered(chunks) => {
                let body_stream = futures_util::stream::iter(
                    chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
                );
                Body::from_stream(body_stream)
            }
            UpstreamBody::Passthrough(stream) => Body::from_stream(stream),
            UpstreamBody::Empty => Body::empty(),
        };

        match client_res_builder.body(body) {
            Ok(res) => res,
            Err(err) => {
                error!("Failed to build proxy response: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to build proxy response: {}", err),
                )
                    .into_response()
            }
        }
    }

    async fn into_late_stream(self, is_sse: bool) -> BoxByteStream {
        if self.status.is_success() {
            match self.body {
                UpstreamBody::Buffered(chunks) => Box::pin(futures_util::stream::iter(
                    chunks.into_iter().map(Ok::<_, std::io::Error>),
                )),
                UpstreamBody::Passthrough(stream) => stream,
                UpstreamBody::Empty => Box::pin(futures_util::stream::empty()),
            }
        } else {
            let err_bytes = match self.body {
                UpstreamBody::Buffered(chunks) => {
                    let total_len: usize = chunks.iter().map(|c| c.len()).sum();
                    let mut buf = Vec::with_capacity(total_len);
                    for c in chunks {
                        buf.extend_from_slice(&c);
                    }
                    Bytes::from(buf)
                }
                UpstreamBody::Passthrough(mut stream) => {
                    let mut buf = Vec::new();
                    while let Some(Ok(c)) = stream.next().await {
                        buf.extend_from_slice(&c);
                    }
                    Bytes::from(buf)
                }
                UpstreamBody::Empty => Bytes::new(),
            };

            let formatted = if is_sse {
                format_sse_error_chunk(self.status, &err_bytes)
            } else {
                format_json_error_chunk(self.status, &err_bytes)
            };

            Box::pin(futures_util::stream::once(async move {
                Ok::<_, std::io::Error>(formatted)
            }))
        }
    }
}

enum KeepAlivePhase<F> {
    SendInitialHeartbeat {
        work_fut: F,
        interval: tokio::time::Interval,
    },
    Waiting {
        work_fut: F,
        interval: tokio::time::Interval,
    },
    Flushing {
        payload_stream: BoxByteStream,
    },
}

fn format_endpoint(url: &reqwest::Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    }
}

async fn execute_proxy_request(
    state: Arc<ProxyState>,
    method: Method,
    target_url: reqwest::Url,
    headers: HeaderMap,
    body: Bytes,
) -> UpstreamOutcome {
    let req_start = Instant::now();
    let endpoint = format_endpoint(&target_url);
    let session = extract_session_from_headers(&headers);
    let session_prefix = session
        .as_ref()
        .map(|s| format!("[#{} {}] ", s.id, s.label))
        .unwrap_or_default();
    let session_id_str = session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
    let model_from_path = extract_model_from_path(target_url.path());
    let active_guard = model_from_path
        .as_ref()
        .map(|_| state.usage.begin_session_request(session.as_ref()));

    let initial_delay = Duration::from_millis(state.config.initial_delay_ms);
    let max_delay = Duration::from_millis(state.config.max_delay_ms);
    let with_jitter = state.config.is_jitter_enabled();
    let max_retries = state.config.max_retries;
    let api_key = state.config.api_key.trim();

    let mut attempt = 0;

    loop {
        debug!(
            session_id = %session_id_str,
            "{}Forwarding request {} {} (attempt {}/{})",
            session_prefix, method, endpoint, attempt, max_retries
        );

        let mut req_builder = state.client.request(method.clone(), target_url.clone());

        // Forward headers, stripping hop-by-hop, accept-encoding (handled by reqwest gzip), and gateway Authorization header
        for (header_name, header_val) in headers.iter() {
            if !is_hop_by_hop(header_name)
                && header_name != axum::http::header::ACCEPT_ENCODING
                && header_name != axum::http::header::AUTHORIZATION
                && header_name.as_str() != "x-goog-api-key"
            {
                req_builder = req_builder.header(header_name, header_val);
            }
        }

        req_builder = req_builder.header("x-goog-api-key", api_key);

        // Set request body
        req_builder = req_builder.body(body.clone());

        match req_builder.send().await {
            Ok(mut upstream_res) => {
                let status = upstream_res.status();

                if is_retriable_status(status) {
                    if attempt < max_retries {
                        let retry_after_duration = upstream_res
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|val| val.to_str().ok())
                            .and_then(parse_retry_after);

                        let delay = calculate_backoff(
                            attempt,
                            initial_delay,
                            max_delay,
                            with_jitter,
                            retry_after_duration,
                        );

                        let mut snippet = String::new();
                        if let Ok(Some(chunk)) = upstream_res.chunk().await {
                            let bytes = &chunk[..chunk.len().min(200)];
                            snippet = String::from_utf8_lossy(bytes).trim().to_string();
                            if chunk.len() > 200 {
                                snippet.push_str("...");
                            }
                        }

                        warn!(
                            session_id = %session_id_str,
                            "{}Upstream returned retriable status {} for {} {}. Retrying in {:?} (attempt {}/{}). Response: {}",
                            session_prefix,
                            status,
                            method,
                            endpoint,
                            delay,
                            attempt + 1,
                            max_retries,
                            snippet
                        );

                        if model_from_path.is_some() {
                            state.usage.record_session_retry(session.as_ref());
                        }
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    } else {
                        warn!(
                            session_id = %session_id_str,
                            "{}Max retries ({}) reached for status {}. Forwarding upstream error to client.",
                            session_prefix, max_retries, status
                        );
                    }
                } else if status.is_success() {
                    if attempt > 0 {
                        info!(
                            session_id = %session_id_str,
                            "{}Request {} {} succeeded after {} retries (status {})",
                            session_prefix, method, endpoint, attempt, status
                        );
                    } else {
                        debug!(
                            session_id = %session_id_str,
                            "{}Request {} {} succeeded (status {})",
                            session_prefix, method, endpoint, status
                        );
                    }
                } else {
                    debug!(
                        session_id = %session_id_str,
                        "{}Request {} {} returned non-retriable status {}",
                        session_prefix, method, endpoint, status
                    );
                }

                let mut res_headers = HeaderMap::new();
                for (name, val) in upstream_res.headers() {
                    if !is_hop_by_hop(name) {
                        res_headers.append(name.clone(), val.clone());
                    }
                }

                let mut stream = upstream_res.bytes_stream();

                if state.config.is_buffer_enabled() {
                    let mut buffered_chunks: Vec<Bytes> = Vec::new();
                    let mut stream_error = None;
                    let mut in_stream_err_details = None;
                    let mut chunk_index = 0;

                    while let Some(chunk_res) = stream.next().await {
                        match chunk_res {
                            Ok(chunk) => {
                                if status.is_success()
                                    && let Some((in_stream_status, err_msg)) =
                                        parse_in_stream_error(&chunk)
                                            .filter(|(s, _)| is_retriable_status(*s))
                                {
                                    in_stream_err_details =
                                        Some((chunk_index, in_stream_status, err_msg));
                                    buffered_chunks.clear();
                                    buffered_chunks.push(chunk);
                                    break;
                                }
                                buffered_chunks.push(chunk);
                                chunk_index += 1;
                            }
                            Err(err) => {
                                stream_error = Some((chunk_index, err));
                                break;
                            }
                        }
                    }

                    if let Some((chunk_idx, in_stream_status, ref err_msg)) = in_stream_err_details
                    {
                        if attempt < max_retries {
                            let delay = calculate_backoff(
                                attempt,
                                initial_delay,
                                max_delay,
                                with_jitter,
                                None,
                            );

                            warn!(
                                session_id = %session_id_str,
                                "{}Upstream returned in-stream retriable error {} ({}) at chunk #{} for {} {}. Retrying in {:?} (attempt {}/{})...",
                                session_prefix,
                                in_stream_status,
                                err_msg,
                                chunk_idx + 1,
                                method,
                                endpoint,
                                delay,
                                attempt + 1,
                                max_retries
                            );

                            if model_from_path.is_some() {
                                state.usage.record_session_retry(session.as_ref());
                            }
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        } else {
                            warn!(
                                session_id = %session_id_str,
                                "{}Max retries ({}) reached for in-stream error {}. Forwarding buffered stream to client.",
                                session_prefix, max_retries, in_stream_status
                            );
                        }
                    } else if let Some((chunk_idx, err)) = stream_error {
                        if is_retriable_error(&err) && attempt < max_retries {
                            let delay = calculate_backoff(
                                attempt,
                                initial_delay,
                                max_delay,
                                with_jitter,
                                None,
                            );

                            warn!(
                                session_id = %session_id_str,
                                "{}Upstream stream error at chunk #{} for {} {}: {}. Retrying in {:?} (attempt {}/{})...",
                                session_prefix,
                                chunk_idx + 1,
                                method,
                                endpoint,
                                err,
                                delay,
                                attempt + 1,
                                max_retries
                            );

                            if model_from_path.is_some() {
                                state.usage.record_session_retry(session.as_ref());
                            }
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        } else {
                            error!(
                                session_id = %session_id_str,
                                "{}Upstream error reading stream at chunk #{} for {} {}: {}",
                                session_prefix,
                                chunk_idx + 1,
                                method,
                                endpoint,
                                err
                            );
                            drop(active_guard);
                            if let Some(ref req_model) = model_from_path {
                                state.usage.record_session_completed_request(
                                    session.as_ref(),
                                    req_model,
                                    StatusCode::BAD_GATEWAY.as_u16(),
                                    attempt,
                                    req_start.elapsed(),
                                    None,
                                );
                            }
                            return UpstreamOutcome::bad_gateway(format!(
                                "Bad Gateway: upstream stream read failed: {}",
                                err
                            ));
                        }
                    }

                    drop(active_guard);
                    if let Some(ref req_model) = model_from_path {
                        let (raw_usage, resp_model) = extract_usage_from_chunks(&buffered_chunks);
                        let effective_model = resp_model.as_deref().unwrap_or(req_model);
                        let effective_status =
                            if let Some((_, in_stream_status, _)) = in_stream_err_details {
                                in_stream_status.as_u16()
                            } else {
                                status.as_u16()
                            };
                        state.usage.record_session_completed_request(
                            session.as_ref(),
                            effective_model,
                            effective_status,
                            attempt,
                            req_start.elapsed(),
                            raw_usage.as_ref(),
                        );
                    }

                    return UpstreamOutcome {
                        status,
                        headers: res_headers,
                        body: UpstreamBody::Buffered(buffered_chunks),
                    };
                }

                // Passthrough mode (--no-buffer): Peek at the initial stream chunk to catch in-stream 503 / 429 errors early
                match stream.next().await {
                    Some(Ok(first_chunk)) => {
                        if status.is_success()
                            && let Some((in_stream_status, err_msg)) =
                                parse_in_stream_error(&first_chunk)
                                    .filter(|(s, _)| is_retriable_status(*s))
                        {
                            if attempt < max_retries {
                                let delay = calculate_backoff(
                                    attempt,
                                    initial_delay,
                                    max_delay,
                                    with_jitter,
                                    None,
                                );

                                warn!(
                                    session_id = %session_id_str,
                                    "{}Upstream returned in-stream retriable error {} ({}) for {} {}. Retrying in {:?} (attempt {}/{})...",
                                    session_prefix,
                                    in_stream_status,
                                    err_msg,
                                    method,
                                    endpoint,
                                    delay,
                                    attempt + 1,
                                    max_retries
                                );

                                if model_from_path.is_some() {
                                    state.usage.record_session_retry(session.as_ref());
                                }
                                tokio::time::sleep(delay).await;
                                attempt += 1;
                                continue;
                            } else {
                                warn!(
                                    session_id = %session_id_str,
                                    "{}Max retries ({}) reached for in-stream error {}. Forwarding stream to client.",
                                    session_prefix, max_retries, in_stream_status
                                );
                            }
                        }

                        // Stream is healthy or retries exhausted: chain first chunk with remainder
                        let full_stream = futures_util::stream::once(async move {
                            Ok::<_, std::io::Error>(first_chunk)
                        })
                        .chain(stream.map(|res| res.map_err(std::io::Error::other)));

                        let passthrough_stream: BoxByteStream =
                            if let Some(req_model) = model_from_path {
                                let tracker = state.usage.clone();
                                let initial_status = status.as_u16();
                                let retries_used = attempt;
                                let extractor = StreamUsageExtractor::new();
                                let inner_stream: BoxByteStream = Box::pin(full_stream);
                                let session_for_stream = session.clone();
                                Box::pin(futures_util::stream::unfold(
                                    (inner_stream, extractor, active_guard, initial_status),
                                    move |(mut s, mut ext, mut guard, mut eff_status)| {
                                        let tracker = tracker.clone();
                                        let req_model = req_model.clone();
                                        let session_for_stream = session_for_stream.clone();
                                        async move {
                                            match s.next().await {
                                                Some(Ok(chunk)) => {
                                                    if (200..300).contains(&eff_status)
                                                        && let Some((in_stream_status, _)) =
                                                            parse_in_stream_error(&chunk)
                                                    {
                                                        eff_status = in_stream_status.as_u16();
                                                    }
                                                    ext.feed_chunk(&chunk);
                                                    Some((Ok(chunk), (s, ext, guard, eff_status)))
                                                }
                                                Some(Err(err)) => {
                                                    if guard.take().is_some() {
                                                        let raw_usage = ext.finish(None);
                                                        let effective_model = ext
                                                            .model_version
                                                            .as_deref()
                                                            .unwrap_or(&req_model);
                                                        tracker.record_session_completed_request(
                                                            session_for_stream.as_ref(),
                                                            effective_model,
                                                            StatusCode::BAD_GATEWAY.as_u16(),
                                                            retries_used,
                                                            req_start.elapsed(),
                                                            raw_usage.as_ref(),
                                                        );
                                                    }
                                                    Some((Err(err), (s, ext, guard, eff_status)))
                                                }
                                                None => {
                                                    if guard.take().is_some() {
                                                        let raw_usage = ext.finish(None);
                                                        let effective_model = ext
                                                            .model_version
                                                            .as_deref()
                                                            .unwrap_or(&req_model);
                                                        tracker.record_session_completed_request(
                                                            session_for_stream.as_ref(),
                                                            effective_model,
                                                            eff_status,
                                                            retries_used,
                                                            req_start.elapsed(),
                                                            raw_usage.as_ref(),
                                                        );
                                                    }
                                                    None
                                                }
                                            }
                                        }
                                    },
                                ))
                            } else {
                                Box::pin(full_stream)
                            };

                        return UpstreamOutcome {
                            status,
                            headers: res_headers,
                            body: UpstreamBody::Passthrough(passthrough_stream),
                        };
                    }
                    Some(Err(err)) => {
                        if is_retriable_error(&err) && attempt < max_retries {
                            let delay = calculate_backoff(
                                attempt,
                                initial_delay,
                                max_delay,
                                with_jitter,
                                None,
                            );

                            warn!(
                                session_id = %session_id_str,
                                "{}Upstream stream error on initial chunk for {} {}: {}. Retrying in {:?} (attempt {}/{})...",
                                session_prefix,
                                method,
                                endpoint,
                                err,
                                delay,
                                attempt + 1,
                                max_retries
                            );

                            if model_from_path.is_some() {
                                state.usage.record_session_retry(session.as_ref());
                            }
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        } else {
                            error!(
                                session_id = %session_id_str,
                                "{}Upstream error reading initial stream chunk for {} {}: {}",
                                session_prefix, method, endpoint, err
                            );
                            drop(active_guard);
                            if let Some(ref req_model) = model_from_path {
                                state.usage.record_session_completed_request(
                                    session.as_ref(),
                                    req_model,
                                    StatusCode::BAD_GATEWAY.as_u16(),
                                    attempt,
                                    req_start.elapsed(),
                                    None,
                                );
                            }
                            return UpstreamOutcome::bad_gateway(format!(
                                "Bad Gateway: upstream stream read failed: {}",
                                err
                            ));
                        }
                    }
                    None => {
                        drop(active_guard);
                        if let Some(ref req_model) = model_from_path {
                            state.usage.record_session_completed_request(
                                session.as_ref(),
                                req_model,
                                status.as_u16(),
                                attempt,
                                req_start.elapsed(),
                                None,
                            );
                        }
                        return UpstreamOutcome {
                            status,
                            headers: res_headers,
                            body: UpstreamBody::Empty,
                        };
                    }
                }
            }
            Err(err) => {
                if is_retriable_error(&err) && attempt < max_retries {
                    let delay =
                        calculate_backoff(attempt, initial_delay, max_delay, with_jitter, None);

                    warn!(
                        session_id = %session_id_str,
                        "{}Upstream network error for {} {}: {}. Retrying in {:?} (attempt {}/{})...",
                        session_prefix,
                        method,
                        endpoint,
                        err,
                        delay,
                        attempt + 1,
                        max_retries
                    );

                    if model_from_path.is_some() {
                        state.usage.record_session_retry(session.as_ref());
                    }
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                } else {
                    error!(
                        session_id = %session_id_str,
                        "{}Upstream error for {} {}: {} (retries exhausted or non-retriable)",
                        session_prefix, method, endpoint, err
                    );
                    drop(active_guard);
                    if let Some(ref req_model) = model_from_path {
                        state.usage.record_session_completed_request(
                            session.as_ref(),
                            req_model,
                            StatusCode::BAD_GATEWAY.as_u16(),
                            attempt,
                            req_start.elapsed(),
                            None,
                        );
                    }
                    return UpstreamOutcome::bad_gateway(format!(
                        "Bad Gateway: upstream request failed: {}",
                        err
                    ));
                }
            }
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    method: Method,
    OriginalUri(original_uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_and_query = original_uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let target_url_str = format!("{}{}", state.upstream_base, path_and_query);
    let target_url = match reqwest::Url::parse(&target_url_str) {
        Ok(url) => url,
        Err(err) => {
            error!("Failed to parse target URL {}: {}", target_url_str, err);
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid target URL: {}", err),
            )
                .into_response();
        }
    };

    let is_sse = is_sse_request(path_and_query, &headers);
    let keep_alive_interval = state.keep_alive_interval;
    let endpoint = format_endpoint(&target_url);

    let mut work_fut = Box::pin(execute_proxy_request(
        state,
        method.clone(),
        target_url,
        headers,
        body,
    ));

    // Wait up to `keep_alive_interval` for the request (including buffering/retries) to finish
    // before committing HTTP 200 OK headers and starting keep-alive heartbeats.
    if let Ok(outcome) = tokio::time::timeout(keep_alive_interval, &mut work_fut).await {
        return outcome.into_fast_response();
    }

    debug!(
        "Request {} {} still in progress after {:?}; committing 200 OK headers and starting keep-alive stream",
        method, endpoint, keep_alive_interval
    );

    let heartbeat_bytes = if is_sse {
        Bytes::from_static(b": keep-alive\n\n")
    } else {
        Bytes::from_static(b"\n")
    };

    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + keep_alive_interval,
        keep_alive_interval,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let initial_phase = KeepAlivePhase::SendInitialHeartbeat { work_fut, interval };

    let keep_alive_stream = futures_util::stream::unfold(initial_phase, move |phase| {
        let heartbeat = heartbeat_bytes.clone();
        let req_method = method.clone();
        let req_endpoint = endpoint.clone();
        async move {
            match phase {
                KeepAlivePhase::SendInitialHeartbeat { work_fut, interval } => Some((
                    Ok::<Bytes, std::io::Error>(heartbeat),
                    KeepAlivePhase::Waiting { work_fut, interval },
                )),
                KeepAlivePhase::Waiting {
                    mut work_fut,
                    mut interval,
                } => {
                    tokio::select! {
                        biased;
                        outcome = &mut work_fut => {
                            let mut payload_stream = outcome.into_late_stream(is_sse).await;
                            let item = payload_stream.next().await?;
                            Some((item, KeepAlivePhase::Flushing { payload_stream }))
                        }
                        _ = interval.tick() => {
                            debug!(
                                "Sending keep-alive heartbeat for in-flight request {} {}",
                                req_method, req_endpoint
                            );
                            Some((
                                Ok(heartbeat),
                                KeepAlivePhase::Waiting { work_fut, interval },
                            ))
                        }
                    }
                }
                KeepAlivePhase::Flushing { mut payload_stream } => {
                    let item = payload_stream.next().await?;
                    Some((item, KeepAlivePhase::Flushing { payload_stream }))
                }
            }
        }
    });

    let mut res_builder = Response::builder().status(StatusCode::OK);
    if is_sse {
        res_builder = res_builder
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .header(axum::http::header::CACHE_CONTROL, "no-cache");
    } else {
        res_builder = res_builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }

    match res_builder.body(Body::from_stream(keep_alive_stream)) {
        Ok(res) => res,
        Err(err) => {
            error!("Failed to build keep-alive proxy response: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build keep-alive proxy response: {}", err),
            )
                .into_response()
        }
    }
}
