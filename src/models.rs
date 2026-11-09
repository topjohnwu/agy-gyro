// SPDX-License-Identifier: MIT

use crate::config::Config;
use serde::Deserialize;
use std::time::Duration;
use tracing::{info, warn};

/// Fallback list of Gemini models if dynamic discovery fails.
pub const DEFAULT_GATEWAY_MODELS: &[&str] = &["gemini-4-argon", "gemini-3.8-flash"];

/// Effort suffixes expanded for each thinking-capable Gemini model.
pub const MODEL_EFFORT_LEVELS: &[&str] = &["low", "medium", "high"];

/// Fixed list of older, lite, alias, or specialized-variant Core Gemini Text / Reasoning
/// models to exclude when dynamically building the model list from `/v1beta/models`.
pub const EXCLUDED_MODELS: &[&str] = &[
    "gemini-flash-latest",
    "gemini-flash-lite-latest",
    "gemini-pro-latest",
    "gemini-2.5-flash-lite",
    "gemini-3-flash-preview",
    "gemini-3.1-pro-preview",
    "gemini-3.1-pro-preview-customtools",
    "gemini-3.1-flash-lite-preview",
    "gemini-3.1-flash-lite",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.6-flash",
    "gemini-3.7-flash",
];

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

/// Strips a trailing `-<effort>` suffix (where `<effort>` is in [`MODEL_EFFORT_LEVELS`]) if present.
pub fn strip_effort_suffix(model_id: &str) -> Option<&str> {
    for effort in MODEL_EFFORT_LEVELS {
        if let Some(prefix) = model_id.strip_suffix(effort)
            && let Some(base) = prefix.strip_suffix('-')
            && !base.is_empty()
        {
            return Some(base);
        }
    }
    None
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
/// (falling back to [`DEFAULT_GATEWAY_MODELS`] if the request fails).
pub async fn resolve_gateway_models(client: &reqwest::Client, config: &Config) -> String {
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
}
