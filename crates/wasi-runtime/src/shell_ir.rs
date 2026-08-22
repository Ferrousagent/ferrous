//! The typed Ferrous Shell intermediate representation (IR).
//!
//! Human shell text and AI tool calls both compile into this IR. The IR is the
//! only thing an executor may interpret: it carries each program and argument
//! as a separate value, so shell metacharacters can never be reinterpreted by
//! a host shell. Every node can be hashed into a canonical
//! [`CommandDigest`] so that approvals and audit records bind to the exact
//! command plan that ran.

use std::fmt;

use sha2::{Digest, Sha256};

/// A validated path relative to the session root.
///
/// The path is a *capability-relative* coordinate, never an absolute host
/// path. It must be non-empty, contain no NUL byte, and contain no `..`
/// parent or absolute components. Resolution against a grant happens in the
/// executor; this type only guarantees the lexical form.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionPath(String);

impl SessionPath {
    /// Validate and construct a capability-relative session path.
    pub fn new(path: impl Into<String>) -> Result<Self, ShellIrError> {
        let path = path.into();
        if path.is_empty() {
            return Err(ShellIrError::EmptyPath);
        }
        if path.chars().any(|character| character == '\0') {
            return Err(ShellIrError::NulByte);
        }
        if path.starts_with('/') || path.starts_with('\\') {
            return Err(ShellIrError::AbsolutePath);
        }
        for component in path.split(['/', '\\']) {
            if component == ".." {
                return Err(ShellIrError::ParentComponent);
            }
        }
        Ok(Self(path))
    }

    /// The underlying relative path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path with `..` collapsed lexically for display purposes only.
    ///
    /// This is *not* a security boundary: the executor re-validates every
    /// resolved path against the capability grant with symlink awareness.
    pub fn display_path(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Errors produced while constructing IR nodes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShellIrError {
    /// A path was empty.
    #[error("session path cannot be empty")]
    EmptyPath,
    /// A path contained a NUL byte.
    #[error("session path cannot contain a NUL byte")]
    NulByte,
    /// A path was absolute; all session paths are capability-relative.
    #[error("session path must be relative to the session root")]
    AbsolutePath,
    /// A path contained a parent (`..`) component.
    #[error("session path cannot contain a parent component")]
    ParentComponent,
}

/// The native shell escape hatch kinds. Each is a distinct high-risk program
/// kind: a native shell can interpret arbitrary text, so it can never be
/// synthesized by ordinary command parsing — only by an explicit policy
/// decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeShellKind {
    /// `/bin/bash` (or the host's bash).
    Bash,
    /// Windows PowerShell.
    PowerShell,
    /// Windows Command Prompt.
    Cmd,
}

/// A built-in command executed in-process by the Ferrous shell.
///
/// Builtins are parsed into structured parameters so the executor never needs
/// to spawn a process (or a shell) for the common cases.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// Print the current working directory.
    Pwd,
    /// Change the session working directory.
    Cd(SessionPath),
    /// List a directory.
    Ls(SessionPath),
    /// Print a file's contents.
    Cat(SessionPath),
    /// Create a directory.
    Mkdir(SessionPath),
    /// Remove a file or empty directory.
    Remove(SessionPath),
    /// Copy a file.
    Copy {
        /// Source path.
        from: SessionPath,
        /// Destination path.
        to: SessionPath,
    },
    /// Move a file.
    Move {
        /// Source path.
        from: SessionPath,
        /// Destination path.
        to: SessionPath,
    },
    /// Print the session environment overlay.
    Env,
    /// Locate an executable on the session PATH.
    Which(String),
    /// Print arguments joined by spaces.
    Echo(Vec<String>),
    /// Set or remove a variable in the session environment overlay.
    Export {
        /// Variable name.
        name: String,
        /// New value, or `None` to remove the variable from the overlay.
        value: Option<String>,
    },
}

/// What a program is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Program {
    /// An in-process builtin.
    Builtin(Builtin),
    /// An external program launched by direct argv (never through a shell).
    External(String),
    /// A WASI component admitted and run through the embedded Wasmtime runtime.
    WasiComponent(String),
    /// An explicit, policy-gated native shell escape hatch.
    NativeShell(NativeShellKind),
}

/// File redirection attached to one command.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Redirect {
    /// Read stdin from a session file.
    Input(SessionPath),
    /// Truncate-and-write stdout to a session file.
    OutputTruncate(SessionPath),
    /// Append stdout to a session file.
    OutputAppend(SessionPath),
    /// Truncate-and-write stderr to a session file.
    ErrorTruncate(SessionPath),
    /// Append stderr to a session file.
    ErrorAppend(SessionPath),
}

/// One command in a plan: a program, its argv, redirects, and working
/// directory (relative to the session root).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandSpec {
    /// The program to run.
    pub program: Program,
    /// Structured arguments; each element is a separate value.
    pub args: Vec<String>,
    /// File redirections, applied in order.
    pub redirects: Vec<Redirect>,
    /// Working directory for this command, relative to the session root.
    pub cwd: SessionPath,
}

/// One statement in a shell plan.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Statement {
    /// A single command.
    Command(CommandSpec),
    /// A pipeline of commands connected by bounded channels.
    Pipeline(Vec<CommandSpec>),
    /// Run the right side only if the left side exited successfully.
    And(Box<Statement>, Box<Statement>),
    /// Run the right side only if the left side failed.
    Or(Box<Statement>, Box<Statement>),
    /// Run statements in order regardless of exit status.
    Sequence(Vec<Statement>),
    /// Run a statement as a session-owned background job.
    Background(Box<Statement>),
}

/// A complete shell plan: one or more statements executed in order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ShellProgram {
    /// The statements that make up the plan.
    pub statements: Vec<Statement>,
}

impl ShellProgram {
    /// An empty plan (executes to success with no effects).
    pub fn empty() -> Self {
        Self {
            statements: Vec::new(),
        }
    }
}

/// A canonical 32-byte fingerprint of an entire shell plan.
///
/// The digest is stable across sessions and hosts because it is computed from
/// a canonical byte encoding of the AST, cwd, argv, and redirect targets —
/// never from formatting or a host-specific representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommandDigest([u8; 32]);

impl CommandDigest {
    /// Compute the canonical digest of a plan.
    pub fn of(program: &ShellProgram) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ferrous-shell-ir-v1\0");
        for statement in &program.statements {
            statement.write_canonical(&mut bytes);
        }
        Self(Sha256::digest(&bytes).into())
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A lowercase hex representation for display and audit records.
    pub fn hex(&self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            output.push_str(&format!("{byte:02x}"));
        }
        output
    }
}

impl fmt::Display for CommandDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

impl Statement {
    fn write_canonical(&self, out: &mut Vec<u8>) {
        match self {
            Statement::Command(spec) => {
                out.push(0);
                spec.write_canonical(out);
            }
            Statement::Pipeline(stages) => {
                out.push(1);
                push_u32(out, stages.len() as u32);
                for stage in stages {
                    stage.write_canonical(out);
                }
            }
            Statement::And(left, right) => {
                out.push(2);
                left.write_canonical(out);
                right.write_canonical(out);
            }
            Statement::Or(left, right) => {
                out.push(3);
                left.write_canonical(out);
                right.write_canonical(out);
            }
            Statement::Sequence(statements) => {
                out.push(4);
                push_u32(out, statements.len() as u32);
                for statement in statements {
                    statement.write_canonical(out);
                }
            }
            Statement::Background(statement) => {
                out.push(5);
                statement.write_canonical(out);
            }
        }
    }
}

impl CommandSpec {
    fn write_canonical(&self, out: &mut Vec<u8>) {
        match &self.program {
            Program::Builtin(builtin) => {
                out.push(0);
                builtin.write_canonical(out);
            }
            Program::External(name) => {
                out.push(1);
                push_str(out, name);
            }
            Program::WasiComponent(path) => {
                out.push(2);
                push_str(out, path);
            }
            Program::NativeShell(kind) => {
                out.push(3);
                push_u8(out, *kind as u8);
            }
        }
        push_str(out, self.cwd.as_str());
        push_str_list(out, &self.args);
        push_u32(out, self.redirects.len() as u32);
        for redirect in &self.redirects {
            match redirect {
                Redirect::Input(path) => {
                    out.push(0);
                    push_str(out, path.as_str());
                }
                Redirect::OutputTruncate(path) => {
                    out.push(1);
                    push_str(out, path.as_str());
                }
                Redirect::OutputAppend(path) => {
                    out.push(2);
                    push_str(out, path.as_str());
                }
                Redirect::ErrorTruncate(path) => {
                    out.push(3);
                    push_str(out, path.as_str());
                }
                Redirect::ErrorAppend(path) => {
                    out.push(4);
                    push_str(out, path.as_str());
                }
            }
        }
    }
}

impl Builtin {
    fn write_canonical(&self, out: &mut Vec<u8>) {
        match self {
            Builtin::Pwd => out.push(0),
            Builtin::Cd(path) => {
                out.push(1);
                push_str(out, path.as_str());
            }
            Builtin::Ls(path) => {
                out.push(2);
                push_str(out, path.as_str());
            }
            Builtin::Cat(path) => {
                out.push(3);
                push_str(out, path.as_str());
            }
            Builtin::Mkdir(path) => {
                out.push(4);
                push_str(out, path.as_str());
            }
            Builtin::Remove(path) => {
                out.push(5);
                push_str(out, path.as_str());
            }
            Builtin::Copy { from, to } => {
                out.push(6);
                push_str(out, from.as_str());
                push_str(out, to.as_str());
            }
            Builtin::Move { from, to } => {
                out.push(7);
                push_str(out, from.as_str());
                push_str(out, to.as_str());
            }
            Builtin::Env => out.push(8),
            Builtin::Which(name) => {
                out.push(9);
                push_str(out, name);
            }
            Builtin::Echo(args) => {
                out.push(10);
                push_str_list(out, args);
            }
            Builtin::Export { name, value } => {
                out.push(11);
                push_str(out, name);
                match value {
                    Some(value) => {
                        out.push(1);
                        push_str(out, value);
                    }
                    None => out.push(0),
                }
            }
        }
    }
}

fn push_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn push_str_list(out: &mut Vec<u8>, values: &[String]) {
    push_u32(out, values.len() as u32);
    for value in values {
        push_str(out, value);
    }
}

/// A summarized set of side effects for approval display and audit.
///
/// This is the *redacted* surface an agent or UI may see: it lists effect
/// targets (paths, hosts, secret *names*), never secret values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectSummary {
    /// Paths the plan reads.
    pub reads: Vec<String>,
    /// Paths the plan writes or creates.
    pub writes: Vec<String>,
    /// Paths the plan deletes.
    pub deletes: Vec<String>,
    /// Network hosts the plan may contact.
    pub network: Vec<String>,
    /// Secret *names* the plan may access (never values).
    pub secrets: Vec<String>,
    /// Scripts or lifecycle hooks the plan may execute.
    pub scripts: Vec<String>,
}

/// One network capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkCapability {
    /// Host or domain pattern, e.g. `registry.npmjs.org`.
    pub host: String,
    /// Ports permitted; empty means no ports.
    pub ports: Vec<u16>,
    /// Whether outgoing connections are permitted.
    pub connect: bool,
    /// Whether binding is permitted.
    pub bind: bool,
}

/// The delta of authority a request needs beyond the session's base grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityDelta {
    /// Additional filesystem grants requested.
    pub filesystem: Vec<String>,
    /// Additional environment variable names requested.
    pub environment: Vec<String>,
    /// Additional network capabilities requested.
    pub network: Vec<NetworkCapability>,
    /// Whether native process execution is requested.
    pub native: bool,
    /// Secret names requested (never values).
    pub secrets: Vec<String>,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn spec(program: Program, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program,
            args: args.iter().map(|value| (*value).to_owned()).collect(),
            redirects: Vec::new(),
            cwd: SessionPath::new(".").expect("valid cwd"),
        }
    }

    #[test]
    fn session_path_rejects_absolute_parent_and_nul() {
        assert!(SessionPath::new("src/main.rs").is_ok());
        assert!(SessionPath::new(".").is_ok());
        assert!(SessionPath::new("/etc/passwd").is_err());
        assert!(SessionPath::new("../secrets").is_err());
        assert!(SessionPath::new("a/../b").is_err());
        assert!(SessionPath::new("").is_err());
        assert!(SessionPath::new("a\0b").is_err());
    }

    #[test]
    fn command_digest_changes_when_argv_changes() {
        let base = ShellProgram {
            statements: vec![Statement::Command(spec(
                Program::External("npm".to_owned()),
                &["install"],
            ))],
        };
        let changed = ShellProgram {
            statements: vec![Statement::Command(spec(
                Program::External("npm".to_owned()),
                &["install", "--unsafe-perm"],
            ))],
        };
        assert_ne!(CommandDigest::of(&base), CommandDigest::of(&changed));
    }

    #[test]
    fn command_digest_changes_when_effect_scope_changes() {
        let base = ShellProgram {
            statements: vec![Statement::Command(spec(
                Program::External("npm".to_owned()),
                &["install"],
            ))],
        };
        let mut redirected = base.clone();
        let Statement::Command(spec) = &mut redirected.statements[0] else {
            panic!("expected command statement");
        };
        spec.redirects.push(Redirect::OutputTruncate(
            SessionPath::new("out.log").expect("valid path"),
        ));
        assert_ne!(
            CommandDigest::of(&base),
            CommandDigest::of(&redirected),
            "a redirect target must change the action digest"
        );
    }

    #[test]
    fn digest_ignores_host_specific_formatting() {
        let plan = ShellProgram {
            statements: vec![
                Statement::Command(spec(Program::Builtin(Builtin::Pwd), &[])),
                Statement::Command(spec(Program::External("git".to_owned()), &["status"])),
            ],
        };
        let first = CommandDigest::of(&plan);
        let second = CommandDigest::of(&plan);
        assert_eq!(first, second);
        assert_eq!(first.hex().len(), 64);
    }

    #[test]
    fn native_shell_is_a_distinct_high_risk_program_kind() {
        let bash = Program::NativeShell(NativeShellKind::Bash);
        let external_bash = Program::External("bash".to_owned());
        assert_ne!(bash, external_bash);
        let plan_a = ShellProgram {
            statements: vec![Statement::Command(spec(bash, &["-c", "echo x"]))],
        };
        let plan_b = ShellProgram {
            statements: vec![Statement::Command(spec(external_bash, &["-c", "echo x"]))],
        };
        assert_ne!(
            CommandDigest::of(&plan_a),
            CommandDigest::of(&plan_b),
            "native-shell escape must hash differently from a plain external program"
        );
    }

    #[test]
    fn effect_summary_is_default_deny() {
        let summary = EffectSummary::default();
        assert!(summary.reads.is_empty());
        assert!(summary.writes.is_empty());
        assert!(summary.deletes.is_empty());
        assert!(summary.network.is_empty());
        assert!(summary.secrets.is_empty());
        assert!(summary.scripts.is_empty());
    }
}
