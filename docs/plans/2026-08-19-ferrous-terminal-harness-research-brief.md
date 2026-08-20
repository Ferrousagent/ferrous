# Ferrous Terminal Harness Research Brief

**Date:** 2026-08-19  
**Status:** Research and design only — no implementation authorized by this brief  
**Decision gate:** This brief must be reviewed before production code changes

## 1. Purpose

Ferrous needs a terminal/computer harness that is powerful enough for normal human
and AI development work, safe against untrusted model output and downloaded code,
portable across Linux/macOS/Windows, and fast enough that safety does not make the
agent unusable.

This brief answers four questions:

1. How much shell/computer power should Ferrous expose?
2. Which safety properties must be enforced, rather than merely advised?
3. How should password elevation, capability grants, scanning, and sandboxing fit together?
4. What architecture and verification gates should precede implementation?

This is not a promise of perfect safety. A human can deliberately approve a harmful
operation, and unknown malware cannot be perfectly identified by a scanner. The goal
is to minimize authority, make risky effects explicit, enforce boundaries below the
model, and preserve recovery when something goes wrong.

## 2. Current Ferrous baseline

The existing repository already has a meaningful Phase 1 foundation:

- Wasmtime 47 with WASI Preview 2/component-model support.
- Deny-by-default filesystem, environment, loopback-network, and native-execution grants.
- Guest memory, fuel, timeout, output, and cancellation limits.
- An action broker with queueing, approval parking, cancellation, audit entries,
  duplicate-session protection, and panic containment.
- A Unix native PTY backend with direct argv, allowlisted environment, bounded output,
  cancellation, process-group cleanup, and fail-closed unsupported-host behavior.
- `portable-pty` 0.9, which is MIT-licensed and already present in the workspace.
- `#![forbid(unsafe_code)]` and a project-wide permissive-only dependency policy.
- A CLI proof surface, but not yet a persistent everyday shell: current input is limited
  to `run-wasi <component>` and explicitly approved `run-native --allow -- <program> [args]`.

Existing decisions remain binding:

- Everything is capability-scoped; no ambient authority.
- WASI is the default AI execution path.
- Native execution is a separate, approval-gated policy adapter.
- Secrets are encrypted at rest and injected only into explicitly granted sandboxes.
- Copyleft dependencies are rejected project-wide.
- Backend protocols are built and tested before the Tauri/wterm UI.

References: `docs/adr/0001-permissive-only-dependency-policy.md`,
`docs/adr/0002-security-and-sandbox-model.md`,
`docs/plans/ferrous-roadmap-spec.md`, and the Phase 1 risk register.

## 3. Research evidence

### 3.1 Manus-style computer environments

The official Manus documentation describes Manus as an autonomous agent operating in a
complete sandbox environment: a virtual computer with internet access, persistent
filesystem, software installation, and custom-tool creation. The product surface also
lists browser operation, cloud computer, projects, branches, scheduled work, and skills.

**Implication:** Ferrous must provide persistent session state, not isolated one-shot
commands. The minimum useful computer abstraction is a durable workspace with a cwd,
environment policy, files, processes, network policy, cancellation, and recoverable
results.

Source: [Manus documentation](https://manus.im/docs/introduction/welcome),
[Manus product page](https://manus.im/).

### 3.2 AI coding harnesses

Claude Code's official documentation describes a terminal/IDE agent that reads and
edits code, runs commands, works with Git, connects MCP servers, uses skills/hooks,
and coordinates agents/background work. Its security documentation separates read-only
commands from mutations, supports working-directory boundaries, sandboxed Bash,
allowlists, network approval, and explicit handling of suspicious commands.

Codex CLI is described as a local coding agent that runs in the terminal. Cursor's
current documentation similarly emphasizes understanding code, planning/building,
fixing, reviewing, and integrating skills, plugins, MCP, rules, and external tools.

**Implication:** Ferrous needs two interfaces over one execution kernel:

- A structured tool API for agents (`fs.read`, `fs.patch`, `terminal.exec`, etc.).
- A human-friendly shell language for interactive use.

The AI should not be forced to serialize every operation as an opaque shell string.
The terminal remains an escape hatch and an interactive surface, while typed tools are
more precise, cheaper to authorize, and easier to audit.

Sources: [Claude Code overview](https://code.claude.com/docs/en/overview),
[Claude Code security](https://code.claude.com/docs/en/security),
[Codex CLI](https://github.com/openai/codex),
[Cursor documentation](https://cursor.com/docs).

### 3.3 Shell semantics

POSIX defines the shell as a language pipeline: tokenize, parse, expand, redirect,
execute builtins/functions/programs, and collect exit status. Quoting, parameter
expansion, command substitution, arithmetic expansion, aliases, functions, and
here-documents are distinct semantics, not one string-splitting step.

Bash adds command editing, history, job control, aliases, functions, arrays, and other
features. GNU Bash is GPLv3, which conflicts with Ferrous's accepted permissive-only
dependency policy; it is not suitable for bundling without changing that policy.

Nushell demonstrates a cross-platform, MIT-licensed, Rust-based shell with structured
pipelines and first-class support for Windows, macOS, and Linux. It is useful design
research, but embedding the full project would need a separate size, API, and sandbox
review; it is not automatically a lightweight dependency.

**Implication:** Build a Ferrous Shell language with a deliberately specified portable
subset. Do not claim exact Bash-script compatibility. Offer a user-installed native
Bash escape hatch for users who need exact Bash behavior, with explicit elevation and
OS policy enforcement.

Sources: [POSIX Shell Command Language](https://pubs.opengroup.org/onlinepubs/9699919799/utilities/V3_chap02.html),
[GNU Bash](https://www.gnu.org/software/bash/),
[Nushell](https://github.com/nushell/nushell).

### 3.4 WASI and Wasmtime

WASI is capability-oriented and modular. WASI Preview 2 provides virtualizable APIs
through the component model; Preview 3 is currently a preview and changes stream/async
interfaces. Wasmtime states that WebAssembly instances must import all external
functionality, have memory isolation, and use explicit filesystem capabilities. It also
warns that terminal output containing ANSI/control sequences can have side effects when
shown to users, so terminal output needs filtering or safe rendering.

**Implication:** WASI is the correct default for untrusted AI-owned tools and Ferrous
builtins. It is not a complete Linux userspace and does not automatically make an
arbitrary native process safe. The shell protocol must treat terminal output as data
before rendering it in a human terminal.

Sources: [WASI](https://github.com/WebAssembly/WASI),
[Wasmtime security](https://docs.wasmtime.dev/security.html),
[Wasmtime StoreLimits](https://docs.wasmtime.dev/api/wasmtime/struct.StoreLimits.html).

### 3.5 Native process containment

Linux Landlock can let unprivileged processes restrict their own future filesystem and
network rights. It is stackable and inherited by descendants, but support is ABI/version
dependent and must be detected. Windows Job Objects manage process trees, resource
limits, accounting, and kill-on-close behavior; Windows ConPTY provides the bidirectional
pseudoconsole transport, with documented threading/deadlock requirements. Apple App
Sandbox is the platform mechanism for restricting access to system resources and user
data.

**Implication:** The broker's in-process capability check is necessary but insufficient
for native children. Each supported OS needs an enforcement adapter. If the adapter
cannot enforce a requested policy, native execution must fail closed rather than fall
back to an ambient process.

Sources: [Linux Landlock](https://docs.kernel.org/userspace-api/landlock.html),
[Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
[Windows ConPTY](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session),
[Apple App Sandbox](https://developer.apple.com/documentation/security/app_sandbox),
[portable-pty source/API](https://github.com/wezterm/wezterm/blob/main/pty/src/lib.rs).

### 3.6 Package installation and supply chain

The npm documentation confirms that package installation can execute lifecycle scripts,
including `preinstall`, `install`, `postinstall`, `prepare`, and dependency scripts. npm
runs scripts through a shell (`/bin/sh` on POSIX or `cmd.exe` on Windows), meaning a
seemingly ordinary install can execute arbitrary package-provided code. npm also
supports lockfiles, integrity data, registry signatures, provenance attestations, and
`npm audit`, but those features do not prove that package code is benign.

Trivy is a broad scanner for vulnerabilities, secrets, misconfigurations, filesystems,
repositories, and licenses. It is not a live command authorization or process sandbox.
SLSA provides supply-chain provenance concepts, not a replacement for runtime isolation.

**Implication:** package installation needs a staged workflow:

1. Resolve and display the dependency plan.
2. Require a lockfile or explicit user override.
3. Download to quarantine with a narrowly allowlisted registry/network policy.
4. Verify integrity/signatures/provenance when available.
5. Scan for known vulnerabilities, secrets, licenses, and policy violations.
6. Show lifecycle scripts and requested effects before elevation.
7. Run scripts in a disposable sandbox with no secrets and restricted network.
8. Promote changes atomically after checks, or roll back to the checkpoint.

`--ignore-scripts` should be the default for a safe package-fetch primitive; if scripts
are necessary, they are a separate explicit execution stage.

Sources: [npm scripts](https://docs.npmjs.com/cli/v11/using-npm/scripts/),
[npm audit/signatures](https://docs.npmjs.com/cli/v11/commands/npm-audit/),
[Trivy](https://github.com/aquasecurity/trivy),
[SLSA v1.0](https://slsa.dev/spec/v1.0/).

### 3.7 AI-specific threats

OWASP identifies prompt injection, insecure output handling, supply-chain risk, insecure
plugins, sensitive-information disclosure, and excessive agency as relevant agent risks.
Indirect prompt injection can arrive through repository files, command output, web pages,
package metadata, or test failures.

**Implication:** command output and fetched content are untrusted data, never new policy.
The model must not be able to approve its own elevation, turn output into a capability,
or widen an existing grant. Only the human/profile policy can approve elevation.

Sources: [OWASP Prompt Injection](https://owasp.org/www-community/attacks/PromptInjection),
[OWASP GenAI/LLM Top 10](https://owasp.org/www-project-top-10-for-large-language-model-applications/).

## 4. Recommended power envelope

### Tier A — safe and automatic

No password or approval for operations that stay inside the session's existing WASI
read-only scope:

- `pwd`, `ls`, `cat`, `head`, `tail`, `find`, repository indexing.
- `git status`, `git diff`, log inspection.
- Reading approved workspace files.
- Pure computation with memory/time/output limits.
- Read-only structured tools.

### Tier B — workspace mutation

Allowed automatically only when the profile/session policy says so; otherwise one
workspace-scoped approval:

- `cd`, `mkdir`, file patches, generated files.
- Local tests/builds with no network.
- `git add` and local checkpoint creation.
- Writing only under the workspace or an explicit build cache.

### Tier C — external effects

Requires a scoped elevation request:

- Package installation and dependency changes.
- Network access, with domains/ports/protocols shown.
- Native process execution when not already allowed by policy.
- Git fetch/push and external service actions.
- Environment variables or credentials.
- Opening a browser session or binding a preview port.

### Tier D — high-impact or incompatible authority

Denied by default and only available through a separate, explicit human-facing mode:

- System directories and machine-wide writes.
- Privilege escalation such as `sudo`.
- Unrestricted home-directory access.
- Unrestricted shell evaluation (`bash -c`, `cmd /c`, PowerShell scripts).
- Raw devices, process inspection/control outside the job, credential stores.
- Unrestricted network or secret export.

## 5. Password elevation design

Ferrous should have a step-up mechanism analogous to `sudo`, but it must mint a
capability lease rather than disable the security model.

```text
ElevationRequest {
    session_id,
    action_digest,
    program_and_argv,
    filesystem_roots_and_operations,
    network_domains_and_ports,
    environment_names,
    secret_names,
    resource_limits,
    expiry,
}
```

The human sees the parsed action and its effects, then authenticates through the local
profile/vault path. The password is never sent to the model or child process. Approval
returns a short-lived, non-transferable lease bound to:

- session identity;
- exact action digest or explicitly bounded command class;
- capability scope;
- resource limits;
- expiration and revocation state.

If the AI changes `npm install` into a compound command that includes an unapproved
network destination, the digest and requested scope change; the prior approval does not
apply.

Approval UX must show actual effects, not only the command string:

- normalized executable and argv;
- cwd;
- paths read/written/deleted;
- network destinations and redirects;
- packages, versions, scripts, and integrity status;
- secrets requested;
- estimated resource limits;
- rollback/checkpoint available.

## 6. Proposed architecture

### 6.1 One protocol, three clients

```text
Human CLI ───────┐
AI terminal API ──┼── TerminalSession protocol ── ActionBroker
Tauri/wterm ─────┘                              ├── WASI backend
                                                └── native OS backend
```

### 6.2 Ferrous Shell IR

Human shell text is parsed into a typed IR. AI agents normally create the IR directly.
The executor never concatenates argv into a shell string.

Required initial nodes:

- simple direct-argv command;
- Ferrous builtin;
- sequence, `&&`, `||`;
- bounded pipeline;
- explicit input/output redirection;
- background job;
- session-state mutation (`cd`, environment overlay);
- explicit native-shell escape node that always has a distinct risk class.

### 6.3 Session state

A persistent session owns:

- session id and actor;
- cwd as a capability-relative path;
- immutable base environment plus approved overlays;
- open stdin/stdout/stderr/event streams;
- process/job table;
- capability lease set;
- cancellation token and resource budgets;
- checkpoint/audit references;
- terminal dimensions and renderer mode.

`cd` changes session state; it must not be implemented as a child process whose cwd
vanishes when that process exits.

### 6.4 Execution backends

**WASI:** Ferrous builtins and sandboxed components. Explicit filesystem roots,
environment names, network grants, memory/fuel/output/time limits, and safe event output.

**Native:** direct argv and persistent PTY. Unix process-group enforcement; Windows
ConPTY plus Job Object/process-tree enforcement; macOS policy adapter. Unsupported or
partially enforceable policy fails closed.

### 6.5 AI-facing results

Every operation returns structured data as well as human-readable output:

```json
{
  "session_id": "...",
  "status": "exited",
  "exit_code": 0,
  "stdout": "...",
  "stderr": "...",
  "cwd": "workspace/app",
  "duration_ms": 842,
  "truncated": false,
  "effects": [],
  "audit_id": "..."
}
```

Terminal bytes must be treated as untrusted input before rendering. ANSI/control-sequence
handling must not permit output to inject keystrokes, alter files, or deceive the human.

## 7. Performance strategy

Safety must not put a model call or approval round-trip in the normal execution hot path.

- Compile/normalize the session policy at session open.
- Use allocation-light path and argv checks for ordinary operations.
- Dispatch safe builtins in-process.
- Execute external commands with direct argv, not an intermediate shell.
- Use bounded event channels and output budgets.
- Cache scoped approval leases until expiry/revocation.
- Batch independent read-only requests where ordering permits.
- Keep audit persistence off the critical execution thread with bounded backpressure;
  never silently drop security-critical terminal events.
- Make scanners asynchronous for known-content checks, but block promotion of an install
  until required checks complete.
- Measure policy checks separately from process spawn, PTY setup, Wasmtime compilation,
  guest startup, and model latency.

Existing Phase 1 microbenchmarks already demonstrate sub-millisecond policy/pipe
primitives; they must not be misrepresented as end-to-end command latency.

## 8. Threat model and required controls

| Threat | Required control | Residual risk |
| --- | --- | --- |
| AI requests destructive path | Canonical capability-relative paths, operation-specific policy, checkpoint, approval tier | Human may approve the effect |
| `..` or symlink escape | Lexical validation plus resolved-ancestor checks; OS/WASI enforcement | TOCTOU/native filesystem edge cases require adversarial tests |
| Malicious repository instructions | Treat files/output as data; agent policy cannot be overridden by workspace text | Model may still be socially manipulated |
| Malicious package lifecycle script | Quarantine, lock/integrity/provenance checks, no secrets, restricted network, sandbox, rollback | Unknown malware may pass static checks |
| Data exfiltration | Default-deny network, domain/port proxy/policy, secret non-inheritance, output redaction | Approved external endpoint can still receive approved data |
| Native child escapes | OS sandbox adapter, process group/job object, no ambient fallback, fail closed | OS/platform bugs and unsupported features |
| Approval spoofing | Human-only auth channel, exact effect summary, action digest, no model-controlled approval | Human can approve a deceptive request |
| Prompt injection in command output | Output taint/provenance, no authority derived from output, tool/result boundaries | Model interpretation remains probabilistic |
| Terminal escape injection | Safe ANSI handling/filtering before renderer; structured events | Renderer bugs require testing |
| Resource exhaustion | WASI StoreLimits, fuel/time/output caps, native watchdog, process-tree kill, queue caps | Host-level contention remains possible |
| Audit tampering | Append-only bounded audit path, actor/session/action digest, integrity checks | Local privileged user can alter host data |

## 9. Conclusions

1. **Do not embed GNU Bash.** It is GPLv3 and assumes a Unix process environment; it
   conflicts with the current license policy and does not become safe merely by being
   placed inside a WASI host.
2. **Do not use Trivy as a live command firewall.** Use scanners at import/install/
   promotion boundaries; use capabilities and OS/WASI isolation for execution.
3. **Build Ferrous Shell as a Bash-like language and execution kernel.** Target the
   common development workflow first, with explicit semantics and structured IR.
4. **Keep exact shell compatibility as an escape hatch.** Run a user-installed Bash,
   PowerShell, or cmd through an explicitly elevated native adapter; never make it the
   unreviewed AI default.
5. **Use password elevation, but scope the result.** Authentication unlocks a temporary
   capability lease; it must never turn into unrestricted ambient authority.
6. **WASI is the default AI path.** Native execution is a separate adapter and must fail
   closed when its OS enforcement is incomplete.
7. **The security boundary is below the parser.** Parsing and risk classification improve
   UX; only sandbox/policy enforcement can contain a successful bypass or malicious code.
8. **The first implementation target is a persistent session protocol, not UI.** CLI,
   AI, and future Tauri/wterm clients should consume the same event and control API.

## 10. Decisions required before implementation

These are the remaining design decisions for review:

1. **Shell compatibility:** approve the Bash-like portable subset plus explicit native
   shell escape, rather than exact Bash cloning.
2. **Default write policy:** choose whether workspace writes are automatic inside the
   active project or require one session-level approval.
3. **Network model:** choose domain-aware proxy/policy as the target, with loopback-only
   WASI as the current safe baseline.
4. **Package installation:** approve the staged/quarantine/install-script model, including
   default `--ignore-scripts` for fetch-only operations.
5. **Native platform order:** finish Unix enforcement first, then Windows ConPTY/Job Object,
   then macOS policy adapter; unsupported paths remain fail-closed.
6. **Elevation duration:** choose per-action, per-command-class, or short session leases;
   recommended default is short-lived scoped leases with explicit revocation.
7. **Compatibility expectations:** define which Bash/POSIX syntax is guaranteed and which
   constructs are intentionally rejected in AI mode.
8. **Rollback:** approve mandatory pre-mutation checkpoints for package installs, broad
   deletes, dependency changes, and other high-impact operations.

## 11. Gated definition of done

Implementation cannot be called complete until all of the following are demonstrated:

- Human and AI clients use the same persistent terminal/session protocol.
- `cd`, environment overlays, stdin, pipelines, redirects, cancellation, and resume work.
- Safe builtins do not spawn a host shell.
- AI commands are structured or parsed into an IR before execution.
- Direct argv is preserved at every native boundary.
- Capability checks are enforced by WASI and supported OS adapters, not only by UI prompts.
- Password elevation produces scoped, expiring, auditable leases and never exposes secrets.
- Package installs are staged, scanned, policy-reviewed, sandboxed, and rollback-capable.
- Prompt-injected output cannot grant authority or approve its own next action.
- Terminal control bytes are safe before reaching a human renderer.
- Unix, Windows, and macOS behavior is either verified or explicitly fails closed.
- Unit, integration, property/fuzz, adversarial race, supply-chain fixture, replay, and
  performance tests pass with honest benchmarks.
- Independent security review remains required before production distribution.

## 12. Research limitations

The web search service returned no result pages for several broad searches in this pass,
so the evidence above prioritizes directly fetched official documentation, project
repositories, POSIX specifications, and the existing Ferrous ADRs. Vendor feature pages
change quickly; final implementation choices must re-check versions, licenses, and platform
APIs immediately before dependency or OS-adapter selection.
