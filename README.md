# Ferrous

A local-first AI IDE that rivals Cursor / Windsurf / Manus: a CodeMirror editor (Stage),
a Framer-style drag-and-drop canvas, and a WASI-hosted environment (terminal + rendered
browser) the AI itself uses. Rust + WASI backend, Tauri v2 shell, React + Vite frontend.

**Status:** Phase 0 — foundation scaffold. No UI yet. See
[`docs/plans/ferrous-roadmap-spec.md`](docs/plans/ferrous-roadmap-spec.md) for the full plan.

## Repository layout

```
crates/
  ferrous/          CLI binary (headless) — `ferrous shell`
  shared/           cross-cutting types: errors, secrets, version
  wasi-runtime/     WASI runtime + shell + sandbox framework   (Phase 1)
  context-index/    Graphify: AST + LSP + embeddings           (Phase 3)
  agent-loop/       agent loop + subagents + skills            (Phase 4)
  router/           model routing + COTP + Mermaid             (Phase 2)
  model-client/     unified local + cloud model client         (Phase 2)
  profiles-vault/   profiles + master-password vault + secrets (Phase 2)
  search/           local lightweight search engine            (Phase 5)
  services/         git, reviewer, scanner, MCP, skills, completion (Phase 5)
docs/
  adr/              architecture decision records
  plans/            roadmap spec (`ferrous-roadmap-spec.md`) + original brief (`plan`)
```

## Build & run

```bash
cargo build --workspace
cargo run -p ferrous -- shell   # interactive shell (Phase 0 built-ins only)
```

## Quality gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo bench --workspace --no-run
cargo deny check licenses advisories bans sources   # requires `cargo-deny`
```

## License

The workspace is provisionally dual-licensed `MIT OR Apache-2.0` while commercial-vs-open
source is undecided. Dependencies are **permissive-only** (see `deny.toml` and
`docs/adr/0001-permissive-only-dependency-policy.md`).
