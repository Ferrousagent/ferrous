//! Command-line argument definitions.

use clap::{Parser, Subcommand};

/// Ferrous — a local-first AI IDE. This binary is the headless (no-UI) CLI.
#[derive(Debug, Parser)]
#[command(
    name = "ferrous",
    version,
    about = "Ferrous — a local-first AI IDE (headless CLI)"
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the WASI shell / REPL.
    Shell,
}
