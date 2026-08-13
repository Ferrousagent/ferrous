//! The `ferrous shell` REPL.
//!
//! Phase 0 implements built-in commands only; WASI command execution is wired in
//! Phase 1 of `docs/plans/ferrous-roadmap-spec.md`.

use std::io::{self, BufRead, Write};

use anyhow::Result;

/// The result of handling a single line of shell input.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Emit a reply and keep the loop running.
    Reply(String),
    /// Terminate the loop.
    Exit,
}

/// Run the interactive shell until `exit`/`quit` or EOF.
///
/// # Errors
///
/// Returns an error if reading stdin or writing stdout fails.
pub fn run() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    tracing::info!("ferrous shell started (Phase 0)");
    if !write_line(
        &mut stdout,
        "ferrous shell (Phase 0). Type `help` for commands, `exit` to quit.",
    )? {
        return Ok(()); // reader already gone — exit cleanly
    }

    for line in stdin.lock().lines() {
        let line = line?;
        match handle_line(line.trim()) {
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

/// Write one line to stdout, returning `Ok(false)` when the reader is gone
/// (broken pipe) so the shell can stop instead of erroring out.
///
/// Rust ignores `SIGPIPE` by default (rust-lang/rust#46016), so writes to a
/// closed pipe surface as `ErrorKind::BrokenPipe`; without handling, `ferrous
/// shell | head -1` would exit non-zero with a spurious error.
fn write_line(stdout: &mut impl Write, text: &str) -> io::Result<bool> {
    match writeln!(stdout, "{text}") {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e),
        Ok(()) => Ok(true),
    }
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
            "`{other}`: WASI command execution arrives in Phase 1 (docs/plans/ferrous-roadmap-spec.md)"
        )),
    }
}

/// The help text printed by `help`.
fn help_text() -> &'static str {
    "Available commands:\n  help      show this help\n  version   print the version\n  exit      quit the shell\n\nWASI execution lands in Phase 1."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_replies_with_command_listing() {
        let Outcome::Reply(reply) = handle_line("help") else {
            panic!("expected a reply");
        };
        assert!(reply.contains("help"));
        assert!(reply.contains("version"));
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
    fn unknown_command_reports_phase_one() {
        let Outcome::Reply(reply) = handle_line("wasi hello") else {
            panic!("expected a reply");
        };
        assert!(reply.contains("Phase 1"));
    }
}
