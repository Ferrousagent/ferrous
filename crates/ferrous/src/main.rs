//! Entry point for the `ferrous` command-line interface.

mod cli;
mod shell;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    init_tracing();

    match cli::Cli::parse().command {
        cli::Command::Shell(args) => shell::run(shell::ShellOptions {
            json: args.json,
            auto_approve_native: args.auto_approve_native,
        }),
    }
}

/// Initialise structured logging, honouring `RUST_LOG` (defaults to `info`).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
