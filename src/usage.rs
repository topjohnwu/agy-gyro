// SPDX-License-Identifier: MIT

use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Default URL for fetching live model pricing (`models.dev` API).
pub const DEFAULT_PRICING_URL: &str = "https://models.dev/api.json";

/// Maximum number of real-time log entries retained in the log ring buffer.
const MAX_LOG_ENTRIES: usize = 200;

/// Token threshold (200k input tokens) above which tiered pricing applies on supported models.
const TIER_200K_THRESHOLD: u64 = 200_000;

/// Per-1M-token pricing rates in USD for a Gemini model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// USD per 1M uncached input tokens (<= 200k input tokens).
    pub input_per_1m: f64,
    /// USD per 1M cached input tokens (<= 200k input tokens).
    pub cached_per_1m: f64,
    /// USD per 1M output + thinking tokens (<= 200k input tokens).
    pub output_per_1m: f64,
    /// Optional USD per 1M uncached input tokens (> 200k input tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_over_200k_per_1m: Option<f64>,
    /// Optional USD per 1M cached input tokens (> 200k input tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_over_200k_per_1m: Option<f64>,
    /// Optional USD per 1M output + thinking tokens (> 200k input tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_over_200k_per_1m: Option<f64>,
}

/// Calculated USD cost breakdown for a single request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CostBreakdown {
    /// Cost of uncached input tokens in USD.
    pub uncached_input_cost_usd: f64,
    /// Cost of cached input tokens in USD.
    pub cached_input_cost_usd: f64,
    /// Total input cost (`uncached_input_cost_usd + cached_input_cost_usd`) in USD.
    pub input_cost_usd: f64,
    /// Cost of output tokens (candidates + thoughts) in USD.
    pub output_cost_usd: f64,
    /// Total cost (`input_cost_usd + output_cost_usd`) in USD.
    pub total_cost_usd: f64,
    /// Estimated USD saved by context caching compared to full uncached input pricing.
    pub cache_savings_usd: f64,
}

impl ModelPricing {
    pub const fn flat(input_per_1m: f64, cached_per_1m: f64, output_per_1m: f64) -> Self {
        Self {
            input_per_1m,
            cached_per_1m,
            output_per_1m,
            input_over_200k_per_1m: None,
            cached_over_200k_per_1m: None,
            output_over_200k_per_1m: None,
        }
    }

    pub const fn tiered(
        input_per_1m: f64,
        cached_per_1m: f64,
        output_per_1m: f64,
        input_over_200k_per_1m: f64,
        cached_over_200k_per_1m: f64,
        output_over_200k_per_1m: f64,
    ) -> Self {
        Self {
            input_per_1m,
            cached_per_1m,
            output_per_1m,
            input_over_200k_per_1m: Some(input_over_200k_per_1m),
            cached_over_200k_per_1m: Some(cached_over_200k_per_1m),
            output_over_200k_per_1m: Some(output_over_200k_per_1m),
        }
    }

    /// Calculates the USD cost breakdown for a request given its token counts.
    pub fn calculate_cost(
        &self,
        uncached_input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    ) -> CostBreakdown {
        let total_input = uncached_input_tokens.saturating_add(cached_input_tokens);
        let over_200k = total_input > TIER_200K_THRESHOLD;

        let input_rate = if over_200k {
            self.input_over_200k_per_1m.unwrap_or(self.input_per_1m)
        } else {
            self.input_per_1m
        };
        let cached_rate = if over_200k {
            self.cached_over_200k_per_1m.unwrap_or(self.cached_per_1m)
        } else {
            self.cached_per_1m
        };
        let output_rate = if over_200k {
            self.output_over_200k_per_1m.unwrap_or(self.output_per_1m)
        } else {
            self.output_per_1m
        };

        let uncached_input_cost_usd = (uncached_input_tokens as f64 / 1_000_000.0) * input_rate;
        let cached_input_cost_usd = (cached_input_tokens as f64 / 1_000_000.0) * cached_rate;
        let input_cost_usd = uncached_input_cost_usd + cached_input_cost_usd;
        let output_cost_usd = (output_tokens as f64 / 1_000_000.0) * output_rate;
        let total_cost_usd = input_cost_usd + output_cost_usd;
        let cache_savings_usd =
            ((cached_input_tokens as f64 / 1_000_000.0) * (input_rate - cached_rate)).max(0.0);

        CostBreakdown {
            uncached_input_cost_usd,
            cached_input_cost_usd,
            input_cost_usd,
            output_cost_usd,
            total_cost_usd,
            cache_savings_usd,
        }
    }
}

/// Pricing lookup table mapping Gemini model IDs to [`ModelPricing`].
#[derive(Debug, Clone)]
pub struct PricingTable {
    pub models: HashMap<String, ModelPricing>,
    pub source: String,
}

impl Default for PricingTable {
    fn default() -> Self {
        Self::built_in()
    }
}

impl PricingTable {
    /// Returns the built-in fallback pricing table for Gemini models.
    pub fn built_in() -> Self {
        let mut models = HashMap::new();

        // Gemini 4 series
        models.insert(
            "gemini-4-argon".to_string(),
            ModelPricing::flat(2.0, 0.1, 10.0),
        );

        // Gemini 3.x Flash series
        for id in [
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-flash-latest",
        ] {
            models.insert(id.to_string(), ModelPricing::flat(0.75, 0.075, 3.75));
        }
        models.insert(
            "gemini-3.5-flash".to_string(),
            ModelPricing::flat(1.5, 0.15, 9.0),
        );
        for id in ["gemini-3.5-flash-lite", "gemini-flash-lite-latest"] {
            models.insert(id.to_string(), ModelPricing::flat(0.3, 0.03, 2.5));
        }
        models.insert(
            "gemini-3-flash-preview".to_string(),
            ModelPricing::flat(0.5, 0.05, 3.0),
        );
        for id in ["gemini-3.1-flash-lite", "gemini-3.1-flash-lite-preview"] {
            models.insert(id.to_string(), ModelPricing::flat(0.25, 0.025, 1.5));
        }

        // Gemini 3.1 Pro series (tiered at 200k tokens)
        for id in [
            "gemini-3.1-pro-preview",
            "gemini-3.1-pro-preview-customtools",
            "gemini-pro-latest",
            "deep-research-preview-04-2026",
            "deep-research-max-preview-04-2026",
        ] {
            models.insert(
                id.to_string(),
                ModelPricing::tiered(2.0, 0.2, 12.0, 4.0, 0.4, 18.0),
            );
        }

        // Gemini 2.5 series
        models.insert(
            "gemini-2.5-pro".to_string(),
            ModelPricing::tiered(1.25, 0.125, 10.0, 2.5, 0.25, 15.0),
        );
        models.insert(
            "gemini-2.5-flash".to_string(),
            ModelPricing::flat(0.3, 0.03, 2.5),
        );
        models.insert(
            "gemini-2.5-flash-lite".to_string(),
            ModelPricing::flat(0.1, 0.01, 0.4),
        );

        Self {
            models,
            source: "built-in".to_string(),
        }
    }

    /// Resolves pricing for a model ID, normalizing prefixes (`models/`) and effort suffixes
    /// (`-low`, `-medium`, `-high`) or matching known model family prefixes.
    pub fn lookup(&self, raw_model: &str) -> Option<ModelPricing> {
        let cleaned = normalize_model_id(raw_model);
        if let Some(p) = self.models.get(&cleaned) {
            return Some(p.clone());
        }

        // Try stripping effort suffixes (-low, -medium, -high)
        for suffix in ["-low", "-medium", "-high"] {
            if let Some(stripped) = cleaned.strip_suffix(suffix)
                && let Some(p) = self.models.get(stripped)
            {
                return Some(p.clone());
            }
        }

        // Try matching by longest prefix in the pricing table (e.g. "gemini-2.5-pro-preview-05-06")
        let mut best_match: Option<(&str, &ModelPricing)> = None;
        for (key, pricing) in &self.models {
            if cleaned.starts_with(key.as_str())
                && best_match.is_none_or(|(best_k, _)| key.len() > best_k.len())
            {
                best_match = Some((key.as_str(), pricing));
            }
        }
        if let Some((_, pricing)) = best_match {
            return Some(pricing.clone());
        }

        None
    }
}

/// Normalizes a Gemini model identifier by stripping `models/` prefix and effort suffixes
/// if the base model is standard.
pub fn normalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = trimmed.strip_prefix("models/").unwrap_or(trimmed);
    for suffix in ["-low", "-medium", "-high"] {
        if let Some(base) = without_prefix.strip_suffix(suffix)
            && base.starts_with("gemini-")
        {
            return base.to_string();
        }
    }
    without_prefix.to_string()
}

/// Extracts the model name from a Gemini API request path
/// (e.g. `/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse` -> `gemini-4-argon`).
pub fn extract_model_from_path(path_and_query: &str) -> Option<String> {
    let path = path_and_query.split('?').next().unwrap_or(path_and_query);
    let after_models = path.split("/models/").nth(1)?;
    let model_part = after_models.split([':', '/']).next()?;
    if model_part.is_empty() {
        None
    } else {
        Some(normalize_model_id(model_part))
    }
}

#[derive(Debug, Deserialize)]
struct ModelsDevResponse {
    #[serde(default)]
    google: Option<ModelsDevProvider>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: HashMap<String, ModelsDevEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevEntry {
    #[serde(default)]
    cost: Option<ModelsDevCost>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    context_over_200k: Option<ModelsDevTierCost>,
    #[serde(default)]
    tiers: Vec<ModelsDevTierEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevTierCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevTierEntry {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
}

/// Parses a `models.dev/api.json` JSON payload and merges Google model pricing into the built-in table.
pub fn parse_models_dev_pricing(json_str: &str) -> Result<PricingTable, String> {
    let parsed: ModelsDevResponse =
        serde_json::from_str(json_str).map_err(|e| format!("Invalid pricing JSON: {}", e))?;

    let mut table = PricingTable::built_in();
    let mut updated_count = 0usize;

    if let Some(google) = parsed.google {
        for (model_id, entry) in google.models {
            if let Some(cost) = entry.cost
                && let (Some(input), Some(output)) = (cost.input, cost.output)
            {
                let cached = cost.cache_read.unwrap_or(0.0);

                let over_200k = cost.context_over_200k.or_else(|| {
                    cost.tiers.into_iter().next().map(|t| ModelsDevTierCost {
                        input: t.input,
                        output: t.output,
                        cache_read: t.cache_read,
                    })
                });

                let pricing = if let Some(tier) = over_200k {
                    ModelPricing {
                        input_per_1m: input,
                        cached_per_1m: cached,
                        output_per_1m: output,
                        input_over_200k_per_1m: tier.input,
                        cached_over_200k_per_1m: tier.cache_read,
                        output_over_200k_per_1m: tier.output,
                    }
                } else {
                    ModelPricing::flat(input, cached, output)
                };

                table.models.insert(model_id, pricing);
                updated_count += 1;
            }
        }
    }

    if updated_count > 0 {
        table.source = "models.dev (live)".to_string();
    }

    Ok(table)
}

/// Fetches live Gemini model pricing from `pricing_url` (usually `https://models.dev/api.json`),
/// falling back gracefully to [`PricingTable::built_in`] on network or parse error.
pub async fn fetch_model_pricing(client: &reqwest::Client, pricing_url: &str) -> PricingTable {
    match client
        .get(pricing_url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => match res.text().await {
            Ok(text) => match parse_models_dev_pricing(&text) {
                Ok(table) => {
                    info!(
                        "Loaded live pricing for {} models from {}",
                        table.models.len(),
                        pricing_url
                    );
                    table
                }
                Err(err) => {
                    warn!(
                        "Failed to parse live pricing from {}: {}; using built-in pricing table",
                        pricing_url, err
                    );
                    PricingTable::built_in()
                }
            },
            Err(err) => {
                warn!(
                    "Failed to read live pricing response from {}: {}; using built-in pricing table",
                    pricing_url, err
                );
                PricingTable::built_in()
            }
        },
        Ok(res) => {
            warn!(
                "Live pricing endpoint {} returned status {}; using built-in pricing table",
                pricing_url,
                res.status()
            );
            PricingTable::built_in()
        }
        Err(err) => {
            debug!(
                "Could not fetch live pricing from {}: {}; using built-in pricing table",
                pricing_url, err
            );
            PricingTable::built_in()
        }
    }
}

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

/// A single real-time tracing log entry streamed by `GET /logs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp_secs: u64,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Point-in-time JSON payload streamed by `GET /usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub uptime_secs: u64,
    pub active_requests: u64,
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub total_retries: u64,
    pub pricing_source: String,
    pub totals: TokenUsageTotals,
    pub models: BTreeMap<String, ModelUsageStats>,
}

#[derive(Debug, Default)]
struct UsageInner {
    totals: TokenUsageTotals,
    models: BTreeMap<String, ModelUsageStats>,
    logs: VecDeque<LogEntry>,
}

/// Thread-safe live usage, cost, and log tracker shared across proxy handlers.
pub struct UsageTracker {
    started_at: Instant,
    active_requests: AtomicU64,
    total_requests: AtomicU64,
    successful_requests: AtomicU64,
    failed_requests: AtomicU64,
    total_retries: AtomicU64,
    next_request_id: AtomicU64,
    next_log_id: AtomicU64,
    pricing: RwLock<PricingTable>,
    inner: RwLock<UsageInner>,
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
            active_requests: AtomicU64::new(0),
            total_requests: AtomicU64::new(0),
            successful_requests: AtomicU64::new(0),
            failed_requests: AtomicU64::new(0),
            total_retries: AtomicU64::new(0),
            next_request_id: AtomicU64::new(1),
            next_log_id: AtomicU64::new(1),
            pricing: RwLock::new(pricing),
            inner: RwLock::new(UsageInner::default()),
            usage_change_tx,
            log_change_tx,
        }
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

    /// Appends a real-time log entry and notifies `GET /logs` SSE subscribers.
    pub fn record_log(&self, level: &str, target: &str, message: impl Into<String>) {
        let log_id = self.next_log_id.fetch_add(1, Ordering::Relaxed);
        let timestamp_secs = self.started_at.elapsed().as_secs();
        let entry = LogEntry {
            id: log_id,
            timestamp_secs,
            level: level.to_string(),
            target: target.to_string(),
            message: message.into(),
        };

        if let Ok(mut inner) = self.inner.write() {
            inner.logs.push_back(entry);
            while inner.logs.len() > MAX_LOG_ENTRIES {
                inner.logs.pop_front();
            }
        }
        self.notify_log_change();
    }

    /// Returns a snapshot of all buffered log entries for the `/logs` endpoint.
    pub fn logs_snapshot(&self) -> Vec<LogEntry> {
        match self.inner.read() {
            Ok(inner) => inner.logs.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Replaces the pricing table (e.g. after live pricing discovery completes).
    pub fn set_pricing(&self, pricing: PricingTable) {
        if let Ok(mut guard) = self.pricing.write() {
            *guard = pricing;
        }
        self.notify_usage_change();
    }

    /// Marks the start of an in-flight proxy request and returns an RAII guard that
    /// decrements `active_requests` when dropped.
    pub fn begin_request(self: &Arc<Self>) -> ActiveRequestGuard {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.active_requests.fetch_add(1, Ordering::Relaxed);
        self.notify_usage_change();
        ActiveRequestGuard {
            tracker: Arc::clone(self),
        }
    }

    /// Increments the global retry counter when a request attempt is retried.
    pub fn record_retry(&self) {
        self.total_retries.fetch_add(1, Ordering::Relaxed);
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
        if (200..300).contains(&status) {
            self.successful_requests.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_requests.fetch_add(1, Ordering::Relaxed);
        }

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

        if let Ok(mut inner) = self.inner.write() {
            inner.totals.record(&usage, &cost, retries);

            let model_entry = inner
                .models
                .entry(canonical_model.clone())
                .or_insert_with(|| ModelUsageStats {
                    model: canonical_model.clone(),
                    totals: TokenUsageTotals::default(),
                    pricing: pricing.clone(),
                });
            if model_entry.pricing.is_none() && pricing.is_some() {
                model_entry.pricing = pricing;
            }
            model_entry.totals.record(&usage, &cost, retries);
        }

        info!(
            "Request #{} [{}] status={} duration={}ms retries={} | in={} (uncached={}, cached={}) out={} (think={}) | cost=${:.6}",
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
            cost.total_cost_usd
        );
        self.notify_usage_change();
    }

    /// Resets all accumulated session metrics and buffered logs while preserving `active_requests` and `pricing`.
    pub fn reset(&self) {
        self.total_requests.store(0, Ordering::Relaxed);
        self.successful_requests.store(0, Ordering::Relaxed);
        self.failed_requests.store(0, Ordering::Relaxed);
        self.total_retries.store(0, Ordering::Relaxed);
        self.next_request_id.store(1, Ordering::Relaxed);
        self.next_log_id.store(1, Ordering::Relaxed);
        if let Ok(mut inner) = self.inner.write() {
            *inner = UsageInner::default();
        }
        self.notify_usage_change();
        self.notify_log_change();
    }

    /// Returns a point-in-time [`UsageSnapshot`] for the `/usage` endpoint.
    pub fn snapshot(&self) -> UsageSnapshot {
        let pricing_source = self
            .pricing
            .read()
            .map(|p| p.source.clone())
            .unwrap_or_else(|_| "built-in".to_string());

        let (totals, models) = match self.inner.read() {
            Ok(inner) => (inner.totals.clone(), inner.models.clone()),
            Err(_) => (TokenUsageTotals::default(), BTreeMap::new()),
        };

        UsageSnapshot {
            uptime_secs: self.started_at.elapsed().as_secs(),
            active_requests: self.active_requests.load(Ordering::Relaxed),
            total_requests: self.total_requests.load(Ordering::Relaxed),
            successful_requests: self.successful_requests.load(Ordering::Relaxed),
            failed_requests: self.failed_requests.load(Ordering::Relaxed),
            total_retries: self.total_retries.load(Ordering::Relaxed),
            pricing_source,
            totals,
            models,
        }
    }
}

fn global_log_trackers() -> &'static RwLock<Vec<Weak<UsageTracker>>> {
    static TRACKERS: OnceLock<RwLock<Vec<Weak<UsageTracker>>>> = OnceLock::new();
    TRACKERS.get_or_init(|| RwLock::new(Vec::new()))
}

#[derive(Default)]
struct LogEventVisitor {
    message: String,
    fields: Vec<String>,
}

impl tracing::field::Visit for LogEventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields.push(format!("{}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(format!("{}={}", field.name(), value));
        }
    }
}

/// A `tracing_subscriber::Layer` that routes `agy_gyro` log events into active [`UsageTracker`] instances.
pub struct UsageLogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for UsageLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
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

        let text = if visitor.fields.is_empty() {
            visitor.message
        } else if visitor.message.is_empty() {
            visitor.fields.join(" ")
        } else {
            format!("{} {}", visitor.message, visitor.fields.join(" "))
        };

        if !text.is_empty() {
            for tracker in trackers {
                tracker.record_log(meta.level().as_str(), meta.target(), text.clone());
            }
        }
    }
}

pub struct ActiveRequestGuard {
    tracker: Arc<UsageTracker>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.tracker.active_requests.fetch_sub(1, Ordering::Relaxed);
        self.tracker.notify_usage_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_model_from_path() {
        assert_eq!(
            extract_model_from_path("/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse"),
            Some("gemini-4-argon".to_string())
        );
        assert_eq!(
            extract_model_from_path("/v1beta/models/gemini-2.5-pro-high:generateContent"),
            Some("gemini-2.5-pro".to_string())
        );
        assert_eq!(extract_model_from_path("/v1beta/models"), None);
    }

    #[test]
    fn test_model_pricing_calculation_flat_and_tiered() {
        let argon = PricingTable::built_in().lookup("gemini-4-argon").unwrap();
        // 100k uncached ($2/1M = $0.20), 400k cached ($0.10/1M = $0.04), 50k output ($10/1M = $0.50)
        let cost = argon.calculate_cost(100_000, 400_000, 50_000);
        assert!((cost.uncached_input_cost_usd - 0.20).abs() < 1e-9);
        assert!((cost.cached_input_cost_usd - 0.04).abs() < 1e-9);
        assert!((cost.output_cost_usd - 0.50).abs() < 1e-9);
        assert!((cost.total_cost_usd - 0.74).abs() < 1e-9);
        assert!((cost.cache_savings_usd - 0.76).abs() < 1e-9);

        // Tiered model: gemini-2.5-pro (<=200k: $1.25 / $0.125 / $10.0; >200k: $2.50 / $0.25 / $15.0)
        let pro = PricingTable::built_in().lookup("gemini-2.5-pro").unwrap();
        let under_200k = pro.calculate_cost(100_000, 50_000, 10_000);
        assert!((under_200k.uncached_input_cost_usd - 0.125).abs() < 1e-9);
        assert!((under_200k.cached_input_cost_usd - 0.00625).abs() < 1e-9);
        assert!((under_200k.output_cost_usd - 0.10).abs() < 1e-9);

        let over_200k = pro.calculate_cost(150_000, 100_000, 10_000); // 250k total input > 200k
        assert!((over_200k.uncached_input_cost_usd - 0.375).abs() < 1e-9);
        assert!((over_200k.cached_input_cost_usd - 0.025).abs() < 1e-9);
        assert!((over_200k.output_cost_usd - 0.15).abs() < 1e-9);
    }

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
    fn test_parse_models_dev_pricing() {
        let sample_json = r#"{
            "google": {
                "models": {
                    "gemini-custom-test": {
                        "cost": {
                            "input": 1.5,
                            "output": 6.0,
                            "cache_read": 0.15,
                            "context_over_200k": {
                                "input": 3.0,
                                "output": 12.0,
                                "cache_read": 0.30
                            }
                        }
                    }
                }
            }
        }"#;

        let table = parse_models_dev_pricing(sample_json).unwrap();
        assert_eq!(table.source, "models.dev (live)");
        let custom = table.lookup("gemini-custom-test-high").unwrap();
        assert_eq!(custom.input_per_1m, 1.5);
        assert_eq!(custom.cached_per_1m, 0.15);
        assert_eq!(custom.output_per_1m, 6.0);
        assert_eq!(custom.input_over_200k_per_1m, Some(3.0));
        assert_eq!(custom.cached_over_200k_per_1m, Some(0.30));
        assert_eq!(custom.output_over_200k_per_1m, Some(12.0));
    }
}
