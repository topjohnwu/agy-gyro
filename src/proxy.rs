// SPDX-License-Identifier: MIT

use crate::config::{Config, DEFAULT_GATEWAY_MODELS, EXCLUDED_MODELS, MODEL_EFFORT_LEVELS};
use crate::retry::{
    calculate_backoff, is_retriable_error, is_retriable_status, parse_in_stream_error,
    parse_retry_after,
};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, OriginalUri, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
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
/// (falling back to [`DEFAULT_GATEWAY_MODELS`] if no API key is available or the request fails).
pub async fn resolve_gateway_models(client: &reqwest::Client, config: &Config) -> String {
    let Some(api_key) = config
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    else {
        debug!("No GEMINI_API_KEY configured; using fallback DEFAULT_GATEWAY_MODELS");
        return expand_gateway_models(DEFAULT_GATEWAY_MODELS);
    };

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

pub struct ProxyState {
    pub config: Config,
    pub client: reqwest::Client,
    pub upstream_base: String,
}

impl ProxyState {
    pub fn new(config: Config, client: reqwest::Client) -> Self {
        let upstream_base = config.upstream.trim_end_matches('/').to_string();
        Self {
            config,
            client,
            upstream_base,
        }
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

pub fn create_router(state: Arc<ProxyState>) -> Router {
    Router::new()
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
            | "content-encoding"
    )
}

/// Extracts the Bearer token from an HTTP `Authorization: Bearer <token>` header.
pub fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let val = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();
    let rest = val
        .strip_prefix("Bearer ")
        .or_else(|| val.strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() { None } else { Some(token) }
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

    let initial_delay = Duration::from_millis(state.config.initial_delay_ms);
    let max_delay = Duration::from_millis(state.config.max_delay_ms);
    let with_jitter = state.config.is_jitter_enabled();
    let max_retries = state.config.max_retries;

    let effective_api_key = state
        .config
        .api_key
        .as_deref()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| {
            headers
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|k| !k.is_empty())
        })
        .or_else(|| extract_bearer_token(&headers));

    let mut attempt = 0;

    loop {
        debug!(
            "Forwarding request {} {} (attempt {}/{})",
            method, target_url, attempt, max_retries
        );

        let mut req_builder = state.client.request(method.clone(), target_url.clone());

        // Forward headers, stripping hop-by-hop and gateway Authorization header
        for (header_name, header_val) in headers.iter() {
            if !is_hop_by_hop(header_name)
                && header_name != axum::http::header::AUTHORIZATION
                && header_name.as_str() != "x-goog-api-key"
            {
                req_builder = req_builder.header(header_name, header_val);
            }
        }

        if let Some(api_key) = effective_api_key {
            req_builder = req_builder.header("x-goog-api-key", api_key);
        }

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
                            "Upstream returned retriable status {} for {} {}. Retrying in {:?} (attempt {}/{}). Response: {}",
                            status,
                            method,
                            target_url,
                            delay,
                            attempt + 1,
                            max_retries,
                            snippet
                        );

                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    } else {
                        warn!(
                            "Max retries ({}) reached for status {}. Forwarding upstream error to client.",
                            max_retries, status
                        );
                    }
                } else if status.is_success() {
                    if attempt > 0 {
                        info!(
                            "Request {} {} succeeded after {} retries (status {})",
                            method, target_url, attempt, status
                        );
                    } else {
                        debug!(
                            "Request {} {} succeeded (status {})",
                            method, target_url, status
                        );
                    }
                } else {
                    debug!(
                        "Request {} {} returned non-retriable status {}",
                        method, target_url, status
                    );
                }

                // Build client response builder
                let mut client_res_builder = Response::builder().status(status.as_u16());

                for (name, val) in upstream_res.headers() {
                    if !is_hop_by_hop(name) {
                        client_res_builder = client_res_builder.header(name, val);
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
                                if let Some((in_stream_status, err_msg)) =
                                    parse_in_stream_error(&chunk)
                                        .filter(|(status, _)| is_retriable_status(*status))
                                {
                                    in_stream_err_details =
                                        Some((chunk_index, in_stream_status, err_msg));
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

                    if let Some((chunk_idx, in_stream_status, err_msg)) = in_stream_err_details {
                        if attempt < max_retries {
                            let delay = calculate_backoff(
                                attempt,
                                initial_delay,
                                max_delay,
                                with_jitter,
                                None,
                            );

                            warn!(
                                "Upstream returned in-stream retriable error {} ({}) at chunk #{} for {} {}. Retrying in {:?} (attempt {}/{})...",
                                in_stream_status,
                                err_msg,
                                chunk_idx + 1,
                                method,
                                target_url,
                                delay,
                                attempt + 1,
                                max_retries
                            );

                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        } else {
                            warn!(
                                "Max retries ({}) reached for in-stream error {}. Forwarding buffered stream to client.",
                                max_retries, in_stream_status
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
                                "Upstream stream error at chunk #{} for {} {}: {}. Retrying in {:?} (attempt {}/{})...",
                                chunk_idx + 1,
                                method,
                                target_url,
                                err,
                                delay,
                                attempt + 1,
                                max_retries
                            );

                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        } else {
                            error!(
                                "Upstream error reading stream at chunk #{} for {} {}: {}",
                                chunk_idx + 1,
                                method,
                                target_url,
                                err
                            );
                            return (
                                StatusCode::BAD_GATEWAY,
                                format!("Bad Gateway: upstream stream read failed: {}", err),
                            )
                                .into_response();
                        }
                    }

                    // All chunks buffered cleanly (or retries exhausted) - stream buffered chunks to client
                    let body_stream = futures_util::stream::iter(
                        buffered_chunks
                            .into_iter()
                            .map(Ok::<_, std::convert::Infallible>),
                    );
                    let body = Body::from_stream(body_stream);

                    return match client_res_builder.body(body) {
                        Ok(res) => res,
                        Err(err) => {
                            error!("Failed to build proxy response: {}", err);
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Failed to build proxy response: {}", err),
                            )
                                .into_response()
                        }
                    };
                }

                // Passthrough mode (--no-buffer): Peek at the initial stream chunk to catch in-stream 503 / 429 errors early
                match stream.next().await {
                    Some(Ok(first_chunk)) => {
                        if let Some((in_stream_status, err_msg)) =
                            parse_in_stream_error(&first_chunk)
                                .filter(|(status, _)| is_retriable_status(*status))
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
                                    "Upstream returned in-stream retriable error {} ({}) for {} {}. Retrying in {:?} (attempt {}/{})...",
                                    in_stream_status,
                                    err_msg,
                                    method,
                                    target_url,
                                    delay,
                                    attempt + 1,
                                    max_retries
                                );

                                tokio::time::sleep(delay).await;
                                attempt += 1;
                                continue;
                            } else {
                                warn!(
                                    "Max retries ({}) reached for in-stream error {}. Forwarding stream to client.",
                                    max_retries, in_stream_status
                                );
                            }
                        }

                        // Stream is healthy or retries exhausted: chain first chunk with remainder
                        let full_stream = futures_util::stream::once(async move {
                            Ok::<_, reqwest::Error>(first_chunk)
                        })
                        .chain(stream);

                        let body = Body::from_stream(full_stream);

                        return match client_res_builder.body(body) {
                            Ok(res) => res,
                            Err(err) => {
                                error!("Failed to build proxy response: {}", err);
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    format!("Failed to build proxy response: {}", err),
                                )
                                    .into_response()
                            }
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
                                "Upstream stream error on initial chunk for {} {}: {}. Retrying in {:?} (attempt {}/{})...",
                                method,
                                target_url,
                                err,
                                delay,
                                attempt + 1,
                                max_retries
                            );

                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        } else {
                            error!(
                                "Upstream error reading initial stream chunk for {} {}: {}",
                                method, target_url, err
                            );
                            return (
                                StatusCode::BAD_GATEWAY,
                                format!("Bad Gateway: upstream stream read failed: {}", err),
                            )
                                .into_response();
                        }
                    }
                    None => {
                        // Empty response body
                        return match client_res_builder.body(Body::empty()) {
                            Ok(res) => res,
                            Err(err) => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Failed to build proxy response: {}", err),
                            )
                                .into_response(),
                        };
                    }
                }
            }
            Err(err) => {
                if is_retriable_error(&err) && attempt < max_retries {
                    let delay =
                        calculate_backoff(attempt, initial_delay, max_delay, with_jitter, None);

                    warn!(
                        "Upstream network error for {} {}: {}. Retrying in {:?} (attempt {}/{})...",
                        method,
                        target_url,
                        err,
                        delay,
                        attempt + 1,
                        max_retries
                    );

                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                } else {
                    error!(
                        "Upstream error for {} {}: {} (retries exhausted or non-retriable)",
                        method, target_url, err
                    );
                    return (
                        StatusCode::BAD_GATEWAY,
                        format!("Bad Gateway: upstream request failed: {}", err),
                    )
                        .into_response();
                }
            }
        }
    }
}
