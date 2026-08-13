# Ferrous — AI IDE Roadmap Spec

> A local-first AI IDE that rivals Cursor / Windsurf / Manus, pairing a CodeMirror
> editor (Stage), a Framer-style drag-and-drop canvas, and a WASI-hosted environment
> (terminal + rendered browser) that the AI itself uses. Rust + WASI backend, Tauri v2
> shell, React + Vite frontend.
>
> **Status:** Planning only. No code has been changed. This document is the roadmap and
> is the input for per-topic deep-dive planning sessions.

---

## 1. Purpose of this document

This spec orders the entire project into a **holistic, backend-first sequence of tasks**.
It is deliberately **topic-by-topic** (Task 1, Task 2, …) rather than fully decomposed:

- Dependencies between tasks are called out at a high level.
- The deep research for *how* each task is built (exact libraries, schemas, benchmarks)
  happens when that task becomes current — it is a **trigger**, not a pre-baked design.
- Every task carries an **acceptance criteria** ("done") block and a **research trigger**
  so an AI agent can pick it up and execute it while the user acts as director/reviewer.

---

## 2. Interview summary (decisions locked in)

These answers override any ambiguity in the original `plan`.

| Topic | Decision |
|---|---|
| Current state | **100% greenfield.** The old `ferrous` Rust scaffold is deleted and not reused. |
| Who builds it | The user directs; **AI agents do the implementation work**. Roadmap must be agent-executable. |
| End goal | Commercial **or** open source — **undecided**. Architecture must stay neutral to both. |
| Licensing | **Permissive-only** dependencies (MIT / Apache-2.0 / BSD). No copyleft. |
| "Argon" | **Argon2** — key derivation / encryption for API keys & passwords in the local credential DB. |
| App shell | **Tauri v2 + Rust + WASI** backend; **React + Vite** frontend (UI kit "Astryx UI" to confirm name/license). Backend must be "blindingly fast." |
| UI timing | **UI last.** All backend/engine work first; the polished React/Tauri 3-panel UI ships at the very end. Backend is verified through a **CLI-first surface** (the WASI shell in the system terminal + audit-log output) — no React app required to test backend tasks. |
| AI engine | **Fully custom Rust agent** (loop, routing, context) with **specialised subagents** and a **skills runtime**. |
| Subagents | **Core feature.** An orchestrator agent spawns specialised subagents, each with focused context, tool subset, and budget. The Mermaid router selects which subagents a task needs. |
| Skills | **Core feature.** Skills = versioned, **sandboxed capability bundles** (WASM components) that agents/subagents load. Registry/marketplace later; runtime early. |
| Profiles & vault | **User profiles with master-password sign-in on launch.** Each profile has its own models, keys, balances, settings, and audit log. Credentials encrypted with Argon2 + sealed keyring. Settings panel included (UI at the end). |
| Sandboxing | **Everything sandboxed.** Every subsystem — agent, tools, browser, MCP servers, skills, search fetchers, git operations — runs in capability-scoped WASI sandboxes. No ambient authority anywhere. |
| Secrets & env vars | **Encrypted private data.** User-provided secrets live in a per-profile encrypted store (vault-backed) and are injected as **environment variables only into capability-scoped sandboxes at runtime** — never plaintext on disk, never in logs/audit. |
| Rich agent plans | **Structured plans, not wall-of-text.** The agent loop emits a typed plan model (steps, status, files, risks, diffs); chat renders it with real fonts/colors/cards/spacing and interactive approve/edit. Model is backend (Phase 4); rendering is UI (Phase 6). |
| Research feature set | From surveying Cursor / Windsurf / Copilot / Devin / Manus / Claude Code: **checkpoints & AI-undo** (auto local git repo), **background agents** (local-only, while the PC is on), **ask/interview mode** (read-only Q&A), **custom slash commands**, **@-mentions**, **self-evolving agent** (installs skills/MCP/plugins with approval), **deploy & preview via the user's MCP hosting**. Skipped for now: model playground, worktree isolation, hooks, auto-fix loop, test/debugger integration, mission-control panel, NL terminal. |
| WASI screen (right panel) | A **VS Code-style terminal + a WASI-rendered headless-browser view** the AI uses to browse, click buttons, and test — like Vercel Agent Browser, **no multimodal model**. Headless core early; rendered view in the UI phase. |
| Model runtime | **Hybrid from day one** — local + cloud with auto-routing. |
| "Ferrous OS / kernel" | **Deferred.** Treat as an in-app **fast WASI runtime + shell**. The literal "custom Linux kernel" is unrealistic for now and is descoped. |
| Platforms | **Universal** via Tauri bundler: `.deb` (Linux), `.exe` (Windows), `.dmg/.app` (macOS). |
| Roadmap format | Flat, **ordered task list** — no time estimates. Dependencies resolved per-task when current. |
| Ordering | **Backend-first; UI at the very end.** |
| Graphify (code graph) | Approach **undecided** → placeholder: **hybrid** tree-sitter AST + LSP + optional embeddings. Deep-research at that milestone. |
| Languages | **As many as possible** (tree-sitter grammars), prioritized: Rust, TS/JS, Python, HTML/CSS. Index engine must be fast, in Rust. |
| Voice (Jarvis / Kokoro-82M) | **Late, opt-in only.** Not default — a popup (Wispr-Flow style); user must agree and download the model themselves. |
| Hardware | **CPU-first** (low RAM, fast rendering, hardware-level processing). GPU accelerates when present; cloud APIs for heavy models. |
| Search engine | **Required, early-ish.** Super-lightweight, high-quality, easy-to-read; doubles as the AI's click/test surface. |
| Quality bar | **"Elite / brutal" standards** = enterprise-grade **performance + security**, hard to replicate / reverse-engineer. |

---

## 3. Constraints & non-negotiables

1. **Permissive-only deps.** Reject GPL/AGPL/SSPL. If a referenced project (e.g. Instackable)
   is copyleft, build a permissive equivalent or use only its MIT/Apache portions. (Research trigger.)
2. **UI last.** Backend + engine first, verified via the CLI surface. The React/Tauri UI
   (stage, canvas, WASI screen render, chat, settings, sign-in) is the final phase.
3. **CPU-first performance.** Local model path and rendering must work on CPU with low RAM.
   GPU/cloud are accelerations, never requirements.
4. **Hybrid routing from day one** of the model layer — never local-only or cloud-only.
5. **Everything sandboxed.** Every subsystem runs in capability-scoped WASI sandboxes
   (process-level isolation where feasible). No ambient authority; hostile input at every ingress.
6. **Profiles & vault.** No unlocked-at-rest state. Launch requires the master password;
   profiles are fully isolated from each other.
7. **No plaintext secrets.** Private data is stored encrypted (vault-backed) and injected only
   as env vars into capability-scoped sandboxes at runtime; redacted in logs and audit.
8. **Agent-executable.** Every task must be written so an AI agent can implement and verify it;
   acceptance criteria are the contract. Backend tasks must be verifiable from the CLI.
9. **No real kernel/OS.** "Ferrous OS" = the in-app WASI runtime + shell. Any real-Linux-distro
   ambition is explicitly out of scope for this roadmap.
10. **No voice in the default path.** Voice is opt-in and never blocks the core loop.

---

## 4. High-level architecture (target)

```
┌────────────────────────────  Tauri v2 app (React + Vite) — LAST PHASE  ──────────────┐
│  Sign-in · Settings │ Stage (CodeMirror 6) │ Canvas (Instackable/Motion) │ WASI Screen │
│  file tree · tabs · inline completion     │ drag-drop · mirror · preview │ terminal +  │
│  floating chat · AI Tab key               │                              │ browser view │
├──────────────────────────  Tauri command bridge (IPC)  ───────────────────────────────┤
│                          Rust core (workspace crates)                                  │
│  wasi-runtime · context-index · agent-loop(+subagents,skills) · router                 │
│  model-client · profiles-vault · search · services · settings                          │
│  CLI verification surface: `ferrous shell` (system terminal)                           │
└───────────────────────────────────────────────────────────────────────────────────────┘
```

- **`wasi-runtime`** — embedded Wasmtime + WASI preview 2 / component model, capability grants, shell/REPL, headless browser core, and the sandbox framework every subsystem uses. This is the "Ferrous OS" centerpiece.
- **`context-index` (Graphify)** — tree-sitter AST graph + LSP semantics + optional embeddings; feeds the loop token-budgeted context. Also defines the canvas-mirror data contract early.
- **`agent-loop`** — custom Rust agent: plan/act/observe, tool registry, **subagents**, **skills runtime** (WASM-component skills), **structured plan model** (steps/status/risks/diffs), state persistence, interrupt.
- **`router`** — custom Rust cost/latency/capability calculator + COTP token pipeline + Mermaid agent routing (task → agents/subagents).
- **`model-client`** — unified local (GGUF/llama.cpp or equiv) + cloud (OpenAI-compatible) streaming client.
- **`profiles-vault`** — profiles + master-password vault (Argon2 + sealed keyring): models, prices, balances, TPM/RPM, benchmarks, settings, audit logs, all per-profile and isolated.
- **`search`** — self-hosted lightweight metasearch (SearXNG-like or lighter), also the AI's click/test surface.
- **`services`** — git/code-hosting integration, code reviewer, security scanner, MCP runtime, skills registry, completion engine — all sandboxed.
- **`canvas-mirror` (Local Visual Engine)** — live JSON/YAML layout tree + direct-state injection (JS event commands over the Tauri bridge). Data contract defined in Phase 3; implementation with the canvas in the UI phase.

---

## 5. The roadmap (holistic order)

> Legend: **AC** = acceptance criteria (definition of done). **RT** = research trigger
> (deep-dive planning when this task becomes current).
> **UI is last.** All Phase 1–5 tasks are verified from the CLI (`ferrous shell` + audit output).

### Phase 0 — Foundation & engineering bar
Everything else depends on this. Non-negotiable to do first.

- **T0.1 — Workspace bootstrap.** Cargo workspace with crates: `wasi-runtime`, `agent-loop`,
  `router`, `context-index`, `model-client`, `profiles-vault`, `search`, `services`, plus a
  `shared` types crate. A CLI binary `ferrous` (no UI) that opens the WASI shell.
  - **AC:** `cargo build` succeeds; `ferrous shell` runs in the system terminal and executes a trivial command.
  - **RT:** finalize crate boundaries and the shared error/type model.
- **T0.2 — Standards & CI gates.** `cargo fmt --check`, `cargo clippy -- -D warnings`,
  strict TypeScript (`strict: true`, no `any`) for the future UI, test + bench in CI. ADR
  directory + security model doc (permissive-only license policy, threat model, sandbox model).
  - **AC:** A PR with a lint/format/test violation cannot merge.
- **T0.3 — Error & observability foundation.** `thiserror`/`anyhow` split (library vs binary),
  structured `tracing`/OpenTelemetry spans, a secrecy type for credentials (redacted in logs).
  - **AC:** Every future crate logs structured spans; secrets are unprintable by construction.

### Phase 1 — WASI runtime ("Ferrous OS" centerpiece)
The substrate everything else runs on — including the sandbox-everything rule.

- **T1.1 — Embedded Wasmtime runtime.** Embed Wasmtime; WASI preview 2 + component model.
  Capability-based sandbox: no ambient authority, explicit grant of stdio/fs/net.
  - **AC:** Load and run a WASI component with only granted capabilities; ungranted access fails closed.
  - **RT:** WASI preview 2 vs 3 maturity; async component bindings.
- **T1.2 — WASI shell / REPL (CLI-first).** The `ferrous shell` runs WASI components, streams
  stdio, tracks cwd/env, enforces per-command capability grants. This is the **verification
  surface for every backend phase**; the Tauri terminal render comes in the UI phase.
  - **AC:** Type a WASI command, see streamed output; `exit`/signals work; killed processes free resources.
- **T1.3 — Headless browser core.** A lightweight headless browser the AI controls to browse,
  click, and test. Headless-only now — the human-facing render comes in the UI phase. No multimodal model.
  - **AC:** AI can navigate a URL, read simplified DOM/text, click an element, and read the result — all from the CLI.
  - **RT:** Evaluate lightweight browser engines with permissive licenses; confirm WASI-host feasibility.
- **T1.4 — Sandbox framework.** The reusable capability-grant + isolation layer for every
  future subsystem: agent tools, skills, MCP servers, search fetchers, git operations.
  Process-level isolation where feasible; in-process WASI capability scoping elsewhere.
  - **AC:** A subsystem granted `read dir A` cannot touch dir B; a sandbox escape attempt fails closed and logs.

> **Risk flag:** T1.1 (WASI preview 2 vs 3) and T1.3 (lightweight WASI-hostable browser engines)
> are the least-mature technology on the roadmap. Do T1.1–T1.2 (runtime + shell) strictly first;
> T1.3 can be parallelized/slipped into Phase 1 without blocking anything — the browse tool
> (T4.2) is its only Phase-1 dependent.

### Phase 2 — Model layer, routing & vault (hybrid from day one)
The routing engine is a named differentiator; profiles/vault make it safe.

- **T2.1 — Unified model client.** One streaming interface for local (GGUF/llama.cpp or equiv)
  and cloud (OpenAI-compatible) backends; tool/JSON-mode support.
  - **AC:** The same call path streams from a local model and a cloud API; cancellation works.
  - **RT:** Pick the local inference engine (CPU-first, low RAM, permissive license).
- **T2.2 — Profiles & vault.** Multiple user profiles; **master-password sign-in on launch**
  (Argon2-derived key + sealed keyring). Per profile: models, prices, balances, TPM/RPM,
  benchmarks, settings, audit logs. Typed settings store (backend — the settings *panel* is UI-phase).
  - **AC:** App refuses to start unlocked; two profiles are fully isolated (keys/settings/logs);
    credentials are unreadable at rest; failed password attempts are rate-limited.
  - **RT:** Embedded DB choice (e.g. redb/sled/SQLite) with encryption; keyring backend per OS.
- **T2.3 — Elite auto-routing.** Custom Rust calculator scoring candidates by cost, latency,
  capability, TPM/RPM budget, and offline availability; selects per request.
  - **AC:** Given a request + budgets, the router picks a defensible model; routing decision is unit-tested and benchmarked.
- **T2.4 — COTP smart token pipeline.** Token-cutting for structured tasks (force COT in JSON/YAML),
  prompt compaction, structured-output enforcement; **exempts creative tasks**.
  - **AC:** Token usage drops on a benchmark task set vs a naive baseline; creative-task quality is unaffected.
- **T2.5 — Mermaid agent routing.** Parse a `.md`/Mermaid flowchart mapping task type → agents
  **and specialised subagents** needed; precompute selection before invoking the loop.
  - **AC:** A flowchart change changes agent/subagent selection deterministically; invalid flowcharts error cleanly.
- **T2.6 — Secrets & encrypted environment variables.** Per-profile encrypted secrets store
  (vault-backed, T2.2); secrets injected as env vars **only into capability-scoped sandboxes**
  (T1.4) at runtime. Redacted everywhere; revocable per environment.
  - **AC:** Define a secret scoped to a profile + environment; it reaches a granted sandbox as an env
    var and exists nowhere else — not on disk in plaintext, not in logs/audit; revoking it cuts future injections.

### Phase 3 — Context engine ("Graphify")
The accuracy/no-hallucination backbone. **Built before the agent** so the loop, subagents,
ask mode, the reviewer, and completions all consume a real interface, not a stub.

- **T3.1 — Tree-sitter AST index.** Language-agnostic symbol/ref/call graph; incremental reparse
  on file change; fast in Rust.
  - **AC:** Open a repo, get symbol/definition/reference results in milliseconds; index updates on save.
  - **RT:** Finalize grammar set (Rust, TS/JS, Python, HTML/CSS first) and index schema.
- **T3.2 — LSP semantic layer.** Attach language servers for precise types, go-to-def/refs.
  - **AC:** Type-accurate navigation for the v1 languages.
  - **RT:** Which LSPs and how to host them sandboxed. Expect some LSPs to require native-process
    hosting (not WASI) — decide the fallback under the Phase 1 sandbox framework.
- **T3.3 — Optional embeddings retrieval.** Fuzzy/semantic retrieval for "what's relevant" queries,
  on top of the deterministic graph; optional and additive.
  - **AC:** A relevance query returns correct files without the model reading the whole repo.
  - **RT:** Local embedding model + vector store (CPU-friendly, permissive).
- **T3.4 — Context assembly & token budget.** Compose graph + diff + relevant snippets into a
  token-budgeted context per agent **and subagent**; verifiable that nothing is fabricated.
  - **AC:** Context is reproducible; token budget is enforced; hallucination-prone gaps are marked.
- **T3.5 — Canvas-mirror data contract (de-risk UI-last).** Define the JSON/YAML layout-tree
  schema and the JS event-command vocabulary that the Local Visual Engine will use — as a
  typed API in this phase, so the canvas work in the UI phase is unblocked.
  - **AC:** The schema and command vocabulary are typed, versioned, and round-trip-tested without any canvas existing.
- **T3.6 — @-mention resolution.** `@file` / `@symbol` / `@web` references in chat resolve via the
  graph index into token-budgeted context. (`@web` uses the headless browser (T1.3) until the
  search engine (T5.4) lands.)
  - **AC:** A mention resolves to the correct file/symbol/page; unresolvable mentions error cleanly.

### Phase 4 — Custom Rust agent engine (loop + subagents + skills)
The loop that turns routing + context + tools into autonomous work. Consumes Phase 3's real context.

- **T4.1 — Agent loop core.** Plan/act/observe state machine, tool registry, iteration budget,
  structured step output; deterministic and resumable; context assembled via the Phase 3 interface.
  - **AC:** An agent plans, calls tools, observes, and terminates on a fixed multi-step task.
- **T4.2 — Tool primitives.** File read/write/edit (patch), run (WASI shell), search (stub → Phase 5),
  browse (T1.3). Tools are capability-gated through the WASI sandbox framework (T1.4).
  - **AC:** Each tool is individually testable and sandboxed; hostile paths are denied.
- **T4.3 — Subagents.** Orchestrator spawns specialised subagents with focused context, tool
  subsets, and budget isolation; results marshaled back to the orchestrator. Mermaid routing
  (T2.5) selects which subagents a task needs; each subagent gets a token-budgeted context (T3.4).
  - **AC:** A task routed to a subagent executes with a constrained tool/context scope and returns a structured result; runaway subagents are interruptible.
- **T4.4 — Skills runtime.** Load, version, and execute **sandboxed skill bundles** (WASM
  components) into agent/subagent toolchains. Skills run inside the WASI sandbox — no ambient authority.
  - **AC:** Install a skill bundle, grant it capabilities, have a subagent invoke it; ungranted skill actions fail closed.
  - **RT:** Skill packaging format (WASM component + manifest) and versioning scheme.
- **T4.5 — State, resume & interrupt.** Persist agent + subagent state; resume after crash;
  **global hold** hard-interrupt stops a runaway loop immediately (feeds the Tab key's hold action).
  - **AC:** Kill mid-run, resume, and verify no partial/torn state.
- **T4.6 — Audit log & trace.** Immutable step log (tool calls, diffs, model choices, subagent
  activity) → printed by the CLI, consumed by the reviewer (Phase 5).
  - **AC:** Every action is replayable from the log.
- **T4.7 — Structured plan model.** The loop emits plans as a **typed, versioned data model** —
  steps with status, file targets, risks, diffs, tool-call refs — **never a wall-of-text blob**.
  CLI renders it; chat UI (T6.6) renders it richly; human can approve/edit/reject steps (CLI-first).
  - **AC:** A task produces a structured plan; each step has a lifecycle status; editing a step
    re-plans downstream steps deterministically.
- **T4.8 — Checkpoints & AI-undo.** Auto-init a **local git repo** in the workspace; snapshot/commit
  before every AI change; restore code and agent state to any checkpoint; no torn state.
  - **AC:** Every AI change is preceded by a checkpoint; restoring to any checkpoint is lossless and revertible.
- **T4.9 — Interaction modes & slash commands.** **Ask/interview mode** (read-only: no mutation
  tools, Q&A over the context index T3.1–T3.4) vs **agent mode**. **Custom slash commands**:
  user-defined reusable prompts (`/fix`, `/test`, `/commit`) with a typed command registry (CLI-first).
  - **AC:** Ask mode cannot mutate anything; a user-defined slash command runs its prompt deterministically.
- **T4.10 — Background & scheduled agents.** Local-only background agents and scheduled tasks that
  run **only while the PC is on** — pause on sleep/lock, resume on wake; state and results flow
  through the audit log. The **scheduling mechanism** lands here; concrete jobs (dependency audit,
  index refresh) wire in with Phase 5 services.
  - **AC:** A scheduled task runs at its cadence while the PC is awake, pauses on sleep, resumes after;
    nothing runs on a powered-off machine.
- **T4.11 — Self-evolving agent.** The agent can **extend itself**: when a task repeatedly needs a
  capability, it proposes installing a skill, MCP server, or plugin from the registries — every
  install requires **user approval** and runs **sandboxed**. (Core mechanism here; MCP wiring lands with T5.5.)
  - **AC:** The agent proposes a capability install, executes only after approval, and never installs outside the sandbox.

### Phase 5 — Backend services (all sandboxed)
The last backend phase before any UI work. Everything here is verifiable from the CLI.

- **T5.1 — Git / code-hosting integration.** Detect the project, required packages, and available
  updates; clone/checkout inside the WASI sandbox. Reuses the local repo handling from T4.8.
  - **AC:** Import a repo, detect language + deps, surface stale deps — all from the CLI, all sandboxed.
- **T5.2 — Harsh code reviewer + audit integration.** Review diffs for bugs/outdated packages;
  reads the audit log (T4.6) and cites the graph index (T3.1).
  - **AC:** A diff yields a review with citations to the graph index.
- **T5.3 — Cybersecurity scanner.** Trivy-like scanner that blocks malicious code/files before
  they enter the editor; runs on every ingress path inside the sandbox.
  - **AC:** A known-bad file is blocked at import with a clear report.
  - **RT:** Scanner engine + feed sources (permissive licenses).
- **T5.4 — Local search engine.** Self-hosted, super-lightweight, high-quality, easy-to-read
  results; the AI's free search and click/test surface (replaces the Phase 4 stub, same interface).
  - **AC:** AI queries it, reads clean results, and can click into pages via T1.3.
  - **RT:** SearXNG-style metasearch vs custom Rust indexer (permissive + lightweight).
- **T5.5 — MCP runtime + marketplace backend.** Run Model Context Protocol servers sandboxed in
  WASI; registry for install/uninstall. (Marketplace UI is the UI phase.)
  - **AC:** Install an MCP server from a manifest and call one tool, sandboxed.
- **T5.6 — Skills registry (backend).** Versioned registry powering the Skills Centre store.
  (The store UI is the UI phase.)
  - **AC:** Publish, pin, and roll back a skill version from the CLI.
- **T5.7 — Deploy & preview via MCP hosting.** Deploy and preview of a web project through the
  **user's own MCP hosting servers** (hosting integrations configured in the MCP runtime).
  (Canvas-specific integration lands with T7.4.)
  - **AC:** A web project artifact deploys via a user-configured MCP hosting server and returns a preview URL.
- **T5.8 — Inline completion engine.** Ultra-low-latency, CPU-first FIM completion engine over the
  model client + router + graph context (Phase 3); graceful GPU/cloud acceleration; benchmarked.
  (Consumed by the stage surface T6.3 and the Tab key T7.5.)
  - **AC:** Sub-second completions offline; latency/prompt-efficiency benchmarks published.
  - **RT:** FIM-capable local models + prompt template for the v1 languages.

### Phase 6 — UI: shell, stage & screens (the very end, part 1)
All UI ships last, on top of fully working, CLI-verified backends.

- **T6.1 — Tauri app skeleton + IPC bridge.** React + Vite wired to the command bridge (typed
  protocol, versioned, backpressure); universal bundling `.deb` / `.exe` / `.dmg/.app`.
  - **AC:** All three artifacts build and launch on their OS.
- **T6.2 — Sign-in, settings panel & profile manager.** Master-password unlock (T2.2), per-profile
  settings panel, profile switcher.
  - **AC:** Launch → sign in → profile-specific settings load; profiles cannot see each other's data.
- **T6.3 — Stage editor (CodeMirror 6).** File tree, tabs, syntax highlighting, LSP hookup, and the
  inline completion surface wired to the completion engine (T5.8).
  - **AC:** Edit/save files, navigate the tree, tabs behave; completions stream from T5.8.
- **T6.4 — WASI screen render (right panel).** Terminal view (T1.2) + rendered headless-browser
  view (T1.3) in one panel; human watches while AI operates.
  - **AC:** Human sees AI terminal output and browser clicks live.
- **T6.5 — Floating chat + collapsible sidebar.** Chat bar anchored to the agent loop; sidebar
  navigation hosting the MCP marketplace (T5.5) and Skills store (T5.6) surfaces.
  - **AC:** Chat sends a task to the loop and streams steps back; marketplace/store browse from the sidebar.
- **T6.6 — Rich plan rendering & interactive chat.** Render the structured plan model (T4.7) with
  real fonts/colors/cards/spacing — not markdown prose. Live step status, expandable diffs, and
  interactive approve/edit/reject/redirect actions.
  - **AC:** A plan renders as cards with live step status; approve/edit/reject round-trips to the loop.
- **T6.7 — Interaction polish (UI).** Ask/agent mode toggle in chat, @-mention autocomplete (T3.6),
  slash-command palette (T4.9), and a checkpoint restore timeline (T4.8).
  - **AC:** All four surfaces work against their backends from the chat bar.

### Phase 7 — Canvas & Local Visual Engine (the very end, part 2)
The "zero-pixel vision" differentiator. Implements the Phase 3 data contract.

- **T7.1 — Drag-and-drop canvas.** Instackable-based (or permissive equivalent) Framer-style
  canvas; Motion for animation; strict mathematical design rules; drag-drop blocks usable by human and AI.
  - **AC:** Build a layout with blocks; constraints are enforced by the math rules.
  - **RT:** Instackable license confirmation; permissive fallback plan.
- **T7.2 — JSON/YAML canvas mirror.** Live, compact text layout tree synced bidirectionally with
  the canvas — implements the T3.5 contract; the AI's only "vision."
  - **AC:** Editing the mirror updates the canvas and vice versa, losslessly.
- **T7.3 — Direct-state injection.** AI "sees" the UI via the YAML mirror and "clicks/drags" by
  emitting JS event commands (T3.5 vocabulary) over the Tauri bridge. No screenshots, no multimodal model.
  - **AC:** The AI selects, moves, and edits a component entirely via injected events.
- **T7.4 — Code ↔ canvas sync + deploy hookup.** Live preview; toggling between Stage (code) and
  Canvas (visual) with changes reflected both ways; deploy through the T5.7 MCP hosting path.
  - **AC:** A code edit appears in the canvas and a canvas drag rewrites code, both round-trip clean;
    a canvas project deploys via MCP hosting.
- **T7.5 — AI Tab key (full).** Single tap (editor: inline completion via T5.8; canvas: lock agent
  focus onto component + graph deps), double tap (floating chat at cursor), hold (hard interrupt → T4.5).
  - **AC:** All gestures dispatch the correct backend actions in both contexts.

### Phase 8 — Voice (opt-in only)
- **T8.1 — Jarvis voice loop.** Kokoro-82M TTS + STT. **Opt-in popup** (Wispr-Flow style);
  user must agree and download the model; never in the default path.
  - **AC:** Enable voice via popup, download model, run a full STT→action→TTS loop; disabled by default.
  - **RT:** Kokoro-82M (Apache-2.0) + STT model choice (CPU-friendly, permissive).

### Phase 9 — Hardening & distribution
- **T9.1 — Security hardening.** Pen-style review of the threat model, keyring/credential path,
  sandbox escapes (T1.4 coverage), and reverse-engineering resistance of the proprietary routing
  engine (if commercial).
  - **AC:** Threat model review clean; no secrets in binaries/logs; benchmarked crypto overhead acceptable.
- **T9.2 — Performance & final packaging.** Full benchmark suite for routing/context/completion;
  final `.deb`/`.exe`/`.dmg` + updater + first-run experience.
  - **AC:** Benchmarks published; all three artifacts install and self-update.

---

## 6. Cross-cutting dependencies (do not lose these)

- **WASI sandbox framework (T1.4)** is the substrate for *every* tool, skill, MCP server, search
  fetcher, browser, and git operation — the single most load-bearing component.
- **Routing (Phase 2)** sits between every agent action and every model call; nothing may bypass it.
- **Context index (Phase 3)** is consumed by the loop, subagents, the reviewer, ask mode,
  completions, and the canvas focus-lock — one index, many consumers.
- **Subagents (T4.3)** depend on Mermaid routing (T2.5) for selection and context budgeting (T3.4).
- **Skills runtime (T4.4)** depends on the sandbox framework (T1.4); the registry (T5.6) and store
  UI depend on the runtime.
- **Audit log (T4.6)** is consumed by the CLI, the WASI screen, the reviewer, and the scanner.
- **Search (T5.4)** replaces a stub placed in Phase 4 (T4.2); the interface must not change.
- **Canvas-mirror contract (T3.5)** is defined before the canvas exists (T7.2) so UI-last doesn't block it.
- **Profiles & vault (T2.2)** is the gate every service passes through; nothing runs unlocked.
- **Secrets store (T2.6)** injects private data only into granted sandboxes (T1.4); never plaintext on disk or in logs.
- **Structured plan model (T4.7)** is consumed by the CLI, the chat UI (T6.6), and the reviewer — no wall-of-text agent output.
- **Checkpoints (T4.8)** build on a local git repo; the restore timeline shows in the chat UI (T6.7).
- **@-mentions (T3.6)** resolve via the graph index; consumed by chat (T6.7).
- **Self-evolving agent (T4.11)** builds on the skills runtime (T4.4) and MCP runtime (T5.5); every self-install requires user approval and runs sandboxed.
- **Completion engine (T5.8)** depends on the model client + router (Phase 2) and graph context (Phase 3); consumed by the stage (T6.3) and Tab key (T7.5).

---

## 7. Explicitly deferred / descoped

- **Real Linux kernel / distro.** "Ferrous OS" = in-app WASI runtime + shell. Revisit only
  after Phase 9 if still desired.
- **Voice as a default feature.** Opt-in only, Phase 8, never blocking.
- **Multimodal/vision models.** The Local Visual Engine deliberately avoids them.
- **All UI.** Stage, canvas, chat, settings/sign-in screens, browser render — Phase 6–7 by design.
- **Model playground** — skipped for now; revisit after the router (T2.3) if trivial.
- **Specific Graphify approach, DB engine, search backend, canvas license.** Undecided — each is
  a research trigger at its task, subject to the permissive-only and CPU-first constraints.

---

## 8. Open questions for future deep-dive sessions

1. Astryx UI — exact name, license, and whether it's actually needed.
2. Instackable — license confirmation and permissive fallback.
3. Local inference engine for CPU-first, low-RAM, permissive-license serving.
4. Embedded DB with encryption for the profiles/vault store.
5. Headless browser engine that is lightweight, permissive, and WASI-hostable.
6. STT model (CPU-friendly, permissive) to pair with Kokoro-82M.
7. Whether "reverse-engineering resistance" means code obfuscation, a proprietary core crate,
   or simply architectural defensibility — decide before Phase 9.
8. Skill bundle format — confirm WASM component + manifest as the packaging standard.
9. Profile model — single-user multi-profile vs multi-user on one machine; master-password
   rotation/recovery UX.
10. Sandbox level — in-process WASI capability scoping vs OS process isolation per subsystem
    (LSP hosting in T3.2 will pressure this decision).
11. Plan-model schema versioning and the human edit/approve surface (CLI-first, then chat UI).
12. Secret rotation/expiry and the per-environment injection policy.
13. Self-evolving agent governance — what may it auto-install without approval, and the approval threshold.
14. Background agents: PC-on semantics (pause on sleep/lock, resume on wake) and scheduled-task cadence.
15. Model playground — deferred unless trivial; revisit after the router (T2.3).

---

## 9. "Elite" definition of done (applies to every task)

- **Performance:** every hot path has a benchmark; no regression merges.
- **Security:** no secrets in logs/binaries; all ingress treated as hostile; capability-based sandboxing everywhere.
- **Correctness:** tests are the proof — unit, integration, property, and replay.
- **Hygiene:** `clippy -D warnings`, strict TS, fmt, and CI gates pass before merge.
- **Reversibility:** state persists and resumes; audit log replays; no torn state on interrupt.

---

## 10. Performance budget (the "speed" bar)

"Blindingly fast" needs numbers. These are the p50 targets hot paths must meet on the
reference CPU-only dev machine, measured via the criterion harness (CI compile-gates;
authoritative runs on demand). Once real benchmarks exist (Phase 1+), a regression
>10% on a target blocks the merge.

| Hot path | Target (p50) |
|---|---|
| Shell command dispatch (WASI) | < 1 ms |
| Context query (symbol/ref via graph index) | < 5 ms |
| Routing decision | < 1 ms |
| Inline completion (local FIM, first token) | < 50 ms |
| IPC event round-trip (Tauri bridge) | < 2 ms |
