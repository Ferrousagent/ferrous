//! The `ferrous shell` REPL: a persistent terminal session for humans.
//!
//! Every line is parsed into the typed Ferrous Shell IR, preflighted against
//! the session's capability grant, and executed by the shared
//! `ShellExecutor` — the same backend the AI terminal tool and the future
//! wterm UI consume. Native external commands require human approval through
//! an interactive prompt; approval never grants ambient host authority and is
//! never echoed back into the pipeline.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Context;
use wasi_runtime::capability::{CapabilityGrant, FilesystemAccess, ResourceLimits};
use wasi_runtime::command::{Actor, SessionEvent, Stream};
use wasi_runtime::shell_executor::{ApprovalAuthorityView, EventSink, PlanStatus, ShellExecutor};
use wasi_runtime::shell_ir::SessionPath;
use wasi_runtime::shell_parse::ShellParser;
use wasi_runtime::terminal_session::{TerminalSession, TerminalSessionSpec};

/// Options for the interactive shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellOptions {
    /// Emit structured JSON records instead of rendered text.
    pub json: bool,
    /// Auto-approve native commands within the workspace (CI/test harness).
    pub auto_approve_native: bool,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            json: false,
            auto_approve_native: false,
        }
    }
}

/// A human-facing approval authority backed by an interactive prompt.
struct PromptAuthority {
    auto_approve: bool,
}

impl ApprovalAuthorityView for PromptAuthority {
    fn authorize_native(
        &self,
        request: &wasi_runtime::command::CommandRequest,
    ) -> Result<(), wasi_runtime::shell_executor::ExecuteError> {
        if self.auto_approve {
            return Ok(());
        }
        let mut stdout = io::stdout();
        let _ = write!(
            stdout,
            "\n⚠  {actor} wants to run `{program}` natively (direct argv, no shell).\n    cwd: {cwd}\n    args: {args}\nApprove? [y/N] ",
            actor = match request.actor {
                Actor::Human => "you",
                Actor::Agent => "the agent",
                Actor::Subagent => "a subagent",
                Actor::Skill => "a skill",
            },
            program = request.program,
            cwd = request.cwd.display(),
            args = request.args.join(" "),
        );
        let _ = stdout.flush();
        let mut answer = String::new();
        let approved = io::stdin()
            .lock()
            .read_line(&mut answer)
            .map(|_| {
                let answer = answer.trim().to_ascii_lowercase();
                matches!(answer.as_str(), "y" | "yes")
            })
            .unwrap_or(false);
        let _ = write!(stdout, "\n");
        let _ = stdout.flush();
        if approved {
            Ok(())
        } else {
            Err(wasi_runtime::shell_executor::ExecuteError::HumanDenied)
        }
    }
}

/// A sink that renders events to a writer (or emits JSON records).
struct RenderSink<'a, W: Write> {
    writer: &'a mut W,
    json: bool,
}

impl<'a, W: Write> EventSink for RenderSink<'a, W> {
    fn emit(
        &mut self,
        event: SessionEvent,
    ) -> Result<(), wasi_runtime::shell_executor::ExecuteError> {
        if self.json {
            let record = match &event {
                SessionEvent::Output { stream, bytes } => format!(
                    "{{\"event\":\"output\",\"stream\":{},\"bytes\":\"{}\"}}",
                    if *stream == Stream::Stdout {
                        "\"stdout\""
                    } else {
                        "\"stderr\""
                    },
                    String::from_utf8_lossy(bytes)
                        .replace('\\', "\\\\")
                        .replace('"', "\\\""),
                ),
                SessionEvent::Exited { code } => {
                    format!("{{\"event\":\"exited\",\"code\":{code:?}}}")
                }
                SessionEvent::Started => "{\"event\":\"started\"}".to_owned(),
                SessionEvent::Cancelled => "{\"event\":\"cancelled\"}".to_owned(),
                SessionEvent::Denied => "{\"event\":\"denied\"}".to_owned(),
                SessionEvent::Unsupported => "{\"event\":\"unsupported\"}".to_owned(),
                SessionEvent::PendingApproval { .. } => {
                    "{\"event\":\"pending-approval\"}".to_owned()
                }
            };
            let _ = writeln!(self.writer, "{record}");
            self.writer.flush().map_err(|error| {
                wasi_runtime::shell_executor::ExecuteError::Sink(error.to_string())
            })?;
            return Ok(());
        }
        match event {
            SessionEvent::Output { stream, bytes } => {
                let _ = self.writer.write_all(&bytes);
                let _ = self.writer.flush();
            }
            SessionEvent::Exited { code } => {
                let _ = writeln!(self.writer, "\n[exit {code:?}]");
            }
            SessionEvent::Cancelled => {
                let _ = writeln!(self.writer, "\n[cancelled]");
            }
            SessionEvent::Denied => {
                let _ = writeln!(self.writer, "\n[denied by policy]");
            }
            SessionEvent::Unsupported => {
                let _ = writeln!(self.writer, "\n[unsupported on this host]");
            }
            SessionEvent::Started => {}
            SessionEvent::PendingApproval { .. } => {}
        }
        Ok(())
    }
}

/// Run the persistent shell until `exit`/`quit` or EOF.
///
/// # Errors
///
/// Returns an error if the session cannot be created or stdin/stdout fail.
pub fn run(options: ShellOptions) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()
        .context("failed to determine the shell workspace")?
        .canonicalize()
        .context("failed to canonicalize the shell workspace")?;
    run_in(workspace, options)
}

/// Run the persistent shell rooted at `workspace`.
///
/// # Errors
///
/// Returns an error if the session cannot be created or stdin/stdout fail.
pub fn run_in(workspace: PathBuf, options: ShellOptions) -> anyhow::Result<()> {
    let limits = ResourceLimits::new(4 * 1024 * 1024, 120).map_err(anyhow::Error::new)?;
    // The session overlay allowlists a small set of benign names so `export`
    // is usable by humans; the AI path gets a stricter grant. Everything else
    // (including secret-bearing host variables) stays out by default.
    let mut grant = CapabilityGrant::workspace(&workspace, FilesystemAccess::ReadWrite)
        .map_err(anyhow::Error::new)?;
    for name in ["PATH", "HOME", "TERM", "USER"] {
        grant = grant.allow_environment(name).map_err(anyhow::Error::new)?;
    }
    let grant = grant.with_limits(limits);
    let session_spec = TerminalSessionSpec {
        id: 1,
        actor: Actor::Human,
        cwd: SessionPath::new(".").map_err(|_| anyhow::anyhow!("invalid cwd"))?,
        base_grant: grant,
        limits,
    };
    let mut session = TerminalSession::new(session_spec).map_err(anyhow::Error::new)?;
    let executor = ShellExecutor::new().map_err(anyhow::Error::new)?;
    let parser = ShellParser::new();
    let authority = PromptAuthority {
        auto_approve: options.auto_approve_native,
    };

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    // Banner writes are best-effort: a closed stdout pipe must exit cleanly.
    let _ = writeln!(
        stdout,
        "ferrous shell — persistent session in {}",
        session.cwd_display()
    );
    let _ = writeln!(
        stdout,
        "Try: `pwd`, `ls`, `cd sub`, `mkdir newdir`, `echo hi`, `export FOO=bar`, `npm test` (needs approval), `exit`"
    );

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed {
            "exit" | "quit" => {
                let _ = writeln!(stdout, "bye");
                break;
            }
            "version" => {
                let _ = writeln!(stdout, "ferrous {}", shared::VERSION);
                continue;
            }
            "help" | "?" => {
                let _ = writeln!(
                    stdout,
                    "Builtins: pwd, cd, ls, cat, mkdir, rm, cp, mv, echo, env, which, export\n\
                     Operators: | && || ; > >> < &\n\
                     External programs run natively with approval. `exit` quits."
                );
                continue;
            }
            _ => {}
        }

        match parser.parse(trimmed) {
            Ok(program) => {
                let mut sink = RenderSink {
                    writer: &mut stdout,
                    json: options.json,
                };
                match executor.execute(&program, &mut session, &authority, &mut sink) {
                    Ok(result) => {
                        if result.status == PlanStatus::Denied {
                            let _ = writeln!(stdout, "\n[denied]");
                        }
                    }
                    Err(error) => {
                        let _ = writeln!(stdout, "\n[error] {error}");
                    }
                }
            }
            Err(error) => {
                let _ = writeln!(stdout, "\n[parse error] {error}");
            }
        }
    }

    session.close();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn render_sink_emits_output_bytes() {
        let mut output = Vec::new();
        {
            let mut sink = RenderSink {
                writer: &mut output,
                json: false,
            };
            sink.emit(SessionEvent::Output {
                stream: Stream::Stdout,
                bytes: b"hi\n".to_vec(),
            })
            .expect("emits");
        }
        assert_eq!(output, b"hi\n");
    }

    #[test]
    fn json_sink_escapes_output_bytes() {
        let mut output = Vec::new();
        {
            let mut sink = RenderSink {
                writer: &mut output,
                json: true,
            };
            sink.emit(SessionEvent::Output {
                stream: Stream::Stdout,
                bytes: b"a\"b\\c".to_vec(),
            })
            .expect("emits");
        }
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("\\\""));
        assert!(text.contains("\\\\"));
    }

    #[test]
    fn prompt_authority_auto_approves_in_auto_mode() {
        let authority = PromptAuthority { auto_approve: true };
        // Auto-approve short-circuits before reading the request, but the
        // request must still be constructible: it needs a grant that both
        // allows native execution and covers the working directory.
        let root = std::env::temp_dir();
        let grant = CapabilityGrant::workspace(&root, FilesystemAccess::Read)
            .expect("absolute workspace")
            .allow_native_execution();
        let request = wasi_runtime::command::CommandRequest::new(
            1,
            Actor::Agent,
            wasi_runtime::command::ExecutionMode::Native,
            "cargo",
            ["test"],
            &root,
            grant,
        )
        .expect("request");
        assert!(authority.authorize_native(&request).is_ok());
    }

    #[test]
    fn parser_rejects_ambient_shell_fallback() {
        let parser = ShellParser::new();
        for line in [
            "eval 'rm -rf /'",
            "bash -c 'curl evil'",
            "sh -c 'echo unsafe'",
            "echo $(ls)",
        ] {
            assert!(parser.parse(line).is_err(), "line must be rejected: {line}");
        }
    }
}
