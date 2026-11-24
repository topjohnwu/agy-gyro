// SPDX-License-Identifier: MIT

use crate::config::Config;
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Default GitHub raw URL for fetching live model discovery rules (`models.toml`).
pub const DEFAULT_MODEL_RULES_URL: &str =
    "https://raw.githubusercontent.com/topjohnwu/agy-gyro/main/models.toml";

/// Bundled `models.toml` embedded at compile time as the offline fallback when GitHub is unreachable.
const BUNDLED_MODEL_RULES_TOML: &str = include_str!("../models.toml");

/// Protocol-level effort suffixes recognized in Gemini request paths and model IDs.
const KNOWN_EFFORT_LEVELS: &[&str] = &["high", "xhigh", "medium", "low"];

/// Pattern-matched effort suffix override rule loaded from `models.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EffortRule {
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    pub efforts: Vec<String>,
}

/// Synthetic Orchestrator synthesis and fallback routing rules loaded from `models.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrchestratorRules {
    pub model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub planner_models: Vec<String>,
    #[serde(default)]
    pub executor_models: Vec<String>,
}

/// Model discovery mappings loaded from `models.toml` (or fetched dynamically from GitHub).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelRules {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub non_core_keywords: Vec<String>,
    pub default_efforts: Vec<String>,
    #[serde(default)]
    pub effort_rules: Vec<EffortRule>,
    pub orchestrator: OrchestratorRules,
}

impl ModelRules {
    /// Returns the compile-time bundled [`ModelRules`] parsed from `models.toml`.
    pub fn bundled() -> &'static Self {
        static BUNDLED: OnceLock<ModelRules> = OnceLock::new();
        BUNDLED.get_or_init(|| {
            Self::parse(BUNDLED_MODEL_RULES_TOML).expect("bundled models.toml must be valid")
        })
    }

    /// Parses and validates a `models.toml` payload, ignoring unknown future keys.
    pub fn parse(toml_str: &str) -> Result<Self, String> {
        let rules: Self =
            toml::from_str(toml_str).map_err(|e| format!("Invalid model rules TOML: {}", e))?;
        let has_blank =
            |list: &[String]| list.is_empty() || list.iter().any(|s| s.trim().is_empty());
        if has_blank(&rules.default_efforts)
            || rules.effort_rules.iter().any(|r| has_blank(&r.efforts))
            || rules.orchestrator.model_id.trim().is_empty()
            || rules.orchestrator.display_name.trim().is_empty()
        {
            return Err("Model rules TOML contains empty required fields".to_string());
        }
        Ok(rules)
    }

    /// Returns the ordered thinking effort suffixes configured for `base_id`.
    pub fn effort_levels_for_model(&self, base_id: &str) -> &[String] {
        for rule in &self.effort_rules {
            if rule
                .patterns
                .iter()
                .any(|p| !p.is_empty() && base_id.contains(p.as_str()))
            {
                return &rule.efforts;
            }
        }
        &self.default_efforts
    }
}

/// Fetches live model discovery rules (`models.toml`) from `rules_url` (usually GitHub),
/// returning `None` on network, HTTP, or TOML parse/validation error.
pub async fn try_fetch_model_rules(
    client: &reqwest::Client,
    rules_url: &str,
) -> Option<ModelRules> {
    let res = match client
        .get(rules_url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => {
            warn!(
                "Model discovery rules endpoint {} returned status {}; keeping current rules",
                rules_url,
                res.status()
            );
            return None;
        }
        Err(err) => {
            debug!(
                "Could not fetch model discovery rules from {}: {}; keeping current rules",
                rules_url, err
            );
            return None;
        }
    };

    let text = match res.text().await {
        Ok(text) => text,
        Err(err) => {
            warn!(
                "Failed to read model discovery rules response from {}: {}; keeping current rules",
                rules_url, err
            );
            return None;
        }
    };

    match ModelRules::parse(&text) {
        Ok(rules) => Some(rules),
        Err(err) => {
            warn!(
                "Failed to parse model discovery rules from {}: {}; keeping current rules",
                rules_url, err
            );
            None
        }
    }
}

/// Response payload from upstream `GET /v1beta/models` and served by the local gateway's
/// `GET /v1beta/models` discovery endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelsResponse {
    #[serde(default)]
    pub models: Vec<GeminiModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,
}

/// Model metadata entry in [`GeminiModelsResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_token_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_generation_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
}

/// Type alias for responses served by the local gateway's `GET /v1beta/models` endpoint.
pub type GatewayModelsResponse = GeminiModelsResponse;

/// Type alias for individual model entries in [`GatewayModelsResponse`].
pub type GatewayModelEntry = GeminiModelInfo;

impl GeminiModelInfo {
    /// Returns the bare model identifier with any leading `"models/"` prefix stripped.
    pub fn id(&self) -> &str {
        self.name.trim().trim_start_matches("models/")
    }
}

fn capitalize_ascii_word(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => {
            let mut s = String::with_capacity(word.len());
            s.push(first.to_ascii_uppercase());
            s.push_str(&chars.as_str().to_ascii_lowercase());
            s
        }
        None => String::new(),
    }
}

fn format_effort_label(effort: &str) -> String {
    match effort {
        "xhigh" => "XHigh".to_string(),
        _ => capitalize_ascii_word(effort),
    }
}

/// Converts a base Gemini model ID (e.g. `gemini-4-argon` or `gemini-3.5-flash-lite`) into a
/// human-readable title (`Gemini 4 Argon`, `Gemini 3.5 Flash Lite`).
pub fn humanize_model_id(base_id: &str) -> String {
    let clean = base_id.trim().trim_start_matches("models/");
    clean
        .split('-')
        .filter(|part| !part.is_empty())
        .map(capitalize_ascii_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Splits a trailing `-<effort>` suffix (where `<effort>` is a recognized effort level such as
/// `low`, `medium`, `high`, or `xhigh`) from a model identifier, returning `(base_model, effort)`.
pub fn split_effort_suffix(model_id: &str) -> Option<(&str, &'static str)> {
    for &effort in KNOWN_EFFORT_LEVELS {
        if let Some(prefix) = model_id.strip_suffix(effort)
            && let Some(base) = prefix.strip_suffix('-')
            && !base.is_empty()
        {
            return Some((base, effort));
        }
    }
    None
}

/// Strips a trailing `-<effort>` suffix if present.
pub fn strip_effort_suffix(model_id: &str) -> Option<&str> {
    split_effort_suffix(model_id).map(|(base, _)| base)
}

/// Normalizes a model identifier by stripping `models/` prefix and effort suffixes
/// (`-low`, `-medium`, `-high`, `-xhigh`) if the base model is a Gemini model.
pub fn normalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = trimmed.strip_prefix("models/").unwrap_or(trimmed);
    if let Some(base) = strip_effort_suffix(without_prefix)
        && base.starts_with("gemini-")
    {
        return base.to_string();
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

struct ModelsPathParts<'a> {
    prefix: &'a str,
    model: &'a str,
    rest_path: &'a str,
    query: Option<&'a str>,
}

impl<'a> ModelsPathParts<'a> {
    fn parse(path_and_query: &'a str) -> Option<Self> {
        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };
        let (prefix, after_models) = path.split_once("/models/")?;
        let sep_idx = after_models.find([':', '/']).unwrap_or(after_models.len());
        Some(Self {
            prefix,
            model: &after_models[..sep_idx],
            rest_path: &after_models[sep_idx..],
            query,
        })
    }

    fn format_with_model(&self, model: &str) -> String {
        match self.query {
            Some(q) => format!("{}/models/{}{}?{}", self.prefix, model, self.rest_path, q),
            None => format!("{}/models/{}{}", self.prefix, model, self.rest_path),
        }
    }
}

/// Strips a trailing `-<effort>` suffix from the model segment of a Gemini API request path
/// (e.g. `/v1beta/models/gemini-4-argon-medium:streamGenerateContent?alt=sse` ->
/// `("/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse", Some("medium"))`).
pub fn rewrite_request_path_and_extract_effort(
    path_and_query: &str,
) -> (String, Option<&'static str>) {
    if let Some(parts) = ModelsPathParts::parse(path_and_query)
        && let Some((base, effort)) = split_effort_suffix(parts.model)
        && base.starts_with("gemini-")
    {
        return (parts.format_with_model(base), Some(effort));
    }

    (path_and_query.to_string(), None)
}

/// Resolves the best available upstream Gemini planner model from `gateway_models` using
/// `rules` for fallback forwarding when a client sends the synthetic orchestrator model directly
/// to the proxy.
pub fn resolve_orchestrator_fallback_model(
    gateway_models: &[GatewayModelEntry],
    rules: &ModelRules,
) -> String {
    for preferred in &rules.orchestrator.planner_models {
        if gateway_models
            .iter()
            .any(|entry| normalize_model_id(entry.id()) == preferred.as_str())
        {
            return preferred.clone();
        }
    }
    for entry in gateway_models {
        let base = normalize_model_id(entry.id());
        if !base.is_empty()
            && base != rules.orchestrator.model_id
            && !base.ends_with("-image")
            && !base.ends_with("-lite")
        {
            return base;
        }
    }
    rules
        .orchestrator
        .planner_models
        .first()
        .cloned()
        .unwrap_or_else(|| rules.orchestrator.model_id.clone())
}

/// Rewrites a gateway request path for upstream forwarding using `rules` and extracts any
/// thinking effort.
///
/// - Requests targeting `rules.orchestrator.model_id` (bare or effort-suffixed) fall back to the
///   best available upstream planner model in `gateway_models()`, defaulting effort to `"high"`.
/// - Other effort-suffixed `gemini-*` models are stripped to their base ID with the extracted
///   effort level, without evaluating `gateway_models()`.
pub fn rewrite_proxy_request_path<F>(
    path_and_query: &str,
    rules: &ModelRules,
    gateway_models: F,
) -> (String, Option<&'static str>)
where
    F: FnOnce() -> Vec<GatewayModelEntry>,
{
    let Some(parts) = ModelsPathParts::parse(path_and_query) else {
        return (path_and_query.to_string(), None);
    };

    let (base, effort) = match split_effort_suffix(parts.model) {
        Some((b, e)) => (b, Some(e)),
        None => (parts.model, None),
    };

    if base == rules.orchestrator.model_id {
        let target_model = resolve_orchestrator_fallback_model(&gateway_models(), rules);
        return (
            parts.format_with_model(&target_model),
            Some(effort.unwrap_or("high")),
        );
    }

    if let Some(effort) = effort
        && base.starts_with("gemini-")
    {
        return (parts.format_with_model(base), Some(effort));
    }

    (path_and_query.to_string(), None)
}

/// Ensures `generationConfig.thinkingConfig.thinkingLevel` is set to `"LOW"`, `"MEDIUM"`,
/// `"HIGH"`, or `"XHIGH"` when a request arrived with an unstripped `-<effort>` model suffix and
/// the client did not already populate `thinkingLevel` (e.g. when using a newly released model that
/// falls back to `thinkingBudget` on the client).
pub fn inject_thinking_level_if_missing(body: Bytes, effort: &str) -> Bytes {
    if body.is_empty() {
        return body;
    }

    let level_str = match effort {
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" => "HIGH",
        "xhigh" => "XHIGH",
        _ => return body,
    };

    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };

    let Some(root_obj) = root.as_object_mut() else {
        return body;
    };

    let Some(gen_obj) = root_obj
        .get_mut("generationConfig")
        .and_then(|v| v.as_object_mut())
    else {
        return body;
    };

    let Some(thinking_obj) = gen_obj
        .get_mut("thinkingConfig")
        .and_then(|v| v.as_object_mut())
    else {
        return body;
    };

    // If the client already populated `thinkingLevel` (or `thinking_level`), keep the original payload untouched.
    if thinking_obj.contains_key("thinkingLevel") || thinking_obj.contains_key("thinking_level") {
        return body;
    }

    // Respect explicit thinking disablement (`includeThoughts: false` or `thinkingBudget: 0`).
    if thinking_obj
        .get("includeThoughts")
        .and_then(|v| v.as_bool())
        == Some(false)
        || thinking_obj
            .get("include_thoughts")
            .and_then(|v| v.as_bool())
            == Some(false)
        || thinking_obj.get("thinkingBudget").and_then(|v| v.as_i64()) == Some(0)
        || thinking_obj.get("thinking_budget").and_then(|v| v.as_i64()) == Some(0)
    {
        return body;
    }

    thinking_obj.remove("thinkingBudget");
    thinking_obj.remove("thinking_budget");
    thinking_obj
        .entry("includeThoughts")
        .or_insert(serde_json::Value::Bool(true));
    thinking_obj.insert(
        "thinkingLevel".to_string(),
        serde_json::Value::String(level_str.to_string()),
    );

    match serde_json::to_vec(&root) {
        Ok(vec) => Bytes::from(vec),
        Err(_) => body,
    }
}

/// Filters a list of `/v1beta/models` entries using `rules` to retain core Gemini Text /
/// Reasoning models (ordered newest-first with primary models before Flash models), followed by
/// the newest non-preview `-flash-image` and `-flash-lite` models if available.
pub fn filter_core_gemini_models(
    models: &[GeminiModelInfo],
    rules: &ModelRules,
) -> Vec<GeminiModelInfo> {
    let mut filtered: Vec<GeminiModelInfo> = models
        .iter()
        .filter(|m| {
            let id = m.id();
            if !id.starts_with("gemini-") {
                return false;
            }
            if m.thinking != Some(true) {
                return false;
            }
            if !m
                .supported_generation_methods
                .iter()
                .any(|method| method == "generateContent")
            {
                return false;
            }
            !rules
                .non_core_keywords
                .iter()
                .any(|kw| !kw.is_empty() && id.contains(kw.as_str()))
        })
        .cloned()
        .collect();

    // The Gemini API returns models oldest-first; reverse so newest models appear first,
    // then stable-sort so primary models always precede Flash models.
    filtered.reverse();
    filtered.sort_by_key(|m| m.id().contains("flash"));

    if !filtered.is_empty() {
        let latest_image = models
            .iter()
            .rev()
            .find(|m| {
                let id = m.id();
                id.starts_with("gemini-")
                    && id.ends_with("-flash-image")
                    && !id.contains("-preview")
                    && m.supported_generation_methods
                        .iter()
                        .any(|method| method == "generateContent")
            })
            .cloned();
        if let Some(image_model) = latest_image {
            filtered.push(image_model);
        }

        let latest_flash_lite = models
            .iter()
            .rev()
            .find(|m| {
                let id = m.id();
                id.starts_with("gemini-")
                    && id.ends_with("-flash-lite")
                    && !id.contains("-preview")
                    && m.thinking == Some(true)
                    && m.supported_generation_methods
                        .iter()
                        .any(|method| method == "generateContent")
            })
            .cloned();
        if let Some(lite_model) = latest_flash_lite {
            filtered.push(lite_model);
        }
    }

    filtered
}

/// Expands a list of base [`GeminiModelInfo`] entries into effort-suffixed [`GatewayModelEntry`]
/// entries according to `rules`, while passing auxiliary `-image`, `-lite`, and orchestrator
/// models through with their upstream metadata preserved.
pub fn expand_gateway_models(
    models: &[GeminiModelInfo],
    rules: &ModelRules,
) -> Vec<GatewayModelEntry> {
    let mut expanded = Vec::new();
    let orch_id = rules.orchestrator.model_id.as_str();

    for model in models {
        let base_id = model.id();
        if base_id.ends_with("-image") {
            let mut entry = model.clone();
            entry.name = format!("models/{}", base_id);
            // Omit `thinking` on image models so the client does not treat them as
            // thinking-capable even if upstream `/v1beta/models` reports `thinking: true`.
            entry.thinking = None;
            expanded.push(entry);
        } else if base_id.ends_with("-lite") || base_id == orch_id {
            let mut entry = model.clone();
            entry.name = format!("models/{}", base_id);
            if entry
                .display_name
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
            {
                entry.display_name = Some(if base_id == orch_id {
                    rules.orchestrator.display_name.clone()
                } else {
                    humanize_model_id(base_id)
                });
            }
            expanded.push(entry);
        } else {
            let base_display = model
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| humanize_model_id(base_id));
            let efforts = rules.effort_levels_for_model(base_id);
            for effort in efforts {
                expanded.push(GatewayModelEntry {
                    name: format!("models/{}-{}", base_id, effort),
                    display_name: Some(format!(
                        "{} ({})",
                        base_display,
                        format_effort_label(effort)
                    )),
                    description: model.description.clone(),
                    input_token_limit: model.input_token_limit,
                    output_token_limit: model.output_token_limit,
                    supported_generation_methods: model.supported_generation_methods.clone(),
                    thinking: model.thinking,
                });
            }
        }
    }

    let already_has_orchestrator = models.iter().any(|m| m.id() == orch_id);
    let preferred_planner = rules
        .orchestrator
        .planner_models
        .iter()
        .find_map(|id| models.iter().find(|m| normalize_model_id(m.id()) == *id));
    let has_executor = models.iter().any(|m| {
        let normalized = normalize_model_id(m.id());
        rules
            .orchestrator
            .executor_models
            .iter()
            .any(|exec| exec == &normalized)
    });

    if !already_has_orchestrator
        && let Some(planner) = preferred_planner
        && has_executor
    {
        expanded.push(GatewayModelEntry {
            name: format!("models/{}", orch_id),
            display_name: Some(rules.orchestrator.display_name.clone()),
            description: None,
            input_token_limit: planner.input_token_limit.or(Some(1_048_576)),
            output_token_limit: planner.output_token_limit.or(Some(65_536)),
            supported_generation_methods: vec!["generateContent".to_string()],
            thinking: Some(true),
        });
    }

    expanded
}

/// Queries `GET {upstream}/v1beta/models` (paginated) and returns all raw [`GeminiModelInfo`]
/// entries reported by the upstream Gemini API.
pub async fn fetch_upstream_gemini_models(
    client: &reqwest::Client,
    config: &Config,
) -> Result<Vec<GeminiModelInfo>, String> {
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
                let msg = format!(
                    "Failed to fetch models from {} (status {})",
                    url,
                    res.status()
                );
                warn!("{}", msg);
                return Err(msg);
            }
            Err(err) => {
                let msg = format!("Error fetching models from {}: {}", url, err);
                warn!("{}", msg);
                return Err(msg);
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
                let msg = format!("Failed to parse /v1beta/models JSON response: {}", err);
                warn!("{}", msg);
                return Err(msg);
            }
        }
    }

    Ok(all_models)
}

/// Filters `all_models` using `rules` and expands them into [`GatewayModelEntry`] items, or returns
/// an error if no matching core Gemini models remain.
pub fn build_gateway_models(
    all_models: &[GeminiModelInfo],
    rules: &ModelRules,
) -> Result<Vec<GatewayModelEntry>, String> {
    let models = filter_core_gemini_models(all_models, rules);
    if models.is_empty() {
        let msg = "Upstream /v1beta/models returned no matching core Gemini models".to_string();
        warn!("{}", msg);
        Err(msg)
    } else {
        let ids: Vec<&str> = models.iter().map(GeminiModelInfo::id).collect();
        info!("Discovered available Gemini models: {}", ids.join(","));
        Ok(expand_gateway_models(&models, rules))
    }
}

/// Queries `GET {upstream}/v1beta/models` and returns the list of active Gemini models
/// filtered and expanded according to `rules`, or an error if the backend request fails or yields
/// no matching core models.
pub async fn fetch_gateway_models(
    client: &reqwest::Client,
    config: &Config,
    rules: &ModelRules,
) -> Result<Vec<GatewayModelEntry>, String> {
    let all_models = fetch_upstream_gemini_models(client, config).await?;
    build_gateway_models(&all_models, rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model(id: &str) -> GeminiModelInfo {
        let is_image = id.ends_with("-image");
        GeminiModelInfo {
            name: format!("models/{}", id),
            display_name: None,
            description: None,
            input_token_limit: None,
            output_token_limit: None,
            supported_generation_methods: vec!["generateContent".to_string()],
            thinking: if is_image { None } else { Some(true) },
        }
    }

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
        assert_eq!(
            extract_model_from_path("/v1beta/models/gemini-4-hydrogen-xhigh:streamGenerateContent"),
            Some("gemini-4-hydrogen".to_string())
        );
        assert_eq!(
            extract_model_from_path("/v1beta/models/gemini-4-neon-xhigh:streamGenerateContent"),
            Some("gemini-4-neon".to_string())
        );
        assert_eq!(extract_model_from_path("/v1beta/models"), None);
    }

    #[test]
    fn test_expand_gateway_models_orders_efforts_and_preserves_upstream_metadata() {
        let base_models = [
            "gemini-4-neon",
            "gemini-4-carbon",
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-3.8-flash",
            "gemini-3.1-flash-image",
            "gemini-3.5-flash-lite",
        ]
        .into_iter()
        .map(test_model)
        .collect::<Vec<_>>();
        let expanded = expand_gateway_models(&base_models, ModelRules::bundled());
        let ids: Vec<&str> = expanded.iter().map(GeminiModelInfo::id).collect();
        assert_eq!(
            ids,
            vec![
                "gemini-4-neon-high",
                "gemini-4-neon-xhigh",
                "gemini-4-neon-medium",
                "gemini-4-neon-low",
                "gemini-4-carbon-medium",
                "gemini-4-carbon-high",
                "gemini-4-carbon-low",
                "gemini-4-hydrogen-high",
                "gemini-4-hydrogen-xhigh",
                "gemini-4-hydrogen-medium",
                "gemini-4-hydrogen-low",
                "gemini-4-argon-medium",
                "gemini-4-argon-high",
                "gemini-4-argon-low",
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "gemini-3.8-flash-low",
                "gemini-3.1-flash-image",
                "gemini-3.5-flash-lite",
                "gemini-pro-orchestrator",
            ]
        );
        assert_eq!(
            expanded[1].display_name.as_deref(),
            Some("Gemini 4 Neon (XHigh)")
        );
        assert_eq!(
            expanded[8].display_name.as_deref(),
            Some("Gemini 4 Hydrogen (XHigh)")
        );
        assert_eq!(
            expanded.last().and_then(|m| m.display_name.as_deref()),
            Some("Orchestrator")
        );

        let upstream = vec![
            GeminiModelInfo {
                name: "models/gemini-4-argon".to_string(),
                display_name: Some("Gemini 4 Argon".to_string()),
                description: Some("Primary reasoning model".to_string()),
                input_token_limit: Some(1_048_576),
                output_token_limit: Some(32_768),
                supported_generation_methods: vec![
                    "generateContent".to_string(),
                    "countTokens".to_string(),
                ],
                thinking: Some(true),
            },
            GeminiModelInfo {
                name: "models/gemini-4-hydrogen".to_string(),
                display_name: Some("Gemini 4 Hydrogen".to_string()),
                description: Some("Flagship reasoning model".to_string()),
                input_token_limit: Some(1_048_576),
                output_token_limit: Some(65_536),
                supported_generation_methods: vec![
                    "generateContent".to_string(),
                    "countTokens".to_string(),
                ],
                thinking: Some(true),
            },
            GeminiModelInfo {
                name: "models/gemini-4-neon".to_string(),
                display_name: Some("Gemini 4 Neon".to_string()),
                description: Some("Flagship reasoning model".to_string()),
                input_token_limit: Some(1_048_576),
                output_token_limit: Some(65_536),
                supported_generation_methods: vec![
                    "generateContent".to_string(),
                    "countTokens".to_string(),
                ],
                thinking: Some(true),
            },
            GeminiModelInfo {
                name: "models/gemini-3.1-flash-image".to_string(),
                display_name: Some("Nano Banana 2".to_string()),
                description: Some("Gemini 3.1 Flash Image".to_string()),
                input_token_limit: Some(65_536),
                output_token_limit: Some(65_536),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
            },
        ];

        let rich_expanded = expand_gateway_models(&upstream, ModelRules::bundled());
        assert_eq!(rich_expanded.len(), 13);
        assert_eq!(rich_expanded[0].name, "models/gemini-4-argon-medium");
        assert_eq!(
            rich_expanded[0].display_name.as_deref(),
            Some("Gemini 4 Argon (Medium)")
        );
        assert_eq!(
            rich_expanded[0].description.as_deref(),
            Some("Primary reasoning model")
        );
        assert_eq!(rich_expanded[0].input_token_limit, Some(1_048_576));
        assert_eq!(rich_expanded[0].output_token_limit, Some(32_768));
        assert_eq!(rich_expanded[0].thinking, Some(true));

        assert_eq!(rich_expanded[4].name, "models/gemini-4-hydrogen-xhigh");
        assert_eq!(
            rich_expanded[4].display_name.as_deref(),
            Some("Gemini 4 Hydrogen (XHigh)")
        );

        assert_eq!(rich_expanded[8].name, "models/gemini-4-neon-xhigh");
        assert_eq!(
            rich_expanded[8].display_name.as_deref(),
            Some("Gemini 4 Neon (XHigh)")
        );

        let image = &rich_expanded[11];
        assert_eq!(image.name, "models/gemini-3.1-flash-image");
        assert_eq!(image.display_name.as_deref(), Some("Nano Banana 2"));
        assert_eq!(image.input_token_limit, Some(65_536));
        assert_eq!(image.output_token_limit, Some(65_536));
        assert_eq!(image.thinking, None);

        let orchestrator = &rich_expanded[12];
        assert_eq!(orchestrator.name, "models/gemini-pro-orchestrator");
        assert_eq!(orchestrator.display_name.as_deref(), Some("Orchestrator"));
        assert_eq!(orchestrator.input_token_limit, Some(1_048_576));
        assert_eq!(orchestrator.output_token_limit, Some(65_536));
        assert_eq!(
            orchestrator.supported_generation_methods,
            vec!["generateContent".to_string()]
        );
        assert_eq!(orchestrator.thinking, Some(true));
    }

    #[test]
    fn test_expand_gateway_models_requires_planner_and_executor_for_orchestrator() {
        let rules = ModelRules::bundled();
        let orch_id = rules.orchestrator.model_id.as_str();

        let planner_only = expand_gateway_models(&[test_model("gemini-4-carbon")], rules);
        assert!(planner_only.iter().all(|m| m.id() != orch_id));

        let executor_only = expand_gateway_models(&[test_model("gemini-4-hydrogen")], rules);
        assert!(executor_only.iter().all(|m| m.id() != orch_id));

        let passthrough_orchestrator = expand_gateway_models(&[test_model(orch_id)], rules);
        assert_eq!(
            passthrough_orchestrator
                .iter()
                .map(GeminiModelInfo::id)
                .collect::<Vec<_>>(),
            vec![orch_id]
        );
    }

    #[test]
    fn test_rewrite_proxy_request_path_orchestrator_fallback() {
        let rules = ModelRules::bundled();
        let with_carbon_and_argon = expand_gateway_models(
            &[
                test_model("gemini-4-argon"),
                test_model("gemini-4-carbon"),
                test_model("gemini-4-hydrogen"),
            ],
            rules,
        );
        assert_eq!(
            rewrite_proxy_request_path(
                "/v1beta/models/gemini-pro-orchestrator:streamGenerateContent?alt=sse",
                rules,
                || with_carbon_and_argon.clone()
            ),
            (
                "/v1beta/models/gemini-4-carbon:streamGenerateContent?alt=sse".to_string(),
                Some("high")
            )
        );

        let with_neon = expand_gateway_models(
            &[
                test_model("gemini-4-neon"),
                test_model("gemini-4-carbon"),
                test_model("gemini-4-hydrogen"),
            ],
            rules,
        );
        assert_eq!(
            rewrite_proxy_request_path(
                "/v1beta/models/gemini-pro-orchestrator-low:generateContent",
                rules,
                || with_neon.clone()
            ),
            (
                "/v1beta/models/gemini-4-neon:generateContent".to_string(),
                Some("low")
            )
        );

        assert_eq!(
            rewrite_proxy_request_path(
                "/v1beta/models/gemini-4-argon-medium:streamGenerateContent?alt=sse",
                rules,
                || panic!("gateway_models should not be read for non-orchestrator paths")
            ),
            (
                "/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse".to_string(),
                Some("medium")
            )
        );
    }

    #[test]
    fn test_rewrite_request_path_and_extract_effort() {
        assert_eq!(
            rewrite_request_path_and_extract_effort(
                "/v1beta/models/gemini-4-argon-medium:streamGenerateContent?alt=sse"
            ),
            (
                "/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse".to_string(),
                Some("medium")
            )
        );
        assert_eq!(
            rewrite_request_path_and_extract_effort(
                "/v1beta/models/gemini-4-hydrogen-xhigh:streamGenerateContent?alt=sse"
            ),
            (
                "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse".to_string(),
                Some("xhigh")
            )
        );
        assert_eq!(
            rewrite_request_path_and_extract_effort(
                "/v1beta/models/gemini-4-neon-xhigh:streamGenerateContent?alt=sse"
            ),
            (
                "/v1beta/models/gemini-4-neon:streamGenerateContent?alt=sse".to_string(),
                Some("xhigh")
            )
        );
        assert_eq!(
            rewrite_request_path_and_extract_effort(
                "/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse"
            ),
            (
                "/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse".to_string(),
                None
            )
        );
    }

    #[test]
    fn test_inject_thinking_level_if_missing() {
        // Replaces fallback thinkingBudget with thinkingLevel when missing
        let fallback_body = Bytes::from(
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"includeThoughts":true,"thinkingBudget":1024}}}"#,
        );
        let updated = inject_thinking_level_if_missing(fallback_body.clone(), "high");
        let parsed: serde_json::Value = serde_json::from_slice(&updated).unwrap();
        assert_eq!(
            parsed["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert!(parsed["generationConfig"]["thinkingConfig"]["thinkingBudget"].is_null());

        let updated_xhigh = inject_thinking_level_if_missing(fallback_body, "xhigh");
        let parsed_xhigh: serde_json::Value = serde_json::from_slice(&updated_xhigh).unwrap();
        assert_eq!(
            parsed_xhigh["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "XHIGH"
        );
        assert!(parsed_xhigh["generationConfig"]["thinkingConfig"]["thinkingBudget"].is_null());

        // Preserves existing thinkingLevel untouched
        let existing_body = Bytes::from(
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MEDIUM"}}}"#,
        );
        let unchanged = inject_thinking_level_if_missing(existing_body.clone(), "high");
        assert_eq!(unchanged, existing_body);

        // Does not inject thinkingConfig into countTokens payloads without generationConfig
        let count_tokens_body =
            Bytes::from(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#);
        let unchanged_count = inject_thinking_level_if_missing(count_tokens_body.clone(), "high");
        assert_eq!(unchanged_count, count_tokens_body);

        // Respects explicit thinking disablement (thinkingBudget: 0 / includeThoughts: false)
        let disabled_body = Bytes::from(
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"includeThoughts":false,"thinkingBudget":0}}}"#,
        );
        let unchanged_disabled = inject_thinking_level_if_missing(disabled_body.clone(), "high");
        assert_eq!(unchanged_disabled, disabled_body);
    }

    #[test]
    fn test_filter_core_gemini_models_orders_primary_before_newer_flash_and_appends_aux() {
        let upstream = vec![
            GeminiModelInfo {
                name: "models/gemini-2.5-flash-image".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(false),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-3.5-flash-lite".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-3.1-flash-image".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-3.8-flash".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-4-argon".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-3.9-flash".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-3.9-flash-lite".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
            GeminiModelInfo {
                name: "models/gemini-99-pro".to_string(),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
                ..Default::default()
            },
        ];

        let filtered = filter_core_gemini_models(&upstream, ModelRules::bundled());
        let filtered_ids: Vec<&str> = filtered.iter().map(GeminiModelInfo::id).collect();
        assert_eq!(
            filtered_ids,
            vec![
                "gemini-99-pro",
                "gemini-4-argon",
                "gemini-3.9-flash",
                "gemini-3.8-flash",
                "gemini-3.1-flash-image",
                "gemini-3.9-flash-lite",
            ]
        );
    }

    #[test]
    fn test_custom_model_rules_parsing_and_discovery() {
        assert!(ModelRules::bundled().version >= 1);

        let rules = ModelRules::parse(
            r#"
            version = 2
            non_core_keywords = ["-preview", "-blocked"]
            default_efforts = ["medium", "low"]
            future_top_level_key = true

            [[effort_rules]]
            match = ["krypton"]
            efforts = ["high", "xhigh", "medium", "low"]

            [orchestrator]
            model_id = "gemini-pro-orchestrator"
            display_name = "Custom Orchestrator"
            planner_models = ["gemini-5-krypton", "gemini-4-neon"]
            executor_models = ["gemini-5-helium"]
            future_nested_key = "ignored"
            "#,
        )
        .unwrap();
        assert_eq!(rules.version, 2);

        assert!(ModelRules::parse("").is_err());
        assert!(ModelRules::parse("unrelated = 123\n").is_err());
        assert!(
            ModelRules::parse(
                "default_efforts = []\n[orchestrator]\nmodel_id = \"a\"\ndisplay_name = \"b\"\n"
            )
            .is_err()
        );
        assert!(
            ModelRules::parse(
                "default_efforts = [\"medium\"]\n[[effort_rules]]\nmatch = [\"x\"]\nefforts = []\n[orchestrator]\nmodel_id = \"a\"\ndisplay_name = \"b\"\n"
            )
            .is_err()
        );
        assert!(
            ModelRules::parse(
                "default_efforts = [\"medium\"]\n[orchestrator]\nmodel_id = \"  \"\ndisplay_name = \"b\"\n"
            )
            .is_err()
        );

        let upstream = vec![
            test_model("gemini-4-neon"),
            test_model("gemini-5-blocked"),
            test_model("gemini-5-helium"),
            test_model("gemini-5-krypton"),
        ];
        let filtered = filter_core_gemini_models(&upstream, &rules);
        let filtered_ids: Vec<&str> = filtered.iter().map(GeminiModelInfo::id).collect();
        assert_eq!(
            filtered_ids,
            vec!["gemini-5-krypton", "gemini-5-helium", "gemini-4-neon"]
        );

        let expanded = expand_gateway_models(&filtered, &rules);
        let expanded_ids: Vec<&str> = expanded.iter().map(GeminiModelInfo::id).collect();
        assert_eq!(
            expanded_ids,
            vec![
                "gemini-5-krypton-high",
                "gemini-5-krypton-xhigh",
                "gemini-5-krypton-medium",
                "gemini-5-krypton-low",
                "gemini-5-helium-medium",
                "gemini-5-helium-low",
                "gemini-4-neon-medium",
                "gemini-4-neon-low",
                "gemini-pro-orchestrator",
            ]
        );
        assert_eq!(
            expanded.last().and_then(|m| m.display_name.as_deref()),
            Some("Custom Orchestrator")
        );

        assert_eq!(
            rewrite_proxy_request_path(
                "/v1beta/models/gemini-pro-orchestrator:streamGenerateContent?alt=sse",
                &rules,
                || expanded.clone()
            ),
            (
                "/v1beta/models/gemini-5-krypton:streamGenerateContent?alt=sse".to_string(),
                Some("high")
            )
        );
    }
}
