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

/// Errors produced while interacting with the broker.
#[derive(Debug, Error)]
pub enum BrokerError {
    /// The session id was never submitted to this broker.
    #[error("no session with id {0} is known to the broker")]
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

/// One queued or running execution.
struct Job {
    request: CommandRequest,
    component: Component,
    result_tx: mpsc::Sender<BrokerOutcome>,
    session: SessionState,
}

/// Serializes execution behind one queue and exposes per-session cancellation.
pub struct ActionBroker {
    engine: wasmtime::Engine,
    queue_tx: mpsc::Sender<Job>,
    handles: Arc<Mutex<HashMap<u64, CancelHandle>>>,
    worker: Option<JoinHandle<()>>,
}

impl ActionBroker {
    /// Create a broker with a fresh runtime and one worker thread.
    pub fn new() -> Result<Self, BrokerError> {
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
            worker: Some(worker),
        })
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
        let handle = CancelHandle::new();
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(request.id, handle);
        let (result_tx, result_rx) = mpsc::channel();
        let job = Job {
            session: SessionState::new(request.id, request.grant.limits()),
            request,
            component,
            result_tx,
        };
        self.queue_tx
            .send(job)
            .map_err(|_| BrokerError::WorkerStopped)?;
        Ok(result_rx)
    }

    /// Request cancellation of a live session.
    ///
    /// Safe to call more than once and for sessions that already finished;
    /// only ids that were never submitted are rejected.
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
        let outcome = match cancel {
            Some(cancel) => execute(&runtime, &mut job, &cancel),
            None => BrokerOutcome::Denied(CommandError::InvalidTransition(
                "session handle disappeared before start",
            )),
        };
        let _ = job.result_tx.send(outcome);
    }
}

fn execute(runtime: &WasiRuntime, job: &mut Job, cancel: &CancelHandle) -> BrokerOutcome {
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
}
