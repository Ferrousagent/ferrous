//! Native session driver: runs one PTY session to completion while streaming
//! output, enforcing the output budget, watching cancellation and the
//! wall-clock deadline, and forwarding keystrokes into the child.
//!
//! Threads (all owned by [`NativeSessionHandle::run`]):
//!
//! - **reader thread** — drains the PTY, accumulates and forwards
//!   `SessionEvent::Output` chunks, and kills the child once the combined
//!   output budget is exceeded;
//! - **watchdog thread** — kills the child when the session is cancelled or
//!   the wall-clock deadline passes, recording the reason;
//! - **input thread** — receives bytes from the broker's `send_input` channel
//!   and writes them to the PTY master.
//!
//! On Unix the PTY child is a session leader, so killing it takes down its
//! whole process group (grandchildren included); on Windows the ConPTY
//! implementation owns the process tree. Every path through `run` joins all
//! three threads, so no task, fd, or process leaks.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::cancel::CancelHandle;
use crate::capability::ResourceLimits;
use crate::command::{SessionEvent, Stream};
use crate::native::{NativeError, NativeOutput, NativeSession, drain_reader};

/// How `run` ended, distinguishing a normal exit from a policy stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeStop {
    /// The child exited on its own.
    Exited,
    /// The session was cancelled before the child completed.
    Cancelled,
    /// The wall-clock deadline passed and the child was killed.
    TimedOut,
    /// The combined output budget was exceeded and the child was killed.
    OutputLimit,
}

/// One running native session and its control threads.
pub struct NativeSessionHandle {
    session: NativeSession,
    cancel: CancelHandle,
    limits: ResourceLimits,
    sink: Sender<SessionEvent>,
    input_tx: Sender<Vec<u8>>,
    input_rx: Receiver<Vec<u8>>,
}

impl NativeSessionHandle {
    /// Wrap a spawned session for a broker-managed run.
    pub fn new(
        session: NativeSession,
        cancel: CancelHandle,
        limits: ResourceLimits,
        sink: Sender<SessionEvent>,
    ) -> Self {
        let (input_tx, input_rx) = mpsc::channel();
        Self {
            session,
            cancel,
            limits,
            sink,
            input_tx,
            input_rx,
        }
    }

    /// A sender the broker stores so `send_input(id, bytes)` reaches this
    /// session's PTY writer.
    pub fn input_sender(&self) -> Sender<Vec<u8>> {
        self.input_tx.clone()
    }

    /// Run the session to completion. The caller emits `Started` first and a
    /// terminal `SessionEvent` after this returns, mirroring the WASI path.
    pub fn run(mut self) -> Result<NativeOutput, NativeError> {
        let budget = self.limits.max_output_bytes();
        let deadline = Instant::now() + Duration::from_secs(self.limits.timeout_seconds());

        // Split the session's handles before spawning threads so each thread
        // owns what it touches and the borrow checker can prove it.
        let reader = self.session.try_clone_reader()?;
        let writer = self.session.take_writer();
        let killer = self.session.child_killer();

        let stop = Arc::new(AtomicBool::new(false));
        let watchdog_stop = stop.clone();
        let reader_stop = stop.clone();

        let kill_reason = Arc::new(std::sync::Mutex::new(None::<NativeStop>));
        let watchdog_reason = kill_reason.clone();
        let reader_reason = kill_reason.clone();

        let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let reader_captured = captured.clone();

        // Watchdog: enforce cancellation + wall-clock deadline by killing the
        // child and recording WHY it was killed.
        let watchdog_cancel = self.cancel.clone();
        let mut watchdog_killer = killer.clone_killer();
        let watchdog = std::thread::spawn(move || {
            loop {
                if watchdog_stop.load(Ordering::SeqCst) {
                    return;
                }
                if watchdog_cancel.is_cancelled() {
                    let _ = watchdog_killer.kill();
                    *watchdog_reason
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(NativeStop::Cancelled);
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = watchdog_killer.kill();
                    *watchdog_reason
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(NativeStop::TimedOut);
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        // Reader: drain the PTY, accumulate output, stream events, and cut the
        // guest off once the combined budget is exceeded.
        let reader_sink = self.sink.clone();
        let reader = std::thread::spawn(move || {
            let mut exceeded = false;
            let _ = drain_reader(reader, &mut |chunk| {
                if reader_stop.load(Ordering::SeqCst) {
                    return;
                }
                {
                    let mut captured = reader_captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    captured.extend_from_slice(chunk);
                    let total = captured.len();
                    if !exceeded && total > budget {
                        exceeded = true;
                        let _ = killer.clone_killer().kill();
                        *reader_reason
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(NativeStop::OutputLimit);
                    }
                }
                let _ = reader_sink.send(SessionEvent::Output {
                    stream: Stream::Stdout,
                    bytes: chunk.to_vec(),
                });
            });
            let _ = exceeded;
        });

        // Input: forward keystrokes from the broker channel into the PTY.
        let input_rx = self.input_rx;
        let input = std::thread::spawn(move || {
            let mut writer = match writer {
                Some(writer) => writer,
                None => return,
            };
            while let Ok(bytes) = input_rx.recv() {
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
            }
        });

        // Main loop: poll the child until it exits (the watchdog or the
        // reader kills it on policy stops, which makes try_wait return).
        let exit_code;
        loop {
            if let Some(status) = self.session.try_exit_status()? {
                exit_code = status;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Teardown, in an order that cannot deadlock:
        // 1. Kill the child first (no-op if already exited) so the reader's
        //    blocked PTY read returns instead of hanging forever.
        // 2. Stop the watchdog so it cannot kill after a clean exit, join it.
        // 3. Drop the input sender and join the input thread.
        // 4. Join the reader and collect the captured output.
        let _ = self.session.kill();
        stop.store(true, Ordering::SeqCst);
        let _ = watchdog.join();
        drop(self.input_tx);
        let _ = input.join();
        let _ = reader.join();

        let output = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        match kill_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(NativeStop::Exited)
        {
            NativeStop::Exited => Ok(NativeOutput {
                stdout: output,
                stderr: Vec::new(),
                exit_code,
            }),
            NativeStop::Cancelled => Err(NativeError::Cancelled),
            NativeStop::TimedOut => Err(NativeError::Timeout),
            NativeStop::OutputLimit => Err(NativeError::OutputLimit),
        }
    }
}
