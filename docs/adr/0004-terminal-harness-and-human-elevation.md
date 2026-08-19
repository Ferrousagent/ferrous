# ADR-0004: Terminal Harness and Human-Only Elevation

- Status: Accepted (Phase 1, T1.2 completion slice)
- Date: 2026-08-19
- Deciders: Ferrous architecture review

## Context

Ferrous needs a persistent terminal harness usable by both humans and AI
agents: Bash-like power for humans, structured capability-enforced execution
for agents, WASI-first execution, and a safety boundary that never exposes the
human's authority to the model.

The previous phase proved the substrate: an embedded Wasmtime/WASI Preview 2
runtime, a capability grant model, a broker with approval gating and audit, and
a native PTY backend for explicitly approved commands. What was missing was the
harness itself: persistent sessions, a typed command IR, a safe Bash-like
parser, builtins, pipelines, and a single protocol that the CLI, AI tools, and
the future Tauri/wterm UI all consume.

## Decision

Build the Ferrous Shell harness:

1. **Typed Shell IR** (`shell_ir.rs`): human shell text and AI tool calls both
   compile into one IR. Each program, argument, redirect, and cwd is a separate
   typed value; shell metacharacters can never be reinterpreted by a host shell.
   Every plan hashes to a canonical `CommandDigest` (SHA-256 over a versioned
   canonical byte encoding), so approvals and audit records bind to the exact
   plan that ran.

2. **Safe Bash-like parser** (`shell_parse.rs`): supports quoting, escaping,
   simple commands, `|`, `&&`, `||`, `;`, redirection, and background `&`. It
   explicitly rejects command substitution, arithmetic expansion, here-docs,
   aliases, startup files, functions, `eval`, and unbounded shell escapes with
   clear errors. The parser never emits a shell command string.

3. **Persistent sessions** (`terminal_session.rs`): a `TerminalSession` owns
   cwd, an environment overlay, a bounded job table, and a lease slot. `cd`
   changes only session state, never the host cwd. Builtins operate on
   capability-relative paths and enforce read/write/delete grants with
   symlink-aware resolution.

4. **Human-only elevation** (`elevation.rs`): an `ElevationRequest` describes
   the exact effects and capability delta; it contains no password field. A
   `HumanApprovalAuthority` verifies a human but takes no password and returns
   no proof. The broker mints a short-lived, digest-bound `ApprovalLease` after
   verification succeeds. The agent API exposes only a redacted
   `PendingApproval`; it cannot construct, submit, derive, replay, or widen a
   lease. Full password verification lives in `profiles-vault` (Argon2) behind
   the trusted CLI authority — never in agent-facing APIs.

5. **Execution** (`shell_executor.rs`): builtins run in-process; external
   programs run by direct argv through the native policy adapter; WASI
   components run through the embedded Wasmtime runtime; pipelines use bounded
   channels with backpressure; `&&`/`||` short-circuit on exit status;
   redirections target capability-checked files; background jobs are owned by
   the session and killed on close.

6. **Single protocol**: the CLI, the AI terminal tool, and the future
   Tauri/wterm UI all consume the same `SessionEvent` protocol. wterm wiring
   uses a WebSocket bridge (`ferrous shell --serve`) with binary framing for
   terminal bytes and JSON for session metadata.

## Consequences

- AI agents get structured execution by default; humans get familiar shell
  syntax; both land in the same capability broker.
- The password never enters `CommandRequest`, events, child environments, logs,
  audit records, or model context.
- Native shells (`bash -c`, PowerShell, cmd) remain an explicit, approval-gated
  escape hatch — never synthesized by parsing.
- Unsupported OS sandbox features return typed denials; nothing falls back to
  ambient execution.
- Passing tests are evidence, not proof of zero bugs; adversarial and fuzz
  coverage is required before claiming the harness is hardened.
