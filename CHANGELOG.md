# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
