use clap::{Parser, Subcommand};
use ferrous_core::config::Config;
use ferrous_core::{Ferrous, FerrousError, Model};

#[derive(Parser)]
#[command(
    name = "ferrous",
    version,
    about = "Ferrous — your local AI brain: model catalog, router, agents, sandbox.",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage ~/.ferrous/config.toml.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Refresh the model catalog (bundled fallback always; live sources with --live).
    Sync {
        /// Also hit LiteLLM + OpenRouter over the network.
        #[arg(long)]
        live: bool,
    },
    /// Inspect the model catalog.
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Write a default config file.
    Init,
    /// Show the resolved config (secrets redacted).
    Show,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// List every known model, cheapest first.
    List,
    /// Text search across slug/name/provider.
    Search { query: String },
    /// Full detail for one slug.
    Info { slug: String },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("ferrous=info")
        .init();

    let cli = Cli::parse();
    let mut brain = Ferrous::load()?;
    tracing::info!(models = brain.catalog.len(), "ferrous loaded");

    match cli.command {
        Command::Config { cmd } => match cmd {
            ConfigCmd::Init => {
                let path = Config::default_path();
                brain.config.save(&path)?;
                println!("wrote {}", path.display());
                println!("add your keys under [api_keys], e.g. api_keys.openai = \"sk-...\"");
            }
            ConfigCmd::Show => print!("{}", brain.config.redacted()),
        },
        Command::Sync { live } => {
            let summary = brain.sync(live)?;
            tracing::info!(
                total = summary.total_models,
                new = summary.new_models,
                errors = summary.errors.len(),
                "sync complete"
            );
            println!(
                "catalog: {} models (+{} new)",
                summary.total_models, summary.new_models
            );
            if let Some(n) = summary.litellm_models {
                println!("  litellm:    {n} models");
            }
            if let Some(n) = summary.openrouter_models {
                println!("  openrouter: {n} models");
            }
            if summary.fallback_used {
                println!(
                    "  fallback:   bundled baseline (approximate prices — use --live to refresh)"
                );
            }
            for e in &summary.errors {
                eprintln!("  warn: {e}");
            }
            println!("snapshot → {}", Config::snapshot_path().display());
        }
        Command::Models { cmd } => match cmd {
            ModelsCmd::List => print_models(brain.catalog.by_price()),
            ModelsCmd::Search { query } => {
                let hits = brain.catalog.search(&query);
                if hits.is_empty() {
                    println!("no models match \"{query}\"");
                } else {
                    print_models(hits);
                }
            }
            ModelsCmd::Info { slug } => match brain.catalog.get(&slug) {
                Some(m) => print_model(m),
                None => {
                    return Err(FerrousError::ModelNotFound(format!(
                        "{slug} — try `ferrous models search {slug}`"
                    ))
                    .into());
                }
            },
        },
    }
    Ok(())
}

fn print_models(models: Vec<&Model>) {
    println!(
        "{:<42} {:>9} {:>9} {:>7} {:>5}",
        "slug", "$in/1M", "$out/1M", "ctx", "tools"
    );
    for m in models {
        println!(
            "{:<42} {:>9.2} {:>9.2} {:>7} {:>5}",
            m.slug,
            m.price_in_usd,
            m.price_out_usd,
            fmt_ctx(m.context_window),
            if m.supports_tools { "yes" } else { "" },
        );
    }
}

fn print_model(m: &Model) {
    println!("{}", m.name);
    println!("  slug:      {}", m.slug);
    println!("  provider:  {}", m.provider);
    println!("  context:   {}", fmt_ctx(m.context_window));
    println!("  price in:  ${:.4}/1M tok", m.price_in_usd);
    println!("  price out: ${:.4}/1M tok", m.price_out_usd);
    if let Some(tpm) = m.tpm {
        println!("  tpm:       {tpm}");
    }
    if let Some(rpm) = m.rpm {
        println!("  rpm:       {rpm}");
    }
    println!(
        "  tools:     {}",
        if m.supports_tools { "yes" } else { "no" }
    );
    println!(
        "  vision:    {}",
        if m.supports_vision { "yes" } else { "no" }
    );
    if let Some(r) = &m.region {
        println!("  region:    {r}");
    }
    let b = &m.benchmarks;
    if b.mmlu.is_some() || b.humaneval.is_some() || b.math.is_some() {
        println!("  benchmarks:");
        if let Some(v) = b.mmlu {
            println!("    mmlu:      {v}");
        }
        if let Some(v) = b.humaneval {
            println!("    humaneval: {v}");
        }
        if let Some(v) = b.math {
            println!("    math:      {v}");
        }
    }
}

/// 128000 → "128k", 1048576 → "1m".
fn fmt_ctx(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{}m", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}
