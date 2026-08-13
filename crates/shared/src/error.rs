//! Shared error types and the project-wide error-handling convention.

/// Errors produced by `shared` utilities.
#[derive(Debug, thiserror::Error)]
pub enum SharedError {
    /// A required environment variable was not set, or not valid UTF-8.
    #[error("environment variable `{name}` is not set or not valid UTF-8")]
    EnvVarMissing {
        /// The name of the missing environment variable.
        name: &'static str,
    },
}

/// Result alias for the `shared` crate.
pub type Result<T, E = SharedError> = core::result::Result<T, E>;
