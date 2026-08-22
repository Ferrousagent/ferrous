# Native PTY Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the fail-closed `NativeBackend` stub with a real, capability-gated PTY backend (`portable-pty`) so `ferrous shell` can run approved `bash`/`cargo`/`npm`/`git` sessions with streaming, input, cancellation, process-tree cleanup, and hard failure on unsupported hosts.

**Architecture:** A `NativeSession` owns a `portable_pty` pair (PTY master/slave) plus the spawned child. The broker gains a mode-aware job kind (`Wasi | Native`), a second worker thread for native sessions, and per-session input channels so the UI/CLI can send keystrokes. Approval, audit, cancellation, output budgets, and the `SessionEvent` protocol are reused unchanged.

**Tech Stack:** Rust 1.97.1, `portable-pty` 0.9.0 (MIT, WezTerm), existing `wasi-runtime` crate (broker/command/policy/capability), `std::sync::mpsc`.

## Global Constraints

- `#![forbid(unsafe_code)]` in every crate; `portable-pty`'s API is entirely safe (verified).
- Lints: `unwrap_used`/`expect_used`/`dbg_macro`/`todo`/`unimplemented` = deny in libs; test modules opt out with `#![allow(clippy::expect_used, clippy::unwrap_used)]`.
- Permissive-only deps (`deny.toml`): `portable-pty` is MIT ✅.
- No shell-string construction anywhere: AI requests use **direct argv** (`CommandBuilder`), never `sh -c`. (Risk R34.)
- Native execution ALWAYS requires explicit grant (`allow_native_execution`) AND human approval (`classify_risk` already returns `NativeExecution`). Never ambient fallback. (Risk R11.)
- Every native session: bounded output (combined budget, kill on overflow), wall-clock timeout (kill via watchdog), cancellation (kill child → process group / ConPTY tree). (Risks R23, R24, R35.)
- Secrets: only grant-allowlisted env names are passed to the child (via `policy::selected_environment`). (Risk R8.)
- All tests must pass on Linux CI (primary), with `#[cfg(unix)]`/`#[cfg(windows)]` where semantics differ.

---

### Task 1: Add `portable-pty` dependency

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/wasi-runtime/Cargo.toml`

- [ ] **Step 1: Add the workspace dependency**

In workspace `Cargo.toml` `[workspace.dependencies]`:

```toml
portable-pty = "0.9"
```

In `crates/wasi-runtime/Cargo.toml` `[dependencies]`:

```toml
portable-pty = { workspace = true }
```

- [ ] **Step 2: Verify the workspace still builds**

Run: `cargo check --workspace`
Expected: `Finished` with no warnings.

- [ ] **Step 3: Verify license gate**

Run: `cargo deny check licenses` (if `cargo-deny` installed) or rely on CI.
Expected: `portable-pty` (MIT) allowed; no copyleft.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/wasi-runtime/Cargo.toml Cargo.lock
git commit -m "chore(wasi-runtime): add portable-pty for the native PTY backend"
```

---

### Task 2: Native backend core types (`native.rs`)

**Files:**
- Rewrite: `crates/wasi-runtime/src/native.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces (produced by this task):**
- `pub struct NativeBackend;` — `impl NativeBackend { pub fn new() -> Self; pub fn spawn(&self, request: &CommandRequest) -> Result<NativeSession, NativeError> }`
- `pub struct NativeSession;` — `impl NativeSession { pub fn write_input(&mut self, bytes: &[u8]) -> Result<(), NativeError>; pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), NativeError>; pub fn child_killer(&mut self) -> Box<dyn ChildKiller + Send + Sync>; pub fn process_id(&self) -> Option<u32>; pub fn try_exit_status(&mut self) -> Result<Option<i32>, NativeError> }`
- `pub enum NativeError` — `WrongMode | NativeNotGranted | UnsupportedOnHost | Io(#[from] std::io::Error) | InvalidRequest(CommandError) | SpawnFailed(String)`
- `pub struct NativeOutput { pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub exit_code: i32 }`

- [ ] **Step 1: Write the failing tests** (add to `native.rs` test module; mirror existing `#[allow(clippy::expect_used, clippy::unwrap_used)]`):

```rust
#[test]
fn spawn_rejects_non_native_requests() { /* mode Wasi -> WrongMode */ }

#[test]
fn spawn_requires_explicit_grant() { /* no allow_native_execution -> NativeNotGranted */ }

#[test]
fn spawn_denies_cwd_outside_grant() { /* symlinked cwd escaping workspace -> InvalidRequest(WorkingDirectoryDenied) */ }

#[test]
fn spawn_uses_direct_argv_and_ignores_shell_metacharacters() {
    // program "echo", args ["$(touch /tmp/pwned)"]
    // child must print "$(touch /tmp/pwned)" literally; the file must NOT exist.
}

#[cfg(unix)]
#[test]
fn spawn_denies_missing_program() { /* nonexistent binary -> SpawnFailed/io error, never ambient fallback */ }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p wasi-runtime native`
Expected: compile error (types/fns missing) — the correct RED.

- [ ] **Step 3: Implement `native.rs`**

Full implementation sketch:

```rust
//! Native terminal boundary: capability-gated PTY execution for approved
//! developer commands (bash, cargo, npm, git).
//!
//! Phase 1 contract: native execution requires an explicit capability grant
//! AND human approval (enforced by the broker). Unsupported hosts return
//! `UnsupportedOnHost` — they never fall back to ambient execution.

use std::io::{Read, Write};
use std::path::Path;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;

use crate::command::{CommandError, CommandRequest, ExecutionMode};
use crate::policy::selected_environment;

/// Fail-closed errors from the native backend boundary.
#[derive(Debug, Error)]
pub enum NativeError {
    #[error("native backend received a non-native request")]
    WrongMode,
    #[error("native execution was not granted")]
    NativeNotGranted,
    #[error("native execution is unsupported on this host")]
    UnsupportedOnHost,
    #[error("invalid native request: {0}")]
    InvalidRequest(#[from] CommandError),
    #[error("failed to spawn native process: {0}")]
    SpawnFailed(String),
    #[error("native I/O failure: {0}")]
    Io(#[from] std::io::Error),
}

/// Captured output and exit status of one native command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// A running PTY session: reader, writer, child killer, and exit status.
pub struct NativeSession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    process_id: Option<u32>,
}

impl NativeSession {
    /// Write raw bytes (keystrokes) to the PTY master.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<(), NativeError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY viewport.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), NativeError> {
        self.master.resize(PtySize {
            rows, cols, pixel_width: 0, pixel_height: 0,
        })?;
        Ok(())
    }

    /// Clone of the child killer for a watchdog/cancel thread.
    pub fn child_killer(&mut self) -> Box<dyn ChildKiller + Send + Sync> {
        self.killer.clone_killer()
    }

    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }
}

/// The native execution backend. Phase 1 runs on hosts where the platform
/// policy adapter can be enforced; otherwise it fails closed.
#[derive(Debug, Default)]
pub struct NativeBackend;

impl NativeBackend {
    pub const fn new() -> Self {
        Self
    }

    /// Whether this host can enforce the native execution policy.
    /// Phase 1: true on unix; Windows/macOS policy adapters land later and
    /// must report unsupported until they are tested (fail closed).
    #[cfg(unix)]
    pub fn supported_on_host() -> bool {
        true
    }
    #[cfg(not(unix))]
    pub fn supported_on_host() -> bool {
        false
    }

    /// Spawn one approved native request into a PTY session.
    pub fn spawn(&self, request: &CommandRequest) -> Result<NativeSession, NativeError> {
        if request.mode != ExecutionMode::Native {
            return Err(NativeError::WrongMode);
        }
        if !request.grant.allows_native_execution() {
            return Err(NativeError::NativeNotGranted);
        }
        request.validate()?;
        if !Self::supported_on_host() {
            return Err(NativeError::UnsupportedOnHost);
        }
        // Cwd must exist and resolve inside the grant (symlink-aware).
        if !request.grant.allows_existing_path(&request.cwd) {
            return Err(CommandError::WorkingDirectoryDenied(request.cwd.clone()).into());
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| NativeError::SpawnFailed(e.to_string()))?;

        let mut builder = CommandBuilder::new(&request.program);
        builder.cwd(Path::new(&request.cwd));
        for arg in &request.args {
            builder.arg(arg);
        }
        // Only grant-allowlisted environment variables reach the child.
        for (name, value) in selected_environment(&request.grant, &|name| std::env::var(name).ok()) {
            builder.env(name, value);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| NativeError::SpawnFailed(e.to_string()))?;
        let writer = pair.master.take_writer().map_err(|e| NativeError::SpawnFailed(e.to_string()))?;
        let process_id = child.process_id();
        let killer = child.clone_killer();

        Ok(NativeSession {
            master: pair.master,
            writer,
            killer,
            process_id,
        })
    }
}

/// Drain `reader` until EOF, forwarding chunks to `emit`. Returns total bytes.
pub(crate) fn drain_reader(
    mut reader: Box<dyn Read + Send>,
    emit: &mut dyn FnMut(&[u8]),
) -> std::io::Result<usize> {
    let mut total = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        total += n;
        emit(&buf[..n]);
    }
    Ok(total)
}
```

Note: `drain_reader` is a free helper so the broker's reader thread has a
testable pure function; `emit` forwards chunks to `SessionEvent::Output`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p wasi-runtime native`
Expected: all new tests pass.

- [ ] **Step 5: Run the full crate gates**

```bash
cargo fmt --all -- --check
cargo clippy -p wasi-runtime --all-targets --all-features -- -D warnings
cargo test -p wasi-runtime --all-features
```

- [ ] **Step 6: Commit**

```bash
git add crates/wasi-runtime/src/native.rs
git commit -m "feat(wasi-runtime): capability-gated native PTY session backend"
```

---

### Task 3: Native session driver (`native_session.rs`)

**Files:**
- Create: `crates/wasi-runtime/src/native_session.rs`
- Modify: `crates/wasi-runtime/src/lib.rs` (add `pub mod native_session;`)
- Test: same file

**Interfaces (produced):**
- `pub struct NativeSessionHandle` — owned by the broker worker; wraps `NativeSession` + cancel + timeout + budget.
- `impl NativeSessionHandle { pub fn new(session: NativeSession, cancel: CancelHandle, limits: ResourceLimits, events: mpsc::Sender<SessionEvent>) -> Self; pub fn run(mut self) -> Result<NativeOutput, NativeError>; }`
- `pub struct NativeInputTx(mpsc::Sender<Vec<u8>>)` — `pub fn send_bytes(&self, bytes: Vec<u8>) -> Result<(), NativeError>`; used by the broker's `send_input`.

- [ ] **Step 1: Write the failing tests**

```rust
// Uses a real `echo`/`sh` PTY session via a tiny helper `spawn_test_session(program, args)`.
#[test]
fn session_streams_output_and_exit_code() {
    // spawn `echo hello`; run(); assert stdout contains "hello", exit_code == 0.
}

#[test]
fn session_write_input_is_delivered() {
    // spawn `cat` (or `sh`); write "hi\n" via input channel; run();
    // assert stdout contains "hi" (echoed by the pty).
}

#[test]
fn session_cancel_kills_the_child() {
    // spawn `sleep 100`; cancel the handle; run() returns Err(Cancelled) promptly (< 5s).
}

#[test]
fn session_timeout_kills_the_child() {
    // spawn `sleep 100` with limits timeout_seconds == 1; run() returns Err(timeout) in ~1s.
}

#[test]
fn session_output_budget_kills_on_overflow() {
    // spawn a program writing > budget bytes (e.g. `yes x | head -c 100000` with budget 1024);
    // run() returns Err(OutputLimit) and does not accumulate unbounded memory.
}

#[test]
fn session_cleanup_leaves_no_children() {
    // spawn `sh -c "sleep 100 &"` (process-tree case); cancel; run() returns;
    // assert the grandchild sleep is gone (kill(pid, 0) fails) — unix-only.
}
```

- [ ] **Step 2: Run the tests to verify they fail** — `cargo test -p wasi-runtime native_session` (RED: missing module).

- [ ] **Step 3: Implement `native_session.rs`**

Design contract (exact code in the executing agent's hands, but the shape):

1. `run()`:
   - spawn a **reader thread** that clones the PTY reader (`pair.master.try_clone_reader()` — require `NativeSession` to expose `try_clone_reader()` in Task 2; add it if missing) and calls `drain_reader` with an emit closure that: forwards `SessionEvent::Output { stream: Stdout, bytes }`, accumulates a combined byte count, and **kills the child + stops forwarding once the combined budget is exceeded**.
   - spawn a **watchdog thread** holding the cloned killer: every 100ms, if `cancel.is_cancelled()` or wall-clock deadline passed → `killer.kill()` and record the reason (Cancelled vs Timeout) in a shared `Mutex<Option<NativeError>>`.
   - **main thread**: poll `try_exit_status()` every 50ms until `Some(code)` (or the shared error is set); then join both threads; return `Ok(NativeOutput { stdout, stderr: vec![], exit_code: code })` or the recorded `Err`.
2. `NativeInputTx::send_bytes` forwards into an `mpsc::Sender<Vec<u8>>`; a small **input thread** owned by `run()` receives and calls `session.write_input(...)`.
3. Errors from the watchdog take precedence over the child exit: if the shared error is `Cancelled` → `Err(NativeError::Cancelled)`; `Timeout` → `Err(NativeError::Timeout)` (add both variants to `NativeError` in Task 2 — note this dependency).

- [ ] **Step 4: Run the tests to verify they pass**

- [ ] **Step 5: Full gates** (fmt, clippy, test as in Task 2).

- [ ] **Step 6: Commit**

```bash
git add crates/wasi-runtime/src/native_session.rs crates/wasi-runtime/src/lib.rs crates/wasi-runtime/src/native.rs
git commit -m "feat(wasi-runtime): native session driver with cancellation, timeout, and output budget"
```

---

### Task 4: Broker mode-awareness + native submission

**Files:**
- Modify: `crates/wasi-runtime/src/broker.rs`
- Test: `crates/wasi-runtime/src/broker.rs` (test module)

**Interfaces (produced):**
- `ActionBroker::submit_native(&self, request: CommandRequest) -> Result<mpsc::Receiver<BrokerOutcome>, BrokerError>` (capturing)
- `ActionBroker::submit_native_streaming(&self, request: CommandRequest) -> Result<mpsc::Receiver<SessionEvent>, BrokerError>`
- `ActionBroker::send_input(&self, id: u64, bytes: Vec<u8>) -> Result<(), BrokerError>` — `BrokerError::NotNative(u64)` when the session is not a native session; `BrokerError::UnknownSession(u64)` when not live.
- `BrokerError::Native(NativeError)` variant.
- `enum JobKind { Wasi(Component), Native(NativeRequest) }` (private) — replaces `Job.component`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn submit_native_requires_approval_then_streams() {
    // request: mode Native, grant.allow_native_execution(), program "echo", args ["hello"],
    // cwd = grant root (exists). submit_native_streaming ->
    //   PendingApproval { reason: NativeExecution } -> approve(1) ->
    //   Started -> Output contains "hello" -> Exited { code: Some(0) }
}

#[test]
fn send_input_reaches_a_running_native_session() {
    // submit native `cat` (or `sh -c` reading stdin), approve,
    // Started, send_input(b"hello\r\n"), expect Output contains "hello".
}

#[test]
fn send_input_to_wasi_session_errors_not_native() {
    // existing WASI streaming session -> send_input -> Err(NotNative(id))
}

#[test]
fn deny_native_never_spawns() {
    // submit_native_streaming, deny(1) -> SessionEvent::Denied; assert no child ran
    // (program writes a sentinel file; file must not exist).
}

#[test]
fn native_session_cancel_reports_cancelled_and_releases() {
    // submit native `sleep 100`, approve, Started, cancel(1) -> Cancelled event;
    // outstanding_sessions() drops to 0.
}

#[test]
fn non_native_request_to_submit_native_errors() { /* mode Wasi -> Err */ }
```

- [ ] **Step 2: Run the tests to verify they fail** (RED: methods missing).

- [ ] **Step 3: Implement**

1. `enum JobKind { Wasi(Component), Native(NativeRequest) }` where `struct NativeRequest { request: CommandRequest }`.
2. `Job { request, kind: JobKind, sink, session, admission }` — update the two existing submit paths to build `JobKind::Wasi`.
3. Add a **second worker** `native_worker_loop` (same panic-barrier pattern as `process_job_guarded`) consuming a separate `native_queue_rx`, sharing the same `BrokerState`; `ActionBroker::drop` joins both workers.
4. `submit_native*`: `request.validate()`, `mode == Native` else `BrokerError::NotWasi`-equivalent (`NativeNotGranted` handled by backend), build `JobKind::Native`, `enqueue` onto the native queue with the same approval/audit flow.
5. `send_input(id, bytes)`: look up `state.inputs` (`Mutex<HashMap<u64, NativeInputTx>>`) — populated when a native job STARTS (in the native worker) and removed when it finishes; `Err(UnknownSession)` if absent, `Err(NotNative(id))` if the session exists but is not native.
6. `execute_native(job, cancel)`: emit `Started`, build `NativeSessionHandle` from `NativeBackend::new().spawn(&request)`, register `NativeInputTx`, `run()`, emit `Exited { code }` or `Cancelled`/`Unsupported`/`Failed`, unregister input.
7. Audit: reuse `AuditOutcome::Completed { exit_code }` / `Cancelled` / `Failed` unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

- [ ] **Step 5: Full gates** (fmt, clippy, `cargo test --workspace --all-features`).

- [ ] **Step 6: Update the module doc of `broker.rs`** to describe the two queues and native input path.

- [ ] **Step 7: Commit**

```bash
git add crates/wasi-runtime/src/broker.rs
git commit -m "feat(wasi-runtime): broker submits native PTY sessions with approval, input, and audit"
```

---

### Task 5: CLI verification surface (`run-native`)

**Files:**
- Modify: `crates/ferrous/src/shell.rs`
- Modify: `crates/ferrous/src/cli.rs` (subcommand `shell` stays; add shell command `run-native`)
- Test: `crates/ferrous/src/shell.rs` (unit tests for parsing; integration via `cargo run -p ferrous -- shell` is manual)

**AC (from the spec T1.2):** "start an explicitly approved native session; `exit`/signals/cancel work; killed processes free all resources; unsupported native policy never falls back ambiently."

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parses_run_native_with_direct_args() {
    // "run-native --allow -- cargo test" -> WasiCommand-like struct { program: "cargo", args: ["test"] }
}

#[test]
fn run_native_without_allow_flag_is_rejected() {
    // "run-native cargo test" -> Outcome::Reply contains "requires explicit approval"
}

#[test]
fn run_native_is_never_a_shell_string() {
    // "run-native --allow -- sh -lc 'echo hi > /tmp/pwned'" -> args are ["-lc", "echo hi > /tmp/pwned"]
    // i.e. the metacharacters are passed as arguments, not interpreted by a host shell.
}
```

- [ ] **Step 2: Run the tests to verify they fail** (RED).

- [ ] **Step 3: Implement**

1. `parse_native_command(line) -> Option<NativeCommand>` with `--allow` flag (maps to the broker's approval) and `--` separator.
2. In `run()`: build `CapabilityGrant::workspace(cwd, ReadWrite).allow_native_execution().with_limits(...)`; submit via `broker.submit_native_streaming`; drive the event receiver: print `Output` bytes to stdout, print `PendingApproval`/`Exited`/`Cancelled`/`Denied`/`Unsupported` as lines; on `Cancelled`, exit the loop.
3. Keep the help text honest: `run-native --allow -- <program> [args...]`.
4. Wire a CLI-level cancel: on `Ctrl-C` (or a `cancel` shell command), call `broker.cancel(session_id)`.

- [ ] **Step 4: Run the tests to verify they pass**

- [ ] **Step 5: Manual smoke test (record output in the PR description)**

```bash
cargo run -p ferrous -- shell
> run-native --allow -- echo "hello from native"
# expect: hello from native, then Exited code 0
> run-native --allow -- bash -c "sleep 100"
# Ctrl-C / cancel -> Cancelled, no lingering sleep processes (pgrep sleep)
> run-native echo hi
# expect: reply "requires explicit approval"
```

- [ ] **Step 6: Full gates** (fmt, clippy, `cargo test --workspace --all-features`).

- [ ] **Step 7: Commit**

```bash
git add crates/ferrous/src/shell.rs crates/ferrous/src/cli.rs
git commit -m "feat(ferrous): CLI run-native verification surface for approved native sessions"
```

---

### Task 6: Cross-platform + red-team hardening pass

**Files:**
- Modify: `crates/wasi-runtime/src/native.rs`, `native_session.rs`, `broker.rs` as needed
- Test: extend test modules

**Motivation:** Phase 1 risk register items R11 (no ambient fallback), R23 (process semantics), R34 (argument injection), R35 (process-tree cleanup), R8 (secrets).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(unix)]
#[test]
fn process_tree_is_cleaned_up_on_cancel() {
    // spawn `sh -c "sleep 300 & wait"`; cancel after Started;
    // after run() returns, `kill(pid_of_grandchild, 0)` must fail (ESRCH).
}

#[test]
fn secret_environment_names_never_reach_the_child() {
    // grant without allow_environment; set a SECRET_* host env var;
    // spawn `env`; assert stdout does NOT contain the secret name.
}

#[test]
fn allowlisted_environment_reaches_the_child() {
    // grant.allow_environment("ALLOWED"); spawn `env`; assert stdout contains ALLOWED=<value>.
}

#[test]
fn output_budget_is_combined_across_streams() {
    // program writes > budget to stdout AND stderr (PTY merges; still verify total is capped).
}

#[test]
fn metacharacter_args_are_never_executed() {
    // program "echo", args ["$(touch /tmp/pwned)", ";", "&&", "|"] — file must not exist.
}

#[test]
fn empty_grant_native_is_denied() {
    // mode Native with CapabilityGrant::empty() -> validate() -> NativeNotGranted.
}
```

- [ ] **Step 2: Run the tests to verify they fail** (RED).

- [ ] **Step 3: Implement fixes until green** (likely small: env plumbing, budget bookkeeping, tree-kill ordering).

- [ ] **Step 4: Full gates** including docs:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
cargo bench --workspace --no-run
```

- [ ] **Step 5: Update the risk register**

Add a line to `docs/plans/risk-register-t11-wasi-runtime.md` (in the root `docs/plans/` — copy the file into this repo if absent): mark R11/R23/R34/R35 mitigations as "implemented + tested" with the test names, or promote any failing AC to a blocking risk.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "test(wasi-runtime): red-team hardening suite for native PTY backend"
```

---

## Self-Review

**Spec coverage:**
- T1.2 AC "start an explicitly approved native session" → Tasks 4–5.
- T1.2 AC "exit/signals/cancel work; killed processes free all resources" → Task 3 (cancel/timeout/tree cleanup) + Task 6 tests.
- T1.2 AC "unsupported native policy never falls back ambiently" → Task 2 (`UnsupportedOnHost`, `supported_on_host()`) + Task 4.
- ADR-0003 decision 7 (direct argv, actor identity, hard failure) → Task 2 + Task 5.
- Risk R34 (no `sh -c`) → every spawn uses `CommandBuilder` direct argv; tests assert metacharacters are inert.
- Risk R8 (secrets) → `selected_environment` allowlist; Task 6 tests.

**Placeholder scan:** Task 3 intentionally leaves the internal threading shape to the executing agent but pins every observable behavior with tests; no TBDs in behavior. The `drain_reader` helper keeps the reader loop testable.

**Type consistency:** `NativeError::Cancelled`/`Timeout` variants must be added in Task 2 (Step 3 note) so Task 3 can construct them — flagged as a cross-task dependency. `BrokerError::Native` and `JobKind` are introduced in Task 4 and used by Tasks 5–6.
