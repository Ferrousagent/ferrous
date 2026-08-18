//! Native terminal boundary: capability-gated PTY execution for approved
//! developer commands (`bash`, `cargo`, `npm`, `git`).
//!
//! Phase 1 contract: native execution requires an explicit capability grant
//! AND human approval (enforced by the broker). Unsupported hosts return
//! [`NativeError::UnsupportedOnHost`] — they never fall back to ambient
//! execution.
//!
//! Every child is spawned with **direct argv** ([`portable_pty::CommandBuilder`]),
//! never through a shell string, so metacharacters in arguments are inert
//! (risk register R34). Only grant-allowlisted environment variables reach the
//! child (R8), and the working directory must resolve inside the grant (R6).

use std::io::{Read, Write};
use std::path::Path;

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;

use crate::command::{CommandError, CommandRequest, ExecutionMode};
use crate::policy::selected_environment;

/// Fail-closed errors from the native backend boundary.
#[derive(Debug, Error)]
pub enum NativeError {
    /// The request selected a different backend.
    #[error("native backend received a non-native request")]
    WrongMode,
    /// Native execution was not granted.
    #[error("native execution was not granted")]
    NativeNotGranted,
    /// No tested platform sandbox adapter is available on this host.
    #[error("native execution is unsupported on this host")]
    UnsupportedOnHost,
    /// The request failed validation before any process could spawn.
    #[error("invalid native request: {0}")]
    InvalidRequest(#[from] CommandError),
    /// The child could not be spawned.
    #[error("failed to spawn native process: {0}")]
    SpawnFailed(String),
    /// A PTY or process I/O operation failed.
    #[error("native I/O failure: {0}")]
    Io(#[from] std::io::Error),
    /// The session was cancelled before the child completed.
    #[error("native session was cancelled")]
    Cancelled,
    /// The session exceeded its wall-clock timeout.
    #[error("native session exceeded its wall-clock timeout")]
    Timeout,
    /// The session exceeded its combined output budget.
    #[error("native session exceeded its output budget")]
    OutputLimit,
}

/// Captured output and exit status of one native command.
///
/// A PTY merges stdout and stderr, so `stderr` is conventionally empty for
/// native sessions; all captured bytes appear in `stdout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOutput {
    /// Captured standard output (includes merged stderr from the PTY).
    pub stdout: Vec<u8>,
    /// Captured standard error (empty for PTY sessions).
    pub stderr: Vec<u8>,
    /// Process exit code.
    pub exit_code: u32,
}

/// A running PTY session: master (resize/reader), writer (input), and child.
///
/// Owned by the session driver; the broker never touches the raw handles.
pub struct NativeSession {
    master: Box<dyn MasterPty + Send>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl NativeSession {
    /// Write raw bytes (keystrokes) to the PTY master.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<(), NativeError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| NativeError::SpawnFailed("PTY writer was already taken".into()))?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Take the writer out of the session, handing it to the input thread.
    /// Returns `None` if it was already taken.
    pub fn take_writer(&mut self) -> Option<Box<dyn Write + Send>> {
        self.writer.take()
    }

    /// Resize the PTY viewport.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), NativeError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| NativeError::SpawnFailed(error.to_string()))
    }

    /// A reader clone for the output-draining thread.
    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, NativeError> {
        self.master
            .try_clone_reader()
            .map_err(|error| NativeError::SpawnFailed(error.to_string()))
    }

    /// Poll whether the child has exited, returning its exit code.
    pub fn try_exit_status(&mut self) -> Result<Option<u32>, NativeError> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(status.exit_code())),
            None => Ok(None),
        }
    }

    /// Clone of the child killer for the watchdog/cancel thread.
    pub fn child_killer(&mut self) -> Box<dyn ChildKiller + Send + Sync> {
        self.child.clone_killer()
    }

    /// Kill the child (and, on Unix, its whole process group). No-op if the
    /// child already exited.
    pub fn kill(&mut self) -> Result<(), NativeError> {
        self.child.kill()?;
        Ok(())
    }

    /// The child's process id, when the platform exposes one.
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

/// The native execution backend.
///
/// Phase 1 supports hosts whose platform policy adapter is implemented and
/// tested; every other host fails closed with [`NativeError::UnsupportedOnHost`].
#[derive(Debug, Default)]
pub struct NativeBackend;

impl NativeBackend {
    /// Create the host-native backend boundary.
    pub const fn new() -> Self {
        Self
    }

    /// Whether this host can enforce the native execution policy.
    ///
    /// Unix (PTY + process-group semantics) is the Phase 1 adapter. Windows
    /// (ConPTY) and macOS policy adapters land in a later pass and must report
    /// unsupported until they are tested — never ambient fallback.
    #[cfg(unix)]
    pub const fn supported_on_host() -> bool {
        true
    }

    /// Non-unix hosts fail closed until their adapter is implemented.
    #[cfg(not(unix))]
    pub const fn supported_on_host() -> bool {
        false
    }

    /// Spawn one approved native request into a PTY session.
    ///
    /// Fails before spawning when: the mode is not native, native execution is
    /// not granted, the request is invalid, the host adapter is unsupported,
    /// or the working directory does not resolve inside the grant.
    pub fn spawn(&self, request: &CommandRequest) -> Result<NativeSession, NativeError> {
        self.spawn_with_env(request, &|name| std::env::var(name).ok())
    }

    /// [`Self::spawn`] with an injectable environment provider.
    ///
    /// The provider resolves host environment values; only grant-allowlisted
    /// names are ever queried and forwarded to the child. Injection keeps the
    /// allowlist filtering testable without mutating the test process
    /// environment (unsafe in edition 2024).
    pub fn spawn_with_env(
        &self,
        request: &CommandRequest,
        env_provider: &dyn Fn(&str) -> Option<String>,
    ) -> Result<NativeSession, NativeError> {
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
        // The cwd must exist and resolve inside the grant (symlink-aware).
        if !request.grant.allows_existing_path(&request.cwd) {
            return Err(CommandError::WorkingDirectoryDenied(request.cwd.clone()).into());
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| NativeError::SpawnFailed(error.to_string()))?;

        let mut builder = CommandBuilder::new(&request.program);
        builder.cwd(Path::new(&request.cwd));
        for argument in &request.args {
            builder.arg(argument);
        }
        // Only grant-allowlisted environment variables reach the child.
        for (name, value) in selected_environment(&request.grant, env_provider) {
            builder.env(name, value);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| NativeError::SpawnFailed(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| NativeError::SpawnFailed(error.to_string()))?;

        Ok(NativeSession {
            master: pair.master,
            writer: Some(writer),
            child,
        })
    }
}

/// Drain `reader` until EOF, forwarding chunks to `emit`.
///
/// Kept as a free function so the session driver's reader thread has a
/// testable pure helper; `emit` forwards chunks to `SessionEvent::Output`.
pub(crate) fn drain_reader(
    mut reader: Box<dyn Read + Send>,
    emit: &mut dyn FnMut(&[u8]),
) -> std::io::Result<usize> {
    let mut total = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count;
        emit(&buffer[..count]);
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::capability::{CapabilityGrant, FilesystemAccess};

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ferrous-native-{name}-{}", std::process::id()))
    }

    fn native_request(program: &str, args: &[&str], grant: CapabilityGrant) -> CommandRequest {
        let cwd = grant
            .filesystem_grants()
            .next()
            .expect("one filesystem grant")
            .root()
            .to_path_buf();
        CommandRequest::new(
            1,
            crate::command::Actor::Agent,
            ExecutionMode::Native,
            program,
            args.iter().copied(),
            cwd,
            grant,
        )
        .expect("valid request")
    }

    fn workspace_grant() -> CapabilityGrant {
        let root = test_root("workspace");
        let _ = std::fs::create_dir_all(&root);
        CapabilityGrant::workspace(&root, FilesystemAccess::ReadWrite)
            .expect("absolute root")
            .allow_native_execution()
    }

    #[test]
    fn spawn_rejects_non_native_requests() {
        let grant = workspace_grant();
        let cwd = grant
            .filesystem_grants()
            .next()
            .expect("one grant")
            .root()
            .to_path_buf();
        let request = CommandRequest::new(
            1,
            crate::command::Actor::Agent,
            ExecutionMode::Wasi,
            "echo",
            std::iter::empty::<&str>(),
            cwd,
            grant,
        )
        .expect("valid request");
        assert!(matches!(
            NativeBackend::new().spawn(&request),
            Err(NativeError::WrongMode)
        ));
    }

    #[test]
    fn spawn_requires_explicit_grant() {
        let root = test_root("no-native");
        let _ = std::fs::create_dir_all(&root);
        // Build the request directly: with a grant that lacks native
        // execution, request validation already fails closed.
        let grant = CapabilityGrant::workspace(&root, FilesystemAccess::Read).expect("absolute");
        let request = CommandRequest::new(
            1,
            crate::command::Actor::Agent,
            ExecutionMode::Native,
            "echo",
            ["hi"],
            &root,
            grant,
        )
        .expect_err("native without a grant must be denied at request construction");
        assert!(request.to_string().contains("native execution"));
    }

    #[cfg(unix)]
    #[test]
    fn spawn_denies_a_symlinked_cwd_that_escapes_the_grant() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink-cwd");
        let outside = test_root("symlink-cwd-outside");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).expect("root created");
        std::fs::create_dir_all(&outside).expect("outside created");
        symlink(&outside, root.join("escape")).expect("symlink created");

        let grant = CapabilityGrant::workspace(&root, FilesystemAccess::ReadWrite)
            .expect("absolute")
            .allow_native_execution();
        let request = CommandRequest::new(
            1,
            crate::command::Actor::Agent,
            ExecutionMode::Native,
            "echo",
            std::iter::empty::<&str>(),
            root.join("escape"),
            grant,
        )
        .expect("lexically valid request");

        assert!(matches!(
            NativeBackend::new().spawn(&request),
            Err(NativeError::InvalidRequest(
                CommandError::WorkingDirectoryDenied(_)
            ))
        ));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_uses_direct_argv_and_ignores_shell_metacharacters() {
        let grant = workspace_grant();
        let marker = test_root("pwned-marker");
        let marker = marker.join("pwned");
        let _ = std::fs::remove_file(&marker);

        // If any implementation routed these through a shell, the marker would
        // be created. With direct argv, /bin/echo prints the string literally.
        // (/bin/echo is a real executable — `echo` alone is a shell builtin
        // and cannot be exec'd without a shell.)
        let arg = format!("$(touch {})", marker.display());
        let request = native_request("/bin/echo", &[&arg], grant.clone());
        let mut session = NativeBackend::new().spawn(&request).expect("spawns");

        let mut output = Vec::new();
        let mut reader = session.try_clone_reader().expect("reader");
        let mut buffer = [0u8; 1024];
        loop {
            let count = reader.read(&mut buffer).expect("read");
            if count == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..count]);
        }
        let status = poll_exit(&mut session, 100).expect("exit status");
        assert_eq!(status, Some(0));

        assert!(
            std::str::from_utf8(&output)
                .expect("utf8")
                .contains("$(touch"),
            "metacharacters must be printed literally, output was {output:?}"
        );
        assert!(
            !marker.exists(),
            "shell metacharacters must never be executed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_denies_a_missing_program() {
        let grant = workspace_grant();
        let request = native_request("ferrous-definitely-missing-binary-xyz", &[], grant);
        assert!(matches!(
            NativeBackend::new().spawn(&request),
            Err(NativeError::SpawnFailed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn only_allowlisted_environment_reaches_the_child() {
        let grant = workspace_grant()
            .allow_environment("ALLOWED_VAR")
            .expect("valid name");
        let request = native_request("env", &[], grant);
        // The provider simulates a host environment: ALLOWED_VAR is granted,
        // LEAKY_VAR is present on the "host" but NOT in the grant.
        let provider = |name: &str| match name {
            "ALLOWED_VAR" => Some("allowed-value".to_owned()),
            "LEAKY_VAR" => Some("leaky-value".to_owned()),
            _ => None,
        };
        let session = NativeBackend::new()
            .spawn_with_env(&request, &provider)
            .expect("spawns");
        let mut output = Vec::new();
        let mut reader = session.try_clone_reader().expect("reader");
        let mut buffer = [0u8; 4096];
        loop {
            let count = reader.read(&mut buffer).expect("read");
            if count == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..count]);
        }
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("ALLOWED_VAR=allowed-value"));
        assert!(
            !text.contains("LEAKY_VAR"),
            "non-allowlisted env must never reach the child"
        );
    }

    /// Poll `try_exit_status` until the child is reaped or `deadline_ms`
    /// elapses. The PTY can report EOF a moment before the child is reaped.
    fn poll_exit(
        session: &mut NativeSession,
        deadline_ms: u64,
    ) -> Result<Option<u32>, NativeError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
        loop {
            if let Some(status) = session.try_exit_status()? {
                return Ok(Some(status));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn drain_reader_forwards_all_bytes_and_reports_total() {
        let (read, mut write) = std::io::pipe().expect("pipe");
        write.write_all(b"hello").expect("write");
        drop(write);
        let mut received = Vec::new();
        let total = drain_reader(Box::new(read), &mut |chunk| {
            received.extend_from_slice(chunk)
        })
        .expect("drain");
        assert_eq!(total, 5);
        assert_eq!(received, b"hello");
    }

    #[test]
    fn empty_grant_native_is_denied() {
        let grant = CapabilityGrant::empty();
        // cwd must be inside a grant for validate(); use a workspace-less path
        // by constructing through the normal path with an empty grant: the
        // request itself fails validation at cwd, which is also fail-closed.
        let request = CommandRequest::new(
            1,
            crate::command::Actor::Agent,
            ExecutionMode::Native,
            "echo",
            std::iter::empty::<&str>(),
            std::env::temp_dir(),
            grant,
        );
        assert!(
            request.is_err(),
            "an empty grant must fail closed at request validation"
        );
    }

    #[test]
    fn supported_on_host_matches_the_platform_adapter() {
        assert_eq!(
            NativeBackend::supported_on_host(),
            cfg!(unix),
            "only unix has a Phase 1 adapter; everything else fails closed"
        );
    }

    /// Guard: prove a cancelled flag can be observed without polling the child.
    #[test]
    fn atomic_cancel_flag_flips() {
        let flag = AtomicBool::new(false);
        flag.store(true, Ordering::SeqCst);
        assert!(flag.load(Ordering::SeqCst));
    }
}
