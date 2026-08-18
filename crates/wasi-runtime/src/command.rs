//! Version-neutral command and terminal-session contracts.

use std::path::PathBuf;

use thiserror::Error;

use crate::capability::{CapabilityGrant, ResourceLimits};

/// The principal requesting an execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// The human deliberately using Ferrous.
    Human,
    /// The primary Ferrous agent.
    Agent,
    /// A constrained specialist agent.
    Subagent,
    /// A versioned WASM skill.
    Skill,
}

/// The execution backend selected by policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Run a capability-scoped WASI component.
    Wasi,
    /// Run a native process through a platform policy adapter.
    Native,
}

/// A command request before a backend starts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRequest {
    /// Stable session identifier supplied by the caller.
    pub id: u64,
    /// Requesting principal.
    pub actor: Actor,
    /// Backend selected by the caller and checked by policy.
    pub mode: ExecutionMode,
    /// Executable/component name.
    pub program: String,
    /// Structured arguments; never a shell command string.
    pub args: Vec<String>,
    /// Absolute, capability-scoped working directory.
    pub cwd: PathBuf,
    /// Authority and limits for this request.
    pub grant: CapabilityGrant,
}

impl CommandRequest {
    /// Construct a request while preserving each argument as a separate value.
    pub fn new<I, S, P, C>(
        id: u64,
        actor: Actor,
        mode: ExecutionMode,
        program: P,
        args: I,
        cwd: C,
        grant: CapabilityGrant,
    ) -> Result<Self, CommandError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        P: Into<String>,
        C: Into<PathBuf>,
    {
        let request = Self {
            id,
            actor,
            mode,
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            grant,
        };
        request.validate()?;
        Ok(request)
    }

    /// Validate the request before any backend or host side effect begins.
    pub fn validate(&self) -> Result<(), CommandError> {
        if self.program.is_empty() || contains_nul(&self.program) {
            return Err(CommandError::InvalidProgram);
        }
        if self.args.iter().any(|argument| contains_nul(argument)) {
            return Err(CommandError::InvalidArgument);
        }
        if self.mode == ExecutionMode::Native && !self.grant.allows_native_execution() {
            return Err(CommandError::NativeNotGranted);
        }
        if !self.grant.allows_path(&self.cwd) {
            return Err(CommandError::WorkingDirectoryDenied(self.cwd.clone()));
        }
        Ok(())
    }
}

fn contains_nul(value: &str) -> bool {
    value.chars().any(|character| character == '\0')
}

/// Errors that prevent a command from starting.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CommandError {
    /// The executable name was empty or contained a NUL byte.
    #[error("invalid program")]
    InvalidProgram,
    /// An argument contained a NUL byte.
    #[error("invalid command argument")]
    InvalidArgument,
    /// Native execution was not granted.
    #[error("native execution requires an explicit capability grant")]
    NativeNotGranted,
    /// The working directory was outside the grant.
    #[error("working directory denied: {0}")]
    WorkingDirectoryDenied(PathBuf),
    /// The lifecycle event was not valid for the current state.
    #[error("invalid terminal lifecycle transition: {0}")]
    InvalidTransition(&'static str),
    /// The output budget would be exceeded.
    #[error("output limit exceeded")]
    OutputLimit,
}

/// Why a session needs human approval before it can start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReason {
    /// The command may write below a granted filesystem root.
    FilesystemWrite,
    /// The command may open loopback TCP sockets.
    NetworkAccess,
    /// The command reads environment variables.
    EnvironmentAccess,
    /// The command requested native process execution.
    NativeExecution,
}

/// Which output stream produced a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Events emitted by WASI and native terminal backends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// Backend accepted and started the request.
    Started,
    /// The session is parked waiting for a human approval decision.
    PendingApproval {
        /// Why this session needs approval.
        reason: ApprovalReason,
    },
    /// A bounded output chunk.
    Output {
        /// Output stream.
        stream: Stream,
        /// Raw bytes; rendering/sanitization belongs at the UI boundary.
        bytes: Vec<u8>,
    },
    /// Process/component exited normally or with an exit code.
    Exited {
        /// Exit code, if the backend supplied one.
        code: Option<i32>,
    },
    /// Session was cancelled by policy or the operator.
    Cancelled,
    /// Request was denied before starting.
    Denied,
    /// Requested backend is unavailable on this host.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Created,
    WaitingApproval,
    Running,
    Finished,
}

/// Validates event ordering and bounds terminal output for one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    id: u64,
    limits: ResourceLimits,
    output_bytes: usize,
    lifecycle: Lifecycle,
}

impl SessionState {
    /// Create a new session state machine.
    pub const fn new(id: u64, limits: ResourceLimits) -> Self {
        Self {
            id,
            limits,
            output_bytes: 0,
            lifecycle: Lifecycle::Created,
        }
    }

    /// Return the session identifier.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Return the accepted output byte count.
    pub const fn output_bytes(&self) -> usize {
        self.output_bytes
    }

    /// Accept one backend event or reject it without changing state.
    pub fn accept(&mut self, event: SessionEvent) -> Result<(), CommandError> {
        match (&self.lifecycle, event) {
            (Lifecycle::Created, SessionEvent::Started) => {
                self.lifecycle = Lifecycle::Running;
                Ok(())
            }
            (Lifecycle::Created, SessionEvent::PendingApproval { .. }) => {
                self.lifecycle = Lifecycle::WaitingApproval;
                Ok(())
            }
            (Lifecycle::WaitingApproval, SessionEvent::Started) => {
                self.lifecycle = Lifecycle::Running;
                Ok(())
            }
            (Lifecycle::WaitingApproval, SessionEvent::Denied | SessionEvent::Unsupported) => {
                self.lifecycle = Lifecycle::Finished;
                Ok(())
            }
            (Lifecycle::Running, SessionEvent::Output { bytes, .. }) => {
                let next = self
                    .output_bytes
                    .checked_add(bytes.len())
                    .ok_or(CommandError::OutputLimit)?;
                if next > self.limits.max_output_bytes() {
                    return Err(CommandError::OutputLimit);
                }
                self.output_bytes = next;
                Ok(())
            }
            (Lifecycle::Running, SessionEvent::Exited { .. } | SessionEvent::Cancelled) => {
                self.lifecycle = Lifecycle::Finished;
                Ok(())
            }
            (Lifecycle::Created, SessionEvent::Denied | SessionEvent::Unsupported) => {
                self.lifecycle = Lifecycle::Finished;
                Ok(())
            }
            (Lifecycle::Created, _) => {
                Err(CommandError::InvalidTransition("session has not started"))
            }
            (Lifecycle::WaitingApproval, _) => Err(CommandError::InvalidTransition(
                "session is awaiting approval",
            )),
            (Lifecycle::Running, SessionEvent::Started) => {
                Err(CommandError::InvalidTransition("session already started"))
            }
            (Lifecycle::Running, SessionEvent::PendingApproval { .. }) => Err(
                CommandError::InvalidTransition("running session cannot await approval"),
            ),
            (Lifecycle::Running, SessionEvent::Denied | SessionEvent::Unsupported) => Err(
                CommandError::InvalidTransition("running session cannot be denied"),
            ),
            (Lifecycle::Finished, _) => Err(CommandError::InvalidTransition("session is finished")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn absolute_test_workspace() -> std::path::PathBuf {
        // Rooted in the platform temp directory so the path is absolute on every
        // OS; a bare `/workspace` is not absolute on Windows.
        std::env::temp_dir().join("ferrous-test-workspace")
    }

    /// Deterministic xorshift64 so the fuzz sequence is reproducible in CI
    /// without adding a property-testing dependency.
    fn xorshift64(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn fuzzed_event_sequences_never_break_state_machine_invariants() {
        // Red-team: feed a deterministic PRNG stream of arbitrary events into
        // the session state machine. No input may panic it, and every accepted
        // event must keep the invariants: output bytes never exceed the budget,
        // terminal states reject everything, and the session cannot output
        // before starting or after finishing.
        let limits = ResourceLimits::new(1024, 30).expect("valid limits");
        let mut state = SessionState::new(0xDEAD_BEEF, limits);
        let mut rng = 0x9E3779B97F4A7C15u64;

        let events = [
            SessionEvent::Started,
            SessionEvent::PendingApproval {
                reason: ApprovalReason::NativeExecution,
            },
            SessionEvent::Output {
                stream: Stream::Stdout,
                bytes: vec![0x41; 128],
            },
            SessionEvent::Output {
                stream: Stream::Stderr,
                bytes: vec![0x42; 512],
            },
            SessionEvent::Exited { code: Some(0) },
            SessionEvent::Cancelled,
            SessionEvent::Denied,
            SessionEvent::Unsupported,
        ];

        for _ in 0..20_000 {
            let index = (xorshift64(&mut rng) as usize) % events.len();
            let event = events[index].clone();
            // Event construction is a no-op; `accept` must never panic and
            // must keep the output accounting honest on every path.
            let _ = state.accept(event);
            assert!(
                state.output_bytes() <= limits.max_output_bytes(),
                "output accounting exceeded the declared budget"
            );
        }

        // After a terminal event the machine must be closed to everything.
        let mut finished = SessionState::new(1, limits);
        finished.accept(SessionEvent::Started).expect("starts");
        finished
            .accept(SessionEvent::Exited { code: Some(0) })
            .expect("exits");
        for event in events.iter().cloned() {
            assert!(
                finished.accept(event).is_err(),
                "a finished session must reject every event, including {event:?}"
            );
        }

        // Output before start and after a denial is refused.
        let mut early = SessionState::new(2, limits);
        assert!(early
            .accept(SessionEvent::Output {
                stream: Stream::Stdout,
                bytes: b"x".to_vec(),
            })
            .is_err());
        early.accept(SessionEvent::Denied).expect("denied");
        assert!(early
            .accept(SessionEvent::Output {
                stream: Stream::Stdout,
                bytes: b"x".to_vec(),
            })
            .is_err());
    }

    #[test]
    fn native_request_with_workspace_and_grant_is_valid() {
        let workspace = absolute_test_workspace();
        let grant =
            CapabilityGrant::workspace(&workspace, crate::capability::FilesystemAccess::Read)
                .expect("workspace is absolute")
                .allow_native_execution();
        let request = CommandRequest::new(
            1,
            Actor::Human,
            ExecutionMode::Native,
            "cargo",
            ["test"],
            &workspace,
            grant,
        );
        assert!(request.is_ok());
    }
}
