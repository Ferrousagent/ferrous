# Ferrous

A local-first AI IDE that pairs a CodeMirror editor (Stage), a Framer-style
drag-and-drop canvas, and a WASI-hosted environment (terminal + rendered
browser) that the AI itself uses. Rust + WASI backend, Tauri v2 shell,
React + Vite frontend.

**Status:** Phase 1 in progress — the WASI execution core is implemented and
verified from the CLI. No UI yet (UI ships last, by design). See
[`docs/adr/`](docs/adr/) for the architecture decisions and the root
`docs/plans/ferrous-roadmap-spec.md` for the full roadmap.

## What works today

The Phase 1 security core — everything else in the product builds on this:

- **`wasi-runtime`** — an embedded Wasmtime (WASI preview 2 / component model)
  runtime with a capability-based sandbox:
  - explicit, typed capability grants (filesystem roots, environment
    allowlists, loopback ports, native execution);
  - deny-by-default posture: missing grants fail closed, with no ambient
    filesystem, environment, or network authority;
  - resource limits (memory, output budget, wall-clock timeout, instruction
    fuel) and cooperative cancellation;
  - live streaming of terminal events to the UI boundary;
  - a serialized action broker with human-in-the-loop approval for risky
    actions and an audit trail;
  - a fail-closed native backend: native execution is never silently
    performed — unsupported hosts return `unsupported`, they do not fall
    back to ambient execution;
  - `#![forbid(unsafe_code)]` throughout.
- **`ferrous`** — the headless CLI. `ferrous shell` runs explicitly selected
  WASI components (`run-wasi <component>`); unknown input never falls through
  to a host shell.
- **`shared`** — cross-cutting types: error-handling convention and secrets
  that are unprintable by construction (no serialization of secret values).

## Repository layout

```text
crates/
  ferrous/          CLI binary (headless) — `ferrous shell`     [implemented]
  shared/           cross-cutting types: errors, secrets        [implemented]
  wasi-runtime/     WASI runtime + shell + sandbox framework    [implemented]
  context-index/    Graphify: AST + LSP + embeddings            [planned — Phase 3]
  agent-loop/       agent loop + subagents + skills             [planned — Phase 4]
  router/           model routing + COTP + Mermaid              [planned — Phase 2]
  model-client/     unified local + cloud model client          [planned — Phase 2]
  profiles-vault/   profiles + master-password vault + secrets  [planned — Phase 2]
  search/           local lightweight search engine             [planned — Phase 5]
  services/         git, reviewer, scanner, MCP, skills, completion [planned — Phase 5]
docs/
  adr/              architecture decision records
  plans/            roadmap spec
```

## Build & run

```bash
cargo build --workspace
cargo run -p ferrous -- shell          # interactive shell (Phase 1)
```

## Quality gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo bench --workspace --no-run
cargo deny check licenses advisories bans sources   # requires `cargo-deny`
```

CI runs these gates on every push and pull request.

## Security model

Deny-by-default, three layers:

1. **The component boundary** — only validated WASI components are admitted;
   no AOT artifacts or serialized modules.
2. **The capability boundary** — filesystem, environment, and network are
   granted explicitly and enforced by the host; guests get no handle outside
   their grants.
3. **The resource boundary** — memory, fuel, output, and wall-clock limits
   bound every guest, with cancellation as the escape hatch.

See [`docs/adr/0002-security-and-sandbox-model.md`](docs/adr/0002-security-and-sandbox-model.md).

## License

Provisional dual-license `MIT OR Apache-2.0` while the commercial-vs-open
question is undecided. Dependencies are **permissive-only** (see
`deny.toml` and [`docs/adr/0001-permissive-only-dependency-policy.md`](docs/adr/0001-permissive-only-dependency-policy.md)).
