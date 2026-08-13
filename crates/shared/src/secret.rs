//! Secret handling: credentials that must never leak into logs or serialization.
//!
//! Wraps the [`secrecy`] crate so every Ferrous crate uses one canonical type.
//!
//! # Serialization policy
//!
//! Secrets must **never be serializable**. The `secrecy` crate's `serde` feature
//! implements `Serialize` for sized `SecretBox<T>` by emitting
//! [`ExposeSecret::expose_secret`]
//! — the plaintext. `SecretString` (`SecretBox<str>`) escapes this only because
//! `str: !Sized`. Therefore the workspace depends on `secrecy` **without** the
//! `serde` feature (see root `Cargo.toml`), so no secret type can be serialized at
//! all — a compile-time guarantee stronger than redaction. Secrets leave this crate
//! only through an explicit [`ExposeSecret`] call at a marked boundary (e.g. the
//! vault injection in Phase 2).

use crate::error::{Result, SharedError};

pub use secrecy::{ExposeSecret, SecretString};

/// Read a secret from an environment variable, failing if it is not set or not
/// valid UTF-8.
///
/// # Errors
///
/// Returns [`SharedError::EnvVarMissing`] when the variable is absent or not valid UTF-8.
pub fn secret_from_env(name: &'static str) -> Result<SecretString> {
    std::env::var(name)
        .map(SecretString::from)
        .map_err(|_| SharedError::EnvVarMissing { name })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap freely

    use super::*;

    #[test]
    fn debug_is_redacted() {
        let secret = SecretString::from("hunter2");
        let debug = format!("{secret:?}");

        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("hunter2"));
    }

    #[test]
    fn expose_roundtrips() {
        let secret = SecretString::from("hunter2");
        assert_eq!(secret.expose_secret(), "hunter2");
    }

    #[test]
    fn secret_from_env_reports_missing_var() {
        // No env mutation (env::set_var/remove_var are unsafe in edition 2024);
        // use a key that is effectively guaranteed to be unset.
        let err = secret_from_env("__FERROUS_DEFINITELY_NOT_SET_12345__").unwrap_err();
        match err {
            SharedError::EnvVarMissing { name } => {
                assert_eq!(name, "__FERROUS_DEFINITELY_NOT_SET_12345__");
            }
        }
    }
}
