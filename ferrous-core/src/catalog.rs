//! The in-memory model catalog.
//!
//! Zero database, zero bloat: a `Vec` plus a slug index, loaded into RAM at
//! startup and queried with plain iterators. For a few thousand rows this is
//! the fastest possible design — you cannot beat RAM.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};

use crate::model::Model;

/// A read-mostly, in-memory collection of models with O(1) slug lookup.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Catalog {
    models: Vec<Model>,
    /// Derived index. Never serialized: rebuilt from `models` on load so it
    /// can never go stale, even if a snapshot was written by another version.
    #[serde(skip)]
    by_slug: HashMap<String, usize>,
}

impl<'de> Deserialize<'de> for Catalog {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            models: Vec<Model>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut catalog = Catalog::default();
        catalog.merge(raw.models);
        Ok(catalog)
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Model> {
        self.models.iter()
    }

    /// O(1) lookup by canonical slug.
    pub fn get(&self, slug: &str) -> Option<&Model> {
        self.by_slug.get(slug).map(|&i| &self.models[i])
    }

    /// Insert or replace by slug. Returns `true` when it was a new model.
    pub fn upsert(&mut self, model: Model) -> bool {
        if let Some(&i) = self.by_slug.get(&model.slug) {
            self.models[i] = model;
            false
        } else {
            self.by_slug.insert(model.slug.clone(), self.models.len());
            self.models.push(model);
            true
        }
    }

    /// Merge a batch (e.g. one sync source). Returns how many were new.
    pub fn merge(&mut self, incoming: impl IntoIterator<Item = Model>) -> usize {
        incoming
            .into_iter()
            .filter(|m| self.upsert(m.clone()))
            .count()
    }

    /// Distinct provider names, sorted.
    pub fn providers(&self) -> Vec<String> {
        let mut set: Vec<String> = self.models.iter().map(|m| m.provider.clone()).collect();
        set.sort();
        set.dedup();
        set
    }

    /// Loose text search across slug/name/provider.
    pub fn search(&self, query: &str) -> Vec<&Model> {
        self.models.iter().filter(|m| m.matches(query)).collect()
    }

    /// Models meeting hard constraints.
    pub fn capable(
        &self,
        min_context: usize,
        needs_tools: bool,
        needs_vision: bool,
    ) -> Vec<&Model> {
        self.models
            .iter()
            .filter(|m| {
                m.context_window >= min_context
                    && (!needs_tools || m.supports_tools)
                    && (!needs_vision || m.supports_vision)
            })
            .collect()
    }

    /// Cheapest capable model, priced on a representative 2:1 in:out request.
    ///
    /// This is the routing *heuristic*; the full policy engine (fallback
    /// chains, budgets, quality-first modes) lands in the router tick.
    pub fn cheapest(
        &self,
        min_context: usize,
        needs_tools: bool,
        needs_vision: bool,
    ) -> Option<&Model> {
        self.capable(min_context, needs_tools, needs_vision)
            .into_iter()
            .min_by(|a, b| {
                a.est_cost(2_000, 1_000)
                    .total_cmp(&b.est_cost(2_000, 1_000))
            })
    }

    /// All models sorted by representative request price, cheapest first.
    pub fn by_price(&self) -> Vec<&Model> {
        let mut v: Vec<&Model> = self.models.iter().collect();
        v.sort_by(|a, b| {
            a.est_cost(2_000, 1_000)
                .total_cmp(&b.est_cost(2_000, 1_000))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(slug: &str, context: usize, price_in: f64, price_out: f64, tools: bool) -> Model {
        Model {
            slug: slug.to_string(),
            name: slug.to_string(),
            provider: slug.split('/').next().unwrap_or("x").to_string(),
            context_window: context,
            price_in_usd: price_in,
            price_out_usd: price_out,
            tpm: None,
            rpm: None,
            supports_tools: tools,
            supports_vision: false,
            benchmarks: Default::default(),
            region: None,
            updated_at: 0,
        }
    }

    fn sample() -> Catalog {
        let mut c = Catalog::new();
        c.merge([
            model("openai/gpt-4o", 128_000, 2.50, 10.00, true),
            model("deepseek/deepseek-chat", 128_000, 0.27, 1.10, true),
            model("google/gemini-2.5-flash", 1_000_000, 0.30, 2.50, true),
            model("openai/gpt-4o-mini", 128_000, 0.15, 0.60, true),
            model("meta/llama-3.3-70b", 128_000, 0.10, 0.30, false),
        ]);
        c
    }

    #[test]
    fn upsert_dedupes_by_slug() {
        let mut c = sample();
        let before = c.len();
        let new = c.upsert(model("openai/gpt-4o", 128_000, 9.99, 9.99, true));
        assert!(!new);
        assert_eq!(c.len(), before);
        assert_eq!(c.get("openai/gpt-4o").unwrap().price_in_usd, 9.99);
    }

    #[test]
    fn merge_counts_only_new() {
        let mut c = sample();
        let added = c.merge([
            model("openai/gpt-4o", 128_000, 9.99, 9.99, true), // exists
            model("qwen/qwen3-coder", 131_072, 0.22, 0.90, true), // new
        ]);
        assert_eq!(added, 1);
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn cheapest_respects_constraints() {
        let c = sample();
        let cheapest = c.cheapest(128_000, true, false).unwrap();
        assert_eq!(cheapest.slug, "openai/gpt-4o-mini");
        // tools off ⇒ llama becomes eligible and wins on price
        let no_tools = c.cheapest(128_000, false, false).unwrap();
        assert_eq!(no_tools.slug, "meta/llama-3.3-70b");
        // impossible constraint set ⇒ None, no panic
        assert!(c.cheapest(2_000_000, true, false).is_none());
    }

    #[test]
    fn search_matches_slug_name_provider() {
        let c = sample();
        assert_eq!(c.search("deepseek").len(), 1);
        assert_eq!(c.search("google").len(), 1);
        assert!(c.search("gpt").len() >= 2);
        assert_eq!(c.search("").len(), c.len());
        assert!(c.search("nope").is_empty());
    }

    #[test]
    fn providers_are_sorted_unique() {
        let c = sample();
        assert_eq!(c.providers(), vec!["deepseek", "google", "meta", "openai"]);
    }

    #[test]
    fn est_cost_math() {
        let m = model("x/y", 128_000, 1.0, 2.0, true);
        assert!((m.est_cost(1_000_000, 0) - 1.0).abs() < 1e-9);
        assert!((m.est_cost(0, 500_000) - 1.0).abs() < 1e-9);
    }
}
