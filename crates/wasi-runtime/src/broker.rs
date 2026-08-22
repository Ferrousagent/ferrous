//! The action broker: serializes execution, gates risky actions on human
//! approval, and records an audit trail.
//!
//! A single worker thread executes at most one session at a time, so commands
//! targeting one terminal never interleave. Each submitted session owns a
//! [`CancelHandle`] registered under its session id. Risky requests (filesystem
//! writes, network, environment, native) are parked awaiting approval: the
//! caller receives [`BrokerOutcome::PendingApproval`] (or
//! [`SessionEvent::PendingApproval`]) and must call [`ActionBroker::approve`]
//! or [`ActionBroker::deny`]. Unanswered approvals expire via a sweeper
//! thread. Every terminal session records an [`AuditEntry`].

use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thiserror::Error;
use wasmtime::component::Component;

use crate::cancel::CancelHandle;
use crate::command::{
    ApprovalReason, CommandError, CommandRequest, ExecutionMode, SessionEvent, SessionState,
};
use crate::native::{NativeBackend, NativeError, NativeOutput};
use crate::native_session::NativeSessionHandle;
use crate::policy::{Risk, classify_risk};
use crate::{RuntimeError, WasiOutput, WasiRuntime};

/// Maximum number of sessions a broker will hold before rejecting submissions.
///
/// The bound protects the host from a runaway agent flooding the queue with
/// unacknowledged work; sessions are released as they complete.
pub const DEFAULT_MAX_OUTSTANDING_SESSIONS: usize = 64;

/// Errors produced while interacting with the broker.
#[derive(Debug, Error)]
pub enum BrokerError {
    /// The session id is not a *live* session of this broker (never submitted,
    /// or already finished and released).
    #[error("no live session with id {0}")]
    UnknownSession(u64),
    /// The worker thread stopped, so the queue is no longer served.
    #[error("the broker worker stopped unexpectedly")]
    WorkerStopped,
    /// The request selected the wrong backend for this submission method.
    #[error("the request is not a WASI request")]
    NotWasi,
    /// The request selected WASI but was submitted to the native backend.
    #[error("the request is not a native request")]
    NotNativeRequest,
    /// The session exists but is not a native PTY session.
    #[error("session {0} is not a native session")]
    NotNative(u64),
    /// The native backend failed.
    #[error("native backend failure: {0}")]
    Native(#[from] NativeError),
    /// The request failed validation.
    #[error("invalid request: {0}")]
    InvalidRequest(#[from] CommandError),
    /// The underlying runtime could not be created or configured.
    #[error("runtime failure: {0}")]
    Runtime(#[from] RuntimeError),
    /// The number of outstanding sessions would exceed the broker capacity.
    #[error("broker capacity exceeded; cancel or wait for a running session")]
    QueueFull,
    /// A live session already owns this identifier.
    #[error("session id {0} is already live")]
    DuplicateSession(u64),
    /// The broker capacity must be greater than zero.
    #[error("broker capacity must be greater than zero")]
    InvalidCapacity,
    /// The session is not parked awaiting approval, so it cannot be approved
    /// or denied.
    #[error("no session with id {0} is awaiting approval")]
    NotPendingApproval(u64),
}

/// Final (or intermediate) result of one broker-managed session.
///
/// A session that requires approval emits [`BrokerOutcome::PendingApproval`]
/// first, then a terminal outcome once the human decides; all other sessions
/// emit exactly one terminal outcome.
#[derive(Debug)]
pub enum BrokerOutcome {
    /// The guest completed and its captured output is available.
    Completed(WasiOutput),
    /// The session is parked waiting for a human approval decision.
    PendingApproval {
        /// Why this session needs approval.
        reason: ApprovalReason,
    },
    /// The session was cancelled before the guest completed.
    Cancelled,
    /// The request was denied before it could start.
    Denied(CommandError),
    /// The WASI backend failed; the guest may have been interrupted by a limit.
    Failed(RuntimeError),
    /// A native process completed and its merged PTY output is available.
    NativeCompleted(NativeOutput),
    /// The native backend failed before or during a PTY session.
    NativeFailed(NativeError),
}

/// How the broker admitted (or rejected) a session, recorded for audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Ran without approval because the request was low-risk.
    AutoApproved,
    /// A human approved the parked request.
    Approved,
    /// A human denied the parked request.
    Denied,
    /// The parked request was auto-denied after the approval timeout.
    TimedOut,
    /// The parked request was cancelled before a decision was made.
    Cancelled,
}

/// Terminal state of a session, recorded for audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The guest finished with an exit code.
    Completed {
        /// The guest's exit code.
        exit_code: i32,
    },
    /// The session was cancelled.
    Cancelled,
    /// The request was denied before it ran.
    Denied,
    /// The backend failed while the guest ran.
    Failed,
}

/// One recorded decision about a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// Session identifier.
    pub id: u64,
    /// Requesting principal.
    pub actor: crate::command::Actor,
    /// Executable/component name.
    pub program: String,
    /// How the session was admitted.
    pub decision: ApprovalDecision,
    /// How the session ended.
    pub outcome: AuditOutcome,
}

/// Where a finished (or live-streamed) job reports its terminal state.
enum JobSink {
    /// Capturing mode: exactly one [`BrokerOutcome`] at the end.
    Outcome(mpsc::Sender<BrokerOutcome>),
    /// Streaming mode: live [`SessionEvent`]s in lifecycle order.
    Events(mpsc::Sender<SessionEvent>),
}

/// One queued, parked, or running execution.
enum JobKind {
    Wasi(Component),
    Native,
}

impl JobKind {
    fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }
}

struct Job {
    request: CommandRequest,
    kind: JobKind,
    sink: JobSink,
    session: SessionState,
    /// How this job was admitted; recorded in the audit trail at completion.
    admission: ApprovalDecision,
}

/// A job parked awaiting human approval.
struct PendingJob {
    job: Job,
    deadline: Instant,
}

/// State shared between the broker, its worker, and the approval sweeper.
struct BrokerState {
    handles: Mutex<HashMap<u64, CancelHandle>>,
    pending: Mutex<HashMap<u64, PendingJob>>,
    inputs: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    audit: Mutex<Vec<AuditEntry>>,
    approval_timeout: Duration,
    /// Test-only: the next job id whose worker execution should panic, used to
    /// prove that a panic is contained and the queue keeps serving.
    #[cfg(test)]
    panic_next: AtomicU64,
}

impl BrokerState {
    fn approval_timeout(&self) -> Duration {
        self.approval_timeout
    }

    /// Append one audit entry, dropping the oldest once the cap is reached.
    fn record(&self, entry: AuditEntry) {
        let mut audit = self.audit.lock().unwrap_or_else(PoisonError::into_inner);
        audit.push(entry);
        if audit.len() > DEFAULT_MAX_AUDIT_ENTRIES {
            let excess = audit.len() - DEFAULT_MAX_AUDIT_ENTRIES;
            audit.drain(0..excess);
        }
    }
}

/// How long a parked approval waits before the sweeper auto-denies it.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on recorded audit entries. The trail is a debugging and
/// accountability surface, not a datastore: once full, the oldest entries are
/// dropped so a long-lived IDE cannot grow memory without bound.
pub const DEFAULT_MAX_AUDIT_ENTRIES: usize = 4096;

/// Serializes execution behind one queue, gates risky actions on human
/// approval, and exposes per-session cancellation.
pub struct ActionBroker {
    engine: wasmtime::Engine,
    queue_tx: mpsc::Sender<Job>,
    native_queue_tx: mpsc::Sender<Job>,
    state: Arc<BrokerState>,
    capacity: usize,
    worker: Option<JoinHandle<()>>,
    native_worker: Option<JoinHandle<()>>,
    sweeper: Option<JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}

impl ActionBroker {
    /// Create a broker with a fresh runtime, one worker thread, the default
    /// outstanding-session capacity, and the default approval timeout.
    pub fn new() -> Result<Self, BrokerError> {
        Self::with_config(DEFAULT_MAX_OUTSTANDING_SESSIONS, DEFAULT_APPROVAL_TIMEOUT)
    }

    /// Create a broker with a custom outstanding-session capacity.
    pub fn with_capacity(capacity: usize) -> Result<Self, BrokerError> {
        Self::with_config(capacity, DEFAULT_APPROVAL_TIMEOUT)
    }

    /// Create a broker with a custom capacity and approval timeout.
    pub fn with_config(capacity: usize, approval_timeout: Duration) -> Result<Self, BrokerError> {
        if capacity == 0 {
            return Err(BrokerError::InvalidCapacity);
        }
        let runtime = WasiRuntime::new()?;
        let engine = runtime.engine().clone();
        let (queue_tx, queue_rx) = mpsc::channel::<Job>();
        let (native_queue_tx, native_queue_rx) = mpsc::channel::<Job>();
        let state = Arc::new(BrokerState {
            handles: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            inputs: Mutex::new(HashMap::new()),
            audit: Mutex::new(Vec::new()),
            approval_timeout,
            #[cfg(test)]
            panic_next: AtomicU64::new(0),
        });
        let worker_state = state.clone();
        let worker = std::thread::spawn(move || worker_loop(runtime, queue_rx, worker_state));
        let native_worker_state = state.clone();
        let native_worker =
            std::thread::spawn(move || native_worker_loop(native_queue_rx, native_worker_state));
        let sweeper_state = state.clone();
        let sweeper_stopped = Arc::new(AtomicBool::new(false));
        let sweeper_stop = sweeper_stopped.clone();
        let sweeper = std::thread::spawn(move || sweeper_loop(sweeper_state, sweeper_stop));
        Ok(Self {
            engine,
            queue_tx,
            native_queue_tx,
            state,
            capacity,
            worker: Some(worker),
            native_worker: Some(native_worker),
            sweeper: Some(sweeper),
            stopped: sweeper_stopped,
        })
    }

    /// Number of sessions currently held by the broker (running, queued, and
    /// parked awaiting approval).
    #[cfg(test)]
    fn outstanding_sessions(&self) -> usize {
        self.state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Number of sessions parked awaiting approval.
    #[cfg(test)]
    fn pending_sessions(&self) -> usize {
        self.state
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Admit and compile a component on this broker's engine.
    ///
    /// Components are bound to the engine that compiled them, so admission must
    /// happen through the broker (or any clone of its engine) rather than a
    /// throwaway runtime.
    pub fn compile_component(&self, bytes: &[u8]) -> Result<Component, RuntimeError> {
        Component::new(&self.engine, bytes).map_err(RuntimeError::Component)
    }

    /// Enqueue an admitted component for capturing execution.
    ///
    /// Returns a channel that receives one or more [`BrokerOutcome`]s: a
    /// [`BrokerOutcome::PendingApproval`] first when the request is risky, then
    /// a terminal outcome once the human decides.
    pub fn submit(
        &self,
        component: Component,
        request: CommandRequest,
    ) -> Result<mpsc::Receiver<BrokerOutcome>, BrokerError> {
        request.validate()?;
        if request.mode != ExecutionMode::Wasi {
            return Err(BrokerError::NotWasi);
        }
        let (result_tx, result_rx) = mpsc::channel();
        let job = Job {
            session: SessionState::new(request.id, request.grant.limits()),
            request,
            kind: JobKind::Wasi(component),
            sink: JobSink::Outcome(result_tx),
            admission: ApprovalDecision::AutoApproved,
        };
        self.enqueue(job)?;
        Ok(result_rx)
    }

    /// Enqueue an admitted component and stream its live session events.
    ///
    /// The receiver yields [`SessionEvent`]s in lifecycle order: optionally a
    /// `PendingApproval`, then `Started`, zero or more `Output` chunks as the
    /// guest produces them, then a terminal event (`Exited`, `Cancelled`,
    /// `Denied`, or `Unsupported`).
    pub fn submit_streaming(
        &self,
        component: Component,
        request: CommandRequest,
    ) -> Result<mpsc::Receiver<SessionEvent>, BrokerError> {
        request.validate()?;
        if request.mode != ExecutionMode::Wasi {
            return Err(BrokerError::NotWasi);
        }
        let (event_tx, event_rx) = mpsc::channel();
        let job = Job {
            session: SessionState::new(request.id, request.grant.limits()),
            request,
            kind: JobKind::Wasi(component),
            sink: JobSink::Events(event_tx),
            admission: ApprovalDecision::AutoApproved,
        };
        self.enqueue(job)?;
        Ok(event_rx)
    }

    /// Register, queue, and bound-check one job.
    fn enqueue(&self, job: Job) -> Result<(), BrokerError> {
        let id = job.request.id;
        let mut handles = self
            .state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if handles.contains_key(&id) {
            return Err(BrokerError::DuplicateSession(id));
        }
        if handles.len() >= self.capacity {
            return Err(BrokerError::QueueFull);
        }
        handles.insert(id, CancelHandle::new());
        drop(handles);
        self.send_job(job)
    }

    /// Send a job to the queue for its selected backend.
    fn send_job(&self, job: Job) -> Result<(), BrokerError> {
        let id = job.request.id;
        let sender = if job.kind.is_native() {
            &self.native_queue_tx
        } else {
            &self.queue_tx
        };
        if sender.send(job).is_err() {
            self.state
                .handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Err(BrokerError::WorkerStopped);
        }
        Ok(())
    }

    /// Submit an approved-capable native request in capturing mode.
    pub fn submit_native(
        &self,
        request: CommandRequest,
    ) -> Result<mpsc::Receiver<BrokerOutcome>, BrokerError> {
        request.validate()?;
        if request.mode != ExecutionMode::Native {
            return Err(BrokerError::NotNativeRequest);
        }
        let (result_tx, result_rx) = mpsc::channel();
        self.enqueue(Job {
            session: SessionState::new(request.id, request.grant.limits()),
            request,
            kind: JobKind::Native,
            sink: JobSink::Outcome(result_tx),
            admission: ApprovalDecision::AutoApproved,
        })?;
        Ok(result_rx)
    }

    /// Submit an approved-capable native request with live PTY events.
    pub fn submit_native_streaming(
        &self,
        request: CommandRequest,
    ) -> Result<mpsc::Receiver<SessionEvent>, BrokerError> {
        request.validate()?;
        if request.mode != ExecutionMode::Native {
            return Err(BrokerError::NotNativeRequest);
        }
        let (event_tx, event_rx) = mpsc::channel();
        self.enqueue(Job {
            session: SessionState::new(request.id, request.grant.limits()),
            request,
            kind: JobKind::Native,
            sink: JobSink::Events(event_tx),
            admission: ApprovalDecision::AutoApproved,
        })?;
        Ok(event_rx)
    }

    /// Send raw PTY input to a running native session.
    pub fn send_input(&self, id: u64, bytes: Vec<u8>) -> Result<(), BrokerError> {
        let input = self
            .state
            .inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned();
        if let Some(input) = input {
            return input.send(bytes).map_err(|_| BrokerError::WorkerStopped);
        }
        if self
            .state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&id)
        {
            return Err(BrokerError::NotNative(id));
        }
        Err(BrokerError::UnknownSession(id))
    }

    /// Approve a session parked awaiting approval and let it run.
    pub fn approve(&self, id: u64) -> Result<(), BrokerError> {
        let mut job = match self
            .state
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
        {
            Some(pending) => pending.job,
            None => return Err(BrokerError::NotPendingApproval(id)),
        };
        // A session cancelled while parked reports cancellation instead of
        // running after the fact.
        let cancelled = self
            .state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .is_some_and(|handle| handle.is_cancelled());
        if cancelled {
            send_terminal(&job.sink, Terminal::Cancelled);
            self.state.record(AuditEntry {
                id,
                actor: job.request.actor,
                program: job.request.program.clone(),
                decision: ApprovalDecision::Cancelled,
                outcome: AuditOutcome::Cancelled,
            });
            self.state
                .handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Ok(());
        }
        job.admission = ApprovalDecision::Approved;
        self.send_job(job)
    }

    /// Deny a session parked awaiting approval; it never runs.
    pub fn deny(&self, id: u64) -> Result<(), BrokerError> {
        let job = match self
            .state
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
        {
            Some(pending) => pending.job,
            None => return Err(BrokerError::NotPendingApproval(id)),
        };
        send_terminal(&job.sink, Terminal::Denied);
        self.state.record(AuditEntry {
            id,
            actor: job.request.actor,
            program: job.request.program.clone(),
            decision: ApprovalDecision::Denied,
            outcome: AuditOutcome::Denied,
        });
        self.state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        Ok(())
    }

    /// Request cancellation of a live session.
    ///
    /// Safe to call more than once; ids that are no longer live (never submitted
    /// or already finished and released) are rejected with
    /// [`BrokerError::UnknownSession`]. A session parked awaiting approval is
    /// cancelled immediately rather than left to expire.
    pub fn cancel(&self, id: u64) -> Result<(), BrokerError> {
        let job = self
            .state
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
            .map(|pending| pending.job);
        if let Some(job) = job {
            send_terminal(&job.sink, Terminal::Cancelled);
            self.state.record(AuditEntry {
                id,
                actor: job.request.actor,
                program: job.request.program.clone(),
                decision: ApprovalDecision::Cancelled,
                outcome: AuditOutcome::Cancelled,
            });
            self.state
                .handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Ok(());
        }
        let handle = self
            .state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
            .ok_or(BrokerError::UnknownSession(id))?;
        handle.cancel();
        Ok(())
    }

    /// The audit trail of every session this broker decided on, oldest first.
    pub fn audit_trail(&self) -> Vec<AuditEntry> {
        self.state
            .audit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for ActionBroker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        // Interrupt any live session so the worker is not stuck in a long run.
        if let Ok(handles) = self.state.handles.lock() {
            for handle in handles.values() {
                handle.cancel();
            }
        }
        // Close the queue first: dropping the last sender makes the worker's
        // blocking recv() return, so joining below cannot deadlock.
        let _ = std::mem::replace(&mut self.queue_tx, mpsc::channel::<Job>().0);
        let _ = std::mem::replace(&mut self.native_queue_tx, mpsc::channel::<Job>().0);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(native_worker) = self.native_worker.take() {
            let _ = native_worker.join();
        }
        if let Some(sweeper) = self.sweeper.take() {
            let _ = sweeper.join();
        }
    }
}

fn worker_loop(runtime: WasiRuntime, queue_rx: mpsc::Receiver<Job>, state: Arc<BrokerState>) {
    while let Ok(job) = queue_rx.recv() {
        process_job_guarded(Some(&runtime), &state, job);
    }
}

fn native_worker_loop(queue_rx: mpsc::Receiver<Job>, state: Arc<BrokerState>) {
    while let Ok(job) = queue_rx.recv() {
        process_job_guarded(None, &state, job);
    }
}

/// Run one job inside a panic barrier so a guest-triggered host bug cannot
/// kill the worker (and with it every queued session) silently.
fn process_job_guarded(runtime: Option<&WasiRuntime>, state: &Arc<BrokerState>, job: Job) {
    // Snapshot everything the terminal/audit paths need before the panic-prone
    // body runs, so a panic can still report the session instead of orphaning it.
    let id = job.request.id;
    let actor = job.request.actor;
    let program = job.request.program.clone();
    let admission = job.admission;
    let notify = match &job.sink {
        JobSink::Outcome(result_tx) => JobSink::Outcome(result_tx.clone()),
        JobSink::Events(event_tx) => JobSink::Events(event_tx.clone()),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_job(runtime, state, job)
    }));
    if let Err(_payload) = result {
        send_terminal(
            &notify,
            Terminal::DeniedWith(CommandError::InvalidTransition(
                "worker panic while executing session",
            )),
        );
        state.record(AuditEntry {
            id,
            actor,
            program,
            decision: admission,
            outcome: AuditOutcome::Failed,
        });
        state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        state
            .inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
}

fn process_job(runtime: Option<&WasiRuntime>, state: &Arc<BrokerState>, mut job: Job) {
    #[cfg(test)]
    if state.panic_next.swap(0, Ordering::SeqCst) == job.request.id {
        panic!("injected broker panic for red-team test");
    }
    let cancel = state
        .handles
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&job.request.id)
        .cloned();
    let Some(cancel) = cancel else {
        send_terminal(
            &job.sink,
            Terminal::DeniedWith(CommandError::InvalidTransition(
                "session handle disappeared before start",
            )),
        );
        state
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&job.request.id);
        return;
    };
    match classify_risk(&job.request) {
        // A human already approved this specific session; run it regardless
        // of its risk class instead of parking it again.
        Risk::RequiresApproval(_) if job.admission == ApprovalDecision::Approved => {
            let outcome = execute(runtime, state, &mut job, &cancel);
            state.record(AuditEntry {
                id: job.request.id,
                actor: job.request.actor,
                program: job.request.program.clone(),
                decision: ApprovalDecision::Approved,
                outcome,
            });
            state
                .handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&job.request.id);
        }
        Risk::AutoApprove => {
            let outcome = execute(runtime, state, &mut job, &cancel);
            state.record(AuditEntry {
                id: job.request.id,
                actor: job.request.actor,
                program: job.request.program.clone(),
                decision: job.admission,
                outcome,
            });
            // Release the session now that it reached a terminal state.
            state
                .handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&job.request.id);
        }
        Risk::RequiresApproval(reason) => {
            let _ = job.session.accept(SessionEvent::PendingApproval { reason });
            // Park before notifying: the moment the caller observes the
            // notification, approve/deny/cancel must find the session.
            // Clone the sink sender first so we can notify after parking.
            let notify_sink = match &job.sink {
                JobSink::Outcome(result_tx) => JobSink::Outcome(result_tx.clone()),
                JobSink::Events(event_tx) => JobSink::Events(event_tx.clone()),
            };
            let deadline = Instant::now() + state.approval_timeout();
            let id = job.request.id;
            state
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id, PendingJob { job, deadline });
            let notified = match &notify_sink {
                JobSink::Outcome(result_tx) => result_tx
                    .send(BrokerOutcome::PendingApproval { reason })
                    .is_ok(),
                JobSink::Events(event_tx) => event_tx
                    .send(SessionEvent::PendingApproval { reason })
                    .is_ok(),
            };
            if !notified {
                // The caller is gone (or already cancelled the parked
                // session); do not leave a session nobody can decide on.
                state
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                state
                    .handles
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
            }
        }
    }
}

/// Auto-deny parked approvals whose deadline passed.
fn sweeper_loop(state: Arc<BrokerState>, stopped: Arc<AtomicBool>) {
    while !stopped.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        let expired: Vec<u64> = {
            let pending = state.pending.lock().unwrap_or_else(PoisonError::into_inner);
            pending
                .iter()
                .filter(|(_, pending)| pending.deadline <= Instant::now())
                .map(|(id, _)| *id)
                .collect()
        };
        for id in expired {
            let job = state
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id)
                .map(|pending| pending.job);
            if let Some(job) = job {
                send_terminal(&job.sink, Terminal::Denied);
                state.record(AuditEntry {
                    id,
                    actor: job.request.actor,
                    program: job.request.program.clone(),
                    decision: ApprovalDecision::TimedOut,
                    outcome: AuditOutcome::Denied,
                });
                state
                    .handles
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
            }
        }
    }
}

/// Terminal signals a parked session can receive without running.
#[derive(Clone)]
enum Terminal {
    /// Cancelled by the operator or policy.
    Cancelled,
    /// Denied (by the human or the approval timeout).
    Denied,
    /// Denied with a specific error.
    DeniedWith(CommandError),
}

/// Send a terminal signal to a job's sink.
fn send_terminal(sink: &JobSink, terminal: Terminal) {
    match (sink, terminal) {
        (JobSink::Outcome(result_tx), Terminal::Cancelled) => {
            let _ = result_tx.send(BrokerOutcome::Cancelled);
        }
        (JobSink::Outcome(result_tx), Terminal::Denied) => {
            let _ = result_tx.send(BrokerOutcome::Denied(CommandError::InvalidTransition(
                "request denied by policy",
            )));
        }
        (JobSink::Outcome(result_tx), Terminal::DeniedWith(error)) => {
            let _ = result_tx.send(BrokerOutcome::Denied(error));
        }
        (JobSink::Events(event_tx), Terminal::Cancelled) => {
            let _ = event_tx.send(SessionEvent::Cancelled);
        }
        (JobSink::Events(event_tx), Terminal::Denied | Terminal::DeniedWith(_)) => {
            let _ = event_tx.send(SessionEvent::Denied);
        }
    }
}

/// Dispatch a job to its capturing or streaming execution path and return the
/// terminal audit outcome.
fn execute(
    runtime: Option<&WasiRuntime>,
    state: &Arc<BrokerState>,
    job: &mut Job,
    cancel: &CancelHandle,
) -> AuditOutcome {
    if job.kind.is_native() {
        return execute_native(state, job, cancel);
    }
    let Some(runtime) = runtime else {
        return AuditOutcome::Failed;
    };
    let streams = matches!(&job.sink, JobSink::Events(_));
    if streams {
        execute_streaming(runtime, job, cancel)
    } else {
        let outcome = execute_capturing(runtime, job, cancel);
        let audit = match &outcome {
            BrokerOutcome::Completed(output) => AuditOutcome::Completed {
                exit_code: output.exit_code,
            },
            BrokerOutcome::PendingApproval { .. } => {
                unreachable!("approval is handled before execution")
            }
            BrokerOutcome::Cancelled => AuditOutcome::Cancelled,
            BrokerOutcome::Denied(_) => AuditOutcome::Denied,
            BrokerOutcome::Failed(_) | BrokerOutcome::NativeFailed(_) => AuditOutcome::Failed,
            BrokerOutcome::NativeCompleted(output) => AuditOutcome::Completed {
                exit_code: i32::try_from(output.exit_code).unwrap_or(i32::MAX),
            },
        };
        if let JobSink::Outcome(result_tx) = &job.sink {
            let _ = result_tx.send(outcome);
        }
        audit
    }
}

/// Run one native PTY job, registering its input channel only after spawn succeeds.
fn execute_native(state: &Arc<BrokerState>, job: &mut Job, cancel: &CancelHandle) -> AuditOutcome {
    if cancel.is_cancelled() {
        if let JobSink::Outcome(sender) = &job.sink {
            let _ = sender.send(BrokerOutcome::Cancelled);
        } else if let JobSink::Events(sender) = &job.sink {
            let _ = sender.send(SessionEvent::Cancelled);
        }
        return AuditOutcome::Cancelled;
    }

    let session = match NativeBackend::new().spawn(&job.request) {
        Ok(session) => session,
        Err(error) => {
            let cancelled = matches!(&error, NativeError::Cancelled);
            match &job.sink {
                JobSink::Outcome(sender) => {
                    if cancelled {
                        let _ = sender.send(BrokerOutcome::Cancelled);
                    } else {
                        let _ = sender.send(BrokerOutcome::NativeFailed(error));
                    }
                }
                JobSink::Events(sender) => {
                    let _ = sender.send(if cancelled {
                        SessionEvent::Cancelled
                    } else {
                        SessionEvent::Unsupported
                    });
                }
            }
            return if cancelled {
                AuditOutcome::Cancelled
            } else {
                AuditOutcome::Failed
            };
        }
    };

    let event_tx = match &job.sink {
        JobSink::Events(sender) => sender.clone(),
        JobSink::Outcome(_) => {
            let (sender, receiver) = mpsc::channel();
            drop(receiver);
            sender
        }
    };
    let handle = NativeSessionHandle::new(
        session,
        cancel.clone(),
        job.request.grant.limits(),
        event_tx,
    );
    state
        .inputs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(job.request.id, handle.input_sender());

    let _ = job.session.accept(SessionEvent::Started);
    if let JobSink::Events(sender) = &job.sink {
        let _ = sender.send(SessionEvent::Started);
    }
    let result = handle.run();
    state
        .inputs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&job.request.id);

    match result {
        Ok(output) => {
            let exit_code = i32::try_from(output.exit_code).unwrap_or(i32::MAX);
            match &job.sink {
                JobSink::Outcome(sender) => {
                    let _ = sender.send(BrokerOutcome::NativeCompleted(output));
                }
                JobSink::Events(sender) => {
                    let _ = job.session.accept(SessionEvent::Exited {
                        code: Some(exit_code),
                    });
                    let _ = sender.send(SessionEvent::Exited {
                        code: Some(exit_code),
                    });
                }
            }
            AuditOutcome::Completed { exit_code }
        }
        Err(error) => {
            let cancelled = matches!(&error, NativeError::Cancelled);
            match &job.sink {
                JobSink::Outcome(sender) => {
                    if cancelled {
                        let _ = sender.send(BrokerOutcome::Cancelled);
                    } else {
                        let _ = sender.send(BrokerOutcome::NativeFailed(error));
                    }
                }
                JobSink::Events(sender) => {
                    if cancelled {
                        let _ = job.session.accept(SessionEvent::Cancelled);
                        let _ = sender.send(SessionEvent::Cancelled);
                    } else {
                        let _ = job.session.accept(SessionEvent::Unsupported);
                        let _ = sender.send(SessionEvent::Unsupported);
                    }
                }
            }
            if cancelled {
                AuditOutcome::Cancelled
            } else {
                AuditOutcome::Failed
            }
        }
    }
}

/// Run a job to completion and return its final outcome.
fn execute_capturing(runtime: &WasiRuntime, job: &mut Job, cancel: &CancelHandle) -> BrokerOutcome {
    if cancel.is_cancelled() {
        let _ = job.session.accept(SessionEvent::Cancelled);
        return BrokerOutcome::Cancelled;
    }
    if job.session.accept(SessionEvent::Started).is_err() {
        return BrokerOutcome::Denied(CommandError::InvalidTransition("session cannot start"));
    }
    let JobKind::Wasi(component) = &job.kind else {
        return BrokerOutcome::NativeFailed(NativeError::WrongMode);
    };
    match runtime.run_wasi_cancellable(component, &job.request, cancel) {
        Ok(output) => {
            let _ = job.session.accept(SessionEvent::Exited {
                code: Some(output.exit_code),
            });
            BrokerOutcome::Completed(output)
        }
        Err(RuntimeError::Cancelled) => {
            let _ = job.session.accept(SessionEvent::Cancelled);
            BrokerOutcome::Cancelled
        }
        Err(RuntimeError::WrongMode) => BrokerOutcome::Denied(CommandError::InvalidTransition(
            "non-WASI request reached the WASI backend",
        )),
        Err(error) => {
            let _ = job.session.accept(SessionEvent::Unsupported);
            BrokerOutcome::Failed(error)
        }
    }
}

/// Run a job while streaming live [`SessionEvent`]s to its event channel.
///
/// Output chunks are emitted as the guest produces them; the output budget is
/// enforced structurally by the bounded pipes inside the runtime. Returns the
/// terminal audit outcome.
fn execute_streaming(runtime: &WasiRuntime, job: &mut Job, cancel: &CancelHandle) -> AuditOutcome {
    let events = match &job.sink {
        JobSink::Events(event_tx) => event_tx,
        JobSink::Outcome(_) => return AuditOutcome::Failed,
    };
    if cancel.is_cancelled() {
        let _ = job.session.accept(SessionEvent::Cancelled);
        let _ = events.send(SessionEvent::Cancelled);
        return AuditOutcome::Cancelled;
    }
    if job.session.accept(SessionEvent::Started).is_err() {
        let _ = events.send(SessionEvent::Denied);
        return AuditOutcome::Denied;
    }
    let _ = events.send(SessionEvent::Started);
    let JobKind::Wasi(component) = &job.kind else {
        let _ = events.send(SessionEvent::Unsupported);
        return AuditOutcome::Failed;
    };
    match runtime.run_wasi_events(component, &job.request, cancel, events) {
        Ok(output) => {
            let _ = job.session.accept(SessionEvent::Exited {
                code: Some(output.exit_code),
            });
            let _ = events.send(SessionEvent::Exited {
                code: Some(output.exit_code),
            });
            AuditOutcome::Completed {
                exit_code: output.exit_code,
            }
        }
        Err(RuntimeError::Cancelled) => {
            let _ = job.session.accept(SessionEvent::Cancelled);
            let _ = events.send(SessionEvent::Cancelled);
            AuditOutcome::Cancelled
        }
        Err(RuntimeError::WrongMode) => {
            let _ = events.send(SessionEvent::Denied);
            AuditOutcome::Denied
        }
        Err(_) => {
            let _ = job.session.accept(SessionEvent::Unsupported);
            let _ = events.send(SessionEvent::Unsupported);
            AuditOutcome::Failed
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::capability::{CapabilityGrant, FilesystemAccess, ResourceLimits};
    use crate::command::Actor;

    /// Minimal WASI command that returns success immediately.
    ///
    /// The `wasi:cli/run@0.2.12` instance export is what the p2 bindings link
    /// against in wasmtime 47; a bare `run` export is not accepted.
    const HELLO_WAT: &str = r#"
        (component
          (core module $m
            (func (export "run") (result i32) (i32.const 0)))
          (core instance $i (instantiate $m))
          (func $run (result (result)) (canon lift (core func $i "run")))
          (instance (export "wasi:cli/run@0.2.12")
            (export "run" (func $run))))
    "#;

    /// WASI command that spins forever; only fuel or epoch interruption stops it.
    const SPIN_WAT: &str = r#"
        (component
          (core module $m
            (func (export "run") (result i32)
              (block $exit
                (loop $l (br $l)))
              (i32.const 0)))
          (core instance $i (instantiate $m))
          (func $run (result (result)) (canon lift (core func $i "run")))
          (instance (export "wasi:cli/run@0.2.12")
            (export "run" (func $run))))
    "#;

    /// Read-only grant: low-risk, auto-approved by the broker.
    fn grant(timeout_seconds: u64, fuel: u64) -> CapabilityGrant {
        let root = std::env::temp_dir().join(format!(
            "ferrous-broker-workspace-{}-{timeout_seconds}-{fuel}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        CapabilityGrant::workspace(&root, FilesystemAccess::Read)
            .expect("temporary root is absolute")
            .with_limits(
                ResourceLimits::new(1_048_576, timeout_seconds)
                    .expect("valid limits")
                    .with_fuel(fuel)
                    .expect("valid fuel"),
            )
    }

    /// Read-write grant: risky, requires human approval.
    fn write_grant(timeout_seconds: u64, fuel: u64) -> CapabilityGrant {
        let root = std::env::temp_dir().join(format!(
            "ferrous-broker-workspace-{}-{timeout_seconds}-{fuel}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        CapabilityGrant::workspace(&root, FilesystemAccess::ReadWrite)
            .expect("temporary root is absolute")
            .with_limits(
                ResourceLimits::new(1_048_576, timeout_seconds)
                    .expect("valid limits")
                    .with_fuel(fuel)
                    .expect("valid fuel"),
            )
    }

    #[cfg(unix)]
    fn native_grant() -> CapabilityGrant {
        let root = std::env::temp_dir().join(format!(
            "ferrous-native-broker-workspace-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        CapabilityGrant::workspace(&root, FilesystemAccess::ReadWrite)
            .expect("temporary root is absolute")
            .allow_native_execution()
    }

    #[cfg(unix)]
    fn native_request(id: u64, program: &str, args: &[&str]) -> CommandRequest {
        let grant = native_grant();
        let cwd = grant
            .filesystem_grants()
            .next()
            .expect("one filesystem grant")
            .root()
            .to_path_buf();
        CommandRequest::new(
            id,
            Actor::Agent,
            ExecutionMode::Native,
            program,
            args.iter().copied(),
            cwd,
            grant,
        )
        .expect("native request is valid")
    }

    fn request(
        broker: &ActionBroker,
        id: u64,
        program: &str,
        wat: &str,
        grant: CapabilityGrant,
    ) -> (Component, CommandRequest) {
        let bytes = wat::parse_str(wat).expect("valid WAT");
        let component = broker
            .compile_component(&bytes)
            .expect("component admission");
        let cwd = grant
            .filesystem_grants()
            .next()
            .expect("one filesystem grant")
            .root()
            .to_path_buf();
        let request = CommandRequest::new(
            id,
            Actor::Agent,
            ExecutionMode::Wasi,
            program,
            std::iter::empty::<&str>(),
            cwd,
            grant,
        )
        .expect("request is valid");
        (component, request)
    }

    #[test]
    fn hello_component_reports_exit_code_zero() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");

        let outcome = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("result within 10s");
        match outcome {
            BrokerOutcome::Completed(output) => assert_eq!(output.exit_code, 0),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn requests_run_in_submission_order() {
        let broker = ActionBroker::new().expect("broker");
        // A spins for one second before timing out; B is instant. If the broker
        // ran them in parallel, B would complete before A.
        let (component_a, request_a) =
            request(&broker, 1, "spin-a", SPIN_WAT, grant(1, 4_000_000_000));
        let (component_b, request_b) =
            request(&broker, 2, "hello-b", HELLO_WAT, grant(30, 1_000_000));
        let receiver_a = broker.submit(component_a, request_a).expect("a submitted");
        let receiver_b = broker.submit(component_b, request_b).expect("b submitted");

        assert!(
            receiver_b.recv_timeout(Duration::from_millis(100)).is_err(),
            "b must wait for a to finish"
        );

        let outcome_a = receiver_a
            .recv_timeout(Duration::from_secs(10))
            .expect("a finishes");
        assert!(
            matches!(outcome_a, BrokerOutcome::Failed(_)),
            "a should be interrupted by its timeout, got {outcome_a:?}"
        );

        let outcome_b = receiver_b
            .recv_timeout(Duration::from_secs(10))
            .expect("b finishes after a");
        assert!(matches!(outcome_b, BrokerOutcome::Completed(_)));
    }

    #[test]
    fn cancel_interrupts_a_running_guest_and_the_queue_continues() {
        let broker = ActionBroker::new().expect("broker");
        let (component_a, request_a) =
            request(&broker, 1, "spin-a", SPIN_WAT, grant(60, 4_000_000_000));
        let (component_b, request_b) =
            request(&broker, 2, "hello-b", HELLO_WAT, grant(30, 1_000_000));
        let receiver_a = broker.submit(component_a, request_a).expect("a submitted");
        let receiver_b = broker.submit(component_b, request_b).expect("b submitted");

        // Let A get running, then interrupt it.
        std::thread::sleep(Duration::from_millis(150));
        broker.cancel(1).expect("running session is cancellable");

        let outcome_a = receiver_a
            .recv_timeout(Duration::from_secs(5))
            .expect("a is interrupted promptly");
        assert!(
            matches!(outcome_a, BrokerOutcome::Cancelled),
            "a should be cancelled, got {outcome_a:?}"
        );

        let outcome_b = receiver_b
            .recv_timeout(Duration::from_secs(10))
            .expect("b still runs after a is cancelled");
        assert!(matches!(outcome_b, BrokerOutcome::Completed(_)));
    }

    #[test]
    fn cancel_before_start_skips_a_queued_action() {
        let broker = ActionBroker::new().expect("broker");
        let (component_a, request_a) =
            request(&broker, 1, "spin-a", SPIN_WAT, grant(60, 4_000_000_000));
        let (component_b, request_b) =
            request(&broker, 2, "hello-b", HELLO_WAT, grant(30, 1_000_000));
        let receiver_a = broker.submit(component_a, request_a).expect("a submitted");
        let receiver_b = broker.submit(component_b, request_b).expect("b submitted");

        // b is queued behind a and never runs: cancelling it skips execution.
        broker.cancel(2).expect("queued session is cancellable");
        broker.cancel(1).expect("running session is cancellable");

        let outcome_a = receiver_a
            .recv_timeout(Duration::from_secs(5))
            .expect("a is cancelled");
        assert!(matches!(outcome_a, BrokerOutcome::Cancelled));

        let outcome_b = receiver_b
            .recv_timeout(Duration::from_secs(10))
            .expect("b is reported cancelled without running");
        assert!(
            matches!(outcome_b, BrokerOutcome::Cancelled),
            "b should be cancelled, got {outcome_b:?}"
        );
    }

    #[test]
    fn cancel_of_an_unknown_session_errors() {
        let broker = ActionBroker::new().expect("broker");
        assert!(matches!(
            broker.cancel(999),
            Err(BrokerError::UnknownSession(999))
        ));
    }

    #[test]
    fn cancel_is_idempotent_for_a_live_session() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "spin", SPIN_WAT, grant(60, 4_000_000_000));
        let receiver = broker.submit(component, request).expect("submitted");

        broker.cancel(1).expect("first cancel");
        broker.cancel(1).expect("second cancel is a no-op");

        let outcome = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("cancelled promptly");
        assert!(matches!(outcome, BrokerOutcome::Cancelled));
    }

    #[test]
    fn completed_sessions_release_their_handles() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("session completes");

        for _ in 0..50 {
            if broker.outstanding_sessions() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            broker.outstanding_sessions(),
            0,
            "finished sessions must be released from the broker"
        );
    }

    #[test]
    fn cancel_after_completion_reports_unknown_session() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("session completes");

        for _ in 0..50 {
            if broker.outstanding_sessions() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            broker.cancel(1),
            Err(BrokerError::UnknownSession(1))
        ));
    }

    #[test]
    fn queue_rejects_submissions_beyond_capacity() {
        let broker = ActionBroker::with_capacity(2).expect("broker");
        let (component_a, request_a) =
            request(&broker, 1, "spin-a", SPIN_WAT, grant(60, 4_000_000_000));
        let (component_b, request_b) =
            request(&broker, 2, "spin-b", SPIN_WAT, grant(60, 4_000_000_000));
        let (component_c, request_c) =
            request(&broker, 3, "spin-c", SPIN_WAT, grant(60, 4_000_000_000));

        broker.submit(component_a, request_a).expect("first fits");
        broker.submit(component_b, request_b).expect("second fits");
        assert!(matches!(
            broker.submit(component_c, request_c),
            Err(BrokerError::QueueFull)
        ));
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            ActionBroker::with_capacity(0),
            Err(BrokerError::InvalidCapacity)
        ));
    }

    #[test]
    fn duplicate_live_session_id_is_rejected_without_replacing_the_first_job() {
        let broker = ActionBroker::new().expect("broker");
        let (component_a, request_a) =
            request(&broker, 41, "spin-a", SPIN_WAT, grant(60, 4_000_000_000));
        let (component_b, request_b) =
            request(&broker, 41, "hello-b", HELLO_WAT, grant(30, 1_000_000));
        let receiver_a = broker.submit(component_a, request_a).expect("first fits");

        assert!(matches!(
            broker.submit(component_b, request_b),
            Err(BrokerError::DuplicateSession(41))
        ));

        broker
            .cancel(41)
            .expect("original session remains cancellable");
        assert!(matches!(
            receiver_a
                .recv_timeout(Duration::from_secs(5))
                .expect("original session reports cancellation"),
            BrokerOutcome::Cancelled
        ));
    }

    #[test]
    fn hello_runs_with_unlimited_fuel() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(
            &broker,
            1,
            "hello",
            HELLO_WAT,
            grant(30, 1_000_000).with_limits(
                ResourceLimits::new(1_048_576, 30)
                    .expect("valid limits")
                    .with_unlimited_fuel(),
            ),
        );
        let receiver = broker.submit(component, request).expect("submitted");
        let outcome = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("result within 10s");
        match outcome {
            BrokerOutcome::Completed(output) => assert_eq!(output.exit_code, 0),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn streaming_hello_emits_started_then_exited() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        let events = broker
            .submit_streaming(component, request)
            .expect("submitted");

        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("exited event"),
            SessionEvent::Exited { code: Some(0) }
        );
        assert!(
            events.recv_timeout(Duration::from_millis(200)).is_err(),
            "no events after the terminal one"
        );
    }

    #[test]
    fn streaming_cancel_interrupts_a_running_guest() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "spin", SPIN_WAT, grant(60, 4_000_000_000));
        let events = broker
            .submit_streaming(component, request)
            .expect("submitted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );

        std::thread::sleep(Duration::from_millis(150));
        broker.cancel(1).expect("running session is cancellable");

        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(5))
                .expect("cancelled event"),
            SessionEvent::Cancelled
        );
    }

    #[test]
    fn read_only_command_runs_without_approval() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");
        let outcome = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("terminal outcome");
        assert!(
            matches!(outcome, BrokerOutcome::Completed(_)),
            "read-only commands must not require approval, got {outcome:?}"
        );
    }

    #[test]
    fn risky_command_emits_pending_approval_then_runs_after_approve() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) =
            request(&broker, 1, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");

        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval"),
            BrokerOutcome::PendingApproval {
                reason: ApprovalReason::FilesystemWrite
            }
        ));
        assert!(
            receiver.recv_timeout(Duration::from_millis(200)).is_err(),
            "nothing runs before approval"
        );

        broker.approve(1).expect("approval accepted");
        let outcome = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("terminal outcome");
        assert!(matches!(outcome, BrokerOutcome::Completed(_)));
    }

    #[test]
    fn denied_command_never_runs() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) =
            request(&broker, 1, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval"),
            BrokerOutcome::PendingApproval { .. }
        ));

        broker.deny(1).expect("denial accepted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("denied outcome"),
            BrokerOutcome::Denied(_)
        ));
    }

    #[test]
    fn approval_timeout_auto_denies() {
        let broker = ActionBroker::with_config(64, Duration::from_millis(300)).expect("broker");
        let (component, request) =
            request(&broker, 1, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval"),
            BrokerOutcome::PendingApproval { .. }
        ));

        // No decision: the sweeper auto-denies after the timeout.
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("auto-denied outcome"),
            BrokerOutcome::Denied(_)
        ));
        assert_eq!(broker.pending_sessions(), 0);
    }

    #[test]
    fn cancel_of_pending_session_reports_cancelled() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) =
            request(&broker, 1, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let receiver = broker.submit(component, request).expect("submitted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval"),
            BrokerOutcome::PendingApproval { .. }
        ));

        broker.cancel(1).expect("parked session is cancellable");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("cancelled outcome"),
            BrokerOutcome::Cancelled
        ));
        assert_eq!(broker.pending_sessions(), 0);
    }

    #[test]
    fn approve_cancel_race_yields_exactly_one_terminal_outcome() {
        // Red-team: a caller that races approve() against cancel()/deny() on
        // the same parked session must never observe two terminal outcomes or
        // a ghost run. The session must be released either way.
        let broker = Arc::new(ActionBroker::new().expect("broker"));
        for iteration in 1..=30u64 {
            let id = 10_000 + iteration;
            let (component, request) =
                request(&broker, id, "hello", HELLO_WAT, write_grant(30, 1_000_000));
            let receiver = broker.submit(component, request).expect("submitted");
            assert!(matches!(
                receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("pending approval"),
                BrokerOutcome::PendingApproval { .. }
            ));

            // Fire both decisions at the same moment so they contend on the
            // pending lock; whichever wins, exactly one terminal must arrive.
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let (ok_a, ok_c) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            );
            let (bar_a, bar_c) = (barrier.clone(), barrier.clone());
            let (ok_a2, ok_c2) = (ok_a.clone(), ok_c.clone());
            let (broker_a, broker_c) = (broker.clone(), broker.clone());
            let thread_a = std::thread::spawn(move || {
                bar_a.wait();
                ok_a2.store(broker_a.approve(id).is_ok(), Ordering::SeqCst);
            });
            let thread_c = std::thread::spawn(move || {
                bar_c.wait();
                ok_c2.store(broker_c.cancel(id).is_ok(), Ordering::SeqCst);
            });
            barrier.wait();
            // Join before reading the results: the barrier only synchronizes
            // the start, not the stores above, so without a join the main
            // thread could observe both flags before either decision landed.
            thread_a.join().expect("approver joins");
            thread_c.join().expect("canceller joins");

            let mut terminals = 0usize;
            let mut saw_denied = false;
            loop {
                match receiver.recv_timeout(Duration::from_millis(300)) {
                    Ok(BrokerOutcome::PendingApproval { .. }) => {}
                    Ok(BrokerOutcome::Denied(_)) => {
                        saw_denied = true;
                        terminals += 1;
                    }
                    Ok(BrokerOutcome::Cancelled) => terminals += 1,
                    Ok(BrokerOutcome::Completed(_)) => terminals += 1,
                    Ok(other) => panic!("unexpected outcome {other:?} in iteration {iteration}"),
                    Err(_) => break,
                }
            }
            assert_eq!(
                terminals, 1,
                "iteration {iteration}: exactly one terminal outcome expected, got {terminals}"
            );
            assert!(
                ok_a.load(Ordering::SeqCst) || ok_c.load(Ordering::SeqCst),
                "iteration {iteration}: at least one of approve/cancel must succeed"
            );
            assert!(
                !(saw_denied && terminals > 1),
                "iteration {iteration}: a denial followed by another terminal is a double-terminal"
            );

            for _ in 0..50 {
                if broker.outstanding_sessions() == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                broker.outstanding_sessions(),
                0,
                "iteration {iteration}: the raced session must be released"
            );
            let trail = broker.audit_trail();
            assert!(
                trail.iter().any(|entry| entry.id == id),
                "iteration {iteration}: the raced session must be audited"
            );
        }
    }

    #[test]
    fn concurrent_duplicate_id_submissions_allow_exactly_one_winner() {
        // Red-team: N threads submit the same session id simultaneously while
        // that id is still live. Exactly one submission may win; every other
        // must be rejected as a duplicate, and the winner must remain
        // cancellable and reach exactly one terminal outcome.
        //
        // The winner uses a read-write grant so it parks for approval instead
        // of completing instantly: the id must stay reserved for the whole
        // flood, or a second submission could legitimately win once the first
        // session was already released.
        let broker = Arc::new(ActionBroker::new().expect("broker"));
        let id = 77;
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let broker = broker.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let (component, request) =
                        request(&broker, id, "hello", HELLO_WAT, write_grant(30, 1_000_000));
                    // Race the submissions together so every contender checks
                    // the live id while the eventual winner still holds it.
                    barrier.wait();
                    broker.submit(component, request)
                })
            })
            .collect();

        let mut winners = Vec::new();
        for thread in threads {
            match thread.join().expect("submitter joins") {
                Ok(receiver) => winners.push(receiver),
                Err(BrokerError::DuplicateSession(77)) => {}
                Err(other) => panic!("unexpected duplicate-id result: {other:?}"),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one submission must win");

        let receiver = winners.pop().expect("one winner");
        // The winner parks for approval (write grant), so the caller must
        // consume the park notification before the terminal outcome arrives.
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("winner parks for approval"),
            BrokerOutcome::PendingApproval { .. }
        ));
        broker
            .cancel(id)
            .expect("the winning session stays cancellable");
        let outcome = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("winner reports a terminal outcome");
        assert!(matches!(outcome, BrokerOutcome::Cancelled));
        assert!(
            receiver.recv_timeout(Duration::from_millis(200)).is_err(),
            "the winner must emit exactly one terminal outcome"
        );
        for _ in 0..50 {
            if broker.outstanding_sessions() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(broker.outstanding_sessions(), 0);
    }

    #[test]
    fn approve_of_non_pending_session_errors() {
        let broker = ActionBroker::new().expect("broker");
        assert!(matches!(
            broker.approve(999),
            Err(BrokerError::NotPendingApproval(999))
        ));
        assert!(matches!(
            broker.deny(999),
            Err(BrokerError::NotPendingApproval(999))
        ));
    }

    #[test]
    fn audit_trail_records_decisions() {
        let broker = ActionBroker::new().expect("broker");

        // Auto-approved read-only command.
        let (component, req) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        let receiver = broker.submit(component, req).expect("submitted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("outcome"),
            BrokerOutcome::Completed(_)
        ));

        // Human-approved write command.
        let (component, req) = request(&broker, 2, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let receiver = broker.submit(component, req).expect("submitted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval"),
            BrokerOutcome::PendingApproval { .. }
        ));
        broker.approve(2).expect("approval accepted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("outcome"),
            BrokerOutcome::Completed(_)
        ));

        // Human-denied write command.
        let (component, req) = request(&broker, 3, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let receiver = broker.submit(component, req).expect("submitted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval"),
            BrokerOutcome::PendingApproval { .. }
        ));
        broker.deny(3).expect("denial accepted");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("denied outcome"),
            BrokerOutcome::Denied(_)
        ));

        let trail = broker.audit_trail();
        assert_eq!(trail.len(), 3, "every session must be recorded");
        assert_eq!(trail[0].decision, ApprovalDecision::AutoApproved);
        assert_eq!(trail[1].decision, ApprovalDecision::Approved);
        assert_eq!(trail[2].decision, ApprovalDecision::Denied);
        assert_eq!(trail[2].outcome, AuditOutcome::Denied);
        assert_eq!(trail[0].program, "hello");
    }

    #[test]
    fn streaming_risky_command_emits_pending_approval() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) =
            request(&broker, 1, "hello", HELLO_WAT, write_grant(30, 1_000_000));
        let events = broker
            .submit_streaming(component, request)
            .expect("submitted");

        assert!(matches!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("pending approval event"),
            SessionEvent::PendingApproval {
                reason: ApprovalReason::FilesystemWrite
            }
        ));
        broker.approve(1).expect("approval accepted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("exited event"),
            SessionEvent::Exited { code: Some(0) }
        );
    }

    #[test]
    fn streaming_cancel_before_start_skips_a_queued_action() {
        let broker = ActionBroker::new().expect("broker");
        let (component_a, request_a) =
            request(&broker, 1, "spin-a", SPIN_WAT, grant(60, 4_000_000_000));
        let (component_b, request_b) =
            request(&broker, 2, "hello-b", HELLO_WAT, grant(30, 1_000_000));
        let events_a = broker
            .submit_streaming(component_a, request_a)
            .expect("a submitted");
        let events_b = broker
            .submit_streaming(component_b, request_b)
            .expect("b submitted");

        broker.cancel(2).expect("queued session is cancellable");
        broker.cancel(1).expect("running session is cancellable");

        // b never started: its first and only event is Cancelled.
        assert_eq!(
            events_b
                .recv_timeout(Duration::from_secs(5))
                .expect("b terminal event"),
            SessionEvent::Cancelled
        );
        assert!(
            events_b.recv_timeout(Duration::from_millis(200)).is_err(),
            "b must not emit any further events"
        );

        // a terminates cancelled, whether the cancel landed mid-flight or
        // before start; drain until the terminal event.
        for _ in 0..4 {
            let event = events_a
                .recv_timeout(Duration::from_secs(5))
                .expect("a event");
            if event == SessionEvent::Cancelled {
                return;
            }
        }
        panic!("a never reported cancellation");
    }

    #[cfg(unix)]
    #[test]
    fn native_submission_requires_approval_then_streams_output() {
        let broker = ActionBroker::new().expect("broker");
        let request = native_request(1, "/bin/echo", &["hello from native"]);
        let events = broker
            .submit_native_streaming(request)
            .expect("native request submitted");

        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event"),
            SessionEvent::PendingApproval {
                reason: ApprovalReason::NativeExecution
            }
        );
        broker.approve(1).expect("approval accepted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );

        let output = events
            .recv_timeout(Duration::from_secs(10))
            .expect("output or exit event");
        assert!(
            matches!(output, SessionEvent::Output { ref bytes, .. } if bytes.windows(5).any(|window| window == b"hello")),
            "native output must be streamed, got {output:?}"
        );
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("exit event"),
            SessionEvent::Exited { code: Some(0) }
        );
    }

    #[cfg(unix)]
    #[test]
    fn send_input_reaches_a_running_native_session() {
        let broker = ActionBroker::new().expect("broker");
        let request = native_request(1, "/bin/cat", &[]);
        let events = broker
            .submit_native_streaming(request)
            .expect("native request submitted");
        assert!(matches!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event"),
            SessionEvent::PendingApproval { .. }
        ));
        broker.approve(1).expect("approval accepted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );

        broker
            .send_input(1, b"hello through the pty\n".to_vec())
            .expect("input reaches native session");
        let mut saw_echo = false;
        for _ in 0..4 {
            if let SessionEvent::Output { bytes, .. } = events
                .recv_timeout(Duration::from_secs(5))
                .expect("native output")
            {
                if bytes.windows(5).any(|window| window == b"hello") {
                    saw_echo = true;
                    break;
                }
            }
        }
        assert!(saw_echo, "PTY input must be echoed by cat");
        broker.cancel(1).expect("cat is cancellable");
        for _ in 0..4 {
            if events
                .recv_timeout(Duration::from_secs(5))
                .expect("native terminal event")
                == SessionEvent::Cancelled
            {
                return;
            }
        }
        panic!("native session never reported cancellation");
    }

    #[test]
    fn send_input_to_a_wasi_session_is_rejected_as_not_native() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "spin", SPIN_WAT, grant(60, 4_000_000_000));
        let events = broker
            .submit_streaming(component, request)
            .expect("WASI request submitted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );
        assert!(matches!(
            broker.send_input(1, b"not native".to_vec()),
            Err(BrokerError::NotNative(1))
        ));
        broker.cancel(1).expect("WASI session is cancellable");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(5))
                .expect("cancel event"),
            SessionEvent::Cancelled
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_native_request_never_starts() {
        let broker = ActionBroker::new().expect("broker");
        let request = native_request(1, "/bin/false", &[]);
        let events = broker
            .submit_native_streaming(request)
            .expect("native request submitted");
        assert!(matches!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event"),
            SessionEvent::PendingApproval { .. }
        ));
        broker.deny(1).expect("denial accepted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(5))
                .expect("denied event"),
            SessionEvent::Denied
        );
        assert!(
            events.recv_timeout(Duration::from_millis(200)).is_err(),
            "denied native sessions must not start"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_cancel_reports_cancelled_and_releases_the_session() {
        let broker = ActionBroker::new().expect("broker");
        let request = native_request(1, "/bin/sleep", &["100"]);
        let events = broker
            .submit_native_streaming(request)
            .expect("native request submitted");
        assert!(matches!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event"),
            SessionEvent::PendingApproval { .. }
        ));
        broker.approve(1).expect("approval accepted");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("started event"),
            SessionEvent::Started
        );
        broker.cancel(1).expect("native session is cancellable");
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(5))
                .expect("cancel event"),
            SessionEvent::Cancelled
        );
        for _ in 0..50 {
            if broker.outstanding_sessions() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(broker.outstanding_sessions(), 0);
    }

    #[test]
    fn submit_native_rejects_a_wasi_request() {
        let broker = ActionBroker::new().expect("broker");
        let (component, request) = request(&broker, 1, "hello", HELLO_WAT, grant(30, 1_000_000));
        drop(component);
        assert!(matches!(
            broker.submit_native(request),
            Err(BrokerError::NotNativeRequest)
        ));
    }

    #[test]
    fn worker_panic_is_contained_and_the_queue_survives() {
        let broker = ActionBroker::new().expect("broker");
        broker.state.panic_next.store(1, Ordering::SeqCst);
        let (component_a, request_a) =
            request(&broker, 1, "spin-a", SPIN_WAT, grant(30, 1_000_000));
        let receiver_a = broker.submit(component_a, request_a).expect("a submitted");

        // a panics inside the worker; its receiver still gets a terminal event.
        let outcome = receiver_a
            .recv_timeout(Duration::from_secs(10))
            .expect("a terminal outcome");
        assert!(
            matches!(outcome, BrokerOutcome::Denied(_)),
            "panicked session must be reported denied, got {outcome:?}"
        );

        // The worker survived the panic: a new job still completes, and the
        // panicked session is released and recorded.
        let (component_b, request_b) =
            request(&broker, 2, "hello-b", HELLO_WAT, grant(30, 1_000_000));
        let receiver_b = broker.submit(component_b, request_b).expect("b submitted");
        let outcome_b = receiver_b
            .recv_timeout(Duration::from_secs(10))
            .expect("b terminal outcome");
        assert!(matches!(outcome_b, BrokerOutcome::Completed(_)));

        assert!(matches!(
            broker.cancel(1),
            Err(BrokerError::UnknownSession(1))
        ));
        let trail = broker.audit_trail();
        assert!(
            trail
                .iter()
                .any(|entry| entry.id == 1 && entry.outcome == AuditOutcome::Failed),
            "panicked session must be audited"
        );
    }

    #[test]
    fn audit_trail_keeps_at_most_max_entries() {
        let state = Arc::new(BrokerState {
            handles: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            inputs: Mutex::new(HashMap::new()),
            audit: Mutex::new(Vec::new()),
            approval_timeout: Duration::from_secs(30),
            panic_next: AtomicU64::new(0),
        });
        let total = (DEFAULT_MAX_AUDIT_ENTRIES + 5) as u64;
        for id in 0..total {
            state.record(AuditEntry {
                id,
                actor: Actor::Agent,
                program: "tool".to_owned(),
                decision: ApprovalDecision::AutoApproved,
                outcome: AuditOutcome::Completed { exit_code: 0 },
            });
        }
        let trail = state.audit.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(trail.len(), DEFAULT_MAX_AUDIT_ENTRIES);
        assert_eq!(trail[0].id, 5, "oldest entries are dropped first");
        assert_eq!(trail.last().expect("nonempty").id, total - 1);
    }
}
