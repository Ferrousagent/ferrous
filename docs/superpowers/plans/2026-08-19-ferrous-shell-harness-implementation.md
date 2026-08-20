# Ferrous Shell Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a persistent Ferrous terminal harness that gives humans Bash-like development power and gives AI agents structured, fast, capability-enforced execution without exposing the human password or ambient host authority.

**Architecture:** Human shell text and AI tool calls compile into one typed Ferrous Shell IR. A persistent `TerminalSession` owns cwd, environment overlays, job state, event streams, limits, checkpoints, and scoped capability leases; the action broker preflights the IR, obtains human-only elevation when required, and dispatches safe builtins/WASI components or direct-argv native processes through a tested OS adapter. GNU Bash is not bundled; exact Bash/PowerShell/cmd compatibility is an explicit elevated native escape hatch.

**Tech Stack:** Rust 2024, Wasmtime 47/WASI Preview 2, `portable-pty` 0.9 (MIT), `argon2` and `password-hash` (RustCrypto MIT/Apache-2.0), `rpassword` 7 (Apache-2.0) for the trusted CLI prompt, SHA-256 for action digests, existing `thiserror`, `anyhow`, `tracing`, and bounded channels. No copyleft dependency is permitted.

## Global Constraints

- Preserve `#![forbid(unsafe_code)]` in every Ferrous crate.
- WASI remains the default AI execution backend; native execution is a separate policy adapter.
- Never concatenate AI text into `sh -c`, `cmd /c`, or PowerShell; native boundaries receive direct argv.
- The raw human password may exist only inside the trusted profile-vault verification path; it must never enter `CommandRequest`, the agent context, an event, a child environment, a log, or an audit record.
- The AI can request an elevation but cannot read, submit, derive, replay, or widen the password/capability lease.
- An elevation lease is short-lived, action-digest-bound, capability-scoped, revocable, and fail-closed on mismatch.
- All filesystem paths are capability-relative and symlink-aware; lexical prefix checks alone are insufficient.
- Untrusted command output, repository files, package metadata, web content, and tool results cannot create authority or change policy.
- Package installs are staged, verified, scanned, sandboxed, and rollback-capable; scanners are not the runtime security boundary.
- Unsupported OS sandbox features return a typed denial; they never fall back to ambient execution.
- Safe builtins and policy checks stay off the model/network hot path and have dedicated benchmarks.
- UI is not required for backend verification; the CLI and event protocol remain the proof surface.
- Every task ends with focused tests before the next task starts.

---

### Task 1: Lock the security contract and public terminal protocol

**Files:**
- Create: `docs/adr/0004-terminal-harness-and-human-elevation.md`
- Create: `crates/wasi-runtime/src/shell_ir.rs`
- Create: `crates/wasi-runtime/src/elevation.rs`
- Modify: `crates/wasi-runtime/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `crates/wasi-runtime/Cargo.toml`

**Interfaces:**

- `ShellProgram { statements: Vec<Statement> }`
- `Statement::{Command(CommandSpec), Pipeline(Vec<CommandSpec>), And(Box<Statement>, Box<Statement>), Or(Box<Statement>, Box<Statement>), Sequence(Vec<Statement>), Background(Box<Statement>) }`
- `SessionPath(String)` (validated capability-relative path)
- `CommandSpec { program: Program, args: Vec<String>, redirects: Vec<Redirect>, cwd: SessionPath }`
- `NativeShellKind::{Bash, PowerShell, Cmd}`
- `Program::{Builtin(Builtin), External(String), WasiComponent(String), NativeShell(NativeShellKind) }`
- `Builtin::{Pwd, Cd(SessionPath), Ls(SessionPath), Cat(SessionPath), Mkdir(SessionPath), Remove(SessionPath), Copy { from: SessionPath, to: SessionPath }, Move { from: SessionPath, to: SessionPath }, Env, Which(String) }`
- `Redirect::{Input(SessionPath), OutputTruncate(SessionPath), OutputAppend(SessionPath), ErrorTruncate(SessionPath), ErrorAppend(SessionPath)}`
- `CommandDigest([u8; 32])` with `CommandDigest::of(&ShellProgram) -> CommandDigest`
- `EffectSummary { reads: Vec<SessionPath>, writes: Vec<SessionPath>, deletes: Vec<SessionPath>, network: Vec<String>, secrets: Vec<String>, scripts: Vec<String> }`
- `NetworkCapability { host: String, ports: Vec<u16>, connect: bool, bind: bool }`
- `CapabilityDelta { filesystem: Vec<FilesystemGrant>, environment: Vec<String>, network: Vec<NetworkCapability>, native: bool, secrets: Vec<String> }`
- `ElevationRequest { session_id: u64, digest: CommandDigest, summary: EffectSummary, requested: CapabilityDelta, expires_after: Duration }`
- `ApprovalLease { lease_id: u128, session_id: u64, digest: CommandDigest, grant: CapabilityGrant, expires_at: Instant }` (stored and validated only by the trusted broker)
- `HumanApprovalAuthority` trait with `verify_human(&self, request: &ElevationRequest) -> Result<(), ElevationError>`; no password, proof, or lease parameter in this trait. The broker mints the lease only after this trusted callback succeeds.
- `ActionBroker::submit_program(&self, program: ShellProgram, session: TerminalSessionSpec) -> Result<Receiver<SessionEvent>, BrokerError>`
- `ActionBroker::write_input`, `resize`, `signal`, `cancel`, `close`

**Security decisions to encode in the ADR:**

- The model/tool API only exposes redacted `ElevationRequest` summaries and `PendingApproval`; it has no method that accepts a password.
- The broker mints a lease only after a trusted `HumanApprovalAuthority` verifies the parked request; the old unconditional `approve(id)` path is removed from the production API and retained only as a private test helper.
- `NativeShell` is a separate high-risk node and cannot be generated by ordinary external-command parsing without an explicit policy decision.
- `CommandDigest` canonicalizes the AST, cwd, argv, redirect targets, network scope, secret names, and limits; it never includes secret values.

- [ ] **Step 1: Write failing contract tests**

```rust
#[test]
fn command_digest_changes_when_argv_or_effect_scope_changes() {}

#[test]
fn elevation_request_contains_effects_but_never_a_password_field() {}

#[test]
fn agent_facing_approval_api_cannot_construct_or_submit_a_lease() {}

#[test]
fn native_shell_is_a_distinct_high_risk_program_kind() {}
```

- [ ] **Step 2: Add `sha2` as a workspace dependency only after `cargo deny` confirms its MIT/Apache-2.0 license; add the RustCrypto Argon2/password-hash dependencies to `profiles-vault` in Task 5 and `rpassword` to `ferrous` in Task 6 after the same check.**

- [ ] **Step 3: Implement the IR, canonical digest, effect-summary types, and private lease fields; ensure `ApprovalLease`, `ElevationRequest`, and all secret-bearing types do not implement `Serialize`, `Debug` with values, or `Clone` for password material.**

- [ ] **Step 4: Run focused tests:** `cargo test -p wasi-runtime shell_ir::tests elevation::tests`.

- [ ] **Step 5: Run `cargo fmt --all -- --check` and `cargo clippy -p wasi-runtime --all-targets -- -D warnings`.**

- [ ] **Step 6: Commit:** `git add docs/adr/0004-terminal-harness-and-human-elevation.md Cargo.toml crates/wasi-runtime && git commit -m "feat: define terminal IR and human-only elevation contract"`.

---

### Task 2: Implement the safe Bash-like parser

**Files:**
- Create: `crates/wasi-runtime/src/shell_parse.rs`
- Modify: `crates/wasi-runtime/src/lib.rs`
- Test: inline in `crates/wasi-runtime/src/shell_parse.rs`

**Interfaces:**

- `ShellParser::parse(input: &str) -> Result<ShellProgram, ParseError>`
- `Token::{Word(String), Operator(Operator), Newline, End}`
- `Operator::{Pipe, And, Or, Semi, RedirectIn, RedirectOut, RedirectAppend, Background, OpenParen, CloseParen}`
- `ParseError::{UnterminatedQuote, InvalidOperator, EmptyCommand, UnsupportedConstruct(&'static str), TrailingEscape, NulByte}`
- `ShellParser::parse_ai_argv(program: &str, args: &[String], cwd: SessionPath) -> ShellProgram`

**Supported grammar:** quoting, escaping, simple commands, `|`, `&&`, `||`, `;`, output/input redirection, background `&`, and the builtins in Task 1. The parser must reject command substitution, arithmetic expansion, here-documents, aliases, startup files, function definitions, `eval`, and unbounded shell escapes with explicit error messages. It must preserve each argv element separately and never perform host-shell expansion.

- [ ] **Step 1: Write failing parser tests**

```rust
#[test]
fn parses_cd_pipeline_and_npm_as_structured_commands() {}

#[test]
fn preserves_quoted_metacharacters_as_one_argument() {}

#[test]
fn parses_and_or_sequence_and_redirects() {}

#[test]
fn rejects_command_substitution_and_unbounded_eval() {}

#[test]
fn rejects_unterminated_quotes_and_trailing_escapes() {}

#[test]
fn parser_never_emits_a_shell_command_string() {}
```

- [ ] **Step 2: Implement a bounded tokenizer with explicit quote state and maximum input/token/argv sizes from `ResourceLimits`; reject NUL, overlong input, unclosed quotes, and unsupported syntax before constructing an execution plan.**

- [ ] **Step 3: Implement a recursive-descent grammar that produces only the Task 1 IR; do not call a shell, process substitution, or filesystem expansion during parsing.**

- [ ] **Step 4: Add deterministic parser error spans and redacted display strings for approval summaries.**

- [ ] **Step 5: Run `cargo test -p wasi-runtime shell_parse::tests` and the parser fuzz corpus with `cargo test -p wasi-runtime --all-features`.**

- [ ] **Step 6: Commit:** `git add crates/wasi-runtime/src/shell_parse.rs crates/wasi-runtime/src/lib.rs && git commit -m "feat: parse bounded Bash-like Ferrous shell syntax"`.

---

### Task 3: Add persistent session state and safe builtins

**Files:**
- Create: `crates/wasi-runtime/src/terminal_session.rs`
- Create: `crates/wasi-runtime/src/builtin.rs`
- Modify: `crates/wasi-runtime/src/lib.rs`
- Modify: `crates/wasi-runtime/src/command.rs`
- Modify: `crates/wasi-runtime/src/capability.rs`

**Interfaces:**

- `TerminalSessionSpec { id: u64, actor: Actor, cwd: SessionPath, base_grant: CapabilityGrant, limits: ResourceLimits }`
- `EnvDelta { set: BTreeMap<String, String>, remove: Vec<String> }`
- `JobTable` owns bounded live jobs and their cancellation handles; `EventSink` accepts bounded `SessionEvent` values.
- `SessionError` covers invalid paths, denied operations, closed sessions, resource limits, and backend failures.
- `TerminalSession { spec: TerminalSessionSpec, cwd: PathBuf, env: BTreeMap<String, String>, jobs: JobTable, lease: Option<ApprovalLease> }`
- `TerminalSession::new(spec) -> Result<Self, SessionError>`
- `TerminalSession::cwd(&self) -> &Path`
- `TerminalSession::change_dir(&mut self, path: &SessionPath) -> Result<(), SessionError>`
- `TerminalSession::apply_env(&mut self, delta: EnvDelta) -> Result<(), SessionError>`
- `BuiltinExecutor::execute(&self, builtin: &Builtin, session: &mut TerminalSession, sink: &mut dyn EventSink) -> Result<ExitStatus, SessionError>`
- `SessionEvent::{Started, Output, ApprovalRequired, JobStarted, JobExited, Cancelled, Failed, Closed}` while preserving compatibility adapters for the current `SessionEvent` consumers

Builtins must receive normalized capability-relative paths, not raw host paths. `cd` changes only session state. `mkdir`, remove, copy, and move enforce operation-specific read/write grants and symlink-aware resolution. `env` exposes only the session's approved overlay, never the host environment or secret values. Directory listings and command output are bounded and terminal-escape-safe at the event/rendering boundary.

- [ ] **Step 1: Write failing session tests**

```rust
#[test]
fn cd_persists_for_the_next_command_without_touching_the_host_cwd() {}

#[test]
fn builtin_mkdir_stays_inside_the_workspace_grant() {}

#[test]
fn builtin_remove_requires_the_delete_capability() {}

#[test]
fn symlinked_builtin_path_cannot_escape_the_grant() {}

#[test]
fn env_never_returns_unallowlisted_host_variables_or_secret_values() {}

#[test]
fn session_close_cancels_all_owned_jobs_and_releases_leases() {}
```

- [ ] **Step 2: Implement the session state machine and bounded job table with explicit maximum jobs, output, and environment sizes.**

- [ ] **Step 3: Implement builtins using capability-relative operations; do not invoke `/bin/sh`, `cmd.exe`, or PowerShell for any builtin.**

- [ ] **Step 4: Add lifecycle compatibility so existing `SessionState` tests remain valid while new persistent events can represent input, resize, signal, and close.**

- [ ] **Step 5: Run `cargo test -p wasi-runtime terminal_session::tests builtin::tests command::tests capability::tests`.**

- [ ] **Step 6: Benchmark `cd`, path authorization, directory listing, and environment projection in `crates/wasi-runtime/benches/terminal_hot_paths.rs`; record policy latency separately from process startup.**

- [ ] **Step 7: Commit:** `git add crates/wasi-runtime/src && git commit -m "feat: add persistent sessions and capability-safe builtins"`.

---

### Task 4: Execute IR plans with bounded pipelines and direct argv

**Files:**
- Create: `crates/wasi-runtime/src/shell_executor.rs`
- Modify: `crates/wasi-runtime/src/native.rs`
- Modify: `crates/wasi-runtime/src/native_session.rs`
- Modify: `crates/wasi-runtime/src/command.rs`

**Interfaces:**

- `ShellExecutor::execute(&self, program: ShellProgram, session: &mut TerminalSession, authority: &dyn ApprovalAuthorityView, sink: EventSink) -> Result<PlanResult, ExecuteError>`
- `StdinSpec::{Closed, SessionInput, Pipe(JobId)}`
- `OutputSpec::{SessionEvents, Pipe(JobId), File(SessionPath, AppendMode)}`
- `ProcessSpec { program: String, args: Vec<String>, cwd: PathBuf, env: Vec<(String, String)>, stdin: StdinSpec, stdout: OutputSpec, stderr: OutputSpec }`
- `ProcessSupervisor::spawn(&self, spec: ProcessSpec, grant: &CapabilityGrant) -> Result<JobHandle, NativeError>`
- `JobHandle::{write_input, resize, signal, cancel, wait}`
- `PipelinePolicy { max_stages: usize, max_buffer_bytes: usize, allow_binary: bool }`
- `ApprovalAuthorityView` exposes only `pending/requested` lease metadata to the executor; it cannot authorize or create a lease.
- `PlanStatus::{Running, Exited, Failed, Cancelled, Denied}`
- `EffectRecord { kind: String, target: String, approved: bool }`
- `ExecuteError` covers invalid IR, denied capability, unsupported backend, resource limits, and child/session failures.
- `PlanResult { status: PlanStatus, exit_code: Option<i32>, effects: Vec<EffectRecord>, audit_id: u128 }`

Execution rules:

- Direct external programs become `ProcessSpec` argv values; no intermediate shell.
- Pipelines use bounded channels and backpressure; a blocked downstream process can be cancelled without deadlocking the broker.
- `&&` and `||` short-circuit based on exact exit status.
- Redirections use capability-checked files and bounded output sinks.
- Background jobs remain owned by the session and are killed on close/cancel.
- A `NativeShell` node is never silently synthesized by the parser and always receives the highest risk class.
- Native PTY sessions remain interactive and persistent; the current run-to-completion driver is refactored so `write_input`, `resize`, `signal`, and `cancel` operate before exit.

- [ ] **Step 1: Write failing execution tests**

```rust
#[test]
fn direct_argv_keeps_shell_metacharacters_literal() {}

#[test]
fn pipeline_backpressure_bounds_memory_and_terminates_cleanly() {}

#[test]
fn and_or_sequences_use_exit_status_without_shell_fallback() {}

#[test]
fn redirection_cannot_write_outside_the_grant() {}

#[test]
fn closing_a_session_kills_all_background_processes() {}

#[test]
fn native_input_resize_signal_and_cancel_round_trip() {}
```

- [ ] **Step 2: Refactor `NativeSession` into a persistent controller while preserving the existing output-limit, timeout, process-tree, and environment tests.**

- [ ] **Step 3: Implement the IR executor and bounded pipeline channels; keep all child setup in `ProcessSpec`/`CommandBuilder` direct argv.**

- [ ] **Step 4: Add output sanitization at the event-to-renderer adapter, keeping raw bytes available only to trusted structured consumers and preventing terminal control injection.**

- [ ] **Step 5: Run `cargo test -p wasi-runtime shell_executor::tests native::tests native_session::tests`.**

- [ ] **Step 6: Run the existing adversarial broker suite and add a 100-iteration pipeline/cancel race hammer.**

- [ ] **Step 7: Commit:** `git add crates/wasi-runtime/src && git commit -m "feat: execute persistent shell plans with bounded direct-argv jobs"`.

---

### Task 5: Implement human-only password elevation and scoped leases

**Files:**
- Modify: `crates/profiles-vault/Cargo.toml`
- Modify: `crates/profiles-vault/src/lib.rs`
- Modify: `crates/wasi-runtime/src/elevation.rs`
- Modify: `crates/wasi-runtime/src/broker.rs`
- Modify: `Cargo.toml`

**Interfaces:**

- `profiles_vault::PasswordHashRecord { profile_id: ProfileId, encoded_argon2: String, failed_attempts: u32, locked_until: Option<SystemTime> }`
- `profiles_vault::Vault::verify_master_password(&self, profile: ProfileId, password: SecretString) -> Result<VerifiedProfile, VaultError>`
- `profiles_vault::Vault::verify_step_up(&self, profile: ProfileId, password: SecretString, request_digest: [u8; 32]) -> Result<(), VaultError>`; the password is consumed inside the vault and no proof value leaves the trusted callback.
- `HumanApprovalAuthority::verify_human(&self, request: &ElevationRequest) -> Result<(), ElevationError>`; the authority may prompt and verify, but never returns a lease to the caller.
- `ActionBroker::approve_with_authority(&self, id: u64, authority: &dyn HumanApprovalAuthority) -> Result<ApprovalLease, BrokerError>`; only the broker creates the lease after verifying the parked request and successful human authorization.
- `ActionBroker::revoke_lease(&self, lease_id: u128) -> Result<(), BrokerError>`
- `ActionBroker::validate_lease(&self, lease: &ApprovalLease, request: &ShellProgram) -> Result<(), ElevationError>`

Use RustCrypto `argon2`/`password-hash` for verification and `secrecy::SecretString` for the in-memory password boundary. Use `rpassword` only in the trusted `ferrous` CLI prompt; Tauri will provide a native password field later. Failed attempts are rate-limited and recorded without the password. A successful password check returns only `Result<(), VaultError>` to the trusted authority callback; the agent receives only `PendingApproval` plus a redacted effect summary.

The broker must reject:

- lease/session mismatch;
- action-digest mismatch;
- expired or revoked lease;
- broader filesystem/network/environment scope than requested;
- a lease with secret access not explicitly shown and approved;
- any attempt to pass password text through command args, env, event bytes, audit, or model context.

- [ ] **Step 1: Write failing vault/elevation tests**

```rust
#[test]
fn wrong_password_is_rate_limited_without_timing_or_message_oracle() {}

#[test]
fn password_value_never_appears_in_debug_serialize_or_audit_output() {}

#[test]
fn agent_can_request_elevation_but_cannot_construct_a_proof_or_lease() {}

#[test]
fn lease_is_rejected_after_expiry_revocation_or_action_digest_change() {}

#[test]
fn lease_cannot_widen_filesystem_network_environment_or_secret_scope() {}

#[test]
fn concurrent_approve_deny_cancel_yields_one_terminal_decision() {}
```

- [ ] **Step 2: Add and license-check `argon2`, `password-hash`, `rpassword`, and the existing `secrecy` dependency configuration; keep secret serialization disabled and do not modify `deny.toml`'s allowlist.**

- [ ] **Step 3: Implement password hashing/verification, exponential/backoff rate limiting, profile lockout, and zero-observable secret error messages.**

- [ ] **Step 4: Implement the broker lease validator and replace the CLI's current automatic approval behavior with a trusted authority callback.**

- [ ] **Step 5: Add a fake authority for unit tests that never accepts a password from an agent API; integration tests must prove an agent request cannot reach the password prompt input.**

- [ ] **Step 6: Run `cargo test -p profiles-vault -p wasi-runtime elevation::tests broker::tests` and `cargo deny check licenses advisories bans sources`.**

- [ ] **Step 7: Commit:** `git add Cargo.toml crates/profiles-vault crates/wasi-runtime && git commit -m "feat: add human-only scoped terminal elevation"`.

---

### Task 6: Build the trusted human CLI over the session protocol

**Files:**
- Modify: `crates/ferrous/Cargo.toml`
- Modify: `crates/ferrous/src/shell.rs`
- Create: `crates/ferrous/src/approval.rs`
- Modify: `crates/ferrous/src/main.rs` only if command wiring requires it
- Test: `crates/ferrous/tests/cli.rs` and inline shell/approval tests

**Interfaces:**

- CLI commands: `session open`, `session exec <command>`, `session write <bytes>`, `session resize <rows> <cols>`, `session signal <name>`, `session cancel`, `session close`
- Human shell aliases: `cd`, `pwd`, builtins, and direct external commands are parsed into the same IR.
- `approval::CliApprovalAuthority::authorize(&self, request: &ElevationRequest) -> Result<ApprovalLease, ElevationError>`
- `ferrous shell --workspace <path>` starts one persistent session and renders sanitized events.
- `ferrous shell --json` emits structured event/result records for AI harness tests and future Tauri IPC.

The CLI must display the normalized command/effects before password input. It must never echo the password, write it to tracing, include it in a child environment, or include it in JSON output. Closing the terminal cancels and joins all owned jobs.

- [ ] **Step 1: Write failing CLI tests**

```rust
#[test]
fn shell_cd_changes_the_next_command_cwd() {}

#[test]
fn shell_runs_npm_or_cargo_by_direct_argv_when_explicitly_allowed() {}

#[test]
fn shell_never_falls_through_unknown_input_to_a_host_shell() {}

#[test]
fn risky_command_shows_effect_summary_before_password_prompt() {}

#[test]
fn json_mode_contains_events_and_audit_ids_but_no_password_material() {}
```

- [ ] **Step 2: Replace the current one-shot `run-wasi`/`run-native` dispatch with a persistent session while retaining compatibility aliases that map to the new protocol.**

- [ ] **Step 3: Implement CLI password prompting only in `CliApprovalAuthority`; keep the authority module separate from parser/executor/agent APIs.**

- [ ] **Step 4: Add broken-pipe, terminal-resize, stdin, cancellation, and EOF handling tests.**

- [ ] **Step 5: Run `cargo test -p ferrous --all-targets` and manual CLI smoke tests in a temporary workspace.**

- [ ] **Step 6: Commit:** `git add crates/ferrous && git commit -m "feat: expose persistent terminal sessions to humans"`.

---

### Task 7: Add the AI terminal tool adapter without secret or approval access

**Files:**
- Modify: `crates/agent-loop/src/lib.rs`
- Create: `crates/agent-loop/src/terminal_tool.rs`
- Modify: `crates/wasi-runtime/src/lib.rs`
- Modify: `crates/shared/src/error.rs` only for a shared redacted tool error if needed
- Test: inline in `crates/agent-loop/src/terminal_tool.rs`

**Interfaces:**

- `TerminalTool::open(spec: AgentSessionSpec) -> Result<AgentSessionHandle, ToolError>`
- `TerminalTool::exec(session: AgentSessionHandle, program: ShellProgram) -> Result<ToolResult, ToolError>`
- `TerminalTool::write`, `resize`, `signal`, `read_events`, `cancel`, `close`
- `AgentSessionSpec` contains actor, workspace, initial safe grant, and resource limits; it has no password, vault handle, or approval callback.
- `ToolResult { status, exit_code, stdout, stderr, cwd, effects, audit_id }`

The agent adapter can request an elevation and wait for `PendingApproval`, but it cannot call `approve`, access `HumanApprovalProof`, prompt for a password, or mint/revoke leases. Approval enters through the trusted CLI/Tauri authority only. Tool output is marked untrusted and cannot be fed back into policy construction without a new broker preflight.

- [ ] **Step 1: Write failing isolation tests**

```rust
#[test]
fn agent_terminal_api_has_no_password_or_approval_constructor() {}

#[test]
fn agent_can_receive_pending_approval_without_receiving_secret_material() {}

#[test]
fn tool_output_cannot_mutate_session_grants_or_lease_scope() {}

#[test]
fn agent_session_close_cancels_all_jobs_and_releases_resources() {}
```

- [ ] **Step 2: Implement the adapter over `ActionBroker::submit_program` and the persistent session event stream.**

- [ ] **Step 3: Add structured effect summaries, truncation metadata, untrusted-output markers, and deterministic audit references.**

- [ ] **Step 4: Run `cargo test -p agent-loop -p wasi-runtime --all-features`.**

- [ ] **Step 5: Commit:** `git add crates/agent-loop crates/shared crates/wasi-runtime && git commit -m "feat: expose safe structured terminal tools to agents"`.

---

### Task 8: Add staged package-install policy and rollback hooks

**Files:**
- Create: `crates/wasi-runtime/src/package_policy.rs`
- Modify: `crates/wasi-runtime/src/lib.rs`
- Modify: `crates/wasi-runtime/src/shell_executor.rs`
- Modify: `crates/wasi-runtime/src/elevation.rs`
- Test: inline in `crates/wasi-runtime/src/package_policy.rs`

**Interfaces:**

- `PackageManager::{Npm, Cargo, Python, Unknown}`
- `CheckpointId(u128)`, `JobId(u64)`, `AppendMode::{Truncate, Append}`, and `StagedInstall { staging_root: SessionPath, plan_digest: CommandDigest }`
- `InstallResult { promoted: bool, checkpoint: CheckpointId, audit_id: u128 }` and `PackagePolicyError` for malformed plans, missing lockfiles, denied effects, failed scans, failed scripts, and rollback failures.
- `InstallRequest { manager, cwd, args, lockfile: Option<SessionPath>, registry_domains: Vec<String>, run_scripts: bool }`
- `InstallPlan { normalized_argv, packages, lockfile_status, domains, lifecycle_scripts, filesystem_effects, required_capabilities }`
- `PackagePolicy::inspect(request: InstallRequest) -> Result<InstallPlan, PackagePolicyError>`
- `PackagePolicy::stage(plan: InstallPlan, session: &TerminalSession) -> Result<StagedInstall, PackagePolicyError>`
- `PackagePolicy::promote(staged: StagedInstall, checkpoint: CheckpointId) -> Result<InstallResult, PackagePolicyError>`
- `PackagePolicy::rollback(staged: StagedInstall) -> Result<(), PackagePolicyError>`

The first implementation must support safe preflight for npm/cargo without pretending to prove package morality. Fetch-only operations default to scripts disabled. Lifecycle scripts require a separate scoped execution stage with no secrets, narrow filesystem, restricted network, resource limits, and a pre-mutation checkpoint. The package plan must show exact registry domains, lockfile/integrity status, package scripts, and writes before elevation.

- [ ] **Step 1: Write fixture-driven failing tests**

```rust
#[test]
fn npm_install_requires_network_write_and_script_capabilities() {}

#[test]
fn lockfile_and_integrity_data_are_included_in_the_effect_summary() {}

#[test]
fn lifecycle_scripts_never_receive_profile_secrets() {}

#[test]
fn package_install_without_lockfile_is_denied_by_strict_policy() {}

#[test]
fn failed_staged_install_rolls_back_without_partial_promotion() {}

#[test]
fn malicious_package_fixture_cannot_write_outside_the_staging_root() {}
```

- [ ] **Step 2: Implement package command normalization and manager-specific effect inspection without executing package code.**

- [ ] **Step 3: Add checkpoint hooks that run before promotion and retain a reversible staging directory until the result is audited.**

- [ ] **Step 4: Connect the plan to the elevation digest so changing a registry, package, script policy, or write root invalidates approval.**

- [ ] **Step 5: Run focused tests and the full workspace security suite.**

- [ ] **Step 6: Commit:** `git add crates/wasi-runtime/src && git commit -m "feat: stage and gate package installation effects"`.

---

### Task 9: Implement and verify native platform policy adapters

**Files:**
- Modify: `crates/wasi-runtime/src/native.rs`
- Modify: `crates/wasi-runtime/src/native_session.rs`
- Create: `crates/wasi-runtime/src/native_policy.rs`
- Modify: `crates/wasi-runtime/Cargo.toml` only for already-reviewed target-specific dependencies
- Modify: `.github/workflows/ci.yml`

**Interfaces:**

- `NativePolicyAdapter::probe() -> HostSupport`
- `NativePolicyAdapter::prepare(spec: &ProcessSpec, grant: &CapabilityGrant) -> Result<PreparedProcess, NativeError>`
- `PreparedProcess::spawn(self) -> Result<NativeSession, NativeError>`
- `HostSupport::{Supported, Unsupported { reason: String }}`
- Unix adapter: process group, no-new-privileges/available filesystem/network restrictions, resource/watchdog integration.
- Windows adapter: ConPTY, Job Object inheritance, kill-on-close, process-tree and output handling.
- macOS adapter: App Sandbox/available process policy integration; unsupported restrictions fail closed.

The adapter must distinguish “PTY works” from “requested security policy is enforced.” A
host may support interactive PTY but still reject a network/filesystem policy it cannot
prove. Every child process and descendant remains owned by the session supervisor.

- [ ] **Step 1: Write platform contract tests**

```rust
#[test]
fn unsupported_policy_returns_typed_denial_without_spawning() {}

#[cfg(unix)]
#[test]
fn unix_child_tree_is_killed_on_cancel_and_timeout() {}

#[cfg(windows)]
#[test]
fn windows_job_close_terminates_descendants() {}

#[test]
fn environment_allowlist_is_identical_across_platform_adapters() {}
```

- [ ] **Step 2: Implement Unix policy enforcement and preserve the existing process-group tests.**

- [ ] **Step 3: Implement Windows ConPTY/Job Object adapter and run it in the Windows CI matrix; do not mark it supported until process-tree, cancellation, environment, output, and policy tests pass.**

- [ ] **Step 4: Implement the macOS adapter or keep the unsupported branch explicit until equivalent enforcement tests exist.**

- [ ] **Step 5: Run Linux, macOS, and Windows CI jobs with common WASI tests and platform-specific native tests.**

- [ ] **Step 6: Commit:** `git add crates/wasi-runtime .github/workflows/ci.yml && git commit -m "feat: enforce native terminal policies per platform"`.

---

### Task 10: Adversarial verification, benchmarks, and documentation

**Files:**
- Create: `crates/wasi-runtime/benches/terminal_hot_paths.rs`
- Create: `crates/wasi-runtime/tests/terminal_adversarial.rs`
- Modify: `docs/plans/risk-register-t11-wasi-runtime.md`
- Modify: `README.md`
- Modify: `.github/workflows/ci.yml`

**Required adversarial coverage:**

- 20,000-step deterministic parser/IR/state-machine fuzz sequence.
- 100-iteration approve/deny/cancel/elevation race hammer.
- Concurrent duplicate session IDs and lease IDs.
- Symlink, rename, TOCTOU, relative-path, mount/bind, and workspace-boundary cases.
- ANSI/control-sequence output injection fixtures.
- Password sentinel tests proving no secret reaches args, env, logs, events, audit, or model tool results.
- Prompt-injected repository/file/web/package-output fixtures attempting to request elevation.
- Package lifecycle scripts attempting secret reads, outside writes, network exfiltration, and process escape.
- Native descendant cancellation, timeout, output-limit, closed-stdin, broken-reader, and teardown races.
- Replay tests proving audit records reconstruct the approved IR/effects without secret values.
- Queue flood, bounded memory, and backpressure tests.

**Benchmarks:**

- Parser/tokenization of representative commands.
- Action digest generation.
- Capability preflight and lease validation.
- Builtin `cd`/path checks/listing.
- Event dispatch and bounded pipe writes.
- Persistent session command latency.
- Native PTY setup and Wasmtime compile/startup separately; do not claim these are sub-millisecond.

- [ ] **Step 1: Write failing adversarial fixtures and benchmark harnesses.**

- [ ] **Step 2: Implement missing hardening found by those tests; each fix requires a regression test before production code changes are retained.**

- [ ] **Step 3: Run local gates:**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo bench -p wasi-runtime --bench terminal_hot_paths -- --noplot
cargo bench --workspace --no-run
cargo deny check licenses advisories bans sources
```

- [ ] **Step 4: Run the Linux/macOS/Windows CI matrix, inspect every native-policy job, and retain logs/artifacts for the risk register.**

- [ ] **Step 5: Update the README with the supported shell grammar, approval tiers, native compatibility limits, and the explicit statement that passing tests are evidence rather than proof of zero bugs.**

- [ ] **Step 6: Commit:** `git add crates/wasi-runtime docs README.md .github/workflows/ci.yml && git commit -m "test: adversarially verify the Ferrous terminal harness"`.

---

## Review gate before implementation

Before Task 1 is executed, the user must approve these explicit choices:

1. Bash-like Ferrous Shell subset plus explicit native shell escape, not bundled GNU Bash.
2. Passwords handled only by the trusted vault/CLI/Tauri authority; agents see no password API.
3. Short-lived, action-digest-bound capability leases.
4. WASI as the default agent backend; native execution only through tested OS adapters.
5. Strict package-install staging with scripts disabled by default and separate script elevation.
6. Mandatory checkpoints before package promotion and high-impact mutations.
7. Fail-closed unsupported platform policy.
8. No Tauri UI dependency for backend verification.

If any of these choices changes, update ADR-0004 and the affected task before touching code.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-19-ferrous-shell-harness-implementation.md`.

**Subagent-Driven (recommended):** dispatch one fresh worker per task, review the diff and focused tests after each task, then run the full security gate at Task 10.

**Inline Execution:** execute the tasks in this session with checkpoints after Tasks 1, 4, 5, and 9; do not begin the next batch until the current tests and policy review pass.
