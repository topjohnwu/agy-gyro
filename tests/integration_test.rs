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
async fn spawn_test_proxy_opts(upstream_url: String, max_retries: u32, no_buffer: bool) -> String {
    spawn_test_proxy_with_keep_alive(
        upstream_url,
        max_retries,
        no_buffer,
        DEFAULT_KEEP_ALIVE_INTERVAL,
    )
    .await
}

/// Helper function to start a test proxy with configurable buffering and keep-alive interval
async fn spawn_test_proxy_with_keep_alive(
    upstream_url: String,
    max_retries: u32,
    no_buffer: bool,
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
        no_buffer,
        request_timeout_secs: 10,
        api_key: "test-key-123".to_string(),
        ..Config::default()
    };

    let client = build_http_client(&config).unwrap();

    let state =
        Arc::new(ProxyState::new(config, client).with_keep_alive_interval(keep_alive_interval));

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

    // Default mode is buffered (no_buffer = false)
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
    assert!(text.contains("clean"));
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_no_buffer_mode_passes_through_midstream_error() {
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

    // Start proxy with no_buffer = true
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
        .args([
            "-c",
            r#"
                test -n "$AGY_LLM_GATEWAY_URL" &&
                test "$AGY_LLM_GATEWAY_WIRE_PROTOCOL" = "genai" &&
                case "$AGY_LLM_GATEWAY_API_KEY" in "gyro:"*":$$:"*) ;; *) exit 1 ;; esac &&
                test -n "$AGY_LLM_GATEWAY_MODELS" &&
                test "$AGY_ENABLE_HUB" = "1" &&
                exit 42
            "#,
        ])
        .status()
        .await
        .unwrap();

    let _ = std::fs::remove_dir_all(&temp_home);
    assert_eq!(status.code(), Some(42));

    // OS-level process monitor detects that the replaced `sh` process ($$) has exited
    let client = Client::new();
    let snap = fetch_usage_snapshot(&client, &format!("{}/usage", proxy_url))
        .await
        .unwrap();
    assert_eq!(snap.sessions.len(), 1);
    assert_eq!(snap.sessions[0].id.len(), 36);
    assert!(!snap.sessions[0].active);
}

#[tokio::test]
async fn test_resolve_gateway_models_fetches_and_expands_models() {
    use agy_gyro::config::EXCLUDED_MODELS;
    use agy_gyro::proxy::resolve_gateway_models;

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
                            "name": format!("models/{}", EXCLUDED_MODELS[0]),
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-flash",
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-pro",
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-99-flash-image",
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
        no_buffer: false,
        request_timeout_secs: 10,
        api_key: "my-gemini-api-key".to_string(),
        ..Config::default()
    };

    let client = Client::new();
    let models = resolve_gateway_models(&client, &config).await;
    assert_eq!(
        models,
        "gemini-99-pro-low,gemini-99-pro-medium,gemini-99-pro-high,gemini-99-flash-low,gemini-99-flash-medium,gemini-99-flash-high"
    );
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

    // Missing config file uses defaults; fails if api_key is not configured
    assert!(Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).is_err());
    assert!(Cli::try_parse_with_config_from(["agy-gyro", "server"], Some(&temp_path)).is_err());

    // Create temporary agy-gyro.toml
    std::fs::write(
        &temp_path,
        r#"
host = "192.168.1.10"
port = 7070
upstream = "https://custom.googleapis.com"
api_key = "toml-gemini-key"
agy_path = "/toml/agy"
log_file = "/tmp/toml.log"
max_retries = 25
initial_delay_ms = 500
max_delay_ms = 30000
no_jitter = true
no_buffer = true
request_timeout_secs = 120
"#,
    )
    .unwrap();

    // 1. Config loaded from TOML file (wrapper mode)
    let cli = Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    assert_eq!(cli.command, Commands::Run(Vec::new()));
    assert_eq!(cli.config.host, "192.168.1.10");
    assert_eq!(cli.config.port, 7070);
    assert_eq!(cli.config.upstream, "https://custom.googleapis.com");
    assert_eq!(cli.config.api_key, "toml-gemini-key");
    assert_eq!(cli.config.agy_path, "/toml/agy");
    assert_eq!(
        cli.config.log_file.as_deref().unwrap().to_str().unwrap(),
        "/tmp/toml.log"
    );
    assert_eq!(cli.config.max_retries, 25);
    assert_eq!(cli.config.initial_delay_ms, 500);
    assert_eq!(cli.config.max_delay_ms, 30000);
    assert!(cli.config.no_jitter);
    assert!(cli.config.no_buffer);
    assert!(!cli.config.is_buffer_enabled());
    assert_eq!(cli.config.request_timeout_secs, 120);

    // 2. Config loaded from TOML file in server mode
    let cli_server =
        Cli::try_parse_with_config_from(["agy-gyro", "server"], Some(&temp_path)).unwrap();
    assert_eq!(cli_server.command, Commands::Server);
    assert_eq!(cli_server.config.api_key, "toml-gemini-key");
    assert_eq!(cli_server.config.port, 7070);
    assert_eq!(cli_server.config.max_retries, 25);

    // 3. Precedence: Environment variables override TOML
    unsafe {
        std::env::set_var("AGY_GYRO_MAX_RETRIES", "42");
        std::env::set_var("AGY_GYRO_HOST", "10.0.0.1");
        std::env::set_var("AGY_GYRO_AGY_PATH", "/custom/agy");
        std::env::set_var("AGY_GYRO_NO_BUFFER", "false");
        std::env::set_var("GEMINI_API_KEY", "env-gemini-key-123");
    }

    let cli_env = Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    clear_test_env_vars();

    assert_eq!(cli_env.config.host, "10.0.0.1");
    assert_eq!(cli_env.config.max_retries, 42);
    assert_eq!(cli_env.config.agy_path, "/custom/agy");
    assert!(!cli_env.config.no_buffer);
    assert!(cli_env.config.is_buffer_enabled());
    assert_eq!(cli_env.config.api_key, "env-gemini-key-123");
    // TOML still applies for fields not set via Env
    assert_eq!(cli_env.config.port, 7070);
    assert_eq!(cli_env.config.initial_delay_ms, 500);

    // 4. Unknown TOML keys are ignored
    std::fs::write(
        &temp_path,
        "api_key = \"toml-gemini-key\"\nunknown_option = 123\n",
    )
    .unwrap();
    let cli = Cli::try_parse_with_config_from(["agy-gyro"], Some(&temp_path)).unwrap();
    assert_eq!(cli.command, Commands::Run(Vec::new()));
    assert_eq!(cli.config.api_key, "toml-gemini-key");

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
        false,
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

    let proxy_url =
        spawn_test_proxy_with_keep_alive(mock_server.uri(), 1, false, Duration::from_millis(25))
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
        spawn_test_proxy_with_keep_alive(mock_server.uri(), 2, false, Duration::from_millis(20))
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

    // Verify per-model cost for gemini-4-argon ($2/1M in, $0.10/1M cached, $10/1M out):
    // uncached: 20k * $2/1M = $0.04; cached: 80k * $0.10/1M = $0.008; output: 5k * $10/1M = $0.05 -> total = $0.098
    let argon = &snap.models["gemini-4-argon"];
    assert_eq!(argon.totals.requests, 1);
    assert_eq!(argon.totals.retries, 1);
    assert!((argon.totals.uncached_input_cost_usd - 0.04).abs() < 1e-9);
    assert!((argon.totals.cached_input_cost_usd - 0.008).abs() < 1e-9);
    assert!((argon.totals.output_cost_usd - 0.05).abs() < 1e-9);
    assert!((argon.totals.total_cost_usd - 0.098).abs() < 1e-9);

    // Verify per-model cost for gemini-2.5-pro ($1.25/1M in, $0.125/1M cached, $10/1M out):
    // uncached: 40k * $1.25/1M = $0.05; cached: 10k * $0.125/1M = $0.00125; output: 1k * $10/1M = $0.01 -> total = $0.06125
    let pro = &snap.models["gemini-2.5-pro"];
    assert_eq!(pro.totals.requests, 1);
    assert!((pro.totals.total_cost_usd - 0.06125).abs() < 1e-9);

    // Verify total cost across both models: $0.098 + $0.06125 = $0.15925
    assert!((snap.totals.total_cost_usd - 0.15925).abs() < 1e-9);

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
        && l.message.contains("cost=$0.098000")));

    // Verify reset endpoint clears counters and logs
    let reset_snap = reset_remote_usage(&client, &usage_url).await.unwrap();
    assert_eq!(reset_snap.total_requests, 0);
    assert_eq!(reset_snap.totals.total_tokens, 0);
    assert_eq!(reset_snap.models.len(), 0);

    let reset_logs = fetch_logs_snapshot(&client, &logs_url).await.unwrap();
    assert!(
        !reset_logs
            .iter()
            .any(|l| l.message.contains("cost=$0.098000"))
    );
}

#[tokio::test]
async fn test_no_buffer_passthrough_mode_records_usage_on_stream_completion() {
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

    let proxy_url = spawn_test_proxy_opts(mock_server.uri(), 3, true).await;
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
    assert!(snap.totals.total_cost_usd > 0.0);
}

#[test]
fn test_cli_parse_monitor_subcommand_without_api_key() {
    use agy_gyro::config::{Cli, Commands};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_test_env_vars();

    let cli_default = Cli::try_parse_with_config_from(["agy-gyro", "monitor"], None).unwrap();
    assert_eq!(cli_default.command, Commands::Monitor);
    assert_eq!(cli_default.config.port, 6470);
    assert_eq!(cli_default.config.usage_url(), "http://127.0.0.1:6470/usage");

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
    use agy_gyro::proxy::{RegisterSessionRequest, RegisterSessionResponse};
    use agy_gyro::tui::{fetch_usage_snapshot, reset_remote_session_usage};

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

    // Register Session 1 (current test process PID, stays alive) and Session 2 (dummy process PID)
    let reg1: RegisterSessionResponse = client
        .post(format!("{}/sessions", proxy_url))
        .json(&RegisterSessionRequest {
            pid: Some(std::process::id()),
            label: "workspace-one".to_string(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reg1.session_id.len(), 36);

    // Re-registering with the same active PID is idempotent and returns session 1's UUID
    let reg1_dup: RegisterSessionResponse = client
        .post(format!("{}/sessions", proxy_url))
        .json(&RegisterSessionRequest {
            pid: Some(std::process::id()),
            label: "workspace-one".to_string(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reg1_dup.session_id, reg1.session_id);

    let reg2: RegisterSessionResponse = client
        .post(format!("{}/sessions", proxy_url))
        .json(&RegisterSessionRequest {
            pid: Some(session2_pid),
            label: "workspace-two".to_string(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reg2.session_id.len(), 36);
    assert_ne!(reg2.session_id, reg1.session_id);
    assert_eq!(
        reg2.api_key,
        format!("gyro:{}:{}:workspace-two", reg2.session_id, session2_pid)
    );

    // Send 1 request for each session
    for api_key in [&reg1.api_key, &reg2.api_key] {
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

    // Terminate Session 2's OS process and reap it so OS-level process monitoring sees it exited
    dummy_proc.kill().await.unwrap();
    let _ = dummy_proc.wait().await;

    let usage_url = format!("{}/usage", proxy_url);
    let snap = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert_eq!(snap.total_requests, 2);
    assert_eq!(snap.totals.input_tokens, 100_000);
    assert_eq!(snap.sessions.len(), 2);
    assert_eq!(snap.sessions[0].id, reg1.session_id);
    assert_eq!(snap.sessions[0].label, "workspace-one");
    assert!(snap.sessions[0].active);
    assert_eq!(snap.sessions[0].totals.input_tokens, 50_000);

    assert_eq!(snap.sessions[1].id, reg2.session_id);
    assert_eq!(snap.sessions[1].label, "workspace-two");
    assert!(!snap.sessions[1].active);
    assert_eq!(snap.sessions[1].totals.input_tokens, 50_000);

    // Selectively resetting exited session 2 removes it and subtracts its tokens from global totals
    let snap_after = reset_remote_session_usage(&client, &usage_url, Some(&reg2.session_id))
        .await
        .unwrap();
    assert_eq!(snap_after.sessions.len(), 1);
    assert_eq!(snap_after.sessions[0].id, reg1.session_id);
    assert_eq!(snap_after.total_requests, 1);
    assert_eq!(snap_after.totals.input_tokens, 50_000);

    // Unconditionally removing session 1 via DELETE /sessions/<id> removes it even while active
    let del_res = client
        .delete(format!("{}/sessions/{}", proxy_url, reg1.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(del_res.status(), 204);
    let snap_empty = fetch_usage_snapshot(&client, &usage_url).await.unwrap();
    assert!(snap_empty.sessions.is_empty());
    assert_eq!(snap_empty.total_requests, 0);

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
    let mock_server = MockServer::start().await;
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
            "host = \"127.0.0.1\"\nport = {}\nupstream = \"{}\"\napi_key = \"test-gemini-key\"\n",
            free_port,
            mock_server.uri()
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
