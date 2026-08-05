//! Central error type for the Ferrous brain.

use thiserror::Error;

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, FerrousError>;

/// Every failure mode the core can hit. Grouped, typed, zero-dependency.
#[derive(Debug, Error)]
pub enum FerrousError {
    /// Filesystem failure (config, snapshot, data dir).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed or unreadable TOML config.
    #[error("config parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    /// Failed to serialize config to TOML.
    #[error("config serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    /// Any network-layer failure during a source sync.
    #[error("network error: {0}")]
    Network(String),

    /// A snapshot file on disk could not be decoded.
    #[error("snapshot corrupted: {0}")]
    Snapshot(String),

    /// `models info` / lookup missed.
    #[error("model not found: {0}")]
    ModelNotFound(String),
}

impl FerrousError {
    /// Lift a `reqwest` failure into our error type without leaking the dependency.
    pub fn network(err: impl std::fmt::Display) -> Self {
        FerrousError::Network(err.to_string())
    }
}
