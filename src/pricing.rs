// SPDX-License-Identifier: MIT

use crate::models::{normalize_model_id, strip_effort_suffix};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Default URL for fetching live model pricing (`models.dev` API).
pub const DEFAULT_PRICING_URL: &str = "https://models.dev/api.json";

/// Token threshold (200k input tokens) above which tiered pricing applies on supported models.
const TIER_200K_THRESHOLD: u64 = 200_000;

/// Per-1M-token pricing rates in USD for a model.
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

/// Pricing lookup table mapping model IDs to [`ModelPricing`].
#[derive(Debug, Clone)]
pub struct PricingTable {
    pub models: HashMap<String, ModelPricing>,
    pub source: String,
}

impl Default for PricingTable {
    fn default() -> Self {
        Self {
            models: HashMap::new(),
            source: "none".to_string(),
        }
    }
}

impl PricingTable {
    /// Resolves pricing for a model ID from the upstream pricing table, normalizing prefixes
    /// (`models/`), version suffixes (`@...`), effort suffixes (`-low`, `-medium`, `-high`),
    /// and `-thinking` suffixes. Returns `None` when the model's pricing is unknown upstream.
    pub fn lookup(&self, raw_model: &str) -> Option<ModelPricing> {
        let normalized = normalize_model_id(raw_model);
        let mut cleaned = normalized.split('@').next().unwrap_or(&normalized);
        if let Some(p) = self.models.get(cleaned) {
            return Some(p.clone());
        }

        if let Some(stripped) = strip_effort_suffix(cleaned) {
            if let Some(p) = self.models.get(stripped) {
                return Some(p.clone());
            }
            cleaned = stripped;
        }

        if let Some(stripped) = cleaned.strip_suffix("-thinking")
            && let Some(p) = self.models.get(stripped)
        {
            return Some(p.clone());
        }

        None
    }
}

#[derive(Debug, Deserialize)]
struct ModelsDevResponse {
    #[serde(default)]
    google: Option<ModelsDevProvider>,
    #[serde(default)]
    anthropic: Option<ModelsDevProvider>,
    #[serde(default)]
    openai: Option<ModelsDevProvider>,
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
    tiers: Vec<ModelsDevTierCost>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevTierCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
}

/// Parses a `models.dev/api.json` JSON payload into a [`PricingTable`] for Google, Anthropic,
/// and OpenAI models.
pub fn parse_models_dev_pricing(json_str: &str) -> Result<PricingTable, String> {
    let parsed: ModelsDevResponse =
        serde_json::from_str(json_str).map_err(|e| format!("Invalid pricing JSON: {}", e))?;

    let mut table = PricingTable::default();

    for provider in [parsed.google, parsed.anthropic, parsed.openai]
        .into_iter()
        .flatten()
    {
        for (model_id, entry) in provider.models {
            if let Some(cost) = entry.cost
                && let (Some(input), Some(output)) = (cost.input, cost.output)
            {
                let cached = cost.cache_read.unwrap_or(0.0);
                let over_200k = cost
                    .context_over_200k
                    .or_else(|| cost.tiers.into_iter().next());

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
            }
        }
    }

    if !table.models.is_empty() {
        table.source = "models.dev (live)".to_string();
    }

    Ok(table)
}

/// Fetches live model pricing from `pricing_url` (usually `https://models.dev/api.json`),
/// falling back to an empty [`PricingTable`] (zero cost) on network or parse error.
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
                        "Failed to parse live pricing from {}: {}; ignoring model pricing",
                        pricing_url, err
                    );
                    PricingTable::default()
                }
            },
            Err(err) => {
                warn!(
                    "Failed to read live pricing response from {}: {}; ignoring model pricing",
                    pricing_url, err
                );
                PricingTable::default()
            }
        },
        Ok(res) => {
            warn!(
                "Live pricing endpoint {} returned status {}; ignoring model pricing",
                pricing_url,
                res.status()
            );
            PricingTable::default()
        }
        Err(err) => {
            debug!(
                "Could not fetch live pricing from {}: {}; ignoring model pricing",
                pricing_url, err
            );
            PricingTable::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_pricing_calculation_flat_and_tiered() {
        let flat = ModelPricing::flat(2.0, 0.1, 10.0);
        // 100k uncached ($2/1M = $0.20), 400k cached ($0.10/1M = $0.04), 50k output ($10/1M = $0.50)
        let cost = flat.calculate_cost(100_000, 400_000, 50_000);
        assert!((cost.uncached_input_cost_usd - 0.20).abs() < 1e-9);
        assert!((cost.cached_input_cost_usd - 0.04).abs() < 1e-9);
        assert!((cost.output_cost_usd - 0.50).abs() < 1e-9);
        assert!((cost.total_cost_usd - 0.74).abs() < 1e-9);
        assert!((cost.cache_savings_usd - 0.76).abs() < 1e-9);

        // Tiered model: <=200k: $1.25 / $0.125 / $10.0; >200k: $2.50 / $0.25 / $15.0
        let tiered = ModelPricing::tiered(1.25, 0.125, 10.0, 2.5, 0.25, 15.0);
        let under_200k = tiered.calculate_cost(100_000, 50_000, 10_000);
        assert!((under_200k.uncached_input_cost_usd - 0.125).abs() < 1e-9);
        assert!((under_200k.cached_input_cost_usd - 0.00625).abs() < 1e-9);
        assert!((under_200k.output_cost_usd - 0.10).abs() < 1e-9);

        let over_200k = tiered.calculate_cost(150_000, 100_000, 10_000); // 250k total input > 200k
        assert!((over_200k.uncached_input_cost_usd - 0.375).abs() < 1e-9);
        assert!((over_200k.cached_input_cost_usd - 0.025).abs() < 1e-9);
        assert!((over_200k.output_cost_usd - 0.15).abs() < 1e-9);
    }

    #[test]
    fn test_parse_models_dev_pricing_and_unknown_models_return_none() {
        let default_table = PricingTable::default();
        assert!(default_table.models.is_empty());
        assert_eq!(default_table.source, "none");
        assert!(default_table.lookup("gemini-2.5-pro").is_none());
        assert!(default_table.lookup("gemini-4-argon").is_none());
        assert!(default_table.lookup("claude-sonnet-4-5").is_none());

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
            },
            "anthropic": {
                "models": {
                    "claude-sonnet-4-5": {
                        "cost": {
                            "input": 3.0,
                            "output": 15.0,
                            "cache_read": 0.3
                        }
                    }
                }
            },
            "openai": {
                "models": {
                    "gpt-5": {
                        "cost": {
                            "input": 1.25,
                            "output": 10.0,
                            "cache_read": 0.125
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

        let claude_thinking = table.lookup("models/claude-sonnet-4-5-thinking").unwrap();
        assert_eq!(claude_thinking.input_per_1m, 3.0);
        assert_eq!(claude_thinking.output_per_1m, 15.0);

        let claude_versioned = table.lookup("claude-sonnet-4-5@20250929").unwrap();
        assert_eq!(claude_versioned.input_per_1m, 3.0);
        assert_eq!(claude_versioned.output_per_1m, 15.0);

        let gpt = table.lookup("gpt-5").unwrap();
        assert_eq!(gpt.input_per_1m, 1.25);
        assert_eq!(gpt.output_per_1m, 10.0);

        // Unknown models must return None (zero cost) without falling back to other models
        assert!(table.lookup("gemini-4-argon").is_none());
        assert!(table.lookup("claude-sonnet-99").is_none());
        assert!(table.lookup("unknown-model").is_none());
    }
}
