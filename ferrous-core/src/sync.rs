//! Source refresh: pull everything into the catalog.
//!
//! Offline-first: the bundled fallback always runs first so the brain is
//! never empty, then live sources enrich/override by slug. Network failures
//! are collected into the summary — never fatal.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::Catalog;
use crate::error::{FerrousError, Result};
use crate::sources::{self, LiteLlmEntry, OpenRouterResponse};

pub const LITELLM_COST_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Something that can GET a URL and hand back the body. The real impl talks
/// HTTP; tests inject canned bodies — keeps sync fully headless-testable.
pub trait Fetcher {
    fn get(&self, url: &str) -> Result<String>;
}

/// Production fetcher backed by blocking `reqwest`.
#[derive(Debug, Clone)]
pub struct HttpFetcher(reqwest::blocking::Client);

impl HttpFetcher {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("ferrous/0.1")
            .build()
            .map_err(FerrousError::network)?;
        Ok(Self(client))
    }
}

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<String> {
        let resp = self.0.get(url).send().map_err(FerrousError::network)?;
        resp.text().map_err(FerrousError::network)
    }
}

/// What a sync pass did.
#[derive(Debug, Default, Clone)]
pub struct SyncSummary {
    pub total_models: usize,
    pub new_models: usize,
    pub fallback_used: bool,
    pub litellm_models: Option<usize>,
    pub openrouter_models: Option<usize>,
    /// Best-effort source failures; recorded, never fatal.
    pub errors: Vec<String>,
}

/// Unix seconds now.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Refresh `catalog` from the bundled fallback and, when asked, live sources.
/// Callers persist via `snapshot::save` afterwards.
pub fn refresh(catalog: &mut Catalog, fetcher: &dyn Fetcher, refresh_network: bool) -> SyncSummary {
    let mut summary = SyncSummary::default();
    let now = now_secs();

    // 1) Offline baseline — always.
    let fallback = sources::from_fallback(now);
    summary.new_models += catalog.merge(fallback);
    summary.fallback_used = true;

    // 2) Live sources — best effort.
    if refresh_network {
        match fetcher.get(LITELLM_COST_URL) {
            Ok(body) => match serde_json::from_str::<BTreeMap<String, LiteLlmEntry>>(&body) {
                Ok(map) => {
                    let v = sources::from_litellm(&map, now);
                    summary.litellm_models = Some(v.len());
                    summary.new_models += catalog.merge(v);
                }
                Err(e) => summary.errors.push(format!("litellm parse: {e}")),
            },
            Err(e) => summary.errors.push(format!("litellm fetch: {e}")),
        }

        match fetcher.get(OPENROUTER_MODELS_URL) {
            Ok(body) => match serde_json::from_str::<OpenRouterResponse>(&body) {
                Ok(resp) => {
                    let v = sources::from_openrouter(&resp, now);
                    summary.openrouter_models = Some(v.len());
                    summary.new_models += catalog.merge(v);
                }
                Err(e) => summary.errors.push(format!("openrouter parse: {e}")),
            },
            Err(e) => summary.errors.push(format!("openrouter fetch: {e}")),
        }
    }

    summary.total_models = catalog.len();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeFetcher {
        litellm: String,
        openrouter: String,
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, url: &str) -> Result<String> {
            if url.contains("litellm") {
                Ok(self.litellm.clone())
            } else if url.contains("openrouter") {
                Ok(self.openrouter.clone())
            } else {
                Err(FerrousError::Network("unknown url".into()))
            }
        }
    }

    fn fake() -> FakeFetcher {
        FakeFetcher {
            litellm: r#"{"acme/a": {"input_cost_per_token": 1e-7, "output_cost_per_token": 2e-7, "max_tokens": 32000}}"#.into(),
            openrouter: r#"{"data": [{"id": "acme/b", "name": "B", "context_length": 64000, "pricing": {"prompt": "0.000001", "completion": "0.000002"}, "supported_parameters": ["tools"]}]}"#.into(),
        }
    }

    #[test]
    fn offline_refresh_uses_only_fallback() {
        let mut catalog = Catalog::new();
        let summary = refresh(&mut catalog, &fake(), false);
        assert_eq!(summary.litellm_models, None);
        assert!(summary.openrouter_models.is_none());
        assert!(summary.fallback_used);
        assert_eq!(summary.total_models, catalog.len());
        assert!(catalog.get("acme/a").is_none());
    }

    #[test]
    fn live_refresh_merges_all_sources() {
        let base = sources::from_fallback(0).len();
        let mut catalog = Catalog::new();
        let summary = refresh(&mut catalog, &fake(), true);
        assert!(catalog.get("acme/a").is_some(), "litellm model present");
        assert!(catalog.get("acme/b").is_some(), "openrouter model present");
        assert!(catalog.get("acme/b").unwrap().supports_tools);
        assert!(summary.new_models >= base + 2);
        assert!(summary.errors.is_empty());
        assert_eq!(summary.litellm_models, Some(1));
        assert_eq!(summary.openrouter_models, Some(1));
    }

    #[test]
    fn broken_source_is_recorded_not_fatal() {
        let mut catalog = Catalog::new();
        let summary = refresh(
            &mut catalog,
            &FakeFetcher {
                litellm: "not json".into(),
                openrouter: "also not json".into(),
            },
            true,
        );
        assert_eq!(summary.errors.len(), 2);
        assert!(!catalog.is_empty(), "fallback still loaded");
    }
}
