# ferrous

A local-first AI IDE brain — Rust core + CLI, built to power a Tauri desktop shell later.

Current phase: **headless backend**. The UI is deliberately not built yet; the product is a testable Rust workspace that a Tauri app will eventually wrap.

## Layout

```
Cargo.toml            workspace + shared deps + zero-bloat release profile (LTO, strip)
ferrous-core/         the brain — pure Rust, no UI, no network at runtime
ferrous-cli/          `ferrous` terminal command
docs/                 design references (Linear dark tokens for the future UI)
```

### ferrous-core

| Module | What it does |
|---|---|
| `model.rs` | `Model` + `Benchmarks` — price, context window, TPM/RPM, tools/vision, region |
| `catalog.rs` | In-memory catalog — HashMap slug index, `cheapest()`, `capable()`, `search()` |
| `sources.rs` | LiteLLM + OpenRouter JSON parsers + bundled offline fallback snapshot |
| `sync.rs` | Injectable fetcher (headless-testable), best-effort network sync |
| `snapshot.rs` | postcard persistence, atomic pid-unique tmp+rename |
| `config.rs` | `~/.ferrous/config.toml`, env overrides, redacted secrets |
| `error.rs` | Typed errors, zero panics |

## CLI

```sh
ferrous config init          # create ~/.ferrous/config.toml
ferrous config show          # masked keys
ferrous models list          # all models, sorted by price
ferrous models search <q>    # e.g. ferrous models search deepseek
ferrous models info <slug>   # benchmarks + region
ferrous sync                 # refresh catalog from remote sources (falls back offline)
```

Works offline out of the box — the bundled snapshot means first run needs no network.

## Dev

```sh
cargo test          # 18 tests — catalog, snapshot, config, sources, merge
cargo clippy        # zero warnings
cargo fmt           # rustfmt
```

## Roadmap (next ticks)

1. Router with fallback — `ferrous route "hello"` picks cheapest capable model, survives dead providers, tracks cost
2. Agent graph + executor (JSON + Mermaid specs)
3. Bastallion — wasmtime sandbox for untrusted code
4. OSV security gate — block vulnerable packages before install
5. GitHub deps layer — octocrab "12 updates available" panel
6. Tauri shell on top — re-scaffold frontend with `create-tauri-app`, apply tokens from `docs/design-linear-tokens.md`
