//! `~/.ferrous/config.toml` — the single source of truth for the brain's knobs.
//!
//! Secrets are never logged; `show` redacts them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// User-editable configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model to use when the router has no opinion.
    pub default_model: Option<String>,
    /// Hard daily spend cap in USD across all providers.
    pub daily_budget_usd: Option<f64>,
    /// Provider → API key. File values are overridable via `FERROUS_<PROVIDER>_KEY`.
    pub api_keys: BTreeMap<String, String>,
}

impl Config {
    /// `~/.ferrous/config.toml`.
    pub fn default_path() -> PathBuf {
        home().join(".ferrous").join("config.toml")
    }

    /// `~/.ferrous/data` — snapshots and caches live here.
    pub fn data_dir() -> PathBuf {
        home().join(".ferrous").join("data")
    }

    /// Snapshot path: `~/.ferrous/data/catalog.fr`.
    pub fn snapshot_path() -> PathBuf {
        Self::data_dir().join("catalog.fr")
    }

    /// Load config; a missing file yields defaults (never an error).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(toml::from_str(&raw)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist config, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Resolve a provider key: env var wins, then the config file.
    pub fn key(&self, provider: &str) -> Option<String> {
        let env = format!("FERROUS_{}_KEY", provider.to_uppercase().replace('-', "_"));
        std::env::var(&env)
            .ok()
            .or_else(|| self.api_keys.get(provider).cloned())
    }

    /// Human-readable, fully redacted dump for `ferrous config show`.
    pub fn redacted(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "default_model    = {}\n",
            self.default_model.as_deref().unwrap_or("(unset)")
        ));
        out.push_str(&format!(
            "daily_budget_usd = {}\n",
            self.daily_budget_usd
                .map(|v| format!("${v:.2}"))
                .unwrap_or_else(|| "(unset)".into())
        ));
        for (provider, key) in &self.api_keys {
            let masked = if key.chars().count() >= 4 {
                let head: String = key.chars().take(3).collect();
                let tail: String = key
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("{head}****{tail}")
            } else {
                "****".into()
            };
            out.push_str(&format!("api_keys.{provider} = {masked}\n"));
        }
        out
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trips_through_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config {
            default_model: Some("deepseek/deepseek-chat".into()),
            daily_budget_usd: Some(0.5),
            ..Config::default()
        };
        cfg.api_keys
            .insert("openai".into(), "sk-abcdef123456".into());

        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.default_model.as_deref(),
            Some("deepseek/deepseek-chat")
        );
        assert_eq!(loaded.daily_budget_usd, Some(0.5));
        assert_eq!(loaded.api_keys.get("openai").unwrap(), "sk-abcdef123456");
    }

    #[test]
    fn missing_file_is_defaults_not_error() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn redacted_hides_keys() {
        let mut cfg = Config::default();
        cfg.api_keys
            .insert("openai".into(), "sk-supersecret1234".into());
        let out = cfg.redacted();
        assert!(out.contains("sk-****1234"));
        assert!(!out.contains("supersecret"));
    }

    #[test]
    fn env_var_overrides_file() {
        let mut cfg = Config::default();
        cfg.api_keys.insert("openai".into(), "file-key".into());
        unsafe {
            std::env::set_var("FERROUS_OPENAI_KEY", "env-key");
        }
        assert_eq!(cfg.key("openai").unwrap(), "env-key");
        unsafe {
            std::env::remove_var("FERROUS_OPENAI_KEY");
        }
        assert_eq!(cfg.key("openai").unwrap(), "file-key");
    }
}
