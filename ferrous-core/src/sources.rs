//! Remote model sources + the bundled offline fallback.
//!
//! - LiteLLM cost map: the industry-standard open JSON of per-model pricing.
//! - OpenRouter `/api/v1/models`: rich metadata (context, pricing, modalities).
//! - Bundled fallback: ships inside the binary so the brain works offline
//!   before the first sync.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::model::{Benchmarks, Model};

const FALLBACK: &str = include_str!("data/fallback.json");

/// Bundled snapshot — approximate prices so the tool works offline.
/// Run `ferrous sync --live` to refresh from real sources.
pub fn from_fallback(now: i64) -> Vec<Model> {
    serde_json::from_str::<Vec<Model>>(FALLBACK)
        .unwrap_or_default()
        .into_iter()
        .map(|mut m| {
            m.updated_at = now;
            m
        })
        .collect()
}

/// One entry of the LiteLLM model cost map.
#[derive(Debug, Clone, Deserialize)]
pub struct LiteLlmEntry {
    #[serde(rename = "input_cost_per_token")]
    pub input_cost_per_token: Option<f64>,
    #[serde(rename = "output_cost_per_token")]
    pub output_cost_per_token: Option<f64>,
    #[serde(rename = "max_tokens")]
    pub max_tokens: Option<u64>,
}

/// The cost map is a JSON object keyed by model slug.
pub type LiteLlmMap = BTreeMap<String, LiteLlmEntry>;

/// Map the LiteLLM cost map onto `Model`s. Sparse by design — it carries
/// pricing + context, not modalities or limits.
pub fn from_litellm(map: &LiteLlmMap, now: i64) -> Vec<Model> {
    map.iter()
        .filter_map(|(slug, e)| {
            let price_in = e.input_cost_per_token.map(|p| p * 1_000_000.0);
            let price_out = e.output_cost_per_token.map(|p| p * 1_000_000.0);
            if price_in.is_none() && price_out.is_none() {
                return None; // no pricing info ⇒ not routable, skip
            }
            Some(Model {
                slug: slug.clone(),
                name: slug.clone(),
                provider: slug.split('/').next().unwrap_or("unknown").to_string(),
                context_window: e.max_tokens.map(|m| m as usize).unwrap_or(0),
                price_in_usd: price_in.unwrap_or(0.0),
                price_out_usd: price_out.unwrap_or(0.0),
                tpm: None,
                rpm: None,
                supports_tools: false,
                supports_vision: false,
                benchmarks: Benchmarks::default(),
                region: None,
                updated_at: now,
            })
        })
        .collect()
}

/// `GET /api/v1/models` response shape (subset we care about).
#[derive(Debug, Clone, Deserialize)]
pub struct OpenRouterResponse {
    pub data: Vec<OpenRouterModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: OpenRouterPricing,
    #[serde(default)]
    pub architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    pub supported_parameters: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterPricing {
    /// Per-token price, serialized as a string by OpenRouter.
    pub prompt: Option<String>,
    pub completion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenRouterArchitecture {
    #[serde(default)]
    pub input_modalities: Option<Vec<String>>,
}

fn parse_price(s: &Option<String>) -> f64 {
    s.as_deref()
        .and_then(|raw| raw.parse::<f64>().ok())
        .map(|per_token| per_token * 1_000_000.0)
        .unwrap_or(0.0)
}

/// Map an OpenRouter listing onto `Model`s — the richest source.
pub fn from_openrouter(resp: &OpenRouterResponse, now: i64) -> Vec<Model> {
    resp.data
        .iter()
        .map(|m| {
            let vision = m
                .architecture
                .as_ref()
                .and_then(|a| a.input_modalities.as_ref())
                .map(|mods| mods.iter().any(|x| x == "image"))
                .unwrap_or(false);
            let tools = m
                .supported_parameters
                .as_ref()
                .map(|p| p.iter().any(|x| x == "tools"))
                .unwrap_or(false);
            Model {
                slug: m.id.clone(),
                name: m.name.clone(),
                provider: m.id.split('/').next().unwrap_or("openrouter").to_string(),
                context_window: m.context_length.map(|c| c as usize).unwrap_or(0),
                price_in_usd: parse_price(&m.pricing.prompt),
                price_out_usd: parse_price(&m.pricing.completion),
                tpm: None,
                rpm: None,
                supports_tools: tools,
                supports_vision: vision,
                benchmarks: Benchmarks::default(),
                region: None,
                updated_at: now,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_parses_and_stamps_time() {
        let v = from_fallback(42);
        assert!(v.len() >= 5, "fallback should carry a useful baseline");
        assert!(v.iter().all(|m| m.updated_at == 42));
    }

    #[test]
    fn litellm_map_converts() {
        let mut map = LiteLlmMap::new();
        map.insert(
            "openai/gpt-4o".into(),
            LiteLlmEntry {
                input_cost_per_token: Some(0.0000025),
                output_cost_per_token: Some(0.00001),
                max_tokens: Some(128_000),
            },
        );
        map.insert(
            "weird/empty".into(),
            LiteLlmEntry {
                input_cost_per_token: None,
                output_cost_per_token: None,
                max_tokens: None,
            },
        );
        let v = from_litellm(&map, 7);
        assert_eq!(v.len(), 1, "entries without pricing must be skipped");
        assert!((v[0].price_in_usd - 2.5).abs() < 1e-9);
        assert!((v[0].price_out_usd - 10.0).abs() < 1e-9);
        assert_eq!(v[0].context_window, 128_000);
    }

    #[test]
    fn openrouter_converts_vision_and_tools() {
        let resp: OpenRouterResponse = serde_json::from_str(
            r#"{
                "data": [{
                    "id": "google/gemini-2.5-flash",
                    "name": "Gemini 2.5 Flash",
                    "context_length": 1048576,
                    "pricing": { "prompt": "0.0000003", "completion": "0.0000025" },
                    "architecture": { "input_modalities": ["text", "image"] },
                    "supported_parameters": ["tools", "temperature"]
                }]
            }"#,
        )
        .unwrap();
        let v = from_openrouter(&resp, 1);
        assert_eq!(v.len(), 1);
        assert!(v[0].supports_vision);
        assert!(v[0].supports_tools);
        assert!((v[0].price_in_usd - 0.3).abs() < 1e-9);
        assert_eq!(v[0].context_window, 1_048_576);
    }
}
