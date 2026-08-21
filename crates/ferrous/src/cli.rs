//! Command-line argument definitions.

use clap::{Args, Parser, Subcommand};

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
    /// Open the persistent Ferrous shell / REPL.
    Shell(ShellArgs),
    /// Serve the shell in a browser terminal (wterm) over WebSocket.
    Wterm(WtermArgs),
}

/// Options for the wterm WebSocket terminal server.
#[derive(Debug, Args)]
pub struct WtermArgs {
    /// Interface to bind (default: loopback only).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// TCP port to bind; 0 selects an ephemeral port.
    #[arg(long, default_value_t = 8765)]
    pub port: u16,
}

/// Options for the persistent shell.
#[derive(Debug, Args)]
pub struct ShellArgs {
    /// Emit structured JSON records for each session event.
    #[arg(long)]
    pub json: bool,
    /// Auto-approve native commands inside the workspace (test harness use
    /// only; never enables ambient authority).
    #[arg(long)]
    pub auto_approve_native: bool,
}
