# 1. Permissive-only dependency policy

- Status: Accepted
- Date: 2026-08-13

## Context

Ferrous may ship as either a commercial product or an open-source project (currently
undecided). Copyleft dependencies (GPL / AGPL / LGPL / SSPL / MPL) would force that
decision early or impose redistribution obligations. Some referenced projects — e.g.
the Instackable canvas — may be copyleft, so the policy directly affects component choice.

## Decision

- Only permissive licenses are allowed: `MIT`, `Apache-2.0`,
  `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`,
  `Zlib`, `0BSD`, `Unlicense`, `MIT-0`, `Unicode-3.0`, `Unicode-DFS-2016`.
- `Apache-2.0 WITH LLVM-exception` was added with the Phase 1 WASI runtime:
  Wasmtime and Cranelift are `Apache-2.0 WITH LLVM-exception`, which cargo-deny
  treats as a distinct SPDX expression from plain `Apache-2.0`. The exception
  clause covers LLVM-derived code and does not introduce copyleft obligations,
  so the permissive policy is unchanged in spirit.
- Copyleft and weak-copyleft licenses are denied project-wide.
- The policy is enforced mechanically by `cargo-deny` in CI (`deny.toml`).
- Any per-crate exception requires a new ADR.

## Consequences

- GPL-only browser engines, editors, and libraries are off-limits.
- If Instackable is copyleft, a permissive equivalent must be built (Phase 7 research
  trigger) rather than vendoring it.
- Slightly narrower library choice, in exchange for keeping both the commercial and
  open-source paths viable without rework.
