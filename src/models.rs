// SPDX-License-Identifier: MIT

use crate::config::Config;
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

/// Fallback list of Gemini models if dynamic discovery fails.
pub const DEFAULT_GATEWAY_MODELS: &[&str] = &[
    "gemini-4-argon",
    "gemini-3.8-flash",
    "gemini-3.1-flash-image",
    "gemini-3.5-flash-lite",
];

/// Effort suffixes recognized for thinking-capable Gemini models (`medium -> high -> low` so
/// `-medium` is the default for primary models).
pub const MODEL_EFFORT_LEVELS: &[&str] = &["medium", "high", "low"];

/// Effort ordering for Flash models (`high -> medium -> low`).
const FLASH_EFFORT_LEVELS: &[&str] = &["high", "medium", "low"];

/// Keywords identifying preview, lite, rolling-alias, or non-text/specialized Gemini variants
/// that should be excluded when discovering core reasoning models from `/v1beta/models`.
pub const NON_CORE_KEYWORDS: &[&str] = &[
    "-preview",
    "-lite",
    "-latest",
    "-customtools",
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

    /// Constructs a fallback [`GeminiModelInfo`] from a model ID when upstream metadata is
    /// unavailable (e.g. offline fallback).
    pub fn from_model_id(id: impl AsRef<str>) -> Self {
        let clean = id.as_ref().trim().trim_start_matches("models/");
        let is_image = clean.ends_with("-image");
        let display_name = if is_image {
            None
        } else {
            match split_effort_suffix(clean) {
                Some((base_id, eff)) => Some(format!(
                    "{} ({})",
                    humanize_model_id(base_id),
                    capitalize_ascii_word(eff)
                )),
                None => Some(humanize_model_id(clean)),
            }
        };

        Self {
            name: format!("models/{}", clean),
            display_name,
            description: None,
            input_token_limit: None,
            output_token_limit: None,
            supported_generation_methods: vec!["generateContent".to_string()],
            thinking: if is_image { None } else { Some(true) },
        }
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

/// Splits a trailing `-<effort>` suffix (where `<effort>` is in [`MODEL_EFFORT_LEVELS`])
/// from a model identifier, returning `(base_model, effort)`.
pub fn split_effort_suffix(model_id: &str) -> Option<(&str, &'static str)> {
    for &effort in MODEL_EFFORT_LEVELS {
        if let Some(prefix) = model_id.strip_suffix(effort)
            && let Some(base) = prefix.strip_suffix('-')
            && !base.is_empty()
        {
            return Some((base, effort));
        }
    }
    None
}

/// Strips a trailing `-<effort>` suffix (where `<effort>` is in [`MODEL_EFFORT_LEVELS`]) if present.
pub fn strip_effort_suffix(model_id: &str) -> Option<&str> {
    split_effort_suffix(model_id).map(|(base, _)| base)
}

/// Normalizes a Gemini model identifier by stripping `models/` prefix and effort suffixes
/// if the base model is standard.
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

/// Strips a trailing `-<effort>` suffix from the model segment of a Gemini API request path
/// (e.g. `/v1beta/models/gemini-4-argon-medium:streamGenerateContent?alt=sse` ->
/// `("/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse", Some("medium"))`).
pub fn rewrite_request_path_and_extract_effort(
    path_and_query: &str,
) -> (String, Option<&'static str>) {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };

    if let Some((prefix, after_models)) = path.split_once("/models/") {
        let sep_idx = after_models.find([':', '/']).unwrap_or(after_models.len());
        let model_part = &after_models[..sep_idx];
        let rest_path = &after_models[sep_idx..];

        if let Some((base, effort)) = split_effort_suffix(model_part)
            && base.starts_with("gemini-")
        {
            let rewritten = match query {
                Some(q) => format!("{}/models/{}{}?{}", prefix, base, rest_path, q),
                None => format!("{}/models/{}{}", prefix, base, rest_path),
            };
            return (rewritten, Some(effort));
        }
    }

    (path_and_query.to_string(), None)
}

/// Ensures `generationConfig.thinkingConfig.thinkingLevel` is set to `"LOW"`, `"MEDIUM"`, or
/// `"HIGH"` when a request arrived with an unstripped `-<effort>` model suffix and the client
/// did not already populate `thinkingLevel` (e.g. when using a newly released model that falls
/// back to `thinkingBudget` on the client).
pub fn inject_thinking_level_if_missing(body: Bytes, effort: &str) -> Bytes {
    if body.is_empty() {
        return body;
    }

    let level_str = match effort {
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" => "HIGH",
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

/// Filters a list of `/v1beta/models` entries to retain core Gemini Text / Reasoning models
/// (ordered newest-first with primary models before Flash models), followed by the newest
/// non-preview `-flash-image` and `-flash-lite` models if available.
pub fn filter_core_gemini_models(models: &[GeminiModelInfo]) -> Vec<GeminiModelInfo> {
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
            !NON_CORE_KEYWORDS.iter().any(|kw| id.contains(kw))
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
/// entries (`medium -> high -> low` for primary models so `-medium` is the default, and
/// `high -> medium -> low` for Flash models), while passing auxiliary `-image` and `-lite` models
/// through with their upstream metadata preserved.
pub fn expand_gateway_models(models: &[GeminiModelInfo]) -> Vec<GatewayModelEntry> {
    let mut expanded = Vec::new();
    for model in models {
        let base_id = model.id();
        if base_id.ends_with("-image") {
            let mut entry = model.clone();
            entry.name = format!("models/{}", base_id);
            // Omit `thinking` on image models so the client does not treat them as
            // thinking-capable even if upstream `/v1beta/models` reports `thinking: true`.
            entry.thinking = None;
            expanded.push(entry);
        } else if base_id.ends_with("-lite") {
            let mut entry = model.clone();
            entry.name = format!("models/{}", base_id);
            if entry
                .display_name
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
            {
                entry.display_name = Some(humanize_model_id(base_id));
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
            let efforts = if base_id.contains("flash") {
                FLASH_EFFORT_LEVELS
            } else {
                MODEL_EFFORT_LEVELS
            };
            for effort in efforts {
                expanded.push(GatewayModelEntry {
                    name: format!("models/{}-{}", base_id, effort),
                    display_name: Some(format!(
                        "{} ({})",
                        base_display,
                        capitalize_ascii_word(effort)
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
    expanded
}

/// Returns the expanded fallback model catalog derived from [`DEFAULT_GATEWAY_MODELS`].
pub fn default_gateway_models() -> Vec<GatewayModelEntry> {
    let base: Vec<GeminiModelInfo> = DEFAULT_GATEWAY_MODELS
        .iter()
        .map(GeminiModelInfo::from_model_id)
        .collect();
    expand_gateway_models(&base)
}

/// Queries `GET {upstream}/v1beta/models` and returns the list of active Gemini models
/// expanded with effort suffixes and upstream metadata (falling back to [`default_gateway_models`]
/// if the request fails).
pub async fn resolve_gateway_models(
    client: &reqwest::Client,
    config: &Config,
) -> Vec<GatewayModelEntry> {
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
                return default_gateway_models();
            }
            Err(err) => {
                warn!(
                    "Error fetching models from {}: {}; falling back to default model list",
                    url, err
                );
                return default_gateway_models();
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
                return default_gateway_models();
            }
        }
    }

    let models = filter_core_gemini_models(&all_models);
    if models.is_empty() {
        warn!("No core Gemini models matched filter; falling back to default model list");
        default_gateway_models()
    } else {
        let ids: Vec<&str> = models.iter().map(GeminiModelInfo::id).collect();
        info!("Discovered available Gemini models: {}", ids.join(","));
        expand_gateway_models(&models)
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
    fn test_expand_gateway_models_orders_efforts_and_preserves_upstream_metadata() {
        let expanded = default_gateway_models();
        let ids: Vec<&str> = expanded.iter().map(GeminiModelInfo::id).collect();
        assert_eq!(
            ids,
            vec![
                "gemini-4-argon-medium",
                "gemini-4-argon-high",
                "gemini-4-argon-low",
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "gemini-3.8-flash-low",
                "gemini-3.1-flash-image",
                "gemini-3.5-flash-lite",
            ]
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
                name: "models/gemini-3.1-flash-image".to_string(),
                display_name: Some("Nano Banana 2".to_string()),
                description: Some("Gemini 3.1 Flash Image".to_string()),
                input_token_limit: Some(65_536),
                output_token_limit: Some(65_536),
                supported_generation_methods: vec!["generateContent".to_string()],
                thinking: Some(true),
            },
        ];

        let rich_expanded = expand_gateway_models(&upstream);
        assert_eq!(rich_expanded.len(), 4);
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

        let image = &rich_expanded[3];
        assert_eq!(image.name, "models/gemini-3.1-flash-image");
        assert_eq!(image.display_name.as_deref(), Some("Nano Banana 2"));
        assert_eq!(image.input_token_limit, Some(65_536));
        assert_eq!(image.output_token_limit, Some(65_536));
        assert_eq!(image.thinking, None);
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
        let updated = inject_thinking_level_if_missing(fallback_body, "high");
        let parsed: serde_json::Value = serde_json::from_slice(&updated).unwrap();
        assert_eq!(
            parsed["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert!(parsed["generationConfig"]["thinkingConfig"]["thinkingBudget"].is_null());

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

        let filtered = filter_core_gemini_models(&upstream);
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
}
