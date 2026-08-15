//! The action broker: serializes execution and propagates cancellation.
//!
//! A single worker thread executes at most one session at a time, so commands
//! targeting one terminal never interleave. Each submitted session owns a
//! [`CancelHandle`] registered under its session id; cancelling that id
//! interrupts the running guest (or skips a queued one) and reports
//! [`BrokerOutcome::Cancelled`] to the caller's result channel.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use thiserror::Error;
use wasmtime::component::Component;

use crate::cancel::CancelHandle;
use crate::command::{CommandError, CommandRequest, ExecutionMode, SessionEvent, SessionState};
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
    /// The broker only executes WASI requests.
    #[error("the broker currently executes WASI requests only")]
    NotWasi,
    /// The request failed validation.
    #[error("invalid request: {0}")]
    InvalidRequest(#[from] CommandError),
    /// The underlying runtime could not be created or configured.
    #[error("runtime failure: {0}")]
    Runtime(#[from] RuntimeError),
    /// The number of outstanding sessions would exceed the broker capacity.
    #[error("broker capacity exceeded; cancel or wait for a running session")]
    QueueFull,
    /// The broker capacity must be greater than zero.
    #[error("broker capacity must be greater than zero")]
    InvalidCapacity,
}

/// Final result of one broker-managed session.
#[derive(Debug)]
pub enum BrokerOutcome {
    /// The guest completed and its captured output is available.
    Completed(WasiOutput),
    /// The session was cancelled before the guest completed.
    Cancelled,
    /// The request was denied before it could start.
    Denied(CommandError),
    /// The backend failed; the guest may have been interrupted by a limit.
    Failed(RuntimeError),
}

/// Where a finished (or live-streamed) job reports its terminal state.
enum JobSink {
    /// Capturing mode: exactly one [`BrokerOutcome`] at the end.
    Outcome(mpsc::Sender<BrokerOutcome>),
    /// Streaming mode: live [`SessionEvent`]s in lifecycle order.
    Events(mpsc::Sender<SessionEvent>),
}

/// One queued or running execution.
struct Job {
    request: CommandRequest,
    component: Component,
    sink: JobSink,
    session: SessionState,
}

/// Serializes execution behind one queue and exposes per-session cancellation.
pub struct ActionBroker {
    engine: wasmtime::Engine,
    queue_tx: mpsc::Sender<Job>,
    handles: Arc<Mutex<HashMap<u64, CancelHandle>>>,
    capacity: usize,
    worker: Option<JoinHandle<()>>,
}

impl ActionBroker {
    /// Create a broker with a fresh runtime, one worker thread, and the default
    /// outstanding-session capacity.
    pub fn new() -> Result<Self, BrokerError> {
        Self::with_capacity(DEFAULT_MAX_OUTSTANDING_SESSIONS)
    }

    /// Create a broker with a custom outstanding-session capacity.
    pub fn with_capacity(capacity: usize) -> Result<Self, BrokerError> {
        if capacity == 0 {
            return Err(BrokerError::InvalidCapacity);
        }
        let runtime = WasiRuntime::new()?;
        let engine = runtime.engine().clone();
        let (queue_tx, queue_rx) = mpsc::channel::<Job>();
        let handles = Arc::new(Mutex::new(HashMap::new()));
        let worker_handles = handles.clone();
        let worker = std::thread::spawn(move || worker_loop(runtime, queue_rx, worker_handles));
        Ok(Self {
            engine,
            queue_tx,
            handles,
            capacity,
            worker: Some(worker),
        })
    }

    /// Number of sessions currently held by the broker (running plus queued).
    #[cfg(test)]
    fn outstanding_sessions(&self) -> usize {
        self.handles
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

    /// Enqueue an admitted component for execution.
    ///
    /// Returns a channel that receives exactly one [`BrokerOutcome`] when the
    /// session reaches a terminal state.    /// Enqueue an admitted component for capturing execution.
    ///
    /// Returns a channel that receives exactly one [`BrokerOutcome`] when the
    /// session reaches a terminal state.
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
            component,
            sink: JobSink::Outcome(result_tx),
        };
        self.enqueue(job)?;
        Ok(result_rx)
    }

    /// Enqueue an admitted component and stream its live session events.
    ///
    /// The receiver yields [`SessionEvent`]s in lifecycle order: `Started`,
    /// zero or more `Output` chunks as the guest produces them, then a
    /// terminal event (`Exited`, `Cancelled`, `Denied`, or `Unsupported`).
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
            component,
            sink: JobSink::Events(event_tx),
        };
        self.enqueue(job)?;
        Ok(event_rx)
    }

    /// Register, queue, and bound-check one job.
    fn enqueue(&self, job: Job) -> Result<(), BrokerError> {
        let id = job.request.id;
        let mut handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        if handles.len() >= self.capacity {
            return Err(BrokerError::QueueFull);
        }
        let handle = CancelHandle::new();
        handles.insert(id, handle);
        drop(handles);
        if self.queue_tx.send(job).is_err() {
            // The worker is gone; do not leave a phantom session registered.
            self.handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Err(BrokerError::WorkerStopped);
        }
        Ok(())
    }

    /// Request cancellation of a live session.
    ///
    /// Safe to call more than once; ids that are no longer live (never submitted
    /// or already finished and released) are rejected with
    /// [`BrokerError::UnknownSession`].
    pub fn cancel(&self, id: u64) -> Result<(), BrokerError> {
        let handle = self
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
            .ok_or(BrokerError::UnknownSession(id))?;
        handle.cancel();
        Ok(())
    }
}

impl Drop for ActionBroker {
    fn drop(&mut self) {
        // Interrupt any live session so the worker is not stuck in a long run.
        if let Ok(handles) = self.handles.lock() {
            for handle in handles.values() {
                handle.cancel();
            }
        }
        // Close the queue first: dropping the last sender makes the worker's
        // blocking recv() return, so joining below cannot deadlock.
        let _ = std::mem::replace(&mut self.queue_tx, mpsc::channel::<Job>().0);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(
    runtime: WasiRuntime,
    queue_rx: mpsc::Receiver<Job>,
    handles: Arc<Mutex<HashMap<u64, CancelHandle>>>,
) {
    while let Ok(mut job) = queue_rx.recv() {
        let cancel = handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&job.request.id)
            .cloned();
        match cancel {
            Some(cancel) => execute(&runtime, &mut job, &cancel),
            None => match &job.sink {
                JobSink::Outcome(result_tx) => {
                    let _ = result_tx.send(BrokerOutcome::Denied(CommandError::InvalidTransition(
                        "session handle disappeared before start",
                    )));
                }
                JobSink::Events(event_tx) => {
                    let _ = event_tx.send(SessionEvent::Denied);
                }
            },
        }
        // Release the session now that it reached a terminal state so the map
        // does not grow without bound across a long-lived broker.
        handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&job.request.id);
    }
}

/// Dispatch a job to its capturing or streaming execution path.
fn execute(runtime: &WasiRuntime, job: &mut Job, cancel: &CancelHandle) {
    let streams = matches!(&job.sink, JobSink::Events(_));
    if streams {
        execute_streaming(runtime, job, cancel);
    } else {
        let outcome = execute_capturing(runtime, job, cancel);
        if let JobSink::Outcome(result_tx) = &job.sink {
            let _ = result_tx.send(outcome);
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
    match runtime.run_wasi_cancellable(&job.component, &job.request, cancel) {
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
/// enforced structurally by the bounded pipes inside the runtime.
fn execute_streaming(runtime: &WasiRuntime, job: &mut Job, cancel: &CancelHandle) {
    let events = match &job.sink {
        JobSink::Events(event_tx) => event_tx,
        JobSink::Outcome(_) => return,
    };
    if cancel.is_cancelled() {
        let _ = job.session.accept(SessionEvent::Cancelled);
        let _ = events.send(SessionEvent::Cancelled);
        return;
    }
    if job.session.accept(SessionEvent::Started).is_err() {
        let _ = events.send(SessionEvent::Denied);
        return;
    }
    let _ = events.send(SessionEvent::Started);
    match runtime.run_wasi_events(&job.component, &job.request, cancel, events) {
        Ok(output) => {
            let _ = job.session.accept(SessionEvent::Exited {
                code: Some(output.exit_code),
            });
            let _ = events.send(SessionEvent::Exited {
                code: Some(output.exit_code),
            });
        }
        Err(RuntimeError::Cancelled) => {
            let _ = job.session.accept(SessionEvent::Cancelled);
            let _ = events.send(SessionEvent::Cancelled);
        }
        Err(RuntimeError::WrongMode) => {
            let _ = events.send(SessionEvent::Denied);
        }
        Err(_) => {
            let _ = job.session.accept(SessionEvent::Unsupported);
            let _ = events.send(SessionEvent::Unsupported);
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

    fn grant(timeout_seconds: u64, fuel: u64) -> CapabilityGrant {
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
}
