//! Cross-cutting types shared by every Ferrous crate.
//!
//! # Error-handling convention
//!
//! - **Library crates** define their own [`thiserror`]-derived error enums and expose a
//!   [`Result`] alias (see the [`error`] module).
//! - The **`ferrous` binary** uses `anyhow` for top-level error handling and attaches
//!   context at each fallible step.

#![forbid(unsafe_code)]

pub mod error;
pub mod secret;

/// The product version, single-sourced from the `shared` crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
