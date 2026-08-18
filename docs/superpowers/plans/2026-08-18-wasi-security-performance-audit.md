# WASI Security and Performance Audit Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the WASI/native execution boundary against authority confusion, resource exhaustion, lifecycle races, and platform drift while adding reproducible benchmarks for the genuinely hot, sub-millisecond operations.

**Architecture:** Keep WASI as the default capability-scoped backend and native PTY as a separate approval-gated adapter. Make security decisions fail closed before side effects, make session IDs and lifecycle ownership unambiguous, and measure policy/IPC primitives separately from intentionally millisecond-scale process and Wasmtime startup work.

**Tech Stack:** Rust 2024, Wasmtime 47, WASI Preview 2, `portable-pty` 0.9, Criterion 0.5, GitHub Actions with `Swatinem/rust-cache@v2`, cargo-deny/RustSec.

## Global Constraints

- Preserve `#![forbid(unsafe_code)]` and the workspace deny-by-default capability model.
- Never convert structured argv into a shell string or add an ambient fallback.
- Never forward host environment variables unless their names are explicitly granted.
- Every security fix gets a failing regression test before production code.
- GitHub Actions is the authoritative build/test/benchmark verification surface; keep the existing Rust cache enabled.
- Do not promise end-to-end sub-millisecond execution: process spawn, PTY setup, and Wasmtime compilation are not sub-millisecond operations. Benchmark only operations where that target is meaningful.

---

### Task 1: Establish the audit baseline and CI benchmark surface

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `crates/wasi-runtime/Cargo.toml`
- Create: `crates/wasi-runtime/benches/runtime_hot_paths.rs`

**Interfaces:**
- Consumes: `CapabilityGrant`, `CommandRequest`, `ResourceLimits`, `NetworkPolicy`, `StreamOutputPipe`.
- Produces: repeatable Criterion measurements for pure policy/request/pipe operations; cached CI execution on Linux, with security tests remaining the gate.

- [ ] Confirm current clean HEAD and latest CI run before edits.
- [ ] Add a `wasi-runtime` Criterion bench target and benchmark request validation, environment selection, network policy checks, and bounded pipe writes/drains using deterministic inputs.
- [ ] Add a manually dispatched or push-triggered `performance` job that restores `Swatinem/rust-cache@v2`, runs `cargo bench -p wasi-runtime --bench runtime_hot_paths`, and stores Criterion output as an artifact; do not make a noisy timing threshold the only correctness gate.
- [ ] Verify the workflow still runs format, clippy, tests, docs, benches, license, and advisory checks.
- [ ] Push the baseline slice and inspect the cloud result before using measurements to choose optimizations.

### Task 2: Make broker identity and approval transitions race-safe

**Files:**
- Modify: `crates/wasi-runtime/src/broker.rs`

**Interfaces:**
- Consumes: `ActionBroker::enqueue`, `approve`, `deny`, `cancel`, `BrokerState`.
- Produces: a duplicate-ID error and atomic ownership of every live session.

- [ ] Add failing tests proving a second submission with an existing live ID is rejected without replacing the first session, its cancel handle, input route, sink, or audit identity.
- [ ] Add failing tests for approval/send failure cleanup: if approval dispatch cannot enqueue, the caller receives one terminal event, the pending job and handle are released, and the failure is audited.
- [ ] Add the smallest `DuplicateSession(u64)` error and reserve IDs under the same lock used for capacity accounting; use `contains_key`/`entry` rather than overwriting with `insert`.
- [ ] Make approval failure use one cleanup helper so no path leaves a pending job, handle, or input route behind.
- [ ] Verify duplicate-ID, approval, cancellation, capacity, panic-containment, and audit tests in cached Actions.

### Task 3: Enforce hard output budgets without over-emitting

**Files:**
- Modify: `crates/wasi-runtime/src/native_session.rs`
- Modify: `crates/wasi-runtime/src/lib.rs`
- Modify: `crates/wasi-runtime/src/pipe.rs`
- Modify: `crates/wasi-runtime/src/command.rs` only if a distinct output-limit terminal/error variant is required.

**Interfaces:**
- Consumes: native PTY reader chunks and `StreamOutputPipe` drained chunks.
- Produces: no `SessionEvent::Output` bytes beyond the declared combined budget, checked arithmetic, and an explicit output-limit result rather than a misleading generic unsupported event.

- [ ] Add failing tests that feed a chunk larger than the remaining native budget and assert the emitted bytes are truncated to the remaining budget, the child is killed, and the terminal result is `OutputLimit`.
- [ ] Add failing WASI streaming tests for stdout/stderr combined overflow and `usize`-overflow-safe accounting.
- [ ] Replace `len() + len()` with checked/saturating accounting, cap emitted chunks before sending them, and close/cancel the child before joining readers.
- [ ] Preserve already-buffered bytes while preventing additional writes; map output exhaustion to a specific runtime/broker outcome and audit it as failure, not unsupported host.
- [ ] Verify output-limit, normal output, cancellation, timeout, and lifecycle-order tests in Actions.

### Task 4: Harden native process lifecycle and environment boundaries

**Files:**
- Modify: `crates/wasi-runtime/src/native.rs`
- Modify: `crates/wasi-runtime/src/native_session.rs`
- Modify: `crates/wasi-runtime/src/broker.rs`

**Interfaces:**
- Consumes: `portable_pty::CommandBuilder`, allowlisted environment provider, `ChildKiller`.
- Produces: explicit environment-inheritance tests, propagated writer/read failures, and deterministic cleanup on every exit path.

- [ ] Add a real-child regression test that places a sentinel secret in the provider/parent environment and proves it is absent unless granted; retain the existing allowlisted-positive assertion.
- [ ] Add failing tests for invalid PTY input/closed writer and reader failure, requiring the session to terminate and report an error rather than silently continue.
- [ ] Audit `CommandBuilder` behavior against its 0.9 documentation: it includes only caller-set environment variables, and preserve that invariant with a comment/test.
- [ ] Replace ignored input/read errors with an owned stop/error signal; ensure timeout, cancellation, output overflow, child exit, and thread join all have one teardown path.
- [ ] Add Unix process-group assertions and platform guards so unsupported Windows native tests do not accidentally invoke `/bin/sh`, `/bin/cat`, `/bin/echo`, or `/usr/bin/env`.
- [ ] Verify native security and teardown tests on the supported Unix runner and compile/test the fail-closed branch on Windows in CI.

### Task 5: Expand cloud verification across supported platform boundaries

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `crates/wasi-runtime/src/native_session.rs` tests
- Modify: `crates/wasi-runtime/src/native.rs` tests
- Modify: `crates/wasi-runtime/src/broker.rs` tests

**Interfaces:**
- Consumes: platform-specific `cfg` guards and `NativeBackend::supported_on_host`.
- Produces: Linux/macOS native adapter coverage and Windows fail-closed coverage without weakening security gates.

- [ ] Add a matrix for `ubuntu-latest`, `macos-latest`, and `windows-latest` to the check job while retaining the cargo cache.
- [ ] Gate Unix-only process-behavior tests and mark unsupported-host expectations explicitly; do not skip common WASI policy tests.
- [ ] Run the matrix in Actions and investigate every platform failure from logs rather than masking it with broad ignores.
- [ ] Keep cargo-deny/RustSec as a separate required job and preserve concurrency cancellation.

### Task 6: Review, document, and measure honestly

**Files:**
- Modify: `docs/plans/risk-register-t11-wasi-runtime.md`
- Modify: `README.md`
- Modify: `docs/adr/0003-wasi-runtime-foundation.md` if the platform/performance boundary changed.

**Interfaces:**
- Consumes: test names, CI run IDs, Criterion reports, and verified platform scope.
- Produces: an auditable record of mitigations, known limits, benchmark methodology, and remaining risks.

- [ ] Record duplicate-ID, output-budget, environment, lifecycle, and platform mitigations with exact test names.
- [ ] Record benchmark commands and distinguish policy/pipe microbenchmarks from process spawn and Wasmtime compile/execute latency.
- [ ] Run the final cached Actions gates, inspect every job and artifact, and confirm a clean working tree.
- [ ] Report residual risk plainly: passing CI is evidence, not proof of zero bugs or bank/healthcare certification; production use would still require independent review, threat modeling, OS sandboxing, secrets governance, and deployment controls.
