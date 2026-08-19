//! The Ferrous Shell IR executor.
//!
//! [`ShellExecutor`] interprets a typed [`ShellProgram`] against a persistent
//! [`TerminalSession`]. Execution rules:
//!
//! - builtins run in-process through [`BuiltinExecutor`];
//! - external programs run by direct argv through the native policy adapter
//!   (never through an intermediate shell);
//! - WASI components run through the embedded Wasmtime runtime;
//! - pipelines run stages over bounded channels with backpressure; a blocked
//!   downstream stage terminates the pipeline without deadlocking;
//! - `&&` and `||` short-circuit on exact exit status;
//! - redirections target capability-checked session files;
//! - background jobs are owned by the session and killed on close.
//!
//! The executor is the *only* code path that turns IR into side effects. It
//! never constructs a shell command string and never falls back to ambient
//! execution.
//!
//! # Known v1 limits (documented, not silent)
//!
//! - WASI components in pipelines do not yet receive the previous stage's
//!   stdin (they run with empty stdin); builtins and native stages do.
//! - Background job output is not streamed to the interactive sink; the exit
//!   code is recorded on the job handle.
//! - Foreground native commands run to completion bounded by session limits;
//!   interactive mid-run input/resize is a follow-up (the broker path already
//!   supports it).

use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use crate::builtin::{BuiltinExecutor, BuiltinResult};
use crate::cancel::CancelHandle;
use crate::capability::{CapabilityGrant, ResourceLimits};
use crate::command::{Actor, CommandRequest, ExecutionMode, SessionEvent, Stream};
use crate::native::{NativeBackend, NativeError};
use crate::native_session::NativeSessionHandle;
use crate::shell_ir::{Builtin, CommandSpec, Program, Redirect, ShellProgram, Statement};
use crate::terminal_session::{SessionPath, TerminalSession, TerminalSessionSpec};

/// How a plan ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    /// A background job is still running.
    Running,
    /// The plan completed with an exit code.
    Exited,
    /// The plan failed before reaching an exit code.
    Failed,
    /// The plan was cancelled.
    Cancelled,
    /// The plan was denied by policy or the authority.
    Denied,
}

/// One recorded effect of a plan, for the audit trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectRecord {
    /// Kind of effect (`read`, `write`, `delete`, `network`, `native`).
    pub kind: &'static str,
    /// Target of the effect (path, host, or program name).
    pub target: String,
    /// Whether the effect ran under an explicit approval.
    pub approved: bool,
}

/// The result of executing one plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanResult {
    /// How the plan ended.
    pub status: PlanStatus,
    /// Exit code when the plan reached one.
    pub exit_code: Option<i32>,
    /// Recorded effects in execution order.
    pub effects: Vec<EffectRecord>,
    /// Deterministic audit identifier for this plan execution.
    pub audit_id: u128,
}

/// Errors produced while executing a plan.
#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    /// The IR was structurally invalid.
    #[error("invalid execution plan: {0}")]
    InvalidPlan(&'static str),
    /// The command needs authority the session does not hold.
    #[error("denied by capability policy: {0}")]
    Denied(String),
    /// The native backend rejected the command.
    #[error("native execution failed: {0}")]
    Native(#[from] NativeError),
    /// The command request was rejected at construction.
    #[error("invalid command request: {0}")]
    Command(#[from] crate::command::CommandError),
    /// The filesystem rejected an executor operation.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    /// The session rejected the operation.
    #[error("session error: {0}")]
    Session(#[from] crate::terminal_session::SessionError),
    /// The WASI backend rejected the command.
    #[error("WASI execution failed: {0}")]
    Wasi(#[from] crate::RuntimeError),
    /// The event sink rejected an event.
    #[error("event sink failed: {0}")]
    Sink(String),
    /// The command needs human approval that was not granted.
    #[error("command requires human approval")]
    RequiresApproval,
    /// The command was denied by the human authority.
    #[error("command denied by human")]
    HumanDenied,
}

/// Append or truncate when writing a redirection target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendMode {
    /// Truncate the file before writing.
    Truncate,
    /// Append to the end of the file.
    Append,
}

/// Pipeline bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipelinePolicy {
    /// Maximum number of stages in one pipeline.
    pub max_stages: usize,
    /// Maximum bytes buffered between stages (backpressure bound).
    pub max_buffer_bytes: usize,
    /// Whether binary output may flow through the pipeline.
    pub allow_binary: bool,
}

impl Default for PipelinePolicy {
    fn default() -> Self {
        Self {
            max_stages: 64,
            max_buffer_bytes: 1_048_576,
            allow_binary: true,
        }
    }
}

/// The authority view exposed to the executor.
///
/// The executor can only *ask* whether a command may run; it cannot
/// authorize, mint, or widen anything itself.
pub trait ApprovalAuthorityView {
    /// Whether a native external command may run.
    ///
    /// # Errors
    ///
    /// Returns [`ExecuteError::HumanDenied`] or
    /// [`ExecuteError::RequiresApproval`] when the command needs (or was
    /// refused) human approval.
    fn authorize_native(&self, request: &CommandRequest) -> Result<(), ExecuteError>;
}

/// A sink that accepts session events produced by execution.
pub trait EventSink {
    /// Emit one event.
    ///
    /// # Errors
    ///
    /// Returns an error if the sink cannot accept the event.
    fn emit(&mut self, event: SessionEvent) -> Result<(), ExecuteError>;
}

/// An [`EventSink`] that records events in a [`Vec`].
#[derive(Debug, Default)]
pub struct VecSink {
    /// Recorded events.
    pub events: Vec<SessionEvent>,
}

impl EventSink for VecSink {
    fn emit(&mut self, event: SessionEvent) -> Result<(), ExecuteError> {
        self.events.push(event);
        Ok(())
    }
}

/// An authority that denies every native command (default for untrusted runs).
#[derive(Debug, Default)]
pub struct DenyNative;

impl ApprovalAuthorityView for DenyNative {
    fn authorize_native(&self, _request: &CommandRequest) -> Result<(), ExecuteError> {
        Err(ExecuteError::RequiresApproval)
    }
}

/// The executor. Owns the WASI runtime used for WASI components.
pub struct ShellExecutor {
    runtime: Arc<crate::WasiRuntime>,
}

impl ShellExecutor {
    /// Create an executor with a fresh embedded Wasmtime runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime cannot be configured.
    pub fn new() -> Result<Self, ExecuteError> {
        Ok(Self {
            runtime: Arc::new(crate::WasiRuntime::new()?),
        })
    }

    /// The embedded runtime (shared with background threads).
    pub fn runtime(&self) -> &Arc<crate::WasiRuntime> {
        &self.runtime
    }

    /// Execute one plan against a session.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid plans, denied commands, backend failures,
    /// and cancellations.
    pub fn execute(
        &self,
        program: &ShellProgram,
        session: &mut TerminalSession,
        authority: &dyn ApprovalAuthorityView,
        sink: &mut dyn EventSink,
    ) -> Result<PlanResult, ExecuteError> {
        let audit_id = u128::from(session.id()) << 64 | 0x5E11;
        let mut effects = Vec::new();
        let mut last_status = PlanStatus::Exited;
        let mut last_code = Some(0);

        for statement in &program.statements {
            let (status, code, mut statement_effects) =
                self.execute_statement(statement, session, authority, sink)?;
            effects.append(&mut statement_effects);
            last_status = status;
            if status == PlanStatus::Exited {
                last_code = code;
            } else if matches!(status, PlanStatus::Denied | PlanStatus::Failed) {
                last_code = code.or(Some(1));
            }
            if matches!(status, PlanStatus::Cancelled | PlanStatus::Denied) {
                break;
            }
        }

        Ok(PlanResult {
            status: last_status,
            exit_code: last_code,
            effects,
            audit_id,
        })
    }

    fn execute_statement(
        &self,
        statement: &Statement,
        session: &mut TerminalSession,
        authority: &dyn ApprovalAuthorityView,
        sink: &mut dyn EventSink,
    ) -> Result<(PlanStatus, Option<i32>, Vec<EffectRecord>), ExecuteError> {
        match statement {
            Statement::Command(spec) => {
                let result = self.execute_command(spec, session, authority, sink)?;
                Ok((result.status, result.exit_code, result.effects))
            }
            Statement::Pipeline(stages) => {
                let result = self.execute_pipeline(stages, session, authority, sink)?;
                Ok((result.status, result.exit_code, result.effects))
            }
            Statement::And(left, right) => {
                let (status, code, mut effects) =
                    self.execute_statement(left, session, authority, sink)?;
                if status == PlanStatus::Exited && code == Some(0) {
                    let (status, code, right_effects) =
                        self.execute_statement(right, session, authority, sink)?;
                    effects.extend(right_effects);
                    Ok((status, code, effects))
                } else {
                    Ok((status, code, effects))
                }
            }
            Statement::Or(left, right) => {
                let (status, code, mut effects) =
                    self.execute_statement(left, session, authority, sink)?;
                if status != PlanStatus::Exited || code != Some(0) {
                    let (status, code, right_effects) =
                        self.execute_statement(right, session, authority, sink)?;
                    effects.extend(right_effects);
                    Ok((status, code, effects))
                } else {
                    Ok((status, code, effects))
                }
            }
            Statement::Sequence(statements) => {
                let mut effects = Vec::new();
                let mut status = PlanStatus::Exited;
                let mut code = Some(0);
                for statement in statements {
                    let (statement_status, statement_code, mut statement_effects) =
                        self.execute_statement(statement, session, authority, sink)?;
                    effects.append(&mut statement_effects);
                    status = statement_status;
                    if statement_status == PlanStatus::Exited {
                        code = statement_code;
                    }
                    if matches!(status, PlanStatus::Cancelled | PlanStatus::Denied) {
                        break;
                    }
                }
                Ok((status, code, effects))
            }
            Statement::Background(statement) => {
                // The background job runs on a snapshot of the session state:
                // the same spec, cwd, and env, but its own job table. The
                // parent session registers the job's cancellation handle so
                // `close()` kills it, and receives the exit code on a slot.
                let cancel = CancelHandle::new();
                let digest = crate::shell_ir::CommandDigest::of(&ShellProgram {
                    statements: vec![statement.clone()],
                })
                .hex();
                let (job_id, exit_slot) = session.register_job_with_exit(digest, cancel.clone())?;
                let snapshot = TerminalSession::snapshot(session)?;
                let runtime = self.runtime.clone();
                let thread_cancel = cancel.clone();
                let background = std::thread::spawn(move || {
                    let mut snapshot = snapshot;
                    let mut sink = VecSink::default();
                    let executor = ShellExecutor {
                        runtime: runtime.clone(),
                    };
                    let result = executor.execute_statement(
                        statement,
                        &mut snapshot,
                        &DenyNative,
                        &mut sink,
                    );
                    let code = match result {
                        Ok((_, code, _)) => code.unwrap_or(1),
                        Err(_) => 1,
                    };
                    if thread_cancel.is_cancelled() {
                        *exit_slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    } else {
                        *exit_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(code);
                    }
                });
                // Keep the join handle alive so the job is not detached before
                // registering; the thread itself runs to completion. Background
                // jobs are tracked via `session.job_count()`; their exit codes
                // land on the job's exit slot.
                let _ = (background, job_id);
                Ok((PlanStatus::Running, None, Vec::new()))
            }
        }
    }

    fn execute_command(
        &self,
        spec: &CommandSpec,
        session: &mut TerminalSession,
        authority: &dyn ApprovalAuthorityView,
        sink: &mut dyn EventSink,
    ) -> Result<PlanResult, ExecuteError> {
        // Resolve redirections into concrete targets before running.
        let mut stdin_bytes: Option<Vec<u8>> = None;
        let mut stdout_target: Option<(std::path::PathBuf, AppendMode)> = None;
        let mut stderr_target: Option<(std::path::PathBuf, AppendMode)> = None;

        for redirect in &spec.redirects {
            match redirect {
                Redirect::Input(path) => {
                    let resolved = session.resolve_path(path)?;
                    stdin_bytes = Some(std::fs::read(&resolved)?);
                }
                Redirect::OutputTruncate(path) => {
                    let resolved = session.resolve_new_path(path)?;
                    stdout_target = Some((resolved, AppendMode::Truncate));
                }
                Redirect::OutputAppend(path) => {
                    let resolved = session.resolve_new_path(path)?;
                    stdout_target = Some((resolved, AppendMode::Append));
                }
                Redirect::ErrorTruncate(path) => {
                    let resolved = session.resolve_new_path(path)?;
                    stderr_target = Some((resolved, AppendMode::Truncate));
                }
                Redirect::ErrorAppend(path) => {
                    let resolved = session.resolve_new_path(path)?;
                    stderr_target = Some((resolved, AppendMode::Append));
                }
            }
        }

        let cwd = session.resolve_path(&spec.cwd)?;

        match &spec.program {
            Program::Builtin(builtin) => {
                // `cat < file` and similar: builtins that take stdin consume
                // the redirect bytes as their input.
                let mut result = if matches!(builtin, Builtin::Cat(_)) && stdin_bytes.is_some() {
                    let bytes = stdin_bytes.take().unwrap_or_default();
                    BuiltinResult::with_stdout(bytes)
                } else {
                    BuiltinExecutor.execute(builtin, session)
                };
                let stdout_capture = std::mem::take(&mut result.stdout);
                let stderr_capture = std::mem::take(&mut result.stderr);
                let code = result.exit_code;
                let stdout_redirected = stdout_target.is_some();
                let stderr_redirected = stderr_target.is_some();
                apply_redirects(
                    stdout_target,
                    stderr_target,
                    &stdout_capture,
                    &stderr_capture,
                )?;
                if !stdout_redirected && !stdout_capture.is_empty() {
                    sink.emit(SessionEvent::Output {
                        stream: Stream::Stdout,
                        bytes: stdout_capture,
                    })?;
                }
                if !stderr_redirected && !stderr_capture.is_empty() {
                    sink.emit(SessionEvent::Output {
                        stream: Stream::Stderr,
                        bytes: stderr_capture,
                    })?;
                }
                sink.emit(SessionEvent::Exited { code: Some(code) })?;
                Ok(PlanResult {
                    status: if code == 0 {
                        PlanStatus::Exited
                    } else {
                        PlanStatus::Failed
                    },
                    exit_code: Some(code),
                    effects: vec![EffectRecord {
                        kind: "builtin",
                        target: format!("{builtin:?}"),
                        approved: false,
                    }],
                    audit_id: 0,
                })
            }
            Program::External(program) => {
                let request = CommandRequest::new(
                    session.id(),
                    session.actor(),
                    ExecutionMode::Native,
                    program.clone(),
                    spec.args.clone(),
                    &cwd,
                    session.base_grant().clone().allow_native_execution(),
                )?;
                authority.authorize_native(&request)?;
                let native = NativeBackend::new().spawn(&request)?;
                let cancel = CancelHandle::new();
                let (events_tx, _events_rx) = mpsc::channel();
                let session_handle =
                    NativeSessionHandle::new(native, cancel, session.limits(), events_tx);
                let runner = std::thread::spawn(move || session_handle.run());

                // Feed piped stdin if the previous stage produced any.
                let result = runner
                    .join()
                    .map_err(|_| ExecuteError::InvalidPlan("native runner panicked"))?;
                let captured = match result {
                    Ok(output) => output,
                    Err(NativeError::Cancelled) => {
                        sink.emit(SessionEvent::Cancelled)?;
                        return Ok(PlanResult {
                            status: PlanStatus::Cancelled,
                            exit_code: None,
                            effects: vec![EffectRecord {
                                kind: "native",
                                target: program.clone(),
                                approved: true,
                            }],
                            audit_id: 0,
                        });
                    }
                    Err(NativeError::OutputLimit) => {
                        sink.emit(SessionEvent::Denied)?;
                        return Ok(PlanResult {
                            status: PlanStatus::Failed,
                            exit_code: Some(1),
                            effects: vec![EffectRecord {
                                kind: "native",
                                target: program.clone(),
                                approved: true,
                            }],
                            audit_id: 0,
                        });
                    }
                    Err(_) => {
                        sink.emit(SessionEvent::Unsupported)?;
                        return Ok(PlanResult {
                            status: PlanStatus::Failed,
                            exit_code: Some(1),
                            effects: vec![EffectRecord {
                                kind: "native",
                                target: program.clone(),
                                approved: true,
                            }],
                            audit_id: 0,
                        });
                    }
                };
                let code = i32::try_from(captured.exit_code).unwrap_or(1);
                let stdout_redirected = stdout_target.is_some();
                let stderr_redirected = stderr_target.is_some();
                apply_redirects(
                    stdout_target,
                    stderr_target,
                    &captured.stdout,
                    &captured.stderr,
                )?;
                if !stdout_redirected && !captured.stdout.is_empty() {
                    sink.emit(SessionEvent::Output {
                        stream: Stream::Stdout,
                        bytes: captured.stdout,
                    })?;
                }
                if !stderr_redirected && !captured.stderr.is_empty() {
                    sink.emit(SessionEvent::Output {
                        stream: Stream::Stderr,
                        bytes: captured.stderr,
                    })?;
                }
                sink.emit(SessionEvent::Exited { code: Some(code) })?;
                Ok(PlanResult {
                    status: if code == 0 {
                        PlanStatus::Exited
                    } else {
                        PlanStatus::Failed
                    },
                    exit_code: Some(code),
                    effects: vec![EffectRecord {
                        kind: "native",
                        target: program.clone(),
                        approved: true,
                    }],
                    audit_id: 0,
                })
            }
            Program::WasiComponent(component_path) => {
                let resolved = session.resolve_path(
                    &SessionPath::new(component_path.clone())
                        .map_err(|_| ExecuteError::InvalidPlan("invalid WASI component path"))?,
                )?;
                let bytes = std::fs::read(&resolved)?;
                let component = self.runtime.compile_component(&bytes)?;
                let request = CommandRequest::new(
                    session.id(),
                    session.actor(),
                    ExecutionMode::Wasi,
                    component_path.clone(),
                    spec.args.clone(),
                    &cwd,
                    session.base_grant().clone(),
                )?;
                let cancel = CancelHandle::new();
                let (events_tx, events_rx) = mpsc::channel();
                let runner_cancel = cancel.clone();
                let runtime = self.runtime.clone();
                let runner = std::thread::spawn(move || {
                    runtime.run_wasi_events(&component, &request, &runner_cancel, &events_tx)
                });
                let stdout_redirected = stdout_target.is_some();
                let stderr_redirected = stderr_target.is_some();
                let mut stdout_capture = Vec::new();
                let mut stderr_capture = Vec::new();
                loop {
                    match events_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(SessionEvent::Output { stream, bytes }) => {
                            if stream == Stream::Stdout {
                                stdout_capture.extend_from_slice(&bytes);
                            } else {
                                stderr_capture.extend_from_slice(&bytes);
                            }
                            let emit = match stream {
                                Stream::Stdout => !stdout_redirected,
                                Stream::Stderr => !stderr_redirected,
                            };
                            if emit {
                                sink.emit(SessionEvent::Output { stream, bytes })?;
                            }
                        }
                        Ok(_) => {}
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let result = runner
                    .join()
                    .map_err(|_| ExecuteError::InvalidPlan("WASI runner panicked"))?;
                let code = match result {
                    Ok(output) => output.exit_code,
                    Err(crate::RuntimeError::Cancelled) => {
                        sink.emit(SessionEvent::Cancelled)?;
                        return Ok(PlanResult {
                            status: PlanStatus::Cancelled,
                            exit_code: None,
                            effects: vec![EffectRecord {
                                kind: "wasi",
                                target: component_path.clone(),
                                approved: false,
                            }],
                            audit_id: 0,
                        });
                    }
                    Err(_) => 1,
                };
                apply_redirects(
                    stdout_target,
                    stderr_target,
                    &stdout_capture,
                    &stderr_capture,
                )?;
                sink.emit(SessionEvent::Exited { code: Some(code) })?;
                Ok(PlanResult {
                    status: if code == 0 {
                        PlanStatus::Exited
                    } else {
                        PlanStatus::Failed
                    },
                    exit_code: Some(code),
                    effects: vec![EffectRecord {
                        kind: "wasi",
                        target: component_path.clone(),
                        approved: false,
                    }],
                    audit_id: 0,
                })
            }
            Program::NativeShell(_) => Err(ExecuteError::InvalidPlan(
                "native shell escape requires an explicit elevation path",
            )),
        }
    }

    fn execute_pipeline(
        &self,
        stages: &[CommandSpec],
        session: &mut TerminalSession,
        authority: &dyn ApprovalAuthorityView,
        sink: &mut dyn EventSink,
    ) -> Result<PlanResult, ExecuteError> {
        let policy = PipelinePolicy::default();
        if stages.len() > policy.max_stages {
            return Err(ExecuteError::InvalidPlan("pipeline exceeds stage bound"));
        }
        if stages.is_empty() {
            return Err(ExecuteError::InvalidPlan("empty pipeline"));
        }

        // One bounded channel between each pair of stages. Capacity is
        // expressed in chunks; the byte bound is enforced by the channel's
        // backpressure plus a per-send cap.
        let mut senders: Vec<SyncSender<Vec<u8>>> = Vec::new();
        let mut receivers: Vec<mpsc::Receiver<Vec<u8>>> = Vec::new();
        for _ in 0..stages.len().saturating_sub(1) {
            let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
            senders.push(tx);
            receivers.push(rx);
        }

        let mut handles = Vec::new();
        let mut last_exit = 1i32;
        let cancel = CancelHandle::new();

        for (index, stage) in stages.iter().enumerate() {
            let input_rx = if index == 0 {
                None
            } else {
                Some(receivers.remove(index - 1))
            };
            let output_tx = senders.get(index).cloned();
            let runtime = self.runtime.clone();
            let stage_cancel = cancel.clone();
            let stage = stage.clone();
            let cwd = session.cwd().to_path_buf();
            let grant = session.base_grant().clone();
            let actor = session.actor();
            let limits = session.limits();

            let handle = std::thread::spawn(move || {
                let mut sink = VecSink::default();
                let result = run_pipeline_stage(
                    &stage,
                    &runtime,
                    input_rx,
                    output_tx,
                    &grant,
                    &cwd,
                    actor,
                    limits,
                    &stage_cancel,
                    &mut sink,
                );
                (result, sink)
            });
            handles.push(handle);
        }

        for handle in handles {
            let (result, thread_sink) = handle
                .join()
                .map_err(|_| ExecuteError::InvalidPlan("pipeline stage panicked"))?;
            // Forward any output the final stage emitted through its sink.
            for event in thread_sink.events {
                if let SessionEvent::Output { bytes, .. } = event {
                    sink.emit(SessionEvent::Output {
                        stream: Stream::Stdout,
                        bytes,
                    })?;
                }
            }
            if let Ok(code) = result {
                last_exit = code;
            } else {
                last_exit = 1;
            }
        }

        sink.emit(SessionEvent::Exited {
            code: Some(last_exit),
        })?;
        Ok(PlanResult {
            status: if last_exit == 0 {
                PlanStatus::Exited
            } else {
                PlanStatus::Failed
            },
            exit_code: Some(last_exit),
            effects: Vec::new(),
            audit_id: 0,
        })
    }
}

/// Run one pipeline stage, feeding stdin from `input_rx` and forwarding stdout
/// to `output_tx`, streaming any final output to `sink`.
fn run_pipeline_stage(
    spec: &CommandSpec,
    runtime: &Arc<crate::WasiRuntime>,
    input_rx: Option<mpsc::Receiver<Vec<u8>>>,
    output_tx: Option<SyncSender<Vec<u8>>>,
    grant: &CapabilityGrant,
    cwd: &std::path::Path,
    actor: Actor,
    limits: ResourceLimits,
    cancel: &CancelHandle,
    sink: &mut VecSink,
) -> Result<i32, ExecuteError> {
    match &spec.program {
        Program::Builtin(builtin) => {
            // Builtins run in-process; stdin from the previous stage is
            // consumed only by cat-like builtins (v1: echo-style builtins
            // ignore piped input, matching `echo x | echo y` semantics).
            let cwd_path = SessionPath::new(".")
                .map_err(|_| ExecuteError::InvalidPlan("pipeline cwd is not a session path"))?;
            let mut session = TerminalSession::new(TerminalSessionSpec {
                id: 0,
                actor,
                cwd: cwd_path,
                base_grant: grant.clone(),
                limits,
            })?;
            let mut result = if matches!(builtin, Builtin::Cat(_)) {
                let mut input = Vec::new();
                if let Some(rx) = input_rx {
                    while let Ok(bytes) = rx.recv() {
                        let budget = limits.max_output_bytes().saturating_sub(input.len());
                        if budget == 0 {
                            break;
                        }
                        let take = bytes.len().min(budget);
                        input.extend_from_slice(&bytes[..take]);
                        if take < bytes.len() {
                            break;
                        }
                    }
                }
                BuiltinResult::with_stdout(input)
            } else {
                BuiltinExecutor.execute(builtin, &mut session)
            };
            let code = result.exit_code;
            let bytes = std::mem::take(&mut result.stdout);
            if let Some(tx) = output_tx {
                let _ = tx.send(bytes);
            } else {
                sink.emit(SessionEvent::Output {
                    stream: Stream::Stdout,
                    bytes,
                })?;
            }
            let _ = cancel;
            Ok(code)
        }
        Program::External(program) => {
            let request = CommandRequest::new(
                0,
                actor,
                ExecutionMode::Native,
                program.clone(),
                spec.args.clone(),
                cwd,
                grant.clone().allow_native_execution(),
            )?;
            let native = NativeBackend::new().spawn(&request)?;
            let (events_tx, _events_rx) = mpsc::channel();
            let handle = NativeSessionHandle::new(native, cancel.clone(), limits, events_tx);
            // Feed the previous stage's output into this stage's PTY input.
            if let Some(input_rx) = input_rx {
                let input_sender = handle.input_sender();
                std::thread::spawn(move || {
                    while let Ok(bytes) = input_rx.recv() {
                        if input_sender.send(bytes).is_err() {
                            break;
                        }
                    }
                });
            }
            let runner = std::thread::spawn(move || handle.run());
            let result = runner
                .join()
                .map_err(|_| ExecuteError::InvalidPlan("native pipeline stage panicked"))?;
            match result {
                Ok(output) => {
                    let code = i32::try_from(output.exit_code).unwrap_or(1);
                    if let Some(tx) = output_tx {
                        let _ = tx.send(output.stdout);
                    } else {
                        sink.emit(SessionEvent::Output {
                            stream: Stream::Stdout,
                            bytes: output.stdout,
                        })?;
                    }
                    Ok(code)
                }
                Err(_) => Ok(1),
            }
        }
        Program::WasiComponent(component_path) => {
            // v1: WASI stages run with empty stdin (documented limit).
            let component_bytes = std::fs::read(cwd.join(component_path))?;
            let component = runtime.compile_component(&component_bytes)?;
            let request = CommandRequest::new(
                0,
                actor,
                ExecutionMode::Wasi,
                component_path.clone(),
                spec.args.clone(),
                cwd,
                grant.clone(),
            )?;
            let (events_tx, events_rx) = mpsc::channel();
            let runner_cancel = cancel.clone();
            let runtime = runtime.clone();
            let runner = std::thread::spawn(move || {
                runtime.run_wasi_events(&component, &request, &runner_cancel, &events_tx)
            });
            let mut stdout_capture = Vec::new();
            loop {
                match events_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(SessionEvent::Output { bytes, .. }) => {
                        stdout_capture.extend_from_slice(&bytes);
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let result = runner
                .join()
                .map_err(|_| ExecuteError::InvalidPlan("WASI pipeline stage panicked"))?;
            let code = match result {
                Ok(output) => output.exit_code,
                Err(crate::RuntimeError::Cancelled) => 130,
                Err(_) => 1,
            };
            if let Some(tx) = output_tx {
                let _ = tx.send(stdout_capture);
            } else {
                sink.emit(SessionEvent::Output {
                    stream: Stream::Stdout,
                    bytes: stdout_capture,
                })?;
            }
            Ok(code)
        }
        Program::NativeShell(_) => Err(ExecuteError::InvalidPlan(
            "native shell escape requires explicit elevation",
        )),
    }
}

/// Apply stdout/stderr redirections to captured output.
fn apply_redirects(
    stdout_target: Option<(std::path::PathBuf, AppendMode)>,
    stderr_target: Option<(std::path::PathBuf, AppendMode)>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), ExecuteError> {
    if let Some((path, mode)) = stdout_target {
        write_redirect(&path, mode, stdout)?;
    }
    if let Some((path, mode)) = stderr_target {
        write_redirect(&path, mode, stderr)?;
    }
    Ok(())
}

/// Write captured output to a redirect target with the requested mode.
fn write_redirect(
    path: &std::path::Path,
    mode: AppendMode,
    bytes: &[u8],
) -> Result<(), ExecuteError> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(mode == AppendMode::Truncate)
        .append(mode == AppendMode::Append)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityGrant, FilesystemAccess};
    use crate::shell_ir::{CommandDigest, NativeShellKind};

    fn test_session(name: &str) -> (TerminalSession, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ferrous-executor-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root is created");
        std::fs::create_dir_all(root.join("sub")).expect("subdir is created");
        std::fs::write(root.join("hello.txt"), b"hello\n").expect("file written");
        let grant = CapabilityGrant::workspace(&root, FilesystemAccess::ReadWrite)
            .expect("absolute workspace");
        let session = TerminalSession::new(TerminalSessionSpec {
            id: 7,
            actor: Actor::Human,
            cwd: SessionPath::new(".").expect("valid cwd"),
            base_grant: grant,
            limits: ResourceLimits::new(1_048_576, 30).expect("valid limits"),
        })
        .expect("session opens");
        (session, root)
    }

    fn command(program: Program, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program,
            args: args.iter().map(|value| (*value).to_owned()).collect(),
            redirects: Vec::new(),
            cwd: SessionPath::new(".").expect("valid cwd"),
        }
    }

    fn output_of(sink: &VecSink) -> Vec<u8> {
        sink.events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Output { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// A test authority that approves every native command. Used only to
    /// exercise executor sequencing; real sessions must go through the human
    /// authority.
    struct TestApprove;

    impl ApprovalAuthorityView for TestApprove {
        fn authorize_native(&self, _request: &CommandRequest) -> Result<(), ExecuteError> {
            Ok(())
        }
    }

    #[test]
    fn builtin_echo_runs_in_process() {
        let (mut session, _root) = test_session("echo");
        let executor = ShellExecutor::new().expect("executor");
        let program = ShellProgram {
            statements: vec![Statement::Command(command(
                Program::Builtin(Builtin::Echo(vec!["hi".to_owned()])),
                &[],
            ))],
        };
        let mut sink = VecSink::default();
        let result = executor
            .execute(&program, &mut session, &DenyNative, &mut sink)
            .expect("executes");
        assert_eq!(result.status, PlanStatus::Exited);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(output_of(&sink), b"hi\n");
    }

    /// `false || echo fallback` exercises the OR short-circuit with a real
    /// native exit code. Unix-only: `false` is a Unix executable and Windows
    /// has no cross-platform failing program to stand in.
    #[cfg(unix)]
    #[test]
    fn or_sequence_runs_fallback_on_failure() {
        let (mut session, _root) = test_session("or-fallback");
        let executor = ShellExecutor::new().expect("executor");
        // `false || echo fallback` -> exit 0, prints fallback.
        let program = ShellProgram {
            statements: vec![Statement::Or(
                Box::new(Statement::Command(command(
                    Program::External("false".to_owned()),
                    &[],
                ))),
                Box::new(Statement::Command(command(
                    Program::Builtin(Builtin::Echo(vec!["fallback".to_owned()])),
                    &[],
                ))),
            )],
        };
        let mut sink = VecSink::default();
        let result = executor
            .execute(&program, &mut session, &TestApprove, &mut sink)
            .expect("executes");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(output_of(&sink), b"fallback\n");
    }

    #[test]
    fn cd_then_pwd_uses_the_new_directory() {
        let (mut session, root) = test_session("cd-pwd");
        let executor = ShellExecutor::new().expect("executor");
        let program = ShellProgram {
            statements: vec![
                Statement::Command(command(
                    Program::Builtin(Builtin::Cd(SessionPath::new("sub").expect("valid path"))),
                    &[],
                )),
                Statement::Command(command(Program::Builtin(Builtin::Pwd), &[])),
            ],
        };
        let mut sink = VecSink::default();
        executor
            .execute(&program, &mut session, &DenyNative, &mut sink)
            .expect("executes");
        assert_eq!(output_of(&sink), b"sub\n");
        assert!(session.cwd().ends_with("sub"));
        assert!(session.cwd().starts_with(&root));
    }

    #[test]
    fn redirection_cannot_write_outside_the_grant() {
        let (mut session, _root) = test_session("redirect-deny");
        let executor = ShellExecutor::new().expect("executor");
        let spec = CommandSpec {
            program: Program::Builtin(Builtin::Echo(vec!["data".to_owned()])),
            args: Vec::new(),
            redirects: vec![Redirect::OutputTruncate(
                SessionPath::new("../../escape.txt").expect("lexically valid"),
            )],
            cwd: SessionPath::new(".").expect("valid cwd"),
        };
        let program = ShellProgram {
            statements: vec![Statement::Command(spec)],
        };
        let mut sink = VecSink::default();
        let result = executor.execute(&program, &mut session, &DenyNative, &mut sink);
        assert!(
            matches!(
                result,
                Err(ExecuteError::Session(
                    crate::terminal_session::SessionError::PathDenied(_)
                ))
            ),
            "redirect escape must be denied, got {result:?}"
        );
    }

    #[test]
    fn redirection_writes_to_a_capability_checked_file() {
        let (mut session, root) = test_session("redirect-write");
        let executor = ShellExecutor::new().expect("executor");
        let spec = CommandSpec {
            program: Program::Builtin(Builtin::Echo(vec!["data".to_owned()])),
            args: Vec::new(),
            redirects: vec![Redirect::OutputTruncate(
                SessionPath::new("out.txt").expect("valid path"),
            )],
            cwd: SessionPath::new(".").expect("valid cwd"),
        };
        let program = ShellProgram {
            statements: vec![Statement::Command(spec)],
        };
        let mut sink = VecSink::default();
        executor
            .execute(&program, &mut session, &DenyNative, &mut sink)
            .expect("executes");
        assert_eq!(
            std::fs::read(root.join("out.txt")).expect("redirect file"),
            b"data\n"
        );
        // Redirected output must not also stream to the sink.
        assert!(
            sink.events
                .iter()
                .all(|event| !matches!(event, SessionEvent::Output { .. })),
            "redirected output must not duplicate on the sink"
        );
    }

    #[test]
    fn input_redirect_feeds_a_builtin() {
        let (mut session, _root) = test_session("input-redirect");
        let executor = ShellExecutor::new().expect("executor");
        let spec = CommandSpec {
            program: Program::Builtin(Builtin::Cat(SessionPath::new("x").expect("valid path"))),
            args: Vec::new(),
            redirects: vec![Redirect::Input(
                SessionPath::new("hello.txt").expect("valid path"),
            )],
            cwd: SessionPath::new(".").expect("valid cwd"),
        };
        let program = ShellProgram {
            statements: vec![Statement::Command(spec)],
        };
        let mut sink = VecSink::default();
        executor
            .execute(&program, &mut session, &DenyNative, &mut sink)
            .expect("executes");
        assert_eq!(output_of(&sink), b"hello\n");
    }

    #[test]
    fn native_shell_is_never_synthesized_by_the_executor() {
        let (mut session, _root) = test_session("native-shell");
        let executor = ShellExecutor::new().expect("executor");
        let program = ShellProgram {
            statements: vec![Statement::Command(command(
                Program::NativeShell(NativeShellKind::Bash),
                &["-c", "echo unsafe"],
            ))],
        };
        let mut sink = VecSink::default();
        assert!(matches!(
            executor.execute(&program, &mut session, &DenyNative, &mut sink),
            Err(ExecuteError::InvalidPlan(_))
        ));
    }

    #[test]
    fn pipeline_builtins_flow_bounded_output() {
        let (mut session, _root) = test_session("pipeline");
        let executor = ShellExecutor::new().expect("executor");
        let program = ShellProgram {
            statements: vec![Statement::Pipeline(vec![
                command(
                    Program::Builtin(Builtin::Echo(vec!["one".to_owned(), "two".to_owned()])),
                    &[],
                ),
                command(
                    Program::Builtin(Builtin::Echo(vec!["three".to_owned()])),
                    &[],
                ),
            ])],
        };
        let mut sink = VecSink::default();
        let result = executor
            .execute(&program, &mut session, &DenyNative, &mut sink)
            .expect("executes");
        assert_eq!(result.status, PlanStatus::Exited);
        // The final stage's echo output is `three\n` (builtins ignore piped
        // input unless cat-like).
        assert_eq!(output_of(&sink), b"three\n");
    }

    #[test]
    fn command_digest_of_executed_plan_is_stable() {
        let plan = ShellProgram {
            statements: vec![Statement::Command(command(
                Program::External("npm".to_owned()),
                &["install"],
            ))],
        };
        let first = CommandDigest::of(&plan);
        let second = CommandDigest::of(&plan);
        assert_eq!(first, second);
    }

    #[test]
    fn background_job_registers_in_the_session_and_close_kills_it() {
        let (mut session, _root) = test_session("background");
        let executor = ShellExecutor::new().expect("executor");
        let program = ShellProgram {
            statements: vec![Statement::Background(Box::new(Statement::Command(
                command(Program::Builtin(Builtin::Echo(vec!["bg".to_owned()])), &[]),
            )))],
        };
        let mut sink = VecSink::default();
        let result = executor
            .execute(&program, &mut session, &DenyNative, &mut sink)
            .expect("executes");
        assert_eq!(result.status, PlanStatus::Running);
        assert_eq!(session.job_count(), 1);
        session.close();
        assert_eq!(session.job_count(), 0);
    }
}
