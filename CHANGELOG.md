# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.4] - 2026-11-23

### Added
- **Synthetic `gemini-pro-orchestrator` Model & Planner Fallback Routing**: Synthesizes `gemini-pro-orchestrator` in `GET /v1beta/models` whenever both an eligible planner (`gemini-4-neon`, `gemini-4-carbon`, or `gemini-4-argon`) and executor (`gemini-4-hydrogen`) are available (inheriting the preferred planner's token limits and passing through any upstream orchestrator entry unsuffixed). Incoming proxy requests targeting `gemini-pro-orchestrator` are automatically rewritten to the highest-priority available planner model with `HIGH` (or suffix-derived) thinking level.

### Changed
- **Keep-Alive Heartbeats Scoped to Buffered Mode**: Bypassed the keep-alive timeout and early `200 OK` heartbeat stream when full SSE buffering is disabled (`buffer = false`), preserving exact upstream status codes, response headers, and unpadded payloads in passthrough mode while keeping keep-alive protection active when full buffering is enabled (including when toggled at runtime via `POST /buffer`).

### Removed
- **Offline Fallback Model Catalog**: Removed the hardcoded offline fallback model catalog (`DEFAULT_GATEWAY_MODELS`, `default_gateway_models`, and `resolve_gateway_models`). `ProxyState` now starts with an empty catalog and `GET /v1beta/models` dynamically fetches from upstream when empty, returning `502 Bad Gateway` with a Gemini JSON error if upstream discovery fails or returns no matching core models.

## [0.2.3] - 2026-11-21

### Added
- **Antigravity Desktop App Launcher (`agy-gyro app`)**: Added the `agy-gyro app` subcommand to launch the Antigravity desktop application detached with Enterprise Gateway environment variables configured. Launches via macOS LaunchServices (`open -b com.google.antigravity` or `-a <app_path>` for `.app` bundles), or auto-discovers and spawns the executable directly on Linux, Windows, and custom binary paths, with `app_path` and `AGY_GYRO_APP_PATH` override support.

### Changed
- **Unified Lazy Session Lifecycle**: Unified both `agy` CLI and desktop app onto lazy session creation driven by `AGY_LLM_GATEWAY_API_KEY`, removing eager `POST /sessions` and `DELETE /sessions/{id}` registration/cleanup calls to prevent stale sessions on quick exits or Electron single-instance handoffs. CLI sessions preserve OS PID liveness tracking, the desktop app uses a fixed `#app Antigravity` session, and catalog/pricing refreshes on already-running daemons are triggered via `GET /sessions?refresh=1`.
- **Recency-Ordered TUI Session Tabs**: Updated `agy-gyro monitor` and `/usage` snapshots to order individual session tabs with the most recently used active session leftmost (immediately after `[All Sessions]`), followed by older active sessions and exited sessions on the right.
- **Simplified Build ID Hashing**: Replaced the custom SHA-256 implementation in `build.rs` with `std::hash::DefaultHasher` over the crate source files (`Cargo.toml`, `Cargo.lock`, `build.rs`, and `src/`).

### Removed
- **`AGY_GYRO_API_KEY` Environment Variable**: Removed `AGY_GYRO_API_KEY` support; the Gemini API key is now configurable exclusively via `GEMINI_API_KEY` or `api_key` in `~/.gemini/config/agy-gyro.toml`.

## [0.2.2] - 2026-11-21

### Added
- **Automatic Daemon Restart on Build Mismatch**: Computes a deterministic SHA-256 `BUILD_ID` at compile time and exposes `build_id` via `GET /sessions`. Launching `agy-gyro`, `agy-gyro server`, or `agy-gyro monitor` now automatically shuts down (with force-kill fallback) and restarts any running daemon whose build ID differs from the current binary (including older daemons without a build ID).
- **Runtime Stream Buffering Toggle (`b` / `POST /buffer`)**: Added a `POST /buffer` endpoint, live `Buffer: ON/OFF` status indicator, and `b` keybinding in `agy-gyro monitor` to toggle full SSE stream buffering at runtime without restarting the daemon.
- **Per-Session Model & Pricing Refresh**: Automatically refetches `/v1beta/models` and live `models.dev` pricing on every new `POST /sessions` connection, coalescing concurrent in-flight refreshes while preserving the existing catalog and pricing on transient errors.
- **`xhigh` Effort Support for Gemini 4 Hydrogen & Neon**: Expanded `gemini-4-hydrogen` and `gemini-4-neon` models in `/v1beta/models` with `-xhigh` (`XHigh` / `thinkingLevel: "XHIGH"`) effort variants (ordered `high`, `xhigh`, `medium`, `low` so `-high` remains the default) and added both models to the fallback gateway catalog.

### Changed
- **Streaming Passthrough by Default (`buffer` Config Option)**: Renamed `no_buffer` in `~/.gemini/config/agy-gyro.toml` to `buffer` and disabled full SSE stream buffering by default (`buffer = false`), verifying Chunk 1 and streaming subsequent tokens immediately for lower time-to-first-token latency.
- **Incremental SSE Log Streaming & Higher Log Cap**: Raised the in-memory log buffer cap from 200 to 10,000 entries and switched `GET /logs` to stream incremental `snapshot` / `append` SSE events with per-client cursors and automatic resync on reset or lag, updating `agy-gyro monitor` for fast incremental log appends and smooth paused scrolling.
- **Gemini API Key Requirement & Pricing Scope**: Restored requiring a Gemini API key (`api_key`, `AGY_GYRO_API_KEY`, or `GEMINI_API_KEY`) for wrapper and `server` modes and scoped live `models.dev` pricing lookups back to Google Gemini models.

### Removed
- **OAuth Passthrough Mode (`oauth` / `AGY_GYRO_OAUTH`)**: Removed personal OAuth passthrough support (`oauth` config option, `AGY_GYRO_OAUTH`, `CLOUD_CODE_URL` routing, `/s/<session_id>/...` path handling, `/v1internal` passthrough, and Anthropic/OpenAI pricing entries).

## [0.2.1] - 2026-11-10

### Added
- **OAuth Passthrough Mode (`oauth` / `AGY_GYRO_OAUTH`)**: Support running `agy` sessions with personal OAuth credentials via `/s/<session_id>/...` (`CLOUD_CODE_URL`), tracking per-session token usage and estimated costs across Google Gemini, Anthropic Claude, and OpenAI models from `models.dev` while bypassing retries and buffering. `api_key` is now optional, automatically running in OAuth passthrough mode when omitted or when `oauth = true` (`AGY_GYRO_OAUTH=1`) is set.
- **Dynamic `/v1beta/models` Discovery Endpoint**: Serves a curated model discovery catalog directly at `GET /v1beta/models` instead of setting `AGY_LLM_GATEWAY_MODELS`, preserving upstream model metadata (`displayName`, `description`, token limits, `supportedGenerationMethods`, and `thinking`) across expanded effort variants (`-low`, `-medium`, `-high`) and dynamically including the newest `-flash-image` and `-flash-lite` models.
- **Dynamic Effort Suffix Rewriting**: Automatically strips `-low`, `-medium`, and `-high` suffixes from upstream request paths and injects `thinkingLevel` into `generationConfig.thinkingConfig` when missing, enabling effort-level variants for newly released Gemini models without requiring `agy` updates.

### Changed
- **UUID v4 Session Identifiers**: Replaced sequential integer session IDs with RFC 4122 UUID v4 identifiers (displaying 8-character short prefixes `#<short_id>` in the TUI dashboard and logs) to prevent session ID collisions across daemon restarts.
- **Live Pricing Resolution**: Removed built-in default pricing and family fallbacks; pricing is now loaded strictly from `models.dev/api.json` across Google, Anthropic, and OpenAI providers (treating unlisted models as zero cost).

### Removed
- **Daemon-Only Environment Variables**: Removed environment variable overrides for daemon-only proxy settings (`AGY_GYRO_LOG_FILE`, `AGY_GYRO_MAX_RETRIES`, `AGY_GYRO_INITIAL_DELAY_MS`, `AGY_GYRO_MAX_DELAY_MS`, `AGY_GYRO_NO_JITTER`, `AGY_GYRO_NO_BUFFER`, and `AGY_GYRO_REQUEST_TIMEOUT_SECS`); these options must now be configured via `~/.gemini/config/agy-gyro.toml`.
- **Configurable `upstream` Option**: Removed user-configurable `upstream` option from `agy-gyro.toml` and environment variables.

## [0.2.0] - 2026-11-08

### Added
- **Local Antigravity Enterprise Gateway**: `agy-gyro` now operates as a local Enterprise Gateway server (`127.0.0.1:6470` by default), translating `genai` gateway wire protocol requests and `Bearer` authentication into `x-goog-api-key` requests to standard Gemini REST endpoints.
- **Dynamic Model Discovery & Effort Expansion**: Automatically queries `/v1beta/models` at startup to discover active thinking-capable Core Gemini Text / Reasoning models and expands effort levels (`-low`, `-medium`, `-high`).
- **Shared Background Daemon & Session Management (`server` / `stop`)**: Runs a single persistent background daemon shared across all `agy` instances, with non-blocking `agy-gyro server` and `agy-gyro stop` commands, in-place `execvp` process replacement in wrapper mode, and OS-level PID liveness tracking per session.
- **Live Token Usage, Cost & Log TUI Monitor (`agy-gyro monitor`)**: Interactive terminal dashboard powered by `/usage` and `/logs` SSE streams, displaying real-time input, cached, output, and thinking token metrics, cache savings, per-model breakdowns, per-session tabs, and soft-wrapped gateway logs.
- **Live Model Pricing Discovery**: Fetches up-to-date Gemini model pricing (including >200k tiered rates and context caching rates) from `models.dev` with a built-in fallback table.
- **Keep-Alive Heartbeat Streaming**: Emits periodic keep-alive heartbeats when upstream buffering or retry backoff exceeds 15 seconds, preventing `agy`'s 60-second `ResponseHeaderTimeout` from dropping long-running thinking or retry requests.
- **TOML Configuration File**: Added support for persistent configuration in `~/.gemini/config/agy-gyro.toml` (with `AGY_GYRO_<KEY>` and `GEMINI_API_KEY` environment variable overrides).
- **Antigravity Hub Mode Support**: Automatically exports `AGY_ENABLE_HUB=1` when launching `agy` to unlock the local Antigravity Hub web UI (`--hub`).
- **Upstream Gzip Decompression**: Enabled automatic `gzip` decompression on upstream responses and switched to `native-tls` across all platforms.

### Changed
- **Transparent CLI Argument Passthrough**: Removed proxy CLI flags; `agy-gyro` now matches only `server`, `stop`, or `monitor` as the first argument and forwards all other arguments directly to `agy`.

### Removed
- **Model Redirection (`--redirect-model`)**: Replaced by native Enterprise Gateway model discovery and effort-level expansion.

## [0.1.2] - 2026-09-02

### Added
- **Model Redirection (`--redirect-model`)**: Remap Gemini model names in API requests via `--redirect-model <SRC>:<DST>` (or `AGY_GYRO_REDIRECT_MODEL` environment variable), enabling instant access to newer, unlisted, or preview models (such as `gemini-3.8-flash`) without waiting for Antigravity CLI updates.
- Support for configuring multiple model redirections simultaneously using repeated CLI flags or comma-separated environment variables.

## [0.1.1] - 2026-08-30

### Added
- **Full Stream Buffering**: Buffered streaming mode is now enabled by default, caching all chunks to verify complete and error-free streams before forwarding to `agy`.
- **Mid-Stream Error Recovery**: Transparent retry on mid-stream errors (`503 UNAVAILABLE`, `429 RESOURCE_EXHAUSTED`) and network connection drops during stream consumption.
- **`--no-buffer` Option**: Added `--no-buffer` command-line flag and `AGY_GYRO_NO_BUFFER` environment variable to allow opting out of stream buffering for minimal time-to-first-token latency.
- **Enhanced SSE Error Parser**: Line-by-line inspection of incoming chunks to detect Gemini error JSON payloads embedded within composite SSE events.
- **GitHub Release CI Automation**: Workflow triggers on release creation to automatically compile binaries and attach release archives to GitHub Releases.

## [0.1.0] - 2026-08-29

### Added
- **Initial Release of `agy-gyro`**: Local retry proxy to stabilize Antigravity CLI (`agy`) against transient Google Gemini API errors.
- **Dual Operating Modes**:
  - **Wrapper Mode (Default)**: Automatically starts an internal proxy on a dynamic port (`127.0.0.1:0`), sets `GOOGLE_GEMINI_BASE_URL`, and launches `agy` child process.
  - **Standalone Server Mode (`server`)**: Runs as a persistent background proxy daemon.
- **Transient Error Retry Engine**: Automatic retries with exponential backoff and randomized jitter for HTTP `429`, `500`, `502`, `503`, `504`, and `408`.
- **`Retry-After` Header Support**: Respects upstream HTTP `Retry-After` header values (both integer seconds and HTTP dates).
- **Chunk 1 Stream Interception**: Peeks at initial SSE chunks in streaming requests to catch early in-stream errors before committing response headers.
- **Large Payload Support**: Disabled default body limit to support multi-megabyte codebase prompts, diffs, and context payloads.
- **Cross-Platform Support**: Binaries for Linux (`x86_64`, `aarch64` musl), macOS (Universal binary `x86_64` + `arm64`), and Windows (`x86_64`, `aarch64`).
