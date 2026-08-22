//! End-to-end tests for the `ferrous` binary.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may unwrap freely

use assert_cmd::Command;
use predicates::prelude::*;

/// A fresh, unique working directory for one shell test. Every test gets its
/// own root so parallel runs never race, repeated runs are idempotent, and no
/// test ever touches the repository checkout.
fn fresh_shell_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrous-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("shell test dir is created");
    dir
}

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
        .current_dir(fresh_shell_dir("help"))
        .args(["shell"])
        .write_stdin("help\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("ferrous shell"))
        .stdout(predicate::str::contains("Builtins:"))
        .stdout(predicate::str::contains("bye"));
}

#[test]
fn shell_version_prints_version() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .current_dir(fresh_shell_dir("version"))
        .args(["shell"])
        .write_stdin("version\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn shell_cd_persists_for_the_next_command() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .current_dir(fresh_shell_dir("cd-persists"))
        .args(["shell"])
        .write_stdin("mkdir sub && cd sub && pwd\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("sub"));
}

#[test]
fn shell_echo_and_builtins_run_in_process() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .current_dir(fresh_shell_dir("echo"))
        .args(["shell"])
        .write_stdin("echo hello ferrous\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello ferrous"));
}

#[test]
fn shell_rejects_eval_and_shell_escapes_without_fallback() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .current_dir(fresh_shell_dir("reject-eval"))
        .args(["shell"])
        .write_stdin("eval 'rm -rf /'\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("parse error"));
}

#[test]
fn shell_never_falls_through_unknown_input_to_a_host_shell() {
    // `wasi hello` parses as an external command; without approval it is
    // denied by the prompt authority, never handed to a host shell.
    Command::cargo_bin("ferrous")
        .unwrap()
        .current_dir(fresh_shell_dir("no-fallback"))
        .args(["shell", "--auto-approve-native"])
        .write_stdin("definitely-not-a-real-tool-xyz arg1\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("error"));
}

#[test]
fn shell_json_mode_emits_structured_records() {
    Command::cargo_bin("ferrous")
        .unwrap()
        .current_dir(fresh_shell_dir("json"))
        .args(["shell", "--json"])
        .write_stdin("echo hi\nexit\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"event\":\"output\""))
        .stdout(predicate::str::contains("\"stream\":\"stdout\""));
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
