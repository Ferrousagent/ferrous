//! The `ferrous shell` REPL.
//!
//! Phase 1 keeps the CLI as the proof surface. Only the explicit `run-wasi` command
//! can execute a component; unknown input never falls through to a host shell.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use wasi_runtime::WasiRuntime;
use wasi_runtime::capability::{CapabilityGrant, FilesystemAccess};
use wasi_runtime::command::{Actor, CommandRequest, ExecutionMode};

/// The result of handling a single built-in shell line.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Emit a reply and keep the loop running.
    Reply(String),
    /// Terminate the loop.
    Exit,
}

#[derive(Debug, PartialEq, Eq)]
struct WasiCommand {
    path: String,
    args: Vec<String>,
}

/// Run the interactive shell until `exit`/`quit` or EOF.
///
/// # Errors
///
/// Returns an error if reading stdin, creating the runtime, or writing stdout fails.
pub fn run() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let workspace = std::env::current_dir()
        .context("failed to determine the shell workspace")?
        .canonicalize()
        .context("failed to canonicalize the shell workspace")?;
    let mut runtime = None;
    let mut session_id = 0_u64;

    tracing::info!(workspace = %workspace.display(), "ferrous shell started (Phase 1)");
    if !write_line(
        &mut stdout,
        "ferrous shell (Phase 1). Type `help` for commands, `run-wasi <component>` to execute a WASI component, or `exit` to quit.",
    )? {
        return Ok(()); // reader already gone — exit cleanly
    }

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if let Some(command) = parse_wasi_command(trimmed) {
            session_id = session_id.saturating_add(1);
            if runtime.is_none() {
                runtime = Some(WasiRuntime::new().map_err(anyhow::Error::new)?);
            }
            let Some(runtime) = runtime.as_ref() else {
                return Err(anyhow::anyhow!("WASI runtime was not initialized"));
            };
            let output = execute_wasi(runtime, &workspace, session_id, command)?;
            if !write_bytes(&mut stdout, &output.stdout)? {
                break;
            }
            if !write_bytes(&mut stdout, &output.stderr)? {
                break;
            }
            if output.exit_code != 0
                && !write_line(&mut stdout, "WASI component exited with code 1")?
            {
                break;
            }
            continue;
        }

        match handle_line(trimmed) {
            Outcome::Reply(reply) if reply.is_empty() => {}
            Outcome::Reply(reply) => {
                if !write_line(&mut stdout, &reply)? {
                    break; // reader closed the pipe — exit cleanly (like `yes | head`)
                }
            }
            Outcome::Exit => {
                let _ = write_line(&mut stdout, "bye"); // best-effort goodbye
                break;
            }
        }
    }

    Ok(())
}

fn execute_wasi(
    runtime: &WasiRuntime,
    workspace: &Path,
    session_id: u64,
    command: WasiCommand,
) -> Result<wasi_runtime::WasiOutput> {
    let component_path = workspace.join(&command.path);
    let component_path = component_path
        .canonicalize()
        .with_context(|| format!("WASI component not found: {}", component_path.display()))?;
    if !component_path.starts_with(workspace) {
        return Err(anyhow::anyhow!(
            "WASI component is outside the selected workspace"
        ));
    }

    let bytes = fs::read(&component_path).with_context(|| {
        format!(
            "failed to read WASI component: {}",
            component_path.display()
        )
    })?;
    let component = runtime
        .compile_component(&bytes)
        .map_err(anyhow::Error::new)
        .context("failed to admit WASI component")?;
    let grant = CapabilityGrant::workspace(workspace.to_path_buf(), FilesystemAccess::ReadWrite)
        .map_err(anyhow::Error::new)
        .context("failed to create workspace capability")?;
    let request = CommandRequest::new(
        session_id,
        Actor::Human,
        ExecutionMode::Wasi,
        component_path.to_string_lossy().into_owned(),
        command.args,
        workspace.to_path_buf(),
        grant,
    )
    .map_err(anyhow::Error::new)
    .context("WASI command was denied by capability policy")?;

    runtime
        .run_wasi(&component, &request)
        .map_err(anyhow::Error::new)
        .context("WASI command failed")
}

/// Write bytes to stdout, returning `Ok(false)` when the reader is gone.
fn write_bytes(stdout: &mut impl Write, bytes: &[u8]) -> io::Result<bool> {
    match stdout.write_all(bytes) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e),
        Ok(()) => Ok(true),
    }
}

/// Write one line to stdout, returning `Ok(false)` when the reader is gone
/// (broken pipe) so the shell can stop instead of erroring out.
fn write_line(stdout: &mut impl Write, text: &str) -> io::Result<bool> {
    match writeln!(stdout, "{text}") {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e),
        Ok(()) => Ok(true),
    }
}

fn parse_wasi_command(line: &str) -> Option<WasiCommand> {
    let mut tokens = line.split_whitespace();
    if tokens.next()? != "run-wasi" {
        return None;
    }
    let path = tokens.next()?.to_owned();
    Some(WasiCommand {
        path,
        args: tokens.map(str::to_owned).collect(),
    })
}

/// Handle one trimmed input line, returning the [`Outcome`] for the caller to act on.
///
/// Kept free of I/O so it can be unit-tested directly.
pub fn handle_line(line: &str) -> Outcome {
    match line {
        "" => Outcome::Reply(String::new()),
        "help" | "?" => Outcome::Reply(help_text().to_owned()),
        "version" => Outcome::Reply(format!("ferrous {}", shared::VERSION)),
        "exit" | "quit" => Outcome::Exit,
        other => Outcome::Reply(format!(
            "`{other}`: unsupported; use `run-wasi <component>` for explicit WASI execution"
        )),
    }
}

/// The help text printed by `help`.
fn help_text() -> &'static str {
    "Available commands:\n  help                         show this help\n  version                      print the version\n  run-wasi <component> [args]  run an explicitly selected WASI component\n  exit                         quit the shell\n\nNative host commands are not enabled in this CLI path."
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn help_replies_with_command_listing() {
        let Outcome::Reply(reply) = handle_line("help") else {
            panic!("expected a reply");
        };
        assert!(reply.contains("help"));
        assert!(reply.contains("version"));
        assert!(reply.contains("run-wasi"));
        assert!(reply.contains("exit"));
    }

    #[test]
    fn version_replies_with_version() {
        let Outcome::Reply(reply) = handle_line("version") else {
            panic!("expected a reply");
        };
        assert!(reply.starts_with("ferrous "));
    }

    #[test]
    fn exit_and_quit_terminate() {
        assert_eq!(handle_line("exit"), Outcome::Exit);
        assert_eq!(handle_line("quit"), Outcome::Exit);
    }

    #[test]
    fn blank_line_is_a_noop() {
        assert_eq!(handle_line(""), Outcome::Reply(String::new()));
    }

    #[test]
    fn unknown_command_is_not_a_native_shell_fallback() {
        let Outcome::Reply(reply) = handle_line("echo hello") else {
            panic!("expected a reply");
        };
        assert!(reply.contains("unsupported"));
    }

    #[test]
    fn parses_only_the_explicit_wasi_command() {
        let parsed = parse_wasi_command("run-wasi ./tool.wasm --flag value")
            .expect("explicit WASI command parses");
        assert_eq!(parsed.path, "./tool.wasm");
        assert_eq!(parsed.args, ["--flag", "value"]);
        assert!(parse_wasi_command("echo hello").is_none());
    }

    #[test]
    fn rejects_a_wasi_command_without_a_component_path() {
        assert!(parse_wasi_command("run-wasi").is_none());
    }
}
