// SPDX-License-Identifier: MIT

pub use crate::models::{extract_model_from_path, normalize_model_id, strip_effort_suffix};
pub use crate::pricing::{
    CostBreakdown, DEFAULT_PRICING_URL, ModelPricing, PricingTable, fetch_model_pricing,
    parse_models_dev_pricing, try_fetch_model_pricing,
};
pub use crate::session::{
    SessionMeta, extract_session_from_headers, format_session_api_key, generate_session_uuid,
    is_pid_alive, parse_session_api_key, sanitize_session_label, short_session_id,
};

use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};
use tracing::info;
use tracing_subscriber::registry::LookupSpan;

/// Maximum number of real-time log entries retained in the log ring buffer.
pub const MAX_LOG_ENTRIES: usize = 10_000;

/// Raw `usageMetadata` fields returned in Gemini `generateContent` / `streamGenerateContent` JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u64,
    #[serde(default)]
    pub cached_content_token_count: u64,
    #[serde(default)]
    pub tool_use_prompt_token_count: u64,
    #[serde(default)]
    pub candidates_token_count: u64,
    #[serde(default)]
    pub thoughts_token_count: u64,
    #[serde(default)]
    pub total_token_count: u64,
}

impl RawUsageMetadata {
    /// Merges another `RawUsageMetadata` from the same response stream, retaining the maximum
    /// cumulative token count reported for each field.
    pub fn merge_cumulative(&mut self, other: &RawUsageMetadata) {
        self.prompt_token_count = self.prompt_token_count.max(other.prompt_token_count);
        self.cached_content_token_count = self
            .cached_content_token_count
            .max(other.cached_content_token_count);
        self.tool_use_prompt_token_count = self
            .tool_use_prompt_token_count
            .max(other.tool_use_prompt_token_count);
        self.candidates_token_count = self
            .candidates_token_count
            .max(other.candidates_token_count);
        self.thoughts_token_count = self.thoughts_token_count.max(other.thoughts_token_count);
        self.total_token_count = self.total_token_count.max(other.total_token_count);
    }

    pub fn has_any_tokens(&self) -> bool {
        self.prompt_token_count > 0
            || self.cached_content_token_count > 0
            || self.tool_use_prompt_token_count > 0
            || self.candidates_token_count > 0
            || self.thoughts_token_count > 0
            || self.total_token_count > 0
    }

    /// Total input tokens (`promptTokenCount + toolUsePromptTokenCount`).
    /// Note: `promptTokenCount` in the Gemini API already includes `cachedContentTokenCount`.
    pub fn input_tokens(&self) -> u64 {
        self.prompt_token_count
            .saturating_add(self.tool_use_prompt_token_count)
    }

    /// Cached input tokens (`cachedContentTokenCount`), capped at `input_tokens()`.
    pub fn cached_input_tokens(&self) -> u64 {
        self.cached_content_token_count.min(self.input_tokens())
    }

    /// Uncached input tokens billed at the standard input rate.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens()
            .saturating_sub(self.cached_input_tokens())
    }

    /// Total output tokens billed at the output rate (`candidatesTokenCount + thoughtsTokenCount`).
    pub fn output_tokens(&self) -> u64 {
        self.candidates_token_count
            .saturating_add(self.thoughts_token_count)
    }

    /// Overall total tokens for the request.
    pub fn total_tokens(&self) -> u64 {
        let computed = self.input_tokens().saturating_add(self.output_tokens());
        self.total_token_count.max(computed)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponseEnvelope {
    usage_metadata: Option<RawUsageMetadata>,
    model_version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GeminiPayloadEnvelope {
    Single(GeminiResponseEnvelope),
    Array(Vec<GeminiResponseEnvelope>),
}

/// Maximum size (8 MiB) buffered for non-SSE multi-line JSON fallback extraction.
const MAX_FALLBACK_JSON_BYTES: usize = 8 * 1024 * 1024;

/// Incremental parser that extracts `usageMetadata` and `modelVersion` across one or more
/// SSE (`data: {...}`) or raw JSON chunks.
#[derive(Debug, Default)]
pub struct StreamUsageExtractor {
    pub usage: RawUsageMetadata,
    pub model_version: Option<String>,
    found_usage: bool,
    saw_sse_data: bool,
    line_buf: Vec<u8>,
    raw_fallback_buf: Vec<u8>,
}

impl StreamUsageExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    fn inspect_json_str(&mut self, json_str: &str) {
        if !json_str.contains("usageMetadata") && !json_str.contains("modelVersion") {
            return;
        }
        if let Ok(parsed) = serde_json::from_str::<GeminiPayloadEnvelope>(json_str) {
            match parsed {
                GeminiPayloadEnvelope::Single(env) => self.apply_envelope(env),
                GeminiPayloadEnvelope::Array(arr) => {
                    for env in arr {
                        self.apply_envelope(env);
                    }
                }
            }
        }
    }

    fn apply_envelope(&mut self, env: GeminiResponseEnvelope) {
        if let Some(mv) = env.model_version.filter(|s| !s.is_empty()) {
            self.model_version = Some(normalize_model_id(&mv));
        }
        if let Some(u) = env.usage_metadata {
            self.usage.merge_cumulative(&u);
            self.found_usage = true;
        }
    }

    fn process_line(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            return;
        }
        let json_candidate = if let Some(rest) = trimmed.strip_prefix("data:") {
            self.saw_sse_data = true;
            self.raw_fallback_buf.clear();
            rest.trim()
        } else {
            trimmed
        };
        if json_candidate.starts_with('{') || json_candidate.starts_with('[') {
            self.inspect_json_str(json_candidate);
        }
    }

    /// Feeds a raw response chunk into the extractor.
    pub fn feed_chunk(&mut self, chunk: &[u8]) {
        if !self.saw_sse_data
            && self.raw_fallback_buf.len().saturating_add(chunk.len()) <= MAX_FALLBACK_JSON_BYTES
        {
            self.raw_fallback_buf.extend_from_slice(chunk);
        }

        self.line_buf.extend_from_slice(chunk);

        while let Some(newline_idx) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.line_buf[..newline_idx]).into_owned();
            self.line_buf.drain(..=newline_idx);
            self.process_line(&line);
        }
    }

    /// Finalizes extraction after all chunks have been received.
    pub fn finish(&mut self, full_bytes_fallback: Option<&[u8]>) -> Option<RawUsageMetadata> {
        if !self.line_buf.is_empty() {
            let remaining = String::from_utf8_lossy(&self.line_buf).into_owned();
            self.line_buf.clear();
            self.process_line(&remaining);
        }

        if !self.found_usage {
            let owned_fallback = std::mem::take(&mut self.raw_fallback_buf);
            let fallback = full_bytes_fallback.or({
                if !owned_fallback.is_empty() {
                    Some(owned_fallback.as_slice())
                } else {
                    None
                }
            });
            if let Some(full) = fallback
                && let Ok(text) = std::str::from_utf8(full)
            {
                let trimmed = text.trim();
                let json_candidate = trimmed
                    .strip_prefix("data:")
                    .map(str::trim)
                    .unwrap_or(trimmed);
                self.inspect_json_str(json_candidate);
            }
        }

        if self.found_usage || self.usage.has_any_tokens() {
            Some(self.usage.clone())
        } else {
            None
        }
    }
}

/// Extracts `usageMetadata` and optional `modelVersion` from a slice of buffered response chunks.
pub fn extract_usage_from_chunks(chunks: &[Bytes]) -> (Option<RawUsageMetadata>, Option<String>) {
    let mut extractor = StreamUsageExtractor::new();
    for chunk in chunks {
        extractor.feed_chunk(chunk);
    }

    let usage = extractor.finish(None);
    (usage, extractor.model_version)
}

/// Aggregated token and cost totals across requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsageTotals {
    pub requests: u64,
    pub retries: u64,
    pub input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub candidates_tokens: u64,
    pub thoughts_tokens: u64,
    pub total_tokens: u64,
    pub uncached_input_cost_usd: f64,
    pub cached_input_cost_usd: f64,
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub total_cost_usd: f64,
    pub cache_savings_usd: f64,
}

impl TokenUsageTotals {
    pub fn record(&mut self, raw: &RawUsageMetadata, cost: &CostBreakdown, retries: u32) {
        self.requests += 1;
        self.retries += retries as u64;
        self.input_tokens += raw.input_tokens();
        self.uncached_input_tokens += raw.uncached_input_tokens();
        self.cached_input_tokens += raw.cached_input_tokens();
        self.output_tokens += raw.output_tokens();
        self.candidates_tokens += raw.candidates_token_count;
        self.thoughts_tokens += raw.thoughts_token_count;
        self.total_tokens += raw.total_tokens();
        self.uncached_input_cost_usd += cost.uncached_input_cost_usd;
        self.cached_input_cost_usd += cost.cached_input_cost_usd;
        self.input_cost_usd += cost.input_cost_usd;
        self.output_cost_usd += cost.output_cost_usd;
        self.total_cost_usd += cost.total_cost_usd;
        self.cache_savings_usd += cost.cache_savings_usd;
    }

    pub fn subtract(&mut self, other: &TokenUsageTotals) {
        self.requests = self.requests.saturating_sub(other.requests);
        self.retries = self.retries.saturating_sub(other.retries);
        self.input_tokens = self.input_tokens.saturating_sub(other.input_tokens);
        self.uncached_input_tokens = self
            .uncached_input_tokens
            .saturating_sub(other.uncached_input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_sub(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_sub(other.output_tokens);
        self.candidates_tokens = self
            .candidates_tokens
            .saturating_sub(other.candidates_tokens);
        self.thoughts_tokens = self.thoughts_tokens.saturating_sub(other.thoughts_tokens);
        self.total_tokens = self.total_tokens.saturating_sub(other.total_tokens);
        self.uncached_input_cost_usd =
            (self.uncached_input_cost_usd - other.uncached_input_cost_usd).max(0.0);
        self.cached_input_cost_usd =
            (self.cached_input_cost_usd - other.cached_input_cost_usd).max(0.0);
        self.input_cost_usd = (self.input_cost_usd - other.input_cost_usd).max(0.0);
        self.output_cost_usd = (self.output_cost_usd - other.output_cost_usd).max(0.0);
        self.total_cost_usd = (self.total_cost_usd - other.total_cost_usd).max(0.0);
        self.cache_savings_usd = (self.cache_savings_usd - other.cache_savings_usd).max(0.0);
    }
}

/// Per-model usage and cost breakdown returned in [`UsageSnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsageStats {
    pub model: String,
    #[serde(flatten)]
    pub totals: TokenUsageTotals,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
}

/// Aggregated request counters, token totals, and per-model breakdown shared by
/// both global [`UsageSnapshot`] and per-session [`SessionUsageStats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScopeUsageStats {
    pub active_requests: u64,
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub total_retries: u64,
    pub totals: TokenUsageTotals,
    pub models: BTreeMap<String, ModelUsageStats>,
}

impl ScopeUsageStats {
    pub fn begin_request(&mut self) {
        self.total_requests += 1;
        self.active_requests += 1;
    }

    pub fn end_request(&mut self) {
        self.active_requests = self.active_requests.saturating_sub(1);
    }

    pub fn record_retry(&mut self) {
        self.total_retries += 1;
    }

    pub fn record_completed(
        &mut self,
        canonical_model: &str,
        is_ok: bool,
        retries: u32,
        usage: &RawUsageMetadata,
        cost: &CostBreakdown,
        pricing: Option<ModelPricing>,
    ) {
        if is_ok {
            self.successful_requests += 1;
        } else {
            self.failed_requests += 1;
        }
        self.totals.record(usage, cost, retries);

        let model_entry = self
            .models
            .entry(canonical_model.to_string())
            .or_insert_with(|| ModelUsageStats {
                model: canonical_model.to_string(),
                totals: TokenUsageTotals::default(),
                pricing: pricing.clone(),
            });
        if pricing.is_some() {
            model_entry.pricing = pricing;
        }
        model_entry.totals.record(usage, cost, retries);
    }

    pub fn reset_completed(&mut self) {
        self.total_requests = self.active_requests;
        self.successful_requests = 0;
        self.failed_requests = 0;
        self.total_retries = 0;
        self.totals = TokenUsageTotals::default();
        self.models.clear();
    }

    pub fn subtract(&mut self, other: &ScopeUsageStats) {
        let completed_reqs = other.total_requests.saturating_sub(other.active_requests);
        self.total_requests = self.total_requests.saturating_sub(completed_reqs);
        self.successful_requests = self
            .successful_requests
            .saturating_sub(other.successful_requests);
        self.failed_requests = self.failed_requests.saturating_sub(other.failed_requests);
        self.total_retries = self.total_retries.saturating_sub(other.total_retries);
        self.totals.subtract(&other.totals);

        for (model_name, model_stats) in &other.models {
            let mut remove_model = false;
            if let Some(global_model) = self.models.get_mut(model_name) {
                global_model.totals.subtract(&model_stats.totals);
                if global_model.totals.requests == 0 {
                    remove_model = true;
                }
            }
            if remove_model {
                self.models.remove(model_name);
            }
        }
    }
}

/// Per-session (`agy` instance) usage, cost, and lifecycle breakdown returned in [`UsageSnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUsageStats {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub active: bool,
    #[serde(flatten)]
    pub stats: ScopeUsageStats,
}

impl Deref for SessionUsageStats {
    type Target = ScopeUsageStats;

    fn deref(&self) -> &Self::Target {
        &self.stats
    }
}

impl DerefMut for SessionUsageStats {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stats
    }
}

/// A single real-time tracing log entry streamed by `GET /logs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp_secs: u64,
    pub level: String,
    pub target: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Incremental SSE event payload streamed by `GET /logs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogStreamEvent {
    /// Full replacement of the buffered log list (sent on initial connect, reset, or lag resync).
    Snapshot { entries: Vec<LogEntry> },
    /// Incremental batch of newly appended log entries.
    Append { entries: Vec<LogEntry> },
}

/// Server-side cursor tracking a `GET /logs` SSE subscriber's position in the log buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogStreamCursor {
    generation: u64,
    last_id: u64,
    initialized: bool,
}

/// Point-in-time JSON payload streamed by `GET /usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub uptime_secs: u64,
    #[serde(flatten)]
    pub stats: ScopeUsageStats,
    pub pricing_source: String,
    #[serde(default)]
    pub buffer_enabled: bool,
    #[serde(default)]
    pub sessions: Vec<SessionUsageStats>,
}

impl Deref for UsageSnapshot {
    type Target = ScopeUsageStats;

    fn deref(&self) -> &Self::Target {
        &self.stats
    }
}

impl DerefMut for UsageSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stats
    }
}

#[derive(Debug, Clone)]
struct SessionEntry {
    last_activity_seq: u64,
    stats: SessionUsageStats,
    track_pid_liveness: bool,
}

#[derive(Debug, Default)]
struct UsageInner {
    global: ScopeUsageStats,
    next_activity_seq: u64,
    sessions: BTreeMap<String, SessionEntry>,
}

impl UsageInner {
    fn ensure_session_entry(
        &mut self,
        meta: &SessionMeta,
        touch_activity: bool,
    ) -> (&mut SessionEntry, bool) {
        use std::collections::btree_map::Entry;
        let next_seq = &mut self.next_activity_seq;
        match self.sessions.entry(meta.id.clone()) {
            Entry::Occupied(entry) => {
                let e = entry.into_mut();
                let was_active = e.stats.active;
                e.stats.active = true;
                if touch_activity || !was_active {
                    *next_seq += 1;
                    e.last_activity_seq = *next_seq;
                }
                if meta.pid.is_some() {
                    if !was_active || !e.track_pid_liveness || e.stats.pid != meta.pid {
                        e.track_pid_liveness = meta.pid.is_some_and(is_pid_alive);
                    }
                    e.stats.pid = meta.pid;
                }
                if !meta.label.is_empty() && meta.label != "agy" {
                    e.stats.label = meta.label.clone();
                }
                (e, false)
            }
            Entry::Vacant(entry) => {
                *next_seq += 1;
                let e = entry.insert(SessionEntry {
                    last_activity_seq: *next_seq,
                    stats: SessionUsageStats {
                        id: meta.id.clone(),
                        label: meta.label.clone(),
                        pid: meta.pid,
                        active: true,
                        stats: ScopeUsageStats::default(),
                    },
                    track_pid_liveness: meta.pid.is_some_and(is_pid_alive),
                });
                (e, true)
            }
        }
    }
}

#[derive(Debug)]
struct LogBuffer {
    next_id: u64,
    generation: u64,
    evicted_through_id: u64,
    entries: VecDeque<LogEntry>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self {
            next_id: 1,
            generation: 0,
            evicted_through_id: 0,
            entries: VecDeque::new(),
        }
    }
}

/// Thread-safe live usage, cost, and log tracker shared across proxy handlers.
pub struct UsageTracker {
    started_at: Instant,
    next_request_id: AtomicU64,
    buffer_enabled: AtomicBool,
    pricing: RwLock<PricingTable>,
    inner: RwLock<UsageInner>,
    logs: RwLock<LogBuffer>,
    usage_change_tx: tokio::sync::watch::Sender<u64>,
    log_change_tx: tokio::sync::watch::Sender<u64>,
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self::new(PricingTable::default())
    }
}

impl UsageTracker {
    pub fn new(pricing: PricingTable) -> Self {
        let (usage_change_tx, _) = tokio::sync::watch::channel(0u64);
        let (log_change_tx, _) = tokio::sync::watch::channel(0u64);
        Self {
            started_at: Instant::now(),
            next_request_id: AtomicU64::new(1),
            buffer_enabled: AtomicBool::new(false),
            pricing: RwLock::new(pricing),
            inner: RwLock::new(UsageInner::default()),
            logs: RwLock::new(LogBuffer::default()),
            usage_change_tx,
            log_change_tx,
        }
    }

    /// Returns whether full SSE response buffering is currently enabled.
    pub fn is_buffer_enabled(&self) -> bool {
        self.buffer_enabled.load(Ordering::Relaxed)
    }

    /// Initializes the full SSE buffering flag without emitting a log entry or SSE notification.
    pub fn init_buffer_enabled(&self, enabled: bool) {
        self.buffer_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Sets full SSE response buffering at runtime, logging the change and notifying `/usage` subscribers.
    pub fn set_buffer_enabled(&self, enabled: bool) -> bool {
        self.buffer_enabled.store(enabled, Ordering::Relaxed);
        info!(
            "Full SSE buffering {}",
            if enabled { "enabled" } else { "disabled" }
        );
        self.notify_usage_change();
        enabled
    }

    /// Toggles full SSE response buffering at runtime and returns the new state.
    pub fn toggle_buffer_enabled(&self) -> bool {
        let prev = self.buffer_enabled.fetch_xor(true, Ordering::AcqRel);
        let next = !prev;
        info!(
            "Full SSE buffering {}",
            if next { "enabled" } else { "disabled" }
        );
        self.notify_usage_change();
        next
    }

    fn notify_usage_change(&self) {
        self.usage_change_tx
            .send_modify(|seq| *seq = seq.wrapping_add(1));
    }

    fn notify_log_change(&self) {
        self.log_change_tx
            .send_modify(|seq| *seq = seq.wrapping_add(1));
    }

    /// Subscribes to live usage/cost state change notifications for `GET /usage` SSE streaming.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.usage_change_tx.subscribe()
    }

    /// Subscribes to live log notifications for `GET /logs` SSE streaming.
    pub fn subscribe_logs(&self) -> tokio::sync::watch::Receiver<u64> {
        self.log_change_tx.subscribe()
    }

    /// Registers this `UsageTracker` as a sink for real-time `tracing` log events.
    pub fn register_global_logger(self: &Arc<Self>) {
        if let Ok(mut list) = global_log_trackers().write() {
            list.retain(|w| w.strong_count() > 0);
            list.push(Arc::downgrade(self));
        }
    }

    /// Spawns a background task that monitors OS-level process liveness for active sessions
    /// and marks sessions as exited when their `agy` process terminates.
    pub fn spawn_process_monitor(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(tracker) = weak.upgrade() else {
                    break;
                };
                tracker.check_session_processes();
            }
        });
    }

    /// Checks OS-level process liveness for all active tracked sessions, transitioning any
    /// session whose process has terminated to `active = false`.
    pub fn check_session_processes(&self) {
        self.reap_exited_sessions(true);
    }

    fn reap_exited_sessions(&self, notify: bool) {
        let has_exited = self.inner.read().ok().is_some_and(|inner| {
            inner.sessions.values().any(|e| {
                e.stats.active
                    && e.track_pid_liveness
                    && e.stats.active_requests == 0
                    && e.stats.pid.is_some_and(|p| !is_pid_alive(p))
            })
        });

        if !has_exited {
            return;
        }

        let mut exited_sessions = Vec::new();
        if let Ok(mut inner) = self.inner.write() {
            for entry in inner.sessions.values_mut() {
                if entry.stats.active
                    && entry.track_pid_liveness
                    && entry.stats.active_requests == 0
                    && let Some(pid) = entry.stats.pid
                    && !is_pid_alive(pid)
                {
                    entry.stats.active = false;
                    entry.track_pid_liveness = false;
                    exited_sessions.push((entry.stats.id.clone(), entry.stats.label.clone(), pid));
                }
            }
        }

        if !exited_sessions.is_empty() {
            for (id, label, pid) in exited_sessions {
                info!(
                    session_id = %id,
                    "[#{} {}] Session exited (pid={})",
                    short_session_id(&id),
                    label,
                    pid
                );
            }
            if notify {
                self.notify_usage_change();
            }
        }
    }

    /// Appends a real-time log entry and notifies `GET /logs` SSE subscribers.
    pub fn record_log(&self, level: &str, target: &str, message: impl Into<String>) {
        self.record_log_with_session(level, target, None, message);
    }

    /// Appends a real-time log entry associated with an optional `session_id` and notifies `GET /logs` subscribers.
    pub fn record_log_with_session(
        &self,
        level: &str,
        target: &str,
        session_id: Option<String>,
        message: impl Into<String>,
    ) {
        let timestamp_secs = self.started_at.elapsed().as_secs();
        if let Ok(mut buf) = self.logs.write() {
            let log_id = buf.next_id;
            buf.next_id = buf.next_id.saturating_add(1);
            buf.entries.push_back(LogEntry {
                id: log_id,
                timestamp_secs,
                level: level.to_string(),
                target: target.to_string(),
                message: message.into(),
                session_id,
            });
            while buf.entries.len() > MAX_LOG_ENTRIES {
                if let Some(evicted) = buf.entries.pop_front() {
                    buf.evicted_through_id = evicted.id;
                }
            }
        }
        self.notify_log_change();
    }

    /// Returns a snapshot of all buffered log entries for the `/logs` endpoint.
    pub fn logs_snapshot(&self) -> Vec<LogEntry> {
        match self.logs.read() {
            Ok(buf) => buf.entries.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Computes the next incremental [`LogStreamEvent`] for a subscriber at `cursor`,
    /// advancing `cursor` in place. Returns `None` if no new log changes have occurred.
    pub fn next_log_stream_event(&self, cursor: &mut LogStreamCursor) -> Option<LogStreamEvent> {
        let buf = self.logs.read().ok()?;
        let latest_id = buf.entries.back().map_or(0, |e| e.id);

        if !cursor.initialized
            || buf.generation != cursor.generation
            || buf.evicted_through_id > cursor.last_id
        {
            cursor.initialized = true;
            cursor.generation = buf.generation;
            cursor.last_id = latest_id;
            return Some(LogStreamEvent::Snapshot {
                entries: buf.entries.iter().cloned().collect(),
            });
        }

        if latest_id <= cursor.last_id {
            return None;
        }

        let new_count = buf
            .entries
            .iter()
            .rev()
            .take_while(|e| e.id > cursor.last_id)
            .count();
        let start_idx = buf.entries.len().saturating_sub(new_count);
        let entries: Vec<LogEntry> = buf.entries.range(start_idx..).cloned().collect();
        cursor.last_id = latest_id;
        Some(LogStreamEvent::Append { entries })
    }

    /// Replaces the pricing table (e.g. after live pricing discovery completes).
    pub fn set_pricing(&self, pricing: PricingTable) {
        if let Ok(mut guard) = self.pricing.write() {
            *guard = pricing;
        }
        self.notify_usage_change();
    }

    /// Marks an existing session as exited (`active = false`) while preserving its accumulated stats.
    pub fn mark_session_exited(&self, session_id: &str) -> bool {
        let mut label_opt = None;
        if let Ok(mut inner) = self.inner.write()
            && let Some(entry) = inner.sessions.get_mut(session_id)
        {
            entry.stats.active = false;
            entry.track_pid_liveness = false;
            label_opt = Some(entry.stats.label.clone());
        }

        if let Some(label) = label_opt {
            info!(
                session_id = %session_id,
                "[#{} {}] Session exited",
                short_session_id(session_id),
                label
            );
            self.notify_usage_change();
            true
        } else {
            false
        }
    }

    fn log_session_started(meta: &SessionMeta) {
        info!(
            session_id = %meta.id,
            "[#{} {}] Session started (pid={})",
            meta.short_id(),
            meta.label,
            meta.pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        );
    }

    /// Marks the start of an in-flight proxy request and returns an RAII guard that
    /// decrements `active_requests` when dropped.
    pub fn begin_request(self: &Arc<Self>) -> ActiveRequestGuard {
        self.begin_session_request(None)
    }

    /// Marks the start of an in-flight proxy request for an optional session and returns an RAII guard.
    pub fn begin_session_request(
        self: &Arc<Self>,
        session: Option<&SessionMeta>,
    ) -> ActiveRequestGuard {
        let mut started_new = false;
        let session_id = if let Ok(mut inner) = self.inner.write() {
            inner.global.begin_request();
            if let Some(meta) = session {
                let (entry, is_new) = inner.ensure_session_entry(meta, true);
                started_new = is_new;
                entry.stats.begin_request();
                Some(meta.id.clone())
            } else {
                None
            }
        } else {
            session.map(|m| m.id.clone())
        };

        if started_new && let Some(meta) = session {
            Self::log_session_started(meta);
        }

        self.notify_usage_change();
        ActiveRequestGuard {
            tracker: Arc::clone(self),
            session_id,
        }
    }

    /// Increments the global retry counter when a request attempt is retried.
    pub fn record_retry(&self) {
        self.record_session_retry(None);
    }

    /// Increments the global and per-session retry counter when a request attempt is retried.
    pub fn record_session_retry(&self, session: Option<&SessionMeta>) {
        let mut started_new = false;
        if let Ok(mut inner) = self.inner.write() {
            inner.global.record_retry();
            if let Some(meta) = session {
                let (entry, is_new) = inner.ensure_session_entry(meta, false);
                started_new = is_new;
                entry.stats.record_retry();
            }
        }
        if started_new && let Some(meta) = session {
            Self::log_session_started(meta);
        }
        self.notify_usage_change();
    }

    /// Records a completed request along with its extracted token usage (if any).
    pub fn record_completed_request(
        &self,
        model: &str,
        status: u16,
        retries: u32,
        duration: Duration,
        raw_usage: Option<&RawUsageMetadata>,
    ) {
        self.record_session_completed_request(None, model, status, retries, duration, raw_usage);
    }

    /// Records a completed request for an optional session along with its extracted token usage.
    pub fn record_session_completed_request(
        &self,
        session: Option<&SessionMeta>,
        model: &str,
        status: u16,
        retries: u32,
        duration: Duration,
        raw_usage: Option<&RawUsageMetadata>,
    ) {
        let is_ok = (200..300).contains(&status);
        let canonical_model = if model.is_empty() {
            "unknown".to_string()
        } else {
            normalize_model_id(model)
        };

        let pricing = self
            .pricing
            .read()
            .ok()
            .and_then(|table| table.lookup(&canonical_model));

        let usage = raw_usage.cloned().unwrap_or_default();
        let cost = pricing
            .as_ref()
            .map(|p| {
                p.calculate_cost(
                    usage.uncached_input_tokens(),
                    usage.cached_input_tokens(),
                    usage.output_tokens(),
                )
            })
            .unwrap_or_default();

        let req_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        let has_pricing = pricing.is_some();
        let mut started_new = false;
        if let Ok(mut inner) = self.inner.write() {
            inner.global.record_completed(
                &canonical_model,
                is_ok,
                retries,
                &usage,
                &cost,
                pricing.clone(),
            );

            if let Some(meta) = session {
                let (sess_entry, is_new) = inner.ensure_session_entry(meta, false);
                started_new = is_new;
                sess_entry.stats.record_completed(
                    &canonical_model,
                    is_ok,
                    retries,
                    &usage,
                    &cost,
                    pricing,
                );
            }
        }

        if started_new && let Some(meta) = session {
            Self::log_session_started(meta);
        }

        let cost_suffix = if has_pricing {
            format!(" | cost=${:.6}", cost.total_cost_usd)
        } else {
            String::new()
        };

        if let Some(meta) = session {
            info!(
                session_id = %meta.id,
                "[#{} {}] Request #{} [{}] status={} duration={}ms retries={} | in={} (uncached={}, cached={}) out={} (think={}){}",
                meta.short_id(),
                meta.label,
                req_id,
                canonical_model,
                status,
                duration.as_millis(),
                retries,
                usage.input_tokens(),
                usage.uncached_input_tokens(),
                usage.cached_input_tokens(),
                usage.output_tokens(),
                usage.thoughts_token_count,
                cost_suffix
            );
        } else {
            info!(
                "Request #{} [{}] status={} duration={}ms retries={} | in={} (uncached={}, cached={}) out={} (think={}){}",
                req_id,
                canonical_model,
                status,
                duration.as_millis(),
                retries,
                usage.input_tokens(),
                usage.uncached_input_tokens(),
                usage.cached_input_tokens(),
                usage.output_tokens(),
                usage.thoughts_token_count,
                cost_suffix
            );
        }
        self.notify_usage_change();
    }

    /// Resets all accumulated metrics and buffered logs across all sessions.
    /// Exited sessions are removed while active sessions are retained with zeroed counters.
    pub fn reset(&self) {
        self.next_request_id.store(1, Ordering::Relaxed);
        if let Ok(mut inner) = self.inner.write() {
            inner.global.reset_completed();
            inner
                .sessions
                .retain(|_, entry| entry.stats.active || entry.stats.active_requests > 0);
            for entry in inner.sessions.values_mut() {
                entry.stats.reset_completed();
            }
        }
        if let Ok(mut buf) = self.logs.write() {
            buf.entries.clear();
            buf.next_id = 1;
            buf.generation = buf.generation.wrapping_add(1);
            buf.evicted_through_id = 0;
        }
        self.notify_usage_change();
        self.notify_log_change();
    }

    /// Resets a single session's accumulated stats (or removes the session if it has already exited),
    /// subtracting its contribution from the global totals.
    pub fn reset_session(&self, session_id: &str) {
        let existed = if let Ok(mut inner) = self.inner.write() {
            if let Some(entry) = inner.sessions.get_mut(session_id) {
                let removed_stats = entry.stats.stats.clone();
                let keep_session = entry.stats.active || entry.stats.active_requests > 0;

                if keep_session {
                    entry.stats.reset_completed();
                } else {
                    inner.sessions.remove(session_id);
                }

                inner.global.subtract(&removed_stats);
                true
            } else {
                false
            }
        } else {
            false
        };

        if !existed {
            return;
        }

        let mut removed_any_logs = false;
        if let Ok(mut buf) = self.logs.write() {
            let before = buf.entries.len();
            buf.entries
                .retain(|l| l.session_id.as_deref() != Some(session_id));
            if buf.entries.len() != before {
                buf.generation = buf.generation.wrapping_add(1);
                buf.evicted_through_id = 0;
                removed_any_logs = true;
            }
        }

        self.notify_usage_change();
        if removed_any_logs {
            self.notify_log_change();
        }
    }

    /// Returns a point-in-time [`UsageSnapshot`] for the `/usage` endpoint.
    pub fn snapshot(&self) -> UsageSnapshot {
        self.reap_exited_sessions(false);

        let pricing_source = self
            .pricing
            .read()
            .map(|p| p.source.clone())
            .unwrap_or_else(|_| "none".to_string());

        let (stats, sessions) = match self.inner.read() {
            Ok(inner) => {
                let mut entries: Vec<(u64, SessionUsageStats)> = inner
                    .sessions
                    .values()
                    .map(|e| (e.last_activity_seq, e.stats.clone()))
                    .collect();
                entries.sort_by(|a, b| {
                    b.1.active
                        .cmp(&a.1.active)
                        .then_with(|| b.0.cmp(&a.0))
                        .then_with(|| a.1.id.cmp(&b.1.id))
                });
                let sessions = entries.into_iter().map(|(_, s)| s).collect();
                (inner.global.clone(), sessions)
            }
            Err(_) => (ScopeUsageStats::default(), Vec::new()),
        };

        UsageSnapshot {
            uptime_secs: self.started_at.elapsed().as_secs(),
            stats,
            pricing_source,
            buffer_enabled: self.is_buffer_enabled(),
            sessions,
        }
    }
}

fn global_log_trackers() -> &'static RwLock<Vec<Weak<UsageTracker>>> {
    static TRACKERS: OnceLock<RwLock<Vec<Weak<UsageTracker>>>> = OnceLock::new();
    TRACKERS.get_or_init(|| RwLock::new(Vec::new()))
}

#[derive(Clone, Debug)]
struct SpanSessionId(String);

#[derive(Default)]
struct SessionFieldVisitor {
    session_id: Option<String>,
}

impl tracing::field::Visit for SessionFieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "session_id" {
            let s = format!("{:?}", value).trim_matches('"').to_string();
            if !s.is_empty() {
                self.session_id = Some(s);
            }
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "session_id" && !value.is_empty() {
            self.session_id = Some(value.to_string());
        }
    }
}

#[derive(Default)]
struct LogEventVisitor {
    message: String,
    session_id: Option<String>,
    fields: Vec<String>,
}

impl tracing::field::Visit for LogEventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else if field.name() == "session_id" {
            let s = format!("{:?}", value).trim_matches('"').to_string();
            if !s.is_empty() {
                self.session_id = Some(s);
            }
        } else {
            self.fields.push(format!("{}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if field.name() == "session_id" {
            if !value.is_empty() {
                self.session_id = Some(value.to_string());
            }
        } else {
            self.fields.push(format!("{}={}", field.name(), value));
        }
    }
}

/// A `tracing_subscriber::Layer` that routes `agy_gyro` log events into active [`UsageTracker`] instances.
pub struct UsageLogLayer;

impl<S> tracing_subscriber::Layer<S> for UsageLogLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = SessionFieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(sid) = visitor.session_id
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(SpanSessionId(sid));
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = SessionFieldVisitor::default();
        values.record(&mut visitor);
        if let Some(sid) = visitor.session_id
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(SpanSessionId(sid));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("agy_gyro") {
            return;
        }

        let trackers: Vec<Arc<UsageTracker>> = match global_log_trackers().read() {
            Ok(list) => list.iter().filter_map(Weak::upgrade).collect(),
            Err(_) => return,
        };
        if trackers.is_empty() {
            return;
        }

        let mut visitor = LogEventVisitor::default();
        event.record(&mut visitor);

        let session_id = visitor.session_id.or_else(|| {
            ctx.event_scope(event).and_then(|scope| {
                for span in scope {
                    if let Some(ext) = span.extensions().get::<SpanSessionId>() {
                        return Some(ext.0.clone());
                    }
                }
                None
            })
        });

        let text = if visitor.fields.is_empty() {
            visitor.message
        } else if visitor.message.is_empty() {
            visitor.fields.join(" ")
        } else {
            format!("{} {}", visitor.message, visitor.fields.join(" "))
        };

        if !text.is_empty() {
            for tracker in trackers {
                tracker.record_log_with_session(
                    meta.level().as_str(),
                    meta.target(),
                    session_id.clone(),
                    text.clone(),
                );
            }
        }
    }
}

pub struct ActiveRequestGuard {
    tracker: Arc<UsageTracker>,
    session_id: Option<String>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.tracker.inner.write() {
            inner.global.end_request();
            if let Some(ref sid) = self.session_id
                && let Some(entry) = inner.sessions.get_mut(sid)
            {
                entry.stats.end_request();
            }
        }
        self.tracker.notify_usage_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_usage_from_sse_and_json_chunks() {
        let chunks = vec![
            Bytes::from_static(
                b"data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Hel\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 1000, \"cachedContentTokenCount\": 600}, \"modelVersion\": \"gemini-4-argon\"}\n\n",
            ),
            Bytes::from_static(
                b"data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"lo\"}]}}], \"usageMetadata\": {\"promptTokenCount\": 1000, \"cachedContentTokenCount\": 600, \"candidatesTokenCount\": 120, \"thoughtsTokenCount\": 380, \"totalTokenCount\": 1500}}\n\n",
            ),
        ];

        let (usage, model) = extract_usage_from_chunks(&chunks);
        let usage = usage.expect("usageMetadata should be extracted");
        assert_eq!(model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(usage.input_tokens(), 1000);
        assert_eq!(usage.cached_input_tokens(), 600);
        assert_eq!(usage.uncached_input_tokens(), 400);
        assert_eq!(usage.candidates_token_count, 120);
        assert_eq!(usage.thoughts_token_count, 380);
        assert_eq!(usage.output_tokens(), 500);
        assert_eq!(usage.total_tokens(), 1500);

        // Multi-line pretty-printed JSON (non-SSE) with split multi-byte UTF-8 ("🚀") across chunks
        let multiline_json = "{\n  \"candidates\": [{\"content\": {\"parts\": [{\"text\": \"🚀\"}]}}],\n  \"usageMetadata\": {\n    \"promptTokenCount\": 2500,\n    \"candidatesTokenCount\": 500\n  },\n  \"modelVersion\": \"gemini-2.5-pro\"\n}";
        let raw_bytes = multiline_json.as_bytes();
        let split_at = multiline_json.find("🚀").unwrap() + 2; // split mid-emoji (4-byte UTF-8 sequence)
        let mut ext = StreamUsageExtractor::new();
        ext.feed_chunk(&raw_bytes[..split_at]);
        ext.feed_chunk(&raw_bytes[split_at..]);
        let ml_usage = ext
            .finish(None)
            .expect("multi-line JSON usage should parse");
        assert_eq!(ext.model_version.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(ml_usage.input_tokens(), 2500);
        assert_eq!(ml_usage.output_tokens(), 500);
    }

    #[test]
    fn test_session_tracking_and_selective_reset() {
        let token = format_session_api_key("1", Some(48210), "my:workspace");
        assert_eq!(token, "gyro:1:48210:my-workspace");

        let parsed = parse_session_api_key(&format!("Bearer {}", token)).unwrap();
        assert_eq!(parsed.id, "1");
        assert_eq!(parsed.pid, Some(48210));
        assert_eq!(parsed.label, "my-workspace");

        let tracker = Arc::new(UsageTracker::default());
        let s1 = SessionMeta {
            id: generate_session_uuid(),
            label: "proj-alpha".to_string(),
            pid: None,
        };
        let s2 = SessionMeta {
            id: generate_session_uuid(),
            label: "proj-beta".to_string(),
            pid: None,
        };
        assert_eq!(s1.id.len(), 36);
        assert_eq!(s2.id.len(), 36);
        assert_ne!(s1.id, s2.id);
        assert_eq!(s1.short_id().len(), 8);

        let u1 = RawUsageMetadata {
            prompt_token_count: 10_000,
            candidates_token_count: 1_000,
            ..Default::default()
        };
        let u2 = RawUsageMetadata {
            prompt_token_count: 20_000,
            candidates_token_count: 2_000,
            ..Default::default()
        };

        {
            let _g1 = tracker.begin_session_request(Some(&s1));
            tracker.record_session_completed_request(
                Some(&s1),
                "gemini-4-argon",
                200,
                0,
                Duration::from_millis(50),
                Some(&u1),
            );
        }
        {
            let _g2 = tracker.begin_session_request(Some(&s2));
            tracker.record_session_completed_request(
                Some(&s2),
                "gemini-4-argon",
                200,
                1,
                Duration::from_millis(80),
                Some(&u2),
            );
        }

        tracker.mark_session_exited(&s2.id);

        let snap = tracker.snapshot();
        assert_eq!(snap.sessions.len(), 2);
        assert!(snap.sessions[0].active);
        assert!(!snap.sessions[1].active);
        assert_eq!(snap.totals.input_tokens, 30_000);
        assert_eq!(snap.sessions[0].totals.input_tokens, 10_000);
        assert_eq!(snap.sessions[1].totals.input_tokens, 20_000);

        // Resetting exited session #2 removes it and subtracts its tokens from global totals
        tracker.reset_session(&s2.id);
        let snap2 = tracker.snapshot();
        assert_eq!(snap2.sessions.len(), 1);
        assert_eq!(snap2.sessions[0].id, s1.id);
        assert_eq!(snap2.totals.input_tokens, 10_000);
    }

    #[test]
    fn test_session_ordering_most_recent_active_first() {
        let tracker = Arc::new(UsageTracker::default());
        let s1 = SessionMeta {
            id: generate_session_uuid(),
            label: "one".to_string(),
            pid: None,
        };
        let s2 = SessionMeta {
            id: generate_session_uuid(),
            label: "two".to_string(),
            pid: None,
        };
        let s3 = SessionMeta {
            id: generate_session_uuid(),
            label: "three".to_string(),
            pid: None,
        };

        let ids = |tracker: &UsageTracker| -> Vec<String> {
            tracker
                .snapshot()
                .sessions
                .into_iter()
                .map(|s| s.id)
                .collect()
        };

        let g1 = tracker.begin_session_request(Some(&s1));
        assert_eq!(ids(&tracker), vec![s1.id.clone()]);

        let g2 = tracker.begin_session_request(Some(&s2));
        assert_eq!(ids(&tracker), vec![s2.id.clone(), s1.id.clone()]);

        let g3 = tracker.begin_session_request(Some(&s3));
        assert_eq!(
            ids(&tracker),
            vec![s3.id.clone(), s2.id.clone(), s1.id.clone()]
        );

        // Sending a new request on older session s1 moves it to the leftmost slot
        let g1_b = tracker.begin_session_request(Some(&s1));
        assert_eq!(
            ids(&tracker),
            vec![s1.id.clone(), s3.id.clone(), s2.id.clone()]
        );

        // Completing an older in-flight request on s2 must NOT steal the leftmost slot from s1
        drop(g2);
        tracker.record_session_completed_request(
            Some(&s2),
            "gemini-4-argon",
            200,
            0,
            Duration::from_millis(10),
            None,
        );
        assert_eq!(
            ids(&tracker),
            vec![s1.id.clone(), s3.id.clone(), s2.id.clone()]
        );

        drop((g1, g1_b, g3));

        // Exiting s1 moves it to the right of active sessions s3 and s2
        tracker.mark_session_exited(&s1.id);
        assert_eq!(
            ids(&tracker),
            vec![s3.id.clone(), s2.id.clone(), s1.id.clone()]
        );

        // Exiting s2 puts both exited sessions on the right, ordered by recency (s1 more recent than s2)
        tracker.mark_session_exited(&s2.id);
        assert_eq!(
            ids(&tracker),
            vec![s3.id.clone(), s1.id.clone(), s2.id.clone()]
        );

        // Reviving exited s2 with a new request moves it back to leftmost
        let _g2_revived = tracker.begin_session_request(Some(&s2));
        assert_eq!(
            ids(&tracker),
            vec![s2.id.clone(), s3.id.clone(), s1.id.clone()]
        );
    }

    #[test]
    fn test_incremental_log_stream_cursor_and_ring_buffer() {
        let tracker = Arc::new(UsageTracker::default());
        let mut cursor = LogStreamCursor::default();

        // 1. Initial read on empty tracker returns empty Snapshot
        assert_eq!(
            tracker.next_log_stream_event(&mut cursor),
            Some(LogStreamEvent::Snapshot { entries: vec![] })
        );
        // No changes -> None
        assert_eq!(tracker.next_log_stream_event(&mut cursor), None);

        // 2. Appending logs returns only newly added entries as Append (batched)
        let s1 = SessionMeta {
            id: generate_session_uuid(),
            label: "alpha".to_string(),
            pid: None,
        };
        let s2 = SessionMeta {
            id: generate_session_uuid(),
            label: "beta".to_string(),
            pid: None,
        };
        let _g1 = tracker.begin_session_request(Some(&s1));
        let _g2 = tracker.begin_session_request(Some(&s2));
        drop(_g1);
        drop(_g2);
        tracker.record_log_with_session("INFO", "agy_gyro", Some(s1.id.clone()), "msg 1");
        tracker.record_log_with_session("WARN", "agy_gyro", Some(s2.id.clone()), "msg 2");
        tracker.record_log_with_session("INFO", "agy_gyro", Some(s1.id.clone()), "msg 3");

        match tracker.next_log_stream_event(&mut cursor) {
            Some(LogStreamEvent::Append { entries }) => {
                let msgs: Vec<&str> = entries.iter().map(|e| e.message.as_str()).collect();
                assert_eq!(msgs, vec!["msg 1", "msg 2", "msg 3"]);
            }
            other => panic!("expected Append, got {:?}", other),
        }
        assert_eq!(tracker.next_log_stream_event(&mut cursor), None);

        // 3. Per-session reset removes only that session's logs and resyncs via Snapshot
        tracker.reset_session(&s2.id);
        match tracker.next_log_stream_event(&mut cursor) {
            Some(LogStreamEvent::Snapshot { entries }) => {
                let msgs: Vec<&str> = entries.iter().map(|e| e.message.as_str()).collect();
                assert_eq!(msgs, vec!["msg 1", "msg 3"]);
            }
            other => panic!("expected Snapshot after reset_session, got {:?}", other),
        }

        // Subsequent append after reset_session resumes incremental Append events
        tracker.record_log("INFO", "agy_gyro", "msg 4");
        match tracker.next_log_stream_event(&mut cursor) {
            Some(LogStreamEvent::Append { entries }) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].message, "msg 4");
            }
            other => panic!("expected Append after reset_session, got {:?}", other),
        }

        // 4. Global reset clears buffer and resyncs via empty Snapshot
        tracker.reset();
        assert_eq!(
            tracker.next_log_stream_event(&mut cursor),
            Some(LogStreamEvent::Snapshot { entries: vec![] })
        );

        // 5. Ring buffer holds up to MAX_LOG_ENTRIES (> 200) and evicts oldest on overflow.
        //    An up-to-date subscriber still receives Append when only already-seen entries are evicted.
        const { assert!(MAX_LOG_ENTRIES >= 10_000) };
        for i in 1..=MAX_LOG_ENTRIES {
            tracker.record_log("INFO", "agy_gyro", format!("bulk {}", i));
        }
        match tracker.next_log_stream_event(&mut cursor) {
            Some(LogStreamEvent::Append { entries }) => {
                assert_eq!(entries.len(), MAX_LOG_ENTRIES);
                assert_eq!(entries.first().unwrap().id, 1);
                assert_eq!(entries.last().unwrap().id, MAX_LOG_ENTRIES as u64);
            }
            other => panic!("expected full Append batch, got {:?}", other),
        }

        // Pushing 3 more evicts entries 1..=3 (which cursor already saw) -> still Append!
        for i in 1..=3 {
            tracker.record_log("INFO", "agy_gyro", format!("overflow {}", i));
        }
        assert_eq!(tracker.logs_snapshot().len(), MAX_LOG_ENTRIES);
        assert_eq!(tracker.logs_snapshot().first().unwrap().id, 4);
        match tracker.next_log_stream_event(&mut cursor) {
            Some(LogStreamEvent::Append { entries }) => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].id, MAX_LOG_ENTRIES as u64 + 1);
                assert_eq!(entries[2].id, MAX_LOG_ENTRIES as u64 + 3);
            }
            other => panic!(
                "expected Append when only seen entries were evicted, got {:?}",
                other
            ),
        }

        // 6. Lagged subscriber (more than MAX_LOG_ENTRIES added without reading) resyncs via Snapshot
        for i in 1..=(MAX_LOG_ENTRIES + 5) {
            tracker.record_log("INFO", "agy_gyro", format!("lag {}", i));
        }
        match tracker.next_log_stream_event(&mut cursor) {
            Some(LogStreamEvent::Snapshot { entries }) => {
                assert_eq!(entries.len(), MAX_LOG_ENTRIES);
                assert_eq!(
                    entries.last().unwrap().message,
                    format!("lag {}", MAX_LOG_ENTRIES + 5)
                );
            }
            other => panic!("expected Snapshot resync on lag, got {:?}", other),
        }
    }
}
