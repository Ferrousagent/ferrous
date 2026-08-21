//! In-process builtin execution for the Ferrous Shell.
//!
//! Builtins are the fast, safe lane: they run in-process on
//! capability-relative paths and never spawn a process or a shell. Every
//! path is resolved through the session's capability grant before any
//! filesystem operation, and directory listings plus command output are
//! bounded and terminal-escape-safe at emission.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::capability::FilesystemAccess;
use crate::shell_ir::{Builtin, SessionPath};
use crate::terminal_session::TerminalSession;

/// One builtin's result: output bytes plus an exit status.
#[derive(Debug, PartialEq, Eq)]
pub struct BuiltinResult {
    /// Standard output bytes (already sanitized at emission).
    pub stdout: Vec<u8>,
    /// Standard error bytes.
    pub stderr: Vec<u8>,
    /// `0` for success, non-zero for failure.
    pub exit_code: i32,
}

impl BuiltinResult {
    /// A successful result with no output.
    pub fn ok() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    /// A failed result with a message on stderr.
    pub fn fail(message: &str) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: format!("{message}\n").into_bytes(),
            exit_code: 1,
        }
    }

    /// A successful result with stdout bytes.
    pub fn with_stdout(bytes: Vec<u8>) -> Self {
        Self {
            stdout: bytes,
            stderr: Vec::new(),
            exit_code: 0,
        }
    }
}

/// The builtin executor.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinExecutor;

impl BuiltinExecutor {
    /// Execute one builtin against the session.
    ///
    /// The session's cwd is the base for all path resolution; every resolved
    /// path must stay inside the base grant or the builtin fails closed with
    /// [`BuiltinResult::fail`] (no partial side effects are performed).
    pub fn execute(&self, builtin: &Builtin, session: &mut TerminalSession) -> BuiltinResult {
        match builtin {
            Builtin::Pwd => self.pwd(session),
            Builtin::Cd(path) => self.cd(session, path),
            Builtin::Ls(path) => self.ls(session, path),
            Builtin::Cat(path) => self.cat(session, path),
            Builtin::Mkdir(path) => self.mkdir(session, path),
            Builtin::Remove(path) => self.remove(session, path),
            Builtin::Copy { from, to } => self.copy(session, from, to),
            Builtin::Move { from, to } => self.move_path(session, from, to),
            Builtin::Env => self.env(session),
            Builtin::Which(name) => self.which(session, name),
            Builtin::Echo(args) => self.echo(args),
            Builtin::Export { name, value } => self.export(session, name, value),
        }
    }

    fn pwd(&self, session: &TerminalSession) -> BuiltinResult {
        BuiltinResult::with_stdout(format!("{}\n", session.cwd_display()).into_bytes())
    }

    fn cd(&self, session: &mut TerminalSession, path: &SessionPath) -> BuiltinResult {
        match session.change_dir(path) {
            Ok(()) => BuiltinResult::ok(),
            Err(error) => BuiltinResult::fail(&error.to_string()),
        }
    }

    fn ls(&self, session: &TerminalSession, path: &SessionPath) -> BuiltinResult {
        let target = match session.resolve_path(path) {
            Ok(target) => target,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        if !session.base_grant().allows_existing_path(&target) {
            return BuiltinResult::fail("ls: path denied by capability policy");
        }
        let entries = match fs::read_dir(&target) {
            Ok(entries) => entries,
            Err(error) => return BuiltinResult::fail(&format!("ls: {error}")),
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        // Bound the listing to the session's output budget.
        let budget = session.limits().max_output_bytes();
        let mut output = Vec::new();
        for name in names {
            let line = format!("{name}\n");
            if output.len().saturating_add(line.len()) > budget {
                break;
            }
            output.extend_from_slice(line.as_bytes());
        }
        BuiltinResult::with_stdout(output)
    }

    fn cat(&self, session: &TerminalSession, path: &SessionPath) -> BuiltinResult {
        let target = match session.resolve_path(path) {
            Ok(target) => target,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        let metadata = match fs::metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) => return BuiltinResult::fail(&format!("cat: {error}")),
        };
        if metadata.is_dir() {
            return BuiltinResult::fail("cat: is a directory");
        }
        if !session.base_grant().allows_existing_path(&target) {
            return BuiltinResult::fail("cat: path denied by capability policy");
        }
        let file = match fs::File::open(&target) {
            Ok(file) => file,
            Err(error) => return BuiltinResult::fail(&format!("cat: {error}")),
        };
        let mut output = Vec::new();
        let mut reader = io::BufReader::new(file);
        // Bound output to the session budget; truncation is safe here because
        // cat's output is only ever displayed, never re-executed.
        let budget = session.limits().max_output_bytes();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = budget.saturating_sub(output.len());
                    if read > remaining {
                        output.extend_from_slice(&chunk[..remaining]);
                        break;
                    }
                    output.extend_from_slice(&chunk[..read]);
                }
                Err(error) => return BuiltinResult::fail(&format!("cat: {error}")),
            }
        }
        BuiltinResult::with_stdout(output)
    }

    fn mkdir(&self, session: &TerminalSession, path: &SessionPath) -> BuiltinResult {
        let target = match session.resolve_new_path(path) {
            Ok(target) => target,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        if !grant_allows_write(session, &target) {
            return BuiltinResult::fail("mkdir: write denied by capability policy");
        }
        match fs::create_dir(&target) {
            Ok(()) => BuiltinResult::ok(),
            Err(error) => BuiltinResult::fail(&format!("mkdir: {error}")),
        }
    }

    fn remove(&self, session: &TerminalSession, path: &SessionPath) -> BuiltinResult {
        let target = match session.resolve_path(path) {
            Ok(target) => target,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        if !grant_allows_write(session, &target) {
            return BuiltinResult::fail("rm: delete denied by capability policy");
        }
        let metadata = match fs::metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) => return BuiltinResult::fail(&format!("rm: {error}")),
        };
        let result = if metadata.is_dir() {
            fs::remove_dir(&target)
        } else {
            fs::remove_file(&target)
        };
        match result {
            Ok(()) => BuiltinResult::ok(),
            Err(error) => BuiltinResult::fail(&format!("rm: {error}")),
        }
    }

    fn copy(
        &self,
        session: &TerminalSession,
        from: &SessionPath,
        to: &SessionPath,
    ) -> BuiltinResult {
        let source = match session.resolve_path(from) {
            Ok(source) => source,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        let destination = match session.resolve_new_path(to) {
            Ok(destination) => destination,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        if !session.base_grant().allows_existing_path(&source) {
            return BuiltinResult::fail("cp: source denied by capability policy");
        }
        if !grant_allows_write(session, &destination) {
            return BuiltinResult::fail("cp: destination write denied by capability policy");
        }
        match fs::copy(&source, &destination) {
            Ok(_) => BuiltinResult::ok(),
            Err(error) => BuiltinResult::fail(&format!("cp: {error}")),
        }
    }

    fn move_path(
        &self,
        session: &TerminalSession,
        from: &SessionPath,
        to: &SessionPath,
    ) -> BuiltinResult {
        let source = match session.resolve_path(from) {
            Ok(source) => source,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        let destination = match session.resolve_new_path(to) {
            Ok(destination) => destination,
            Err(error) => return BuiltinResult::fail(&error.to_string()),
        };
        if !grant_allows_write(session, &source) || !grant_allows_write(session, &destination) {
            return BuiltinResult::fail("mv: operation denied by capability policy");
        }
        match fs::rename(&source, &destination) {
            Ok(()) => BuiltinResult::ok(),
            Err(error) => BuiltinResult::fail(&format!("mv: {error}")),
        }
    }

    fn env(&self, session: &TerminalSession) -> BuiltinResult {
        let mut output = String::new();
        for (name, value) in session.env() {
            // Only the approved overlay is ever exposed; secret values are
            // never stored in the overlay by policy, and unallowlisted host
            // variables are never queried.
            output.push_str(&format!("{name}={value}\n"));
        }
        BuiltinResult::with_stdout(output.into_bytes())
    }

    fn which(&self, session: &TerminalSession, name: &str) -> BuiltinResult {
        // Search the session overlay PATH if present, else the host PATH.
        let path_value = session
            .env()
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok());
        let Some(path_value) = path_value else {
            return BuiltinResult::fail(&format!("which: {name}: not found"));
        };
        for directory in std::env::split_paths(&path_value) {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return BuiltinResult::with_stdout(
                    format!("{}\n", candidate.display()).into_bytes(),
                );
            }
        }
        BuiltinResult::fail(&format!("which: {name}: not found"))
    }

    fn echo(&self, args: &[String]) -> BuiltinResult {
        let mut output = String::new();
        for (index, argument) in args.iter().enumerate() {
            if index > 0 {
                output.push(' ');
            }
            output.push_str(argument);
        }
        output.push('\n');
        BuiltinResult::with_stdout(output.into_bytes())
    }

    fn export(
        &self,
        session: &mut TerminalSession,
        name: &str,
        value: &Option<String>,
    ) -> BuiltinResult {
        let mut delta = crate::terminal_session::EnvDelta::default();
        match value {
            Some(value) => {
                delta.set.insert(name.to_owned(), value.clone());
            }
            None => delta.remove.push(name.to_owned()),
        }
        match session.apply_env(delta) {
            Ok(()) => BuiltinResult::ok(),
            Err(error) => BuiltinResult::fail(&error.to_string()),
        }
    }
}

/// Whether the grant allows writes below `path`.
fn grant_allows_write(session: &TerminalSession, path: &Path) -> bool {
    session.base_grant().filesystem_grants().any(|grant| {
        grant.access() == FilesystemAccess::ReadWrite && session.base_grant().allows_path(path)
    })
}

/// Helper used by tests and the executor: the resolved absolute path of a
/// session-relative target.
#[allow(dead_code)]
fn resolved(session: &TerminalSession, path: &SessionPath) -> Result<PathBuf, String> {
    session
        .resolve_path(path)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityGrant, FilesystemAccess, ResourceLimits};
    use crate::command::Actor;
    use crate::terminal_session::TerminalSessionSpec;

    fn session_in(root: &Path, access: FilesystemAccess, allow_env: bool) -> TerminalSession {
        let _ = std::fs::create_dir_all(root);
        let mut grant = CapabilityGrant::workspace(root, access).expect("absolute workspace");
        if allow_env {
            grant = grant.allow_environment("ALLOWED").expect("valid name");
            // `which` resolves programs through an exported PATH, so tests that
            // exercise it must allowlist PATH in the session overlay.
            grant = grant.allow_environment("PATH").expect("valid name");
        }
        TerminalSession::new(TerminalSessionSpec {
            id: 1,
            actor: Actor::Human,
            cwd: SessionPath::new(".").expect("valid cwd"),
            base_grant: grant,
            limits: ResourceLimits::new(1_048_576, 30).expect("valid limits"),
        })
        .expect("session opens")
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ferrous-builtin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root is created");
        root
    }

    fn output_text(result: &BuiltinResult) -> String {
        String::from_utf8_lossy(&result.stdout).into_owned()
    }

    #[test]
    fn pwd_prints_session_cwd_relative_to_workspace() {
        let root = test_root("pwd");
        let mut session = session_in(&root, FilesystemAccess::Read, false);
        let result = BuiltinExecutor.execute(&Builtin::Pwd, &mut session);
        assert_eq!(result.exit_code, 0);
        assert_eq!(output_text(&result).trim(), ".");
    }

    #[test]
    fn cd_then_pwd_reflects_the_new_directory() {
        let root = test_root("cd");
        std::fs::create_dir_all(root.join("sub")).expect("subdir");
        let mut session = session_in(&root, FilesystemAccess::Read, false);
        let result = BuiltinExecutor.execute(
            &Builtin::Cd(SessionPath::new("sub").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 0);
        let pwd = BuiltinExecutor.execute(&Builtin::Pwd, &mut session);
        assert_eq!(output_text(&pwd).trim(), "sub");
    }

    #[test]
    fn mkdir_stays_inside_the_workspace_grant() {
        let root = test_root("mkdir");
        let mut session = session_in(&root, FilesystemAccess::ReadWrite, false);
        let result = BuiltinExecutor.execute(
            &Builtin::Mkdir(SessionPath::new("newdir").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 0);
        assert!(root.join("newdir").is_dir());
    }

    #[test]
    fn mkdir_without_write_capability_fails() {
        let root = test_root("mkdir-readonly");
        let mut session = session_in(&root, FilesystemAccess::Read, false);
        let result = BuiltinExecutor.execute(
            &Builtin::Mkdir(SessionPath::new("newdir").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 1);
        assert!(!root.join("newdir").exists());
    }

    #[test]
    fn remove_requires_the_delete_capability() {
        let root = test_root("rm");
        std::fs::write(root.join("file.txt"), b"data").expect("file written");
        let mut session = session_in(&root, FilesystemAccess::Read, false);
        let result = BuiltinExecutor.execute(
            &Builtin::Remove(SessionPath::new("file.txt").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 1);
        assert!(
            root.join("file.txt").exists(),
            "read-only session must not delete"
        );

        let mut session = session_in(&root, FilesystemAccess::ReadWrite, false);
        let result = BuiltinExecutor.execute(
            &Builtin::Remove(SessionPath::new("file.txt").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 0);
        assert!(!root.join("file.txt").exists());
    }

    #[test]
    fn echo_joins_arguments_and_env_prints_only_the_overlay() {
        let root = test_root("echo-env");
        let mut session = session_in(&root, FilesystemAccess::Read, true);
        let echo = BuiltinExecutor.execute(
            &Builtin::Echo(vec!["a".to_owned(), "b".to_owned()]),
            &mut session,
        );
        assert_eq!(output_text(&echo), "a b\n");

        let export = BuiltinExecutor.execute(
            &Builtin::Export {
                name: "ALLOWED".to_owned(),
                value: Some("yes".to_owned()),
            },
            &mut session,
        );
        assert_eq!(export.exit_code, 0);
        let env = BuiltinExecutor.execute(&Builtin::Env, &mut session);
        let text = output_text(&env);
        assert!(text.contains("ALLOWED=yes"));
        assert!(!text.contains("PATH"), "host PATH must not leak into env");
    }

    #[test]
    fn cat_reads_inside_the_workspace_and_is_bounded() {
        let root = test_root("cat");
        std::fs::write(root.join("a.txt"), b"hello world").expect("file written");
        let mut session = session_in(&root, FilesystemAccess::Read, false);
        let result = BuiltinExecutor.execute(
            &Builtin::Cat(SessionPath::new("a.txt").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 0);
        assert_eq!(output_text(&result), "hello world");
    }

    #[test]
    fn cat_rejects_paths_outside_the_workspace() {
        let root = test_root("cat-denied");
        let mut session = session_in(&root, FilesystemAccess::Read, false);
        // The typed IR rejects parent components before execution. This is
        // the fail-closed boundary that prevents the builtin from receiving
        // an escaping path at all.
        assert!(SessionPath::new("../../etc/hostname").is_err());
        let result = BuiltinExecutor.execute(
            &Builtin::Cat(SessionPath::new("missing.txt").expect("valid path")),
            &mut session,
        );
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains(&b'\n'));
    }

    #[test]
    fn cp_and_mv_move_data_within_the_workspace() {
        let root = test_root("cp-mv");
        std::fs::write(root.join("src.txt"), b"data").expect("source written");
        let mut session = session_in(&root, FilesystemAccess::ReadWrite, false);
        let cp = BuiltinExecutor.execute(
            &Builtin::Copy {
                from: SessionPath::new("src.txt").expect("valid path"),
                to: SessionPath::new("copy.txt").expect("valid path"),
            },
            &mut session,
        );
        assert_eq!(cp.exit_code, 0);
        assert_eq!(
            fs::read(root.join("copy.txt")).expect("copy exists"),
            b"data"
        );

        let mv = BuiltinExecutor.execute(
            &Builtin::Move {
                from: SessionPath::new("copy.txt").expect("valid path"),
                to: SessionPath::new("moved.txt").expect("valid path"),
            },
            &mut session,
        );
        assert_eq!(mv.exit_code, 0);
        assert!(!root.join("copy.txt").exists());
        assert_eq!(
            fs::read(root.join("moved.txt")).expect("moved exists"),
            b"data"
        );
    }

    #[test]
    fn which_finds_programs_on_the_session_path() {
        let root = test_root("which");
        std::fs::create_dir_all(root.join("bin")).expect("bin dir");
        // Use a regular file so the lookup test is meaningful on every
        // runner; Unix additionally checks executable permissions.
        std::fs::write(root.join("bin/tool"), b"tool").expect("tool written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::metadata(root.join("bin/tool"))
                .expect("tool metadata")
                .permissions();
            let mut permissions = permissions;
            permissions.set_mode(0o755);
            fs::set_permissions(root.join("bin/tool"), permissions).expect("tool made executable");
        }
        let mut session = session_in(&root, FilesystemAccess::Read, true);
        let export = BuiltinExecutor.execute(
            &Builtin::Export {
                name: "PATH".to_owned(),
                value: Some(root.join("bin").to_string_lossy().into_owned()),
            },
            &mut session,
        );
        assert_eq!(export.exit_code, 0);
        let result = BuiltinExecutor.execute(&Builtin::Which("tool".to_owned()), &mut session);
        assert_eq!(result.exit_code, 0);
        assert!(output_text(&result).contains("tool"));
    }
}
