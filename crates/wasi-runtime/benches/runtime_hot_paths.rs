//! Microbenchmarks for operations that can plausibly target sub-millisecond latency.
//!
//! Process creation, PTY setup, Wasmtime compilation, and guest execution are
//! intentionally not included: they are boundary operations with millisecond-
//! scale costs and need separate end-to-end measurements.

#![allow(missing_docs, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use wasi_runtime::capability::{CapabilityGrant, FilesystemAccess, ResourceLimits};
use wasi_runtime::command::{Actor, CommandRequest, ExecutionMode};
use wasi_runtime::pipe::StreamOutputPipe;
use wasi_runtime::policy::{NetworkPolicy, selected_environment};
use wasmtime_wasi::p2::OutputStream;
use wasmtime_wasi::sockets::SocketAddrUse;

fn benchmark_grant() -> (CapabilityGrant, PathBuf) {
    let root = std::env::temp_dir().join("ferrous-benchmark-workspace");
    let _ = std::fs::create_dir_all(&root);
    let grant = CapabilityGrant::workspace(&root, FilesystemAccess::Read)
        .expect("benchmark workspace is absolute")
        .allow_loopback_port(3000)
        .expect("benchmark port is valid")
        .with_limits(ResourceLimits::new(4096, 30).expect("benchmark limits are valid"));
    (grant, root)
}

fn benchmark_request(grant: CapabilityGrant, cwd: PathBuf) -> CommandRequest {
    CommandRequest::new(
        7,
        Actor::Agent,
        ExecutionMode::Wasi,
        "tool",
        ["--flag", "value", "another-argument"],
        cwd,
        grant,
    )
    .expect("benchmark request is valid")
}

fn bench_request_validation(c: &mut Criterion) {
    let (grant, cwd) = benchmark_grant();
    let request = benchmark_request(grant, cwd);
    c.bench_function("wasi_command_request_validate", |b| {
        b.iter(|| black_box(request.validate()).is_ok());
    });
}

fn bench_environment_selection(c: &mut Criterion) {
    let (grant, _) = benchmark_grant();
    let grant = grant
        .allow_environment("PATH")
        .expect("environment name is valid")
        .allow_environment("LANG")
        .expect("environment name is valid");
    let provider = |name: &str| match name {
        "PATH" => Some("/usr/bin".to_owned()),
        "LANG" => Some("C.UTF-8".to_owned()),
        _ => None,
    };
    c.bench_function("wasi_allowlisted_environment_selection", |b| {
        b.iter(|| black_box(selected_environment(&grant, &provider)));
    });
}

fn bench_network_policy(c: &mut Criterion) {
    let (grant, _) = benchmark_grant();
    let policy = NetworkPolicy::from_grant(&grant);
    let address: SocketAddr = "127.0.0.1:3000"
        .parse()
        .expect("benchmark address is valid");
    c.bench_function("wasi_loopback_policy_check", |b| {
        b.iter(|| black_box(policy.permits(address, SocketAddrUse::TcpConnect)));
    });
}

fn bench_pipe_write_and_drain(c: &mut Criterion) {
    c.bench_function("wasi_stream_pipe_write_and_drain", |b| {
        b.iter(|| {
            let mut pipe = StreamOutputPipe::new(4096);
            OutputStream::write(&mut pipe, Bytes::from_static(b"bounded output chunk"))
                .expect("benchmark write fits");
            let (bytes, eof) = pipe.wait_and_drain(Duration::ZERO);
            black_box((bytes, eof));
        });
    });
}

criterion_group!(
    benches,
    bench_request_validation,
    bench_environment_selection,
    bench_network_policy,
    bench_pipe_write_and_drain
);
criterion_main!(benches);
