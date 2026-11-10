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

        if let Some(stripped) = strip_effort_suffix(&cleaned)
            && let Some(p) = self.models.get(stripped)
        {
            return Some(p.clone());
        }

        // Try matching by longest prefix in the pricing table (e.g. "gemini-2.5-pro-preview-05-06"),
        // without cross-matching a base `-flash` key against a `-flash-lite` model.
        let is_lite = cleaned.contains("-lite");
        let mut best_match: Option<(&str, &ModelPricing)> = None;
        for (key, pricing) in &self.models {
            if cleaned.starts_with(key.as_str())
                && (key.contains("-lite") == is_lite)
                && best_match.is_none_or(|(best_k, _)| key.len() > best_k.len())
            {
                best_match = Some((key.as_str(), pricing));
            }
        }
        if let Some((_, pricing)) = best_match {
            return Some(pricing.clone());
        }

        // Family fallback for newly released Gemini models not yet listed in the pricing table.
        if cleaned.starts_with("gemini-") && !cleaned.contains("embedding") {
            if cleaned.contains("flash-lite") {
                return self.models.get("gemini-3.5-flash-lite").cloned();
            }
            if cleaned.contains("flash") {
                return self.models.get("gemini-3.8-flash").cloned();
            }
            return self.models.get("gemini-4-argon").cloned();
        }

        None
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
    tiers: Vec<ModelsDevTierCost>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevTierCost {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_pricing_lookup_future_gemini_family_fallback() {
        let table = PricingTable::built_in();

        let future_primary = table.lookup("gemini-5-neon-high").unwrap();
        assert_eq!(future_primary.input_per_1m, 2.0);
        assert_eq!(future_primary.output_per_1m, 10.0);

        let future_flash = table.lookup("gemini-3.9-flash-medium").unwrap();
        assert_eq!(future_flash.input_per_1m, 0.75);
        assert_eq!(future_flash.output_per_1m, 3.75);

        let future_lite = table.lookup("gemini-3.9-flash-lite").unwrap();
        assert_eq!(future_lite.input_per_1m, 0.30);
        assert_eq!(future_lite.output_per_1m, 2.50);

        // Must not prefix-match "gemini-3.8-flash" when looking up "gemini-3.8-flash-lite"
        let same_ver_lite = table.lookup("gemini-3.8-flash-lite").unwrap();
        assert_eq!(same_ver_lite.input_per_1m, 0.30);
        assert_eq!(same_ver_lite.output_per_1m, 2.50);

        assert!(table.lookup("non-gemini-model").is_none());
    }
}
