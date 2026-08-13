//! End-to-end tests for the `ferrous` binary.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may unwrap freely

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_flag_prints_version() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn shell_runs_help_and_exits() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .args(["shell"])
        .write_stdin("help\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Available commands"))
        .stdout(predicate::str::contains("bye"));
}

#[test]
fn shell_version_prints_version() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .args(["shell"])
        .write_stdin("version\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn shell_unknown_command_reports_phase_one() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .args(["shell"])
        .write_stdin("wasi hello\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Phase 1"));
}

#[test]
fn missing_subcommand_fails() {
    Command::cargo_bin("ferrous").unwrap().assert().failure();
}

/// Rust ignores SIGPIPE (rust-lang/rust#46016): closing our read end of the
/// child's stdout must not kill the shell with exit 1 — it should exit cleanly.
#[cfg(unix)]
#[test]
fn shell_exits_cleanly_when_stdout_pipe_closes() {
    use std::io::Write;
    use std::process::{Command as StdCommand, Stdio};

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("ferrous"))
        .arg("shell")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Close our read end immediately — the child's next write hits EPIPE.
    drop(child.stdout.take());

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"help\nexit\n"); // child may already have exited
    }

    let status = child.wait().unwrap();
    assert!(status.success(), "broken pipe must exit 0, got {status:?}");
}
