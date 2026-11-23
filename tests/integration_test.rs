// SPDX-License-Identifier: MIT

use agy_gyro::config::Config;
use agy_gyro::proxy::{DEFAULT_KEEP_ALIVE_INTERVAL, ProxyState, build_http_client, create_router};
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper function to start a test proxy pointing to the given upstream URL
async fn spawn_test_proxy(upstream_url: String, max_retries: u32) -> String {
    spawn_test_proxy_opts(upstream_url, max_retries, false).await
}

/// Helper function to start a test proxy with configurable buffering
async fn spawn_test_proxy_opts(upstream_url: String, max_retries: u32, buffer: bool) -> String {
    spawn_test_proxy_with_keep_alive(
        upstream_url,
        max_retries,
        buffer,
        DEFAULT_KEEP_ALIVE_INTERVAL,
    )
    .await
}

/// Helper function to start a test proxy with configurable buffering and keep-alive interval
async fn spawn_test_proxy_with_keep_alive(
    upstream_url: String,
    max_retries: u32,
    buffer: bool,
    keep_alive_interval: Duration,
) -> String {
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0, // OS assigns random available port
        upstream: upstream_url,
        max_retries,
        initial_delay_ms: 10, // fast retries in tests
        max_delay_ms: 100,
        no_jitter: true,
        buffer,
        request_timeout_secs: 10,
        api_key: "test-key-123".to_string(),
        ..Config::default()
    };

    let client = build_http_client(&config).unwrap();
    let pricing = agy_gyro::usage::parse_models_dev_pricing(
        r#"{
            "google": {
                "models": {
                    "gemini-2.5-pro": {
                        "cost": {
                            "input": 1.25,
                            "output": 10.0,
                            "cache_read": 0.125,
                            "context_over_200k": {
                                "input": 2.5,
                                "output": 15.0,
                                "cache_read": 0.25
                            }
                        }
                    }
                }
            }
        }"#,
    )
    .unwrap();

    let pricing_url = format!("{}/pricing.json", config.upstream.trim_end_matches('/'));
    let state = Arc::new(
        ProxyState::new(config, client)
            .with_keep_alive_interval(keep_alive_interval)
            .with_pricing(pricing)
            .with_pricing_url(pricing_url),
    );

    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{}", local_addr)
}

#[tokio::test]
async fn test_successful_request_forwarding() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .and(header("x-goog-api-key", "test-key-123"))
        .and(query_param("alt", "json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "candidates": [{
                        "content": { "parts": [{ "text": "Hello world" }] }
                    }]
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:generateContent?alt=json",
            proxy_url
        ))
        .header("x-goog-api-key", "test-key-123")
        .header("content-type", "application/json")
        .body(r#"{"contents":[{"parts":[{"text":"Hi"}]}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "Hello world"
    );
}

#[tokio::test]
async fn test_retry_on_429_resource_exhausted_until_success() {
    let mock_server = MockServer::start().await;

    // First two requests return 429 Too Many Requests
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string("RESOURCE_EXHAUSTED: Rate limit exceeded")
                .insert_header("retry-after", "0"),
        )
        .up_to_n_times(2)
        .expect(2)
        .mount(&mock_server)
        .await;

    // Third request succeeds with 200 OK
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Recovered from 429\"}]}}]}\n\n")
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 5).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            proxy_url
        ))
        .header("x-goog-api-key", "test-key")
        .body(r#"{"contents":[{"parts":[{"text":"Stream"}]}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("Recovered from 429"));
}

#[tokio::test]
async fn test_non_retriable_400_bad_request_passed_through_immediately() {
    let mock_server = MockServer::start().await;

    // Upstream returns 400 Bad Request
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({
                    "error": { "code": 400, "message": "Invalid JSON payload" }
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1) // must only be called ONCE (no retry)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 5).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:generateContent",
            proxy_url
        ))
        .header("content-type", "application/json")
        .body("invalid-json")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], 400);
}

#[tokio::test]
async fn test_exhausted_max_retries_returns_upstream_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string("RESOURCE_EXHAUSTED: Persistent quota error"),
        )
        .expect(3) // 1 initial + 2 retries = 3 attempts total
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 2).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:generateContent",
            proxy_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    let text = response.text().await.unwrap();
    assert!(text.contains("RESOURCE_EXHAUSTED: Persistent quota error"));
}

#[tokio::test]
async fn test_large_request_body_exceeding_default_limit() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Large body received OK"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    // Create a 5MB payload (which exceeds Axum's default 2MB limit)
    let large_payload = vec![b'a'; 5 * 1024 * 1024];

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:generateContent",
            proxy_url
        ))
        .body(large_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert_eq!(text, "Large body received OK");
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_test_env_vars() {
    for &var in agy_gyro::config::KNOWN_ENV_VARS {
        unsafe {
            std::env::remove_var(var);
        }
    }
    for &var in &[
        "AGY_GYRO_API_KEY",
        "AGY_GYRO_MAX_RETRIES",
        "AGY_GYRO_INITIAL_DELAY_MS",
        "AGY_GYRO_MAX_DELAY_MS",
        "AGY_GYRO_NO_JITTER",
        "AGY_GYRO_BUFFER",
        "AGY_GYRO_REQUEST_TIMEOUT_SECS",
        "AGY_GYRO_LOG_FILE",
    ] {
        unsafe {
            std::env::remove_var(var);
        }
    }
}

#[test]
fn test_cli_parse_server_command() {
    use agy_gyro::config::{Cli, Commands};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_test_env_vars();

    let temp_path =
        std::env::temp_dir().join(format!("agy-gyro-server-test-{}.toml", std::process::id()));
    std::fs::write(&temp_path, "port = 9090\napi_key = \"test-key\"\n").unwrap();

    let cli = Cli::try_parse_with_config_from(["agy-gyro", "server"], Some(&temp_path)).unwrap();
    let _ = std::fs::remove_file(&temp_path);

    assert_eq!(cli.command, Commands::Server);
    assert_eq!(cli.config.port, 9090);
    assert_eq!(cli.config.api_key, "test-key");
}

#[test]
fn test_cli_parse_wrapper_default_and_passthrough() {
    use agy_gyro::config::{Cli, Commands};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_test_env_vars();

    let temp_path =
        std::env::temp_dir().join(format!("agy-gyro-wrapper-test-{}.toml", std::process::id()));
    std::fs::write(
        &temp_path,
        "api_key = \"test-key\"\nlog_file = \"/tmp/test.log\"\n",
    )
    .unwrap();

    let cli = Cli::try_parse_with_config_from(
        ["agy-gyro", "--model", "gemini-2.5-pro", "--hub"],
        Some(&temp_path),
    )
    .unwrap();
    let _ = std::fs::remove_file(&temp_path);

    let Commands::Run(agy_args) = cli.command else {
        panic!("expected Run command");
    };
    assert_eq!(cli.config.port, 6470);
    assert_eq!(cli.config.api_key, "test-key");
    assert_eq!(
        cli.config.log_file.as_deref().unwrap().to_str().unwrap(),
        "/tmp/test.log"
    );
    assert_eq!(agy_args, vec!["--model", "gemini-2.5-pro", "--hub"]);
}

#[tokio::test]
async fn test_buffered_mode_recovers_from_midstream_error() {
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use futures_util::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let call_count = Arc::new(AtomicUsize::new(0));
    let count_clone = call_count.clone();

    let app = axum::Router::new().route(
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        post(move || {
            let count = count_clone.clone();
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // First attempt: Chunk 1 valid candidates, Chunk 2 in-stream 503 error
                    let s = stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data("{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Thinking...\"}]}}]}")
                        ),
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data("{\"error\": {\"code\": 503, \"message\": \"Mid-stream overload\", \"status\": \"UNAVAILABLE\"}}")
                        ),
                    ]);
                    Sse::new(s)
                } else {
                    // Second attempt: Clean valid chunks
                    let s = stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data("{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Thinking completed. \"}]}}]}")
                        ),
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data("{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Here is the full answer!\"}]}}]}")
                        ),
                    ]);
                    Sse::new(s)
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_upstream = format!("http://{}", addr);

    // Start proxy with full buffering enabled (buffer = true)
    let proxy_url = spawn_test_proxy_opts(mock_upstream, 5, true).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            proxy_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    // Proxy must have retried after chunk 2 error, giving the complete recovered stream without error
    assert!(!text.contains("Mid-stream overload"));
    assert!(text.contains("Thinking completed."));
    assert!(text.contains("Here is the full answer!"));
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_buffered_mode_recovers_from_midstream_connection_drop() {
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use futures_util::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let call_count = Arc::new(AtomicUsize::new(0));
    let count_clone = call_count.clone();

    let app = axum::Router::new().route(
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        post(move || {
            let count = count_clone.clone();
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // Send 1 chunk then simulate IO stream error / drop
                    let s = stream::iter(vec![
                        Ok(Event::default().data("{\"candidates\": [{\"partial\": true}]}")),
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "simulated stream disconnect",
                        )),
                    ]);
                    Sse::new(s)
                } else {
                    let s = stream::iter(vec![Ok(
                        Event::default().data("{\"candidates\": [{\"clean\": true}]}")
                    )]);
                    Sse::new(s)
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_upstream = format!("http://{}", addr);

    let proxy_url = spawn_test_proxy_opts(mock_upstream, 5, true).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            proxy_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("clean"));
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_default_streaming_mode_passes_through_midstream_error() {
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use futures_util::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let call_count = Arc::new(AtomicUsize::new(0));
    let count_clone = call_count.clone();

    let app = axum::Router::new().route(
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        post(move || {
            let count = count_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                let s = stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(
                        Event::default().data("{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Partial text...\"}]}}]}")
                    ),
                    Ok::<_, std::convert::Infallible>(
                        Event::default().data("{\"error\": {\"code\": 503, \"message\": \"Mid-stream overload\", \"status\": \"UNAVAILABLE\"}}")
                    ),
                ]);
                Sse::new(s)
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_upstream = format!("http://{}", addr);

    // Default mode has full buffering disabled (buffer = false)
    let proxy_url = spawn_test_proxy(mock_upstream, 5).await;
    let client = Client::new();

    let response = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            proxy_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    // In passthrough mode, client receives the partial text followed by the mid-stream error without retry
    assert!(text.contains("Partial text..."));
    assert!(text.contains("Mid-stream overload"));
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_run_wrapper_executes_child_and_propagates_exit_code() {
    use agy_gyro::tui::fetch_usage_snapshot;

    let mock_server = MockServer::start().await;
    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let port = reqwest::Url::parse(&proxy_url)
        .unwrap()
        .port()
        .expect("proxy port");

    let temp_home =
        std::env::temp_dir().join(format!("agy-gyro-wrapper-home-{}", std::process::id()));
    let config_dir = temp_home.join(".gemini").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("agy-gyro.toml"),
        format!(
            "host = \"127.0.0.1\"\nport = {}\napi_key = \"test-gemini-key\"\nagy_path = \"sh\"\n",
            port
        ),
    )
    .unwrap();

    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .env("HOME", &temp_home)
        .env("USERPROFILE", &temp_home)
        .env_remove("AGY_LLM_GATEWAY_MODELS")
        .args([
            "-c",
            r#"
                test -n "$AGY_LLM_GATEWAY_URL" &&
                test "$AGY_LLM_GATEWAY_WIRE_PROTOCOL" = "genai" &&
                case "$AGY_LLM_GATEWAY_API_KEY" in "gyro:"*":$$:"*) ;; *) exit 1 ;; esac &&
                test -z "$AGY_LLM_GATEWAY_MODELS" &&
                test "$AGY_ENABLE_HUB" = "1" &&
                exit 42
            "#,
        ])
        .status()
        .await
        .unwrap();

    let _ = std::fs::remove_dir_all(&temp_home);
    assert_eq!(status.code(), Some(42));

    // Lazy session mechanism: child exited without sending any LLM request, so no stale session is created
    let client = Client::new();
    let snap = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert!(snap.sessions.is_empty());
}

#[tokio::test]
async fn test_resolve_gateway_models_fetches_and_expands_models() {
    use agy_gyro::proxy::{
        GatewayModelsResponse, ProxyState, create_router, resolve_gateway_models,
    };

    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header("x-goog-api-key", "my-gemini-api-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "models": [
                        {
                            "name": "models/gemini-flash-latest",
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-3.1-pro-preview",
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-flash",
                            "displayName": "Gemini 99 Flash",
                            "description": "Fast next-gen Flash model",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-pro",
                            "displayName": "Gemini 99 Pro",
                            "description": "Flagship next-gen reasoning model",
                            "inputTokenLimit": 2097152,
                            "outputTokenLimit": 32768,
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-4-hydrogen",
                            "displayName": "Gemini 4 Hydrogen",
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-4-neon",
                            "displayName": "Gemini 4 Neon",
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-flash-image",
                            "displayName": "Nano Banana 99",
                            "description": "Gemini 99 Flash Image",
                            "inputTokenLimit": 65536,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-flash-lite",
                            "displayName": "Gemini 99 Flash-Lite",
                            "description": "Gemini 99 Flash-Lite",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        }
                    ]
                })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        upstream: mock_server.uri(),
        max_retries: 3,
        initial_delay_ms: 10,
        max_delay_ms: 100,
        no_jitter: true,
        buffer: false,
        request_timeout_secs: 10,
        api_key: "my-gemini-api-key".to_string(),
        ..Config::default()
    };

    let client = Client::new();
    let models = resolve_gateway_models(&client, &config).await;
    let model_ids: Vec<&str> = models.iter().map(|m| m.id()).collect();
    assert_eq!(
        model_ids,
        vec![
            "gemini-4-neon-high",
            "gemini-4-neon-xhigh",
            "gemini-4-neon-medium",
            "gemini-4-neon-low",
            "gemini-4-hydrogen-high",
            "gemini-4-hydrogen-xhigh",
            "gemini-4-hydrogen-medium",
            "gemini-4-hydrogen-low",
            "gemini-99-pro-medium",
            "gemini-99-pro-high",
            "gemini-99-pro-low",
            "gemini-99-flash-high",
            "gemini-99-flash-medium",
            "gemini-99-flash-low",
            "gemini-99-flash-image",
            "gemini-99-flash-lite",
            "gemini-pro-orchestrator",
        ]
    );

    // Verify local `GET /v1beta/models` endpoint serves the curated catalog with real upstream metadata
    let state = Arc::new(ProxyState::new(config, client.clone()));
    state.set_gateway_models(models);
    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let gateway_res: GatewayModelsResponse = client
        .get(format!("http://{}/v1beta/models?pageSize=1000", addr))
        .header("Authorization", "Bearer gyro:1:123:test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let model_names: Vec<&str> = gateway_res.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        model_names,
        vec![
            "models/gemini-4-neon-high",
            "models/gemini-4-neon-xhigh",
            "models/gemini-4-neon-medium",
            "models/gemini-4-neon-low",
            "models/gemini-4-hydrogen-high",
            "models/gemini-4-hydrogen-xhigh",
            "models/gemini-4-hydrogen-medium",
            "models/gemini-4-hydrogen-low",
            "models/gemini-99-pro-medium",
            "models/gemini-99-pro-high",
            "models/gemini-99-pro-low",
            "models/gemini-99-flash-high",
            "models/gemini-99-flash-medium",
            "models/gemini-99-flash-low",
            "models/gemini-99-flash-image",
            "models/gemini-99-flash-lite",
            "models/gemini-pro-orchestrator",
        ]
    );
    assert_eq!(
        gateway_res.models[1].display_name.as_deref(),
        Some("Gemini 4 Neon (XHigh)")
    );
    assert_eq!(
        gateway_res.models[5].display_name.as_deref(),
        Some("Gemini 4 Hydrogen (XHigh)")
    );
    let first_entry = &gateway_res.models[8];
    assert_eq!(
        first_entry.display_name.as_deref(),
        Some("Gemini 99 Pro (Medium)")
    );
    assert_eq!(
        first_entry.description.as_deref(),
        Some("Flagship next-gen reasoning model")
    );
    assert_eq!(first_entry.input_token_limit, Some(2_097_152));
    assert_eq!(first_entry.output_token_limit, Some(32_768));
    assert_eq!(first_entry.thinking, Some(true));

    let image_entry = &gateway_res.models[14];
    assert_eq!(image_entry.name, "models/gemini-99-flash-image");
    assert_eq!(image_entry.display_name.as_deref(), Some("Nano Banana 99"));
    assert_eq!(image_entry.input_token_limit, Some(65_536));
    assert_eq!(image_entry.output_token_limit, Some(65_536));
    assert_eq!(image_entry.thinking, None);

    let flash_lite_entry = &gateway_res.models[15];
    assert_eq!(
        flash_lite_entry.display_name.as_deref(),
        Some("Gemini 99 Flash-Lite")
    );
    assert_eq!(flash_lite_entry.input_token_limit, Some(1_048_576));
    assert_eq!(flash_lite_entry.output_token_limit, Some(65_536));
    assert_eq!(flash_lite_entry.thinking, Some(true));

    let orchestrator_entry = gateway_res.models.last().unwrap();
    assert_eq!(orchestrator_entry.name, "models/gemini-pro-orchestrator");
    assert_eq!(
        orchestrator_entry.display_name.as_deref(),
        Some("Orchestrator")
    );
    assert_eq!(orchestrator_entry.input_token_limit, Some(1_048_576));
    assert_eq!(orchestrator_entry.output_token_limit, Some(65_536));
    assert_eq!(
        orchestrator_entry.supported_generation_methods,
        vec!["generateContent".to_string()]
    );
    assert_eq!(orchestrator_entry.thinking, Some(true));
}

#[tokio::test]
async fn test_orchestrator_fallback_forwards_to_planner_model() {
    use agy_gyro::tui::fetch_usage_snapshot;

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-neon:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Orchestrator fallback OK\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 1000, \"candidatesTokenCount\": 200, \"totalTokenCount\": 1200}}\n\n",
                ),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 1).await;
    let client = Client::new();

    let res = client
        .post(format!(
            "{}/v1beta/models/gemini-pro-orchestrator:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .header("Authorization", "Bearer gyro:11:123:orch-test")
        .header("Content-Type", "application/json")
        .body(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"includeThoughts":true,"thinkingBudget":1024}}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    assert!(
        res.text()
            .await
            .unwrap()
            .contains("Orchestrator fallback OK")
    );

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let forwarded_json: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        forwarded_json["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );

    let snap = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert!(snap.models.contains_key("gemini-4-neon"));
    assert!(!snap.models.contains_key("gemini-pro-orchestrator"));
}

#[tokio::test]
async fn test_effort_suffixed_request_rewrites_upstream_path_and_thinking_level() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-99-pro:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "test-key-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Rewritten response\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 1000, \"candidatesTokenCount\": 500, \"totalTokenCount\": 1500}}\n\n",
                ),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    let url = format!(
        "{}/v1beta/models/gemini-99-pro-high:streamGenerateContent?alt=sse",
        proxy_url
    );
    let res = client
        .post(&url)
        .header("Authorization", "Bearer gyro:1:123:test")
        .header("Content-Type", "application/json")
        .body(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"includeThoughts":true,"thinkingBudget":1024}}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("Rewritten response"));

    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let forwarded_json: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        forwarded_json["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert!(forwarded_json["generationConfig"]["thinkingConfig"]["thinkingBudget"].is_null());
}

#[tokio::test]
async fn test_gateway_bearer_auth_translated_to_x_goog_api_key() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-3.8-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "test-key-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"PONG\"}]}}]}\n\n"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    let res = client
        .post(format!(
            "{}/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .header("Authorization", "Bearer agy-gyro")
        .header("content-type", "application/json")
        .body(r#"{"contents":[{"parts":[{"text":"Reply with PONG"}]}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains("PONG"));

    // Verify Authorization header was stripped before reaching upstream
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(!received[0].headers.contains_key("authorization"));
}

#[test]
fn test_cli_parse_toml_config() {
    use agy_gyro::config::{Cli, Commands, default_config_path};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_test_env_vars();

    let default_path = default_config_path().expect("home dir should resolve");
    assert!(default_path.ends_with(".gemini/config/agy-gyro.toml"));

    let temp_path = std::env::temp_dir().join(format!("agy-gyro-test-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&temp_path);

    // Missing config file and missing API key returns an error for wrapper, app, and server commands
    assert!(Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).is_err());
    assert!(Cli::try_parse_with_config_from(["agy-gyro", "server"], Some(&temp_path)).is_err());
    assert!(Cli::try_parse_with_config_from(["agy-gyro", "app"], Some(&temp_path)).is_err());

    // AGY_GYRO_API_KEY is not recognized; API key must come from TOML or GEMINI_API_KEY
    unsafe {
        std::env::set_var("AGY_GYRO_API_KEY", "ignored-agy-gyro-key");
    }
    assert!(Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).is_err());
    clear_test_env_vars();

    // Create temporary agy-gyro.toml
    std::fs::write(
        &temp_path,
        r#"
host = "192.168.1.10"
port = 7070
api_key = "toml-gemini-key"
agy_path = "/toml/agy"
app_path = "/Applications/CustomAntigravity.app"
log_file = "/tmp/toml.log"
max_retries = 25
initial_delay_ms = 500
max_delay_ms = 30000
no_jitter = true
buffer = true
request_timeout_secs = 120
"#,
    )
    .unwrap();

    // 1. Config loaded from TOML file (wrapper mode)
    let cli = Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    assert_eq!(cli.command, Commands::Run(Vec::new()));
    assert_eq!(cli.config.host, "192.168.1.10");
    assert_eq!(cli.config.port, 7070);
    assert_eq!(
        cli.config.upstream,
        "https://generativelanguage.googleapis.com"
    );
    assert_eq!(cli.config.api_key, "toml-gemini-key");
    assert_eq!(cli.config.agy_path, "/toml/agy");
    assert_eq!(
        cli.config.app_path.as_deref(),
        Some("/Applications/CustomAntigravity.app")
    );
    assert_eq!(
        cli.config.log_file.as_deref().unwrap().to_str().unwrap(),
        "/tmp/toml.log"
    );
    assert_eq!(cli.config.max_retries, 25);
    assert_eq!(cli.config.initial_delay_ms, 500);
    assert_eq!(cli.config.max_delay_ms, 30000);
    assert!(cli.config.no_jitter);
    assert!(cli.config.buffer);
    assert!(cli.config.is_buffer_enabled());
    assert_eq!(cli.config.request_timeout_secs, 120);

    // 2. Config loaded from TOML file in server mode
    let cli_server =
        Cli::try_parse_with_config_from(["agy-gyro", "server"], Some(&temp_path)).unwrap();
    assert_eq!(cli_server.command, Commands::Server);
    assert_eq!(cli_server.config.api_key, "toml-gemini-key");
    assert_eq!(cli_server.config.port, 7070);
    assert_eq!(cli_server.config.max_retries, 25);

    // 2b. App subcommand parsing & passthrough (with and without leading `--`)
    let cli_app = Cli::try_parse_with_config_from(
        ["agy-gyro", "app", "--headless", "--ssh-host=dev"],
        Some(&temp_path),
    )
    .unwrap();
    assert_eq!(
        cli_app.command,
        Commands::App(vec!["--headless".to_string(), "--ssh-host=dev".to_string()])
    );
    assert_eq!(
        cli_app.config.app_path.as_deref(),
        Some("/Applications/CustomAntigravity.app")
    );

    let cli_app_sep =
        Cli::try_parse_with_config_from(["agy-gyro", "app", "--", "--headless"], Some(&temp_path))
            .unwrap();
    assert_eq!(
        cli_app_sep.command,
        Commands::App(vec!["--headless".to_string()])
    );

    // 3. Precedence: Environment variables override TOML for wrapper/network settings,
    // but daemon-only settings and AGY_GYRO_API_KEY are not recognized via environment variables.
    unsafe {
        std::env::set_var("AGY_GYRO_API_KEY", "ignored-agy-gyro-key");
        std::env::set_var("AGY_GYRO_MAX_RETRIES", "42");
        std::env::set_var("AGY_GYRO_HOST", "10.0.0.1");
        std::env::set_var("AGY_GYRO_AGY_PATH", "/custom/agy");
        std::env::set_var("AGY_GYRO_APP_PATH", "/custom/Antigravity.app");
        std::env::set_var("AGY_GYRO_BUFFER", "false");
    }

    let cli_ignored_api_key =
        Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    assert_eq!(cli_ignored_api_key.config.api_key, "toml-gemini-key");

    unsafe {
        std::env::set_var("GEMINI_API_KEY", "env-gemini-key-123");
    }

    let cli_env = Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    clear_test_env_vars();

    assert_eq!(cli_env.config.host, "10.0.0.1");
    assert_eq!(cli_env.config.agy_path, "/custom/agy");
    assert_eq!(
        cli_env.config.app_path.as_deref(),
        Some("/custom/Antigravity.app")
    );
    assert_eq!(cli_env.config.api_key, "env-gemini-key-123");
    // Daemon-only settings remain as configured in TOML (not overridden by env)
    assert_eq!(cli_env.config.max_retries, 25);
    assert!(cli_env.config.buffer);
    assert!(cli_env.config.is_buffer_enabled());
    // TOML still applies for fields not set via Env
    assert_eq!(cli_env.config.port, 7070);
    assert_eq!(cli_env.config.initial_delay_ms, 500);

    // 4. Unknown TOML keys are ignored; `buffer` defaults to false when omitted
    std::fs::write(
        &temp_path,
        "api_key = \"toml-gemini-key\"\nunknown_option = 123\n",
    )
    .unwrap();
    let cli = Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    assert_eq!(cli.command, Commands::Run(Vec::new()));
    assert_eq!(cli.config.api_key, "toml-gemini-key");
    assert!(!cli.config.buffer);
    assert!(!cli.config.is_buffer_enabled());

    // 5. Invalid TOML syntax returns an error
    std::fs::write(&temp_path, "invalid = [\n").unwrap();
    assert!(Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).is_err());

    let _ = std::fs::remove_file(&temp_path);
}

#[tokio::test]
async fn test_buffered_mode_sends_keep_alive_during_long_generation_and_retries() {
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use futures_util::{StreamExt, stream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let call_count = Arc::new(AtomicUsize::new(0));
    let count_clone = call_count.clone();

    let app = axum::Router::new().route(
        "/v1beta/models/gemini-4-argon:streamGenerateContent",
        post(move || {
            let count = count_clone.clone();
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // Attempt 0: emit partial chunk 1, pause mid-stream (> keep_alive_interval), then fail with 503
                    let s = stream::once(async {
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data(
                                "{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Partial aborted text\"}]}}]}",
                            ),
                        )
                    })
                    .chain(stream::once(async {
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data(
                                "{\"error\": {\"code\": 503, \"message\": \"Mid-stream overload\", \"status\": \"UNAVAILABLE\"}}",
                            ),
                        )
                    }));
                    Sse::new(s.boxed())
                } else {
                    // Attempt 1: pause mid-stream while thinking, then emit clean chunks
                    let s = stream::once(async {
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Ok::<_, std::convert::Infallible>(
                            Event::default().data(
                                "{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Recovered after keep-alive!\"}]}}]}",
                            ),
                        )
                    });
                    Sse::new(s.boxed())
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let proxy_url = spawn_test_proxy_with_keep_alive(
        format!("http://{}", addr),
        3,
        true,
        Duration::from_millis(25),
    )
    .await;

    let req_client = Client::new();
    // Enforce a strict 80ms response header timeout while upstream buffering + retry takes ~130ms
    let response = tokio::time::timeout(
        Duration::from_millis(80),
        req_client
            .post(format!(
                "{}/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse",
                proxy_url
            ))
            .header("Authorization", "Bearer gyro:7:999:keepalive-sess")
            .send(),
    )
    .await
    .expect("response headers must arrive before client header timeout")
    .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );

    let text = response.text().await.unwrap();
    // Must contain multiple SSE keep-alive comment frames emitted while buffering and retrying
    assert!(text.matches(": keep-alive\n\n").count() >= 2);
    // Must NOT contain partial chunks or mid-stream error from aborted attempt 0
    assert!(!text.contains("Partial aborted text"));
    assert!(!text.contains("Mid-stream overload"));
    // Must contain the clean recovered response from attempt 1
    assert!(text.contains("Recovered after keep-alive!"));
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    // Verify keep-alive logs in /logs are attributed to session #7
    let logs = agy_gyro::tui::fetch_logs_snapshot(&req_client, &format!("{}/logs", proxy_url))
        .await
        .unwrap();
    assert!(logs.iter().any(|l| {
        l.session_id.as_deref() == Some("7")
            && l.message
                .contains("[#7 keepalive-sess] Sending keep-alive heartbeat")
    }));
}

#[tokio::test]
async fn test_keep_alive_forwards_in_stream_error_when_retries_exhausted() {
    use agy_gyro::retry::parse_in_stream_error;

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_delay(Duration::from_millis(40))
                .set_body_json(serde_json::json!({
                    "error": {
                        "code": 503,
                        "message": "Persistent backend overload",
                        "status": "UNAVAILABLE"
                    }
                })),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    // Buffered mode (buffer = true) enables keep-alive heartbeats while retrying
    let proxy_url =
        spawn_test_proxy_with_keep_alive(mock_server.uri(), 1, true, Duration::from_millis(25))
            .await;

    let req_client = Client::new();
    let response = req_client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .send()
        .await
        .unwrap();

    // 200 OK headers were already committed due to keep-alive timer
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains(": keep-alive\n\n"));

    let (err_status, err_msg) = parse_in_stream_error(text.as_bytes()).unwrap();
    assert_eq!(err_status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err_msg, "Persistent backend overload");
}

#[tokio::test]
async fn test_buffered_mode_json_keep_alive_on_slow_generate_content() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(60))
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "candidates": [{
                        "content": { "parts": [{ "text": "Slow non-SSE JSON response" }] }
                    }]
                })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url =
        spawn_test_proxy_with_keep_alive(mock_server.uri(), 2, true, Duration::from_millis(20))
            .await;

    let req_client = Client::new();
    let response = req_client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:generateContent",
            proxy_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "Slow non-SSE JSON response"
    );
}

#[tokio::test]
async fn test_passthrough_mode_skips_keep_alive_heartbeat() {
    use agy_gyro::tui::{fetch_logs_snapshot, toggle_remote_buffer};

    let mock_server = MockServer::start().await;

    // 1. Slow SSE response exceeding keep_alive_interval
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(60))
                .insert_header("content-type", "text/event-stream")
                .insert_header("x-upstream-marker", "passthrough-sse")
                .set_body_string(
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Slow passthrough SSE\"}]}}]}\n\n",
                ),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    // 2. Slow non-SSE JSON response exceeding keep_alive_interval
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(60))
                .insert_header("content-type", "application/json")
                .insert_header("x-upstream-marker", "passthrough-json")
                .set_body_json(serde_json::json!({
                    "candidates": [{
                        "content": { "parts": [{ "text": "Slow passthrough JSON" }] }
                    }]
                })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // 3. Slow 503 error response exceeding keep_alive_interval across retries
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-neon:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_delay(Duration::from_millis(40))
                .set_body_json(serde_json::json!({
                    "error": {
                        "code": 503,
                        "message": "Persistent backend overload",
                        "status": "UNAVAILABLE"
                    }
                })),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    let proxy_url =
        spawn_test_proxy_with_keep_alive(mock_server.uri(), 1, false, Duration::from_millis(20))
            .await;

    let client = Client::new();

    // 1. Slow SSE in passthrough mode (buffer = false): no keep-alive frame, original headers preserved
    let sse_res = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .header("Authorization", "Bearer gyro:8:999:nokeepalive-sess")
        .send()
        .await
        .unwrap();
    assert_eq!(sse_res.status(), 200);
    assert_eq!(
        sse_res
            .headers()
            .get("x-upstream-marker")
            .and_then(|v| v.to_str().ok()),
        Some("passthrough-sse")
    );
    let sse_text = sse_res.text().await.unwrap();
    assert!(!sse_text.contains(": keep-alive"));
    assert!(sse_text.contains("Slow passthrough SSE"));

    // 2. Slow JSON in passthrough mode (buffer = false): no leading newline keep-alive byte
    let json_res = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:generateContent",
            proxy_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(json_res.status(), 200);
    assert_eq!(
        json_res
            .headers()
            .get("x-upstream-marker")
            .and_then(|v| v.to_str().ok()),
        Some("passthrough-json")
    );
    let json_bytes = json_res.bytes().await.unwrap();
    assert!(!json_bytes.starts_with(b"\n"));
    let json_val: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
    assert_eq!(
        json_val["candidates"][0]["content"]["parts"][0]["text"],
        "Slow passthrough JSON"
    );

    // 3. Exhausted retries in passthrough mode return actual upstream 503 without committing 200 OK
    let err_res = client
        .post(format!(
            "{}/v1beta/models/gemini-4-neon:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(err_res.status(), 503);
    let err_text = err_res.text().await.unwrap();
    assert!(!err_text.contains(": keep-alive"));
    assert!(err_text.contains("Persistent backend overload"));

    // Verify no keep-alive heartbeats were logged while buffering was disabled
    let logs = fetch_logs_snapshot(&client, &format!("{}/logs", proxy_url))
        .await
        .unwrap();
    assert!(!logs.iter().any(|l| {
        l.session_id.as_deref() == Some("8")
            && (l.message.contains("Sending keep-alive heartbeat")
                || l.message.contains("starting keep-alive stream"))
    }));

    // 4. Enabling buffering at runtime via POST /buffer re-enables keep-alive heartbeats
    let toggled_on = toggle_remote_buffer(&client, &format!("{}/buffer", proxy_url), Some(true))
        .await
        .unwrap();
    assert!(toggled_on.buffer_enabled);

    let buffered_sse_res = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .header("Authorization", "Bearer gyro:8:999:nokeepalive-sess")
        .send()
        .await
        .unwrap();
    assert_eq!(buffered_sse_res.status(), 200);
    let buffered_sse_text = buffered_sse_res.text().await.unwrap();
    assert!(buffered_sse_text.contains(": keep-alive\n\n"));
    assert!(buffered_sse_text.contains("Slow passthrough SSE"));
}

#[tokio::test]
async fn test_gzip_compressed_error_response_decompressed_cleanly() {
    let mock_server = MockServer::start().await;

    // Pre-compressed gzip payload for: {"error":{"code":400,"message":"Invalid argument"}}
    const GZIP_ERROR_BODY: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xab, 0x56, 0x4a, 0x2d, 0x2a,
        0xca, 0x2f, 0x52, 0xb2, 0xaa, 0x56, 0x4a, 0xce, 0x4f, 0x49, 0x55, 0xb2, 0x32, 0x31, 0x30,
        0xd0, 0x51, 0xca, 0x4d, 0x2d, 0x2e, 0x4e, 0x4c, 0x07, 0xf2, 0x94, 0x3c, 0xf3, 0xca, 0x12,
        0x73, 0x32, 0x53, 0x14, 0x12, 0x8b, 0xd2, 0x4b, 0x73, 0x53, 0xf3, 0x4a, 0x94, 0x6a, 0x6b,
        0x01, 0x49, 0x2d, 0xbc, 0x73, 0x33, 0x00, 0x00, 0x00,
    ];

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("content-type", "application/json")
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(GZIP_ERROR_BODY),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;

    // Use a client with gzip disabled to verify the proxy itself delivers uncompressed UTF-8
    let raw_client = Client::builder().no_gzip().build().unwrap();
    let response = raw_client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:generateContent",
            proxy_url
        ))
        .header("accept-encoding", "gzip")
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let raw_bytes = response.bytes().await.unwrap();
    let utf8_str = std::str::from_utf8(&raw_bytes).expect("body must be valid decompressed UTF-8");
    let json: serde_json::Value = serde_json::from_str(utf8_str).unwrap();
    assert_eq!(json["error"]["code"], 400);
    assert_eq!(json["error"]["message"], "Invalid argument");
}

#[tokio::test]
async fn test_live_usage_endpoint_tracks_tokens_retries_and_cost() {
    use agy_gyro::tui::{fetch_logs_snapshot, fetch_usage_snapshot, reset_remote_usage};

    let mock_server = MockServer::start().await;

    // 1. First request to gemini-4-argon: fails once with 429, then succeeds with SSE stream containing usageMetadata
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string("RESOURCE_EXHAUSTED")
                .insert_header("retry-after", "0"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(concat!(
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Part 1\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 100000, \"cachedContentTokenCount\": 80000}, \"modelVersion\": \"gemini-4-argon\"}\n\n",
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Part 2\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 100000, \"cachedContentTokenCount\": 80000, \"candidatesTokenCount\": 2000, \"thoughtsTokenCount\": 3000, \"totalTokenCount\": 105000}, \"modelVersion\": \"gemini-4-argon\"}\n\n"
                )),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // 2. Second request to gemini-2.5-pro: non-streaming generateContent
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "candidates": [{
                        "content": { "parts": [{ "text": "Done" }] }
                    }],
                    "usageMetadata": {
                        "promptTokenCount": 50000,
                        "cachedContentTokenCount": 10000,
                        "candidatesTokenCount": 1000,
                        "thoughtsTokenCount": 0,
                        "totalTokenCount": 51000
                    },
                    "modelVersion": "gemini-2.5-pro"
                })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    // Execute both requests through the proxy
    let r1 = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    let _ = r1.text().await.unwrap();

    let r2 = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:generateContent",
            proxy_url
        ))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    let _ = r2.text().await.unwrap();

    // Query live /usage endpoint
    let usage_url = format!("{}/usage", proxy_url);
    let snap = fetch_usage_snapshot(&client, &usage_url).await.unwrap();

    assert_eq!(snap.total_requests, 2);
    assert_eq!(snap.successful_requests, 2);
    assert_eq!(snap.failed_requests, 0);
    assert_eq!(snap.total_retries, 1);
    assert_eq!(snap.active_requests, 0);

    // Verify aggregated token counts:
    // Request 1: input=100k (20k uncached, 80k cached), output=5k (2k cand + 3k thoughts), total=105k
    // Request 2: input=50k (40k uncached, 10k cached), output=1k (1k cand + 0 thoughts), total=51k
    assert_eq!(snap.totals.input_tokens, 150_000);
    assert_eq!(snap.totals.uncached_input_tokens, 60_000);
    assert_eq!(snap.totals.cached_input_tokens, 90_000);
    assert_eq!(snap.totals.output_tokens, 6_000);
    assert_eq!(snap.totals.candidates_tokens, 3_000);
    assert_eq!(snap.totals.thoughts_tokens, 3_000);
    assert_eq!(snap.totals.total_tokens, 156_000);

    // Verify gemini-4-argon (unknown upstream pricing) has zero cost and no pricing entry
    let argon = &snap.models["gemini-4-argon"];
    assert_eq!(argon.totals.requests, 1);
    assert_eq!(argon.totals.retries, 1);
    assert!(argon.pricing.is_none());
    assert_eq!(argon.totals.uncached_input_cost_usd, 0.0);
    assert_eq!(argon.totals.cached_input_cost_usd, 0.0);
    assert_eq!(argon.totals.output_cost_usd, 0.0);
    assert_eq!(argon.totals.total_cost_usd, 0.0);

    // Verify per-model cost for gemini-2.5-pro ($1.25/1M in, $0.125/1M cached, $10/1M out):
    // uncached: 40k * $1.25/1M = $0.05; cached: 10k * $0.125/1M = $0.00125; output: 1k * $10/1M = $0.01 -> total = $0.06125
    let pro = &snap.models["gemini-2.5-pro"];
    assert_eq!(pro.totals.requests, 1);
    assert!(pro.pricing.is_some());
    assert!((pro.totals.total_cost_usd - 0.06125).abs() < 1e-9);

    // Verify total cost across both models: $0.0 + $0.06125 = $0.06125
    assert!((snap.totals.total_cost_usd - 0.06125).abs() < 1e-9);

    // Verify real-time tracing logs (including 429 retry warning and completed request logs) are streamed on /logs
    let logs_url = format!("{}/logs", proxy_url);
    let logs = fetch_logs_snapshot(&client, &logs_url).await.unwrap();
    assert!(!logs.is_empty());
    assert!(
        logs.iter()
            .any(|l| l.level == "WARN" && l.message.contains("429"))
    );
    assert!(logs.iter().any(|l| l.level == "INFO"
        && l.message.contains("gemini-4-argon")
        && !l.message.contains("cost=")));
    assert!(logs.iter().any(|l| l.level == "INFO"
        && l.message.contains("gemini-2.5-pro")
        && l.message.contains("cost=$0.061250")));

    // Verify reset endpoint clears counters and logs
    let reset_snap = reset_remote_usage(&client, &usage_url).await.unwrap();
    assert_eq!(reset_snap.total_requests, 0);
    assert_eq!(reset_snap.totals.total_tokens, 0);
    assert_eq!(reset_snap.models.len(), 0);

    let reset_logs = fetch_logs_snapshot(&client, &logs_url).await.unwrap();
    assert!(
        !reset_logs
            .iter()
            .any(|l| l.message.contains("cost=$0.061250"))
    );
}

#[tokio::test]
async fn test_default_passthrough_mode_records_usage_on_stream_completion() {
    use agy_gyro::tui::fetch_usage_snapshot;

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-3.8-flash:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Fast\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 20000, \"cachedContentTokenCount\": 5000, \"candidatesTokenCount\": 500, \"thoughtsTokenCount\": 500, \"totalTokenCount\": 21000}, \"modelVersion\": \"gemini-3.8-flash\"}\n\n",
                ),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    let res = client
        .post(format!(
            "{}/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let snap = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert_eq!(snap.totals.input_tokens, 20_000);
    assert_eq!(snap.totals.uncached_input_tokens, 15_000);
    assert_eq!(snap.totals.cached_input_tokens, 5_000);
    assert_eq!(snap.totals.output_tokens, 1_000);
    assert_eq!(snap.totals.total_cost_usd, 0.0);
}

#[test]
fn test_cli_parse_monitor_subcommand_without_api_key() {
    use agy_gyro::config::{Cli, Commands};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_test_env_vars();

    let cli_default = Cli::try_parse_with_config_from(["agy-gyro", "monitor"], None).unwrap();
    assert_eq!(cli_default.command, Commands::Monitor);
    assert_eq!(cli_default.config.port, 6470);
    assert_eq!(
        cli_default.config.usage_url(),
        "http://127.0.0.1:6470/usage"
    );

    let temp_path =
        std::env::temp_dir().join(format!("agy-gyro-monitor-test-{}.toml", std::process::id()));
    std::fs::write(&temp_path, "port = 9090\n").unwrap();

    let cli = Cli::try_parse_with_config_from(["agy-gyro", "monitor"], Some(&temp_path)).unwrap();
    assert_eq!(cli.command, Commands::Monitor);
    assert_eq!(cli.config.port, 9090);
    assert_eq!(cli.config.usage_url(), "http://127.0.0.1:9090/usage");

    let cli_stop = Cli::try_parse_with_config_from(["agy-gyro", "stop"], Some(&temp_path)).unwrap();
    let _ = std::fs::remove_file(&temp_path);

    assert_eq!(cli_stop.command, Commands::Stop);
    assert_eq!(cli_stop.config.port, 9090);
    assert_eq!(cli_stop.config.base_url(), "http://127.0.0.1:9090");
}

#[tokio::test]
async fn test_multi_session_registration_attribution_and_exit() {
    use agy_gyro::tui::{fetch_usage_snapshot, reset_remote_session_usage};
    use agy_gyro::usage::{format_session_api_key, generate_session_uuid};

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "candidates": [{ "content": { "parts": [{ "text": "OK" }] } }],
                    "usageMetadata": {
                        "promptTokenCount": 50000,
                        "cachedContentTokenCount": 25000,
                        "candidatesTokenCount": 1000,
                        "thoughtsTokenCount": 1000,
                        "totalTokenCount": 52000
                    },
                    "modelVersion": "gemini-4-argon"
                })),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let client = Client::new();

    // Spawn a short-lived OS process to represent Session 2's `agy` instance
    let mut dummy_proc = tokio::process::Command::new("sh")
        .args(["-c", "sleep 10"])
        .spawn()
        .unwrap();
    let session2_pid = dummy_proc.id().expect("dummy process pid");

    // Mint lazy session tokens locally (Session 1 = current test PID, Session 2 = dummy PID)
    let session1_id = generate_session_uuid();
    let session2_id = generate_session_uuid();
    assert_eq!(session1_id.len(), 36);
    assert_eq!(session2_id.len(), 36);
    assert_ne!(session1_id, session2_id);

    let api_key1 = format_session_api_key(&session1_id, Some(std::process::id()), "workspace-one");
    let api_key2 = format_session_api_key(&session2_id, Some(session2_pid), "workspace-two");
    assert_eq!(
        api_key2,
        format!("gyro:{}:{}:workspace-two", session2_id, session2_pid)
    );

    let usage_url = format!("{}/usage", proxy_url);
    let snap_before = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert!(snap_before.sessions.is_empty());

    // Send 1 request for each session (lazily creates both sessions with OS PID tracking)
    for api_key in [&api_key1, &api_key2] {
        let res = client
            .post(format!(
                "{}/v1beta/models/gemini-4-argon:generateContent",
                proxy_url
            ))
            .header("Authorization", format!("Bearer {}", api_key))
            .body(r#"{"contents":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // While both sessions are active, Session 2 (most recently used) appears first
    let snap_active = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert_eq!(snap_active.sessions.len(), 2);
    assert_eq!(snap_active.sessions[0].id, session2_id);
    assert!(snap_active.sessions[0].active);
    assert_eq!(snap_active.sessions[1].id, session1_id);
    assert!(snap_active.sessions[1].active);

    // Terminate Session 2's OS process and reap it so OS-level process monitoring sees it exited
    dummy_proc.kill().await.unwrap();
    let _ = dummy_proc.wait().await;

    let snap = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert_eq!(snap.total_requests, 2);
    assert_eq!(snap.totals.input_tokens, 100_000);
    assert_eq!(snap.sessions.len(), 2);
    assert_eq!(snap.sessions[0].id, session1_id);
    assert_eq!(snap.sessions[0].label, "workspace-one");
    assert!(snap.sessions[0].active);
    assert_eq!(snap.sessions[0].totals.input_tokens, 50_000);

    assert_eq!(snap.sessions[1].id, session2_id);
    assert_eq!(snap.sessions[1].label, "workspace-two");
    assert!(!snap.sessions[1].active);
    assert_eq!(snap.sessions[1].totals.input_tokens, 50_000);

    // Selectively resetting exited session 2 removes it and subtracts its tokens from global totals
    let snap_after = reset_remote_session_usage(&client, &usage_url, Some(&session2_id))
        .await
        .unwrap();
    assert_eq!(snap_after.sessions.len(), 1);
    assert_eq!(snap_after.sessions[0].id, session1_id);
    assert_eq!(snap_after.total_requests, 1);
    assert_eq!(snap_after.totals.input_tokens, 50_000);

    // Selectively resetting active session 1 zeroes its counters while keeping the active tab
    let snap_zeroed = reset_remote_session_usage(&client, &usage_url, Some(&session1_id))
        .await
        .unwrap();
    assert_eq!(snap_zeroed.sessions.len(), 1);
    assert!(snap_zeroed.sessions[0].active);
    assert_eq!(snap_zeroed.total_requests, 0);

    // POST /shutdown triggers daemon shutdown signal
    let shutdown_res: agy_gyro::proxy::ShutdownResponse = client
        .post(format!("{}/shutdown", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shutdown_res.status, "stopping");
    assert_eq!(shutdown_res.pid, std::process::id());
}

#[tokio::test]
async fn test_server_and_stop_subcommands_spawn_and_stop_background_daemon() {
    let free_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let temp_home =
        std::env::temp_dir().join(format!("agy-gyro-daemon-home-{}", std::process::id()));
    let config_dir = temp_home.join(".gemini").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("agy-gyro.toml"),
        format!(
            "host = \"127.0.0.1\"\nport = {}\napi_key = \"test-daemon-key\"\n",
            free_port
        ),
    )
    .unwrap();

    // 1. `agy-gyro server` spawns background daemon and returns immediately without blocking
    let out1 = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .arg("server")
        .env("HOME", &temp_home)
        .env("USERPROFILE", &temp_home)
        .output()
        .await
        .unwrap();
    assert!(out1.status.success());
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(stdout1.contains("Started agy-gyro daemon"));

    // Verify daemon exposes the embedded deterministic build ID on GET /sessions
    assert_eq!(agy_gyro::BUILD_ID.len(), 16);
    assert!(agy_gyro::BUILD_ID.chars().all(|c| c.is_ascii_hexdigit()));
    let status: agy_gyro::proxy::DaemonStatusResponse = Client::new()
        .get(format!("http://127.0.0.1:{}/sessions", free_port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status.build_id.as_deref(), Some(agy_gyro::BUILD_ID));

    // 2. Second `agy-gyro server` on the same port detects existing daemon and returns immediately
    let out2 = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .arg("server")
        .env("HOME", &temp_home)
        .env("USERPROFILE", &temp_home)
        .output()
        .await
        .unwrap();
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("already running"));

    // 3. `agy-gyro stop` stops the background daemon and waits for port release
    let out_stop = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .arg("stop")
        .env("HOME", &temp_home)
        .env("USERPROFILE", &temp_home)
        .output()
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&temp_home);
    assert!(out_stop.status.success());
    let stdout_stop = String::from_utf8_lossy(&out_stop.stdout);
    assert!(stdout_stop.contains("Stopped agy-gyro daemon"));
}

#[tokio::test]
async fn test_new_session_refetches_model_catalog_and_pricing() {
    use agy_gyro::proxy::{DaemonStatusResponse, GatewayModelsResponse};
    use agy_gyro::tui::fetch_usage_snapshot;
    use agy_gyro::usage::{format_session_api_key, generate_session_uuid};

    let mock_server = MockServer::start().await;

    // Fetch #1 (startup): serves gemini-4-argon and initial pricing ($2/1M in, $10/1M out)
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header("x-goog-api-key", "test-key-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "models": [
                        {
                            "name": "models/gemini-4-argon",
                            "displayName": "Gemini 4 Argon",
                            "supportedGenerationMethods": ["generateContent"],
                            "thinking": true
                        }
                    ]
                })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pricing.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "google": {
                        "models": {
                            "gemini-4-argon": {
                                "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.2 }
                            }
                        }
                    }
                })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock_server)
        .await;

    // Fetch #2 (new session #1): serves gemini-4-neon + updated pricing ($4/1M in, $20/1M out)
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header("x-goog-api-key", "test-key-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "models": [
                        {
                            "name": "models/gemini-4-neon",
                            "displayName": "Gemini 4 Neon",
                            "supportedGenerationMethods": ["generateContent"],
                            "thinking": true
                        }
                    ]
                })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pricing.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "google": {
                        "models": {
                            "gemini-4-argon": {
                                "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 }
                            },
                            "gemini-4-neon": {
                                "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.4 }
                            }
                        }
                    }
                })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock_server)
        .await;

    // Fetch #3 (new session #2): upstream returns 503 -> must preserve last good catalog & pricing
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header("x-goog-api-key", "test-key-123"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pricing.json"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "candidates": [{ "content": { "parts": [{ "text": "Argon OK" }] } }],
                    "usageMetadata": {
                        "promptTokenCount": 500_000,
                        "cachedContentTokenCount": 0,
                        "candidatesTokenCount": 0,
                        "thoughtsTokenCount": 0,
                        "totalTokenCount": 500_000
                    },
                    "modelVersion": "gemini-4-argon"
                })),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-neon:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "candidates": [{ "content": { "parts": [{ "text": "Neon OK" }] } }],
                    "usageMetadata": {
                        "promptTokenCount": 500_000,
                        "cachedContentTokenCount": 0,
                        "candidatesTokenCount": 50_000,
                        "thoughtsTokenCount": 0,
                        "totalTokenCount": 550_000
                    },
                    "modelVersion": "gemini-4-neon"
                })),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        upstream: mock_server.uri(),
        max_retries: 1,
        initial_delay_ms: 10,
        max_delay_ms: 100,
        no_jitter: true,
        buffer: false,
        request_timeout_secs: 10,
        api_key: "test-key-123".to_string(),
        ..Config::default()
    };
    let client = build_http_client(&config).unwrap();
    let state = Arc::new(
        ProxyState::new(config, client.clone())
            .with_pricing_url(format!("{}/pricing.json", mock_server.uri())),
    );
    state.refresh_catalog_and_pricing().await;

    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Initially, /v1beta/models serves Fetch #1 (gemini-4-argon) and pricing is $2/1M input
    let initial_models: GatewayModelsResponse = client
        .get(format!("{}/v1beta/models", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let initial_ids: Vec<&str> = initial_models.models.iter().map(|m| m.id()).collect();
    assert_eq!(
        initial_ids,
        vec![
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-argon-low",
        ]
    );

    let initial_argon = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:generateContent",
            proxy_url
        ))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(initial_argon.status(), 200);
    let snap0 = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert_eq!(
        snap0.models["gemini-4-argon"]
            .pricing
            .as_ref()
            .unwrap()
            .input_per_1m,
        2.0
    );
    assert!((snap0.totals.total_cost_usd - 1.0).abs() < 1e-9);

    // Trigger refresh #2 via GET /sessions?refresh=1 (gemini-4-neon + updated pricing)
    let status1: DaemonStatusResponse = client
        .get(format!("{}/sessions?refresh=1", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        status1
            .gateway_models
            .contains(&"gemini-4-neon-high".to_string())
    );

    // Plain GET /sessions without refresh=1 must NOT trigger another refetch
    let status1_plain: DaemonStatusResponse = client
        .get(format!("{}/sessions", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status1_plain.gateway_models, status1.gateway_models);

    let session1_id = generate_session_uuid();
    let api_key1 = format_session_api_key(&session1_id, Some(std::process::id()), "session-one");

    let refreshed_models: GatewayModelsResponse = client
        .get(format!("{}/v1beta/models", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let refreshed_ids: Vec<&str> = refreshed_models.models.iter().map(|m| m.id()).collect();
    assert_eq!(
        refreshed_ids,
        vec![
            "gemini-4-neon-high",
            "gemini-4-neon-xhigh",
            "gemini-4-neon-medium",
            "gemini-4-neon-low",
        ]
    );

    // Existing model (gemini-4-argon) picks up refreshed pricing ($3/1M in -> +$1.50) and updates stored metadata
    let refreshed_argon = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:generateContent",
            proxy_url
        ))
        .header("Authorization", format!("Bearer {}", api_key1))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(refreshed_argon.status(), 200);

    // Generation request on session #1 uses newly refetched pricing:
    // 500k input * $4/1M = $2.00 + 50k output * $20/1M = $1.00 -> +$3.00 (cumulative = $1.00 + $1.50 + $3.00 = $5.50)
    let gen1 = client
        .post(format!(
            "{}/v1beta/models/gemini-4-neon-xhigh:generateContent",
            proxy_url
        ))
        .header("Authorization", format!("Bearer {}", api_key1))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(gen1.status(), 200);

    let snap1 = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert_eq!(snap1.pricing_source, "models.dev (live)");
    assert_eq!(
        snap1.models["gemini-4-argon"]
            .pricing
            .as_ref()
            .unwrap()
            .input_per_1m,
        3.0
    );
    assert!((snap1.totals.total_cost_usd - 5.5).abs() < 1e-9);

    // Trigger refresh #3 via GET /sessions?refresh=1 -> Fetch #3 returns 503, so last known catalog & pricing must be preserved
    let status2: DaemonStatusResponse = client
        .get(format!("{}/sessions?refresh=1", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status2.gateway_models, status1.gateway_models);

    let session2_id = generate_session_uuid();
    let api_key2 = format_session_api_key(&session2_id, None, "session-two");
    assert_ne!(session2_id, session1_id);

    let preserved_models: GatewayModelsResponse = client
        .get(format!("{}/v1beta/models", proxy_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let preserved_ids: Vec<&str> = preserved_models.models.iter().map(|m| m.id()).collect();
    assert_eq!(preserved_ids, refreshed_ids);

    let gen2 = client
        .post(format!(
            "{}/v1beta/models/gemini-4-neon-high:generateContent",
            proxy_url
        ))
        .header("Authorization", format!("Bearer {}", api_key2))
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(gen2.status(), 200);

    let snap2 = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert!((snap2.totals.total_cost_usd - 8.5).abs() < 1e-9);
}

#[tokio::test]
async fn test_runtime_buffer_toggle_switches_buffering_in_real_time() {
    use agy_gyro::tui::{fetch_logs_snapshot, fetch_usage_snapshot, toggle_remote_buffer};
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use futures_util::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let call_count = Arc::new(AtomicUsize::new(0));
    let count_clone = call_count.clone();

    // Attempts 0, 1, 3 emit chunk 1 + mid-stream 503; attempt 2 emits a clean recovered stream
    let app = axum::Router::new().route(
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        post(move || {
            let count = count_clone.clone();
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 2 {
                    let s = stream::iter(vec![Ok::<_, std::convert::Infallible>(
                        Event::default().data(
                            "{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Recovered cleanly\"}]}}]}",
                        ),
                    )]);
                    Sse::new(s)
                } else {
                    let s = stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(Event::default().data(
                            "{\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Partial...\"}]}}]}",
                        )),
                        Ok::<_, std::convert::Infallible>(Event::default().data(
                            "{\"error\": {\"code\": 503, \"message\": \"Mid-stream overload\", \"status\": \"UNAVAILABLE\"}}",
                        )),
                    ]);
                    Sse::new(s)
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Default mode: full buffering disabled (buffer = false)
    let proxy_url = spawn_test_proxy(upstream_url, 3).await;
    let client = Client::new();
    let usage_url = format!("{}/usage", proxy_url);
    let buffer_url = format!("{}/buffer", proxy_url);
    let logs_url = format!("{}/logs", proxy_url);

    let initial_snap = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert!(!initial_snap.buffer_enabled);

    // 1. With buffering OFF, mid-stream 503 passes through without retry (attempt 0)
    let res1 = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res1.status(), 200);
    let text1 = res1.text().await.unwrap();
    assert!(text1.contains("Partial..."));
    assert!(text1.contains("Mid-stream overload"));
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    // 2. Toggle buffering ON at runtime via POST /buffer (simulating pressing `b` in monitor TUI)
    let toggled_on = toggle_remote_buffer(&client, &buffer_url, None)
        .await
        .unwrap();
    assert!(toggled_on.buffer_enabled);
    let snap_on = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert!(snap_on.buffer_enabled);

    // With buffering ON, mid-stream 503 (attempt 1) is discarded and retried cleanly (attempt 2)
    let res2 = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res2.status(), 200);
    let text2 = res2.text().await.unwrap();
    assert!(!text2.contains("Mid-stream overload"));
    assert!(text2.contains("Recovered cleanly"));
    assert_eq!(call_count.load(Ordering::SeqCst), 3);

    // 3. Explicitly disable buffering at runtime via POST /buffer {"enabled": false}
    let toggled_off = toggle_remote_buffer(&client, &buffer_url, Some(false))
        .await
        .unwrap();
    assert!(!toggled_off.buffer_enabled);
    let snap_off = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert!(!snap_off.buffer_enabled);

    // With buffering OFF again, mid-stream 503 passes through without retry (attempt 3)
    let res3 = client
        .post(format!(
            "{}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            proxy_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res3.status(), 200);
    let text3 = res3.text().await.unwrap();
    assert!(text3.contains("Mid-stream overload"));
    assert_eq!(call_count.load(Ordering::SeqCst), 4);

    // Verify runtime toggle events are recorded in gateway logs
    let logs = fetch_logs_snapshot(&client, &logs_url).await.unwrap();
    assert!(
        logs.iter()
            .any(|l| l.message.contains("Full SSE buffering enabled"))
    );
    assert!(
        logs.iter()
            .any(|l| l.message.contains("Full SSE buffering disabled"))
    );
}

#[tokio::test]
async fn test_logs_sse_streams_incremental_appends_and_resyncs_on_reset() {
    use agy_gyro::usage::LogStreamEvent;
    use futures_util::StreamExt;

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        upstream: "http://127.0.0.1:1".to_string(),
        max_retries: 0,
        api_key: "test-key-123".to_string(),
        ..Config::default()
    };
    let http_client = build_http_client(&config).unwrap();
    let state = Arc::new(ProxyState::new(config, http_client));
    let app = create_router(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = Client::new();

    let s1 = agy_gyro::usage::SessionMeta {
        id: agy_gyro::usage::generate_session_uuid(),
        label: "sess-one".to_string(),
        pid: None,
    };
    let s2 = agy_gyro::usage::SessionMeta {
        id: agy_gyro::usage::generate_session_uuid(),
        label: "sess-two".to_string(),
        pid: None,
    };
    let g1 = state.usage.begin_session_request(Some(&s1));
    let g2 = state.usage.begin_session_request(Some(&s2));
    drop(g1);
    drop(g2);
    state
        .usage
        .record_log_with_session("INFO", "agy_gyro", Some(s1.id.clone()), "initial-log-1");
    state
        .usage
        .record_log_with_session("INFO", "agy_gyro", Some(s2.id.clone()), "initial-log-2");

    // Open persistent SSE connection to GET /logs
    let res = client
        .get(format!("{}/logs", proxy_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mut stream = res.bytes_stream();
    let mut buf = Vec::new();

    async fn next_log_event<S, E>(stream: &mut S, buf: &mut Vec<u8>) -> LogStreamEvent
    where
        S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
        E: std::fmt::Debug,
    {
        loop {
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
                buf.drain(..=pos);
                if let Some(json_str) = line.strip_prefix("data:").map(str::trim)
                    && !json_str.is_empty()
                {
                    return serde_json::from_str::<LogStreamEvent>(json_str)
                        .expect("valid LogStreamEvent JSON");
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .expect("timed out waiting for SSE event")
                .expect("stream ended unexpectedly")
                .expect("stream read error");
            buf.extend_from_slice(&chunk);
        }
    }

    // 1. First event must be Snapshot containing the pre-existing logs
    match next_log_event(&mut stream, &mut buf).await {
        LogStreamEvent::Snapshot { entries } => {
            assert!(entries.iter().any(|e| e.message == "initial-log-1"));
            assert!(entries.iter().any(|e| e.message == "initial-log-2"));
        }
        other => panic!("expected initial Snapshot event, got {:?}", other),
    }

    // 2. Recording a new log emits an incremental Append (never re-sending initial logs)
    //    Note: parallel integration tests may emit unrelated global tracing events into this tracker.
    state
        .usage
        .record_log_with_session("WARN", "agy_gyro", Some(s1.id.clone()), "live-append-1");
    loop {
        match next_log_event(&mut stream, &mut buf).await {
            LogStreamEvent::Append { entries } => {
                assert!(
                    !entries
                        .iter()
                        .any(|e| e.message == "initial-log-1" || e.message == "initial-log-2"),
                    "incremental Append must not resend pre-existing logs"
                );
                if let Some(found) = entries.iter().find(|e| e.message == "live-append-1") {
                    assert_eq!(found.session_id.as_deref(), Some(s1.id.as_str()));
                    break;
                }
            }
            other => panic!("expected incremental Append event, got {:?}", other),
        }
    }

    // 3. Per-session reset resyncs via Snapshot without s2's logs
    let reset_s2 = client
        .delete(format!("{}/usage?session={}", proxy_url, s2.id))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_s2.status(), 200);
    loop {
        match next_log_event(&mut stream, &mut buf).await {
            LogStreamEvent::Snapshot { entries } => {
                assert!(entries.iter().any(|e| e.message == "initial-log-1"));
                assert!(entries.iter().any(|e| e.message == "live-append-1"));
                assert!(!entries.iter().any(|e| e.message == "initial-log-2"));
                break;
            }
            LogStreamEvent::Append { .. } => {
                // Ignore any concurrent global tracing appends emitted before the DELETE landed
            }
        }
    }

    // 4. Subsequent log still arrives incrementally as Append
    state
        .usage
        .record_log("INFO", "agy_gyro", "live-append-after-session-reset");
    loop {
        match next_log_event(&mut stream, &mut buf).await {
            LogStreamEvent::Append { entries } => {
                assert!(
                    !entries
                        .iter()
                        .any(|e| e.message == "initial-log-1" || e.message == "live-append-1"),
                    "incremental Append must not resend snapshot entries"
                );
                if entries
                    .iter()
                    .any(|e| e.message == "live-append-after-session-reset")
                {
                    break;
                }
            }
            other => panic!("expected Append after session reset, got {:?}", other),
        }
    }

    // 5. Global reset resyncs via Snapshot clearing all prior logs
    let reset_all = client
        .delete(format!("{}/usage", proxy_url))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_all.status(), 200);
    loop {
        match next_log_event(&mut stream, &mut buf).await {
            LogStreamEvent::Snapshot { entries } => {
                assert!(!entries.iter().any(|e| {
                    e.message == "initial-log-1"
                        || e.message == "live-append-1"
                        || e.message == "live-append-after-session-reset"
                }));
                break;
            }
            LogStreamEvent::Append { .. } => {
                // Ignore any concurrent global tracing appends emitted before the DELETE landed
            }
        }
    }
}

async fn spawn_mock_old_daemon(
    build_id: Option<&'static str>,
) -> (u16, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::get, routing::post};

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let tx_clone = shutdown_tx.clone();

    let app = Router::new()
        .route(
            "/sessions",
            get(move || async move {
                let mut payload = serde_json::json!({
                    "pid": 0,
                    "gateway_models": [],
                });
                if let Some(id) = build_id {
                    payload["build_id"] = serde_json::Value::String(id.to_string());
                }
                Json(payload)
            }),
        )
        .route(
            "/shutdown",
            post(move || {
                let tx = tx_clone.clone();
                async move {
                    let _ = tx.send(true);
                    Json(serde_json::json!({
                        "status": "stopping",
                        "pid": 0,
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.wait_for(|&v| v).await;
            })
            .await;
    });
    (port, handle)
}

#[tokio::test]
async fn test_server_restarts_automatically_on_build_id_mismatch_and_legacy_daemon() {
    for (case_idx, old_build_id) in [
        (
            0,
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        ),
        (1, None),
    ] {
        let (port, old_handle) = spawn_mock_old_daemon(old_build_id).await;

        let temp_home = std::env::temp_dir().join(format!(
            "agy-gyro-build-restart-home-{}-{}",
            std::process::id(),
            case_idx
        ));
        let config_dir = temp_home.join(".gemini").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("agy-gyro.toml"),
            format!(
                "host = \"127.0.0.1\"\nport = {}\napi_key = \"test-daemon-key\"\n",
                port
            ),
        )
        .unwrap();

        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
            .arg("server")
            .env("HOME", &temp_home)
            .env("USERPROFILE", &temp_home)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "case {} failed: stderr={}",
            case_idx,
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("Restarted agy-gyro daemon"),
            "expected restart message, got stdout={}",
            stdout
        );
        assert!(
            stderr.contains("server build id mismatch"),
            "expected mismatch diagnostic, got stderr={}",
            stderr
        );

        // Old server must have exited cleanly
        tokio::time::timeout(Duration::from_secs(2), old_handle)
            .await
            .expect("old mock daemon should have shut down")
            .unwrap();

        // New daemon on the same port must advertise the current executable's BUILD_ID
        let status: agy_gyro::proxy::DaemonStatusResponse = Client::new()
            .get(format!("http://127.0.0.1:{}/sessions", port))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status.build_id.as_deref(), Some(agy_gyro::BUILD_ID));
        assert!(status.pid > 0);

        // Clean up background daemon
        let out_stop = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
            .arg("stop")
            .env("HOME", &temp_home)
            .env("USERPROFILE", &temp_home)
            .output()
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&temp_home);
        assert!(out_stop.status.success());
    }
}

#[tokio::test]
async fn test_wrapper_restarts_mismatched_daemon_before_running_child() {
    let (port, old_handle) = spawn_mock_old_daemon(Some(
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    ))
    .await;

    let temp_home = std::env::temp_dir().join(format!(
        "agy-gyro-wrapper-build-restart-{}",
        std::process::id()
    ));
    let config_dir = temp_home.join(".gemini").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let (agy_path, arg) = if cfg!(windows) {
        ("cmd", "/C echo ok")
    } else {
        ("/bin/echo", "ok")
    };
    std::fs::write(
        config_dir.join("agy-gyro.toml"),
        format!(
            "host = \"127.0.0.1\"\nport = {}\napi_key = \"test-daemon-key\"\nagy_path = \"{}\"\n",
            port, agy_path
        ),
    )
    .unwrap();

    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .arg(arg)
        .env("HOME", &temp_home)
        .env("USERPROFILE", &temp_home)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "wrapper failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("server build id mismatch"));

    tokio::time::timeout(Duration::from_secs(2), old_handle)
        .await
        .expect("old mock daemon should have shut down")
        .unwrap();

    let status: agy_gyro::proxy::DaemonStatusResponse = Client::new()
        .get(format!("http://127.0.0.1:{}/sessions", port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status.build_id.as_deref(), Some(agy_gyro::BUILD_ID));

    // Clean up background daemon
    let out_stop = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .arg("stop")
        .env("HOME", &temp_home)
        .env("USERPROFILE", &temp_home)
        .output()
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&temp_home);
    assert!(out_stop.status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn test_run_app_launches_detached_and_tracks_lazy_desktop_session() {
    use agy_gyro::tui::fetch_usage_snapshot;
    use std::os::unix::fs::PermissionsExt;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-4-argon:generateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(serde_json::json!({
                    "candidates": [{ "content": { "parts": [{ "text": "Desktop OK" }] } }],
                    "usageMetadata": {
                        "promptTokenCount": 12000,
                        "cachedContentTokenCount": 0,
                        "candidatesTokenCount": 500,
                        "thoughtsTokenCount": 0,
                        "totalTokenCount": 12500
                    },
                    "modelVersion": "gemini-4-argon"
                })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let proxy_url = spawn_test_proxy(mock_server.uri(), 3).await;
    let port = reqwest::Url::parse(&proxy_url)
        .unwrap()
        .port()
        .expect("proxy port");

    let temp_dir = std::env::temp_dir().join(format!(
        "agy-gyro-app-test-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let config_dir = temp_dir.join(".gemini").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let env_dump = temp_dir.join("env.txt");
    let mock_app = temp_dir.join("mock-antigravity");
    std::fs::write(
        &mock_app,
        format!(
            "#!/bin/sh\n{{\n  printf 'URL=%s\\n' \"$AGY_LLM_GATEWAY_URL\"\n  printf 'PROTO=%s\\n' \"$AGY_LLM_GATEWAY_WIRE_PROTOCOL\"\n  printf 'KEY=%s\\n' \"$AGY_LLM_GATEWAY_API_KEY\"\n  printf 'HUB=%s\\n' \"$AGY_ENABLE_HUB\"\n  printf 'ARGS=%s\\n' \"$*\"\n}} > \"{}\"\n",
            env_dump.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&mock_app, std::fs::Permissions::from_mode(0o755)).unwrap();

    std::fs::write(
        config_dir.join("agy-gyro.toml"),
        format!(
            "host = \"127.0.0.1\"\nport = {}\napi_key = \"test-gemini-key\"\napp_path = \"{}\"\n",
            port,
            mock_app.display()
        ),
    )
    .unwrap();

    // Even if AGY_ENABLE_HUB=1 is set in the caller's shell (e.g., inside an agy-gyro CLI session),
    // `agy-gyro app` must strip it when launching the desktop app.
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_agy-gyro"))
        .env("HOME", &temp_dir)
        .env("USERPROFILE", &temp_dir)
        .env("AGY_ENABLE_HUB", "1")
        .args(["app", "--headless", "--ssh-host=dev"])
        .status()
        .await
        .unwrap();
    assert!(status.success(), "agy-gyro app failed: {:?}", status);

    // Wait briefly for the detached mock app script to finish writing its env dump
    let mut dump = String::new();
    for _ in 0..50 {
        if let Ok(contents) = std::fs::read_to_string(&env_dump)
            && contents.contains("ARGS=")
        {
            dump = contents;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = std::fs::remove_dir_all(&temp_dir);

    assert!(
        dump.contains(&format!("URL=http://127.0.0.1:{}", port)),
        "unexpected dump:\n{}",
        dump
    );
    assert!(dump.contains("PROTO=genai"), "unexpected dump:\n{}", dump);
    assert!(
        dump.contains("KEY=gyro:app:0:Antigravity"),
        "unexpected dump:\n{}",
        dump
    );
    assert!(dump.contains("HUB=\n"), "unexpected dump:\n{}", dump);
    assert!(
        dump.contains("ARGS=--headless --ssh-host=dev"),
        "unexpected dump:\n{}",
        dump
    );

    // No session should exist prior to the first proxied request
    let client = Client::new();
    let usage_url = format!("{}/usage", proxy_url);
    let snap_before = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert!(snap_before.sessions.is_empty());

    // First request with the desktop app's API key lazily creates `#app Antigravity`
    let res = client
        .post(format!(
            "{}/v1beta/models/gemini-4-argon:generateContent",
            proxy_url
        ))
        .header("Authorization", "Bearer gyro:app:0:Antigravity")
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let snap_after = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert_eq!(snap_after.sessions.len(), 1);
    assert_eq!(snap_after.sessions[0].id, "app");
    assert_eq!(snap_after.sessions[0].label, "Antigravity");
    assert_eq!(snap_after.sessions[0].pid, None);
    assert!(snap_after.sessions[0].active);
    assert_eq!(snap_after.sessions[0].totals.input_tokens, 12_000);
    assert_eq!(snap_after.sessions[0].totals.output_tokens, 500);
}
