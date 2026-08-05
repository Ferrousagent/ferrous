//! The model catalog data model — the thing the whole brain reasons over.

use serde::{Deserialize, Serialize};

/// One LLM the router can choose from.
///
/// Lives in memory, persists as a compact snapshot, and is refreshed from
/// LiteLLM / OpenRouter / the bundled fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// Canonical id, e.g. `deepseek/deepseek-chat`.
    pub slug: String,
    /// Human name, e.g. "DeepSeek Chat".
    pub name: String,
    /// Serving provider (slug prefix by convention).
    pub provider: String,
    /// Context window in tokens.
    pub context_window: usize,
    /// USD per 1M *input* tokens.
    pub price_in_usd: f64,
    /// USD per 1M *output* tokens.
    pub price_out_usd: f64,
    /// Tokens-per-minute limit, when known.
    pub tpm: Option<u64>,
    /// Requests-per-minute limit, when known.
    pub rpm: Option<u64>,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub benchmarks: Benchmarks,
    /// Hosting region, when the source exposes it.
    pub region: Option<String>,
    /// Unix seconds of the last refresh.
    pub updated_at: i64,
}

/// The benchmarks that matter for routing decisions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Benchmarks {
    pub mmlu: Option<f64>,
    pub humaneval: Option<f64>,
    pub math: Option<f64>,
    /// Median time-to-first-token in ms, when measured.
    pub latency_ms: Option<u64>,
}

impl Model {
    /// Estimated total cost of a request in USD.
    pub fn est_cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 * self.price_in_usd + output_tokens as f64 * self.price_out_usd)
            / 1_000_000.0
    }

    /// Loose substring match across slug, name and provider.
    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        self.slug.to_lowercase().contains(&q)
            || self.name.to_lowercase().contains(&q)
            || self.provider.to_lowercase().contains(&q)
    }
}
