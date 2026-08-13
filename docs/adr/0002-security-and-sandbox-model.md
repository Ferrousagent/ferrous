# 2. Capability-based security & sandbox model

- Status: Accepted
- Date: 2026-08-13

## Context

Ferrous executes untrusted inputs — code files, web content, model output, MCP servers,
skills, and git repositories — and stores user credentials. A breach in any subsystem
compromises the user's machine, so isolation must be the default, not an afterthought.

## Decision

1. **Everything sandboxed.** Every subsystem runs in capability-scoped WASI sandboxes
   (embedded Wasmtime). No ambient authority; stdio/fs/net are explicitly granted per
   subsystem.
2. **Layered isolation.** Process-level isolation where feasible; in-process WASI
   capability scoping elsewhere.
3. **Credentials.** Encrypted at rest (Argon2 + OS keyring); injected only as environment
   variables into granted sandboxes; never plaintext on disk or in logs.
4. **No unsafe code.** All crates `#![forbid(unsafe_code)]` (also enforced at workspace
   level via `unsafe_code = "forbid"`).
5. **Locked on launch.** Master-password sign-in is required; nothing runs unlocked.

## Consequences

- Sandboxing overhead must be benchmarked — performance is never sacrificed silently.
- Some language servers (LSP) may require native-process hosting outside WASI; that is a
  documented, capability-limited fallback (open question #10 in the roadmap spec).
- Phase 9 (security hardening) re-audits sandbox escapes and the keyring path before
  distribution.
