//! Ferrous core — the brain.
//!
//! Pure Rust, zero UI, zero database: an in-memory model catalog, a config
//! system, snapshot persistence, and source sync. The router, agents and
//! sandbox build on top of this crate.

pub mod catalog;
pub mod config;
pub mod error;
pub mod model;
pub mod snapshot;
pub mod sources;
pub mod sync;

pub use catalog::Catalog;
pub use config::Config;
pub use error::{FerrousError, Result};
pub use model::{Benchmarks, Model};

/// The assembled brain: config + catalog, ready to serve the CLI or Tauri.
#[derive(Debug)]
pub struct Ferrous {
    pub config: Config,
    pub catalog: Catalog,
}

impl Ferrous {
    /// Load config + catalog snapshot from disk (defaults when absent).
    ///
    /// When no snapshot exists yet, the bundled fallback fills the catalog
    /// so the brain is never empty on a fresh machine — even offline.
    pub fn load() -> Result<Self> {
        let config = Config::load(&Config::default_path())?;
        let mut catalog = snapshot::load_or_empty(&Config::snapshot_path());
        if catalog.is_empty() {
            catalog.merge(sources::from_fallback(sync::now_secs()));
        }
        Ok(Self { config, catalog })
    }

    /// Persist the catalog snapshot.
    pub fn save_catalog(&self) -> Result<()> {
        snapshot::save(&self.catalog, &Config::snapshot_path())
    }

    /// Refresh sources into the catalog (fallback always; network on demand)
    /// and persist the result.
    pub fn sync(&mut self, fetch_network: bool) -> Result<sync::SyncSummary> {
        let fetcher = sync::HttpFetcher::new()?;
        let summary = sync::refresh(&mut self.catalog, &fetcher, fetch_network);
        self.save_catalog()?;
        Ok(summary)
    }
}
