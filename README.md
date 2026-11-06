<p align="center">
  <img src="assets/logo.svg" alt="agy-gyro logo" width="200" />
</p>

# agy-gyro

> **Disclaimer**: `agy-gyro` is an independent open-source project and is not affiliated with, endorsed by, or sponsored by Google or the Antigravity team.

`agy-gyro` is a high-performance, lightweight local **Antigravity Enterprise Gateway** and retry proxy written in Rust. It translates Antigravity's Enterprise Gateway `genai` wire protocol to standard Google Gemini API endpoints (`generativelanguage.googleapis.com`) and stabilizes the Antigravity CLI (`agy`) against transient API errors.


## Motivation

### 1. First-Class Enterprise Gateway Mode vs Raw Gemini API Key Mode

While Antigravity supports a Gemini API key mode (`"modelProvider": "gemini"`), many first-class features and latest models are incomplete or unavailable in that mode.

By contrast, [Antigravity Enterprise Gateway](https://docs.cloud.google.com/gemini/enterprise/docs/ai-developer-tools/deploy-enterprise-gateway) is a first-class integration mode across `agy` and IDE extensions. `agy-gyro` acts as a local Enterprise Gateway server that translates the `genai` gateway wire protocol to standard Gemini API endpoints and dynamically queries `/v1beta/models` to discover available Core Gemini Text / Reasoning models (filtering out legacy/lite/alias variants and expanding effort levels) so newly released Gemini models become available in `agy` automatically.

### 2. Transient API Errors & Lack of Retries

When interacting with the Google Gemini API directly using an API key, requests occasionally fail due to transient infrastructure errors or capacity limits:

- **HTTP 429 (`RESOURCE_EXHAUSTED`)**: Rate limits (RPM/TPM) or temporary backend capacity spikes.
- **HTTP 503 (`UNAVAILABLE`)**: Model servers temporarily overloaded.
- **HTTP 500 / 502 / 504**: Transient backend gateway or server errors.
- **HTTP 408 / Transport Drops**: Network timeouts or aborted socket connections.

By default, the Antigravity CLI (`agy`) does not implement automatic retry loops with exponential backoff. When an error status code is encountered, `agy` terminates the active session immediately, disrupting the agentic workflow.

<p align="center">
  <img src="assets/error.png" alt="The dreaded &quot;Agent execution terminated&quot; error" width="650" /><br/>
  <em>The dreaded "Agent execution terminated" error</em>
</p>

`agy-gyro` solves this by catching transient errors, retrying the requests internally using exponential backoff with jitter (following the [Gemini API Troubleshooting Guide](https://ai.google.dev/gemini-api/docs/troubleshooting)), and only returning the final result to `agy` once resolved (or when retry attempts are exhausted).

```mermaid
flowchart LR
    AGY["Antigravity CLI\n(agy)"]
    GYRO["agy-gyro\n(127.0.0.1:8080)"]
    GEMINI["Google Gemini API\n(generativelanguage.googleapis.com)"]

    AGY -- "1. Prompt / stream request\ngenai Gateway Wire Protocol" --> GYRO
    GYRO -- "2. Buffers & forwards request\nGemini REST API" --> GEMINI
    GEMINI -. "3. 429 / 503 / 500 / 504\nBackoff + retry" .-> GYRO
    GEMINI -- "4. 200 OK" --> GYRO
    GYRO -- "5. Streams response\n(200 OK)" --> AGY
```

## Installation

Install `agy-gyro` using `cargo`:

```bash
cargo install agy-gyro
```

Prebuilt executables for Linux, macOS, and Windows are also available in the [latest GitHub release](https://github.com/topjohnwu/agy-gyro/releases/latest).

## Configuration & Modes

`agy-gyro` operates in two modes: **Wrapper Mode** (default) and **Standalone Server Mode**.

### Mode 1: Wrapper Mode (Default)

Running `agy-gyro` without subcommands automatically starts an in-process Enterprise Gateway server on a dynamic TCP port (`127.0.0.1:0`), configures `AGY_LLM_GATEWAY_URL`, `AGY_LLM_GATEWAY_WIRE_PROTOCOL=genai`, `AGY_LLM_GATEWAY_API_KEY`, `AGY_LLM_GATEWAY_MODELS`, and `AGY_ENABLE_HUB=1`, and launches `agy`.

```bash
# Simply launch agy wrapped with agy-gyro gateway:
agy-gyro

# Pass arguments directly through to agy:
agy-gyro -- --dangerously-skip-permissions

# Optionally write proxy logs to a file:
agy-gyro --log-file /tmp/gyro.log
```

### Mode 2: Standalone Server Mode (`agy-gyro server`)

To run `agy-gyro` as a persistent background daemon or standalone Enterprise Gateway server:

```bash
agy-gyro server --port 8080
```

### Options & Flags

Options are resolved in order of precedence: **CLI flags > Environment variables > `~/.gemini/config/agy-gyro.toml` > Defaults**.

| Flag                     | Environment Variable            | Config Key (`agy-gyro.toml`) | Default                                     | Description                                             |
| :----------------------- | :------------------------------ | :--------------------------- | :------------------------------------------ | :------------------------------------------------------ |
| `-H`, `--host`           | `AGY_GYRO_HOST`                 | `host`                       | `127.0.0.1`                                 | Host address to bind the gateway server                 |
| `-p`, `--port`           | `AGY_GYRO_PORT`                 | `port`                       | `0` (wrapper) / `8080` (server)             | Port to listen on (`0` = OS-assigned free port)         |
| `-u`, `--upstream`       | `AGY_GYRO_UPSTREAM_URL`         | `upstream`                   | `https://generativelanguage.googleapis.com` | Target upstream Gemini API base URL                     |
| `--api-key`              | `GEMINI_API_KEY`                | `api_key`                    | _Required_                                  | Gemini API key for authenticating upstream requests     |
| `--agy-path`             | `AGY_GYRO_AGY_PATH`             | `agy_path`                   | `agy`                                       | Executable path for Antigravity CLI                     |
| `--log-file`             | `AGY_GYRO_LOG_FILE`             | `log_file`                   | _None_                                      | Path to log file for proxy tracing logs in wrapper mode |
| `--max-retries`          | `AGY_GYRO_MAX_RETRIES`          | `max_retries`                | `15`                                        | Maximum number of retry attempts for retriable errors   |
| `--initial-delay-ms`     | `AGY_GYRO_INITIAL_DELAY_MS`     | `initial_delay_ms`           | `1000`                                      | Initial retry backoff delay in milliseconds             |
| `--max-delay-ms`         | `AGY_GYRO_MAX_DELAY_MS`         | `max_delay_ms`               | `60000`                                     | Maximum backoff delay cap in milliseconds               |
| `--no-jitter`            | `AGY_GYRO_NO_JITTER`            | `no_jitter`                  | `false`                                     | Disable randomized jitter in backoff calculation        |
| `--no-buffer`            | `AGY_GYRO_NO_BUFFER`            | `no_buffer`                  | `false`                                     | Disable full stream buffering (stream chunks immediately)|
| `--request-timeout-secs` | `AGY_GYRO_REQUEST_TIMEOUT_SECS` | `request_timeout_secs`       | `600`                                       | Timeout per attempt in seconds                          |

### Configuration File (`~/.gemini/config/agy-gyro.toml`)

You can persist your API key and preferred settings in `~/.gemini/config/agy-gyro.toml`:

```toml
api_key = "your-gemini-api-key"
max_retries = 15
log_file = "/tmp/gyro.log"
```

## Quick Start with Antigravity (`agy`)

Set your Gemini API key and run `agy-gyro`:

```bash
export GEMINI_API_KEY="your-gemini-api-key"

# Launch agy
agy-gyro
```

`agy-gyro` handles local gateway startup, dynamic port binding, `AGY_LLM_GATEWAY_*` environment variable setup, protocol translation, and signal forwarding seamlessly!

### Antigravity Hub Web UI (`--hub`)

`agy-gyro` automatically exports `AGY_ENABLE_HUB=1` in wrapper mode, unlocking Antigravity's local Hub web interface (which requires Enterprise Gateway mode):

```bash
agy-gyro -- --hub [--hub-port 4400]
```

Once the server is running, open the Hub UI in your browser with `?useWebSocket=true`:

```text
http://localhost:4400/?useWebSocket=true
```

> **Important**: Always include `?useWebSocket=true` when accessing the Hub UI over `http://localhost:4400`. Without it, the web UI opens multiple long-lived HTTP/1.1 streaming RPCs that exhaust the browser's 6-connection-per-origin limit and cause the UI to stall when switching or starting conversations. With `?useWebSocket=true`, all RPC streams are multiplexed over a single WebSocket connection.

## Streaming & Error Recovery Behavior

### Default Mode: Full Stream Buffering

In default mode, `agy-gyro` buffers all incoming stream chunks before delivering them to `agy`:
1. **Full Verification**: Ensures every chunk in the response is error-free and that the upstream stream completes cleanly.
2. **Zero-Byte Leakage**: If an error (e.g. `503 UNAVAILABLE`, `429 RESOURCE_EXHAUSTED`, or network disconnect) occurs at chunk 1 or mid-stream, `agy-gyro` discards the buffered chunks and replays the request from scratch.
3. **Flawless Delivery**: `agy` only receives verified, complete streams without partial output or syntax errors.

### Passthrough Mode (`--no-buffer`)

When running with `--no-buffer`, `agy-gyro` verifies Chunk 1 and immediately streams subsequent tokens to minimize time-to-first-token latency:
- **Chunk 1 Error**: Retried seamlessly before sending headers to `agy`.
- **Mid-Stream Error (Chunk $N > 1$)**: Forwarded downstream to prevent duplicated tokens.

## License

This project is licensed under the [MIT License](LICENSE).
