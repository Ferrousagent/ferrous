//! Compact snapshot persistence (postcard) with atomic writes.
//!
//! The catalog is memory-first; this is just how we get it back after a
//! restart. Writes go to a temp file then rename, so a crash mid-write
//! never corrupts the last good snapshot.

use std::path::Path;

use crate::catalog::Catalog;
use crate::error::{FerrousError, Result};

/// Serialize + atomically write the catalog.
pub fn save(catalog: &Catalog, path: &Path) -> Result<()> {
    let bytes =
        postcard::to_allocvec(catalog).map_err(|e| FerrousError::Snapshot(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Pid-unique temp name: concurrent saves from different processes never
    // collide on the same temp file before the atomic rename.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a snapshot, or an empty catalog when none exists yet.
pub fn load(path: &Path) -> Result<Catalog> {
    let bytes = std::fs::read(path)?;
    postcard::from_bytes(&bytes).map_err(|e| FerrousError::Snapshot(e.to_string()))
}

/// Load or fall back to an empty catalog (never errors).
pub fn load_or_empty(path: &Path) -> Catalog {
    if !path.exists() {
        return Catalog::default(); // fresh machine — no snapshot yet
    }
    match load(path) {
        Ok(catalog) => catalog,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "catalog snapshot unreadable; starting empty");
            Catalog::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Benchmarks, Model};
    use tempfile::tempdir;

    fn model(slug: &str, price_in: f64, price_out: f64) -> Model {
        Model {
            slug: slug.to_string(),
            name: slug.to_string(),
            provider: slug.split('/').next().unwrap_or("x").to_string(),
            context_window: 128_000,
            price_in_usd: price_in,
            price_out_usd: price_out,
            tpm: Some(10_000),
            rpm: Some(100),
            supports_tools: true,
            supports_vision: false,
            benchmarks: Benchmarks {
                mmlu: Some(88.0),
                ..Default::default()
            },
            region: Some("cn".into()),
            updated_at: 1234,
        }
    }

    #[test]
    fn snapshot_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("catalog.fr");

        let mut c = Catalog::new();
        c.merge([
            model("deepseek/deepseek-chat", 0.27, 1.10),
            model("openai/gpt-4o", 2.5, 10.0),
        ]);
        save(&c, &path).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded.get("deepseek/deepseek-chat").unwrap(),
            &model("deepseek/deepseek-chat", 0.27, 1.10)
        );
        assert_eq!(
            loaded.get("openai/gpt-4o").unwrap().region.as_deref(),
            Some("cn")
        );
    }

    #[test]
    fn load_missing_file_yields_empty() {
        let dir = tempdir().unwrap();
        assert!(load(&dir.path().join("missing.fr")).is_err());
        assert!(load_or_empty(&dir.path().join("missing.fr")).is_empty());
    }
}
