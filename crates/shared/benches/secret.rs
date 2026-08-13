//! Benchmarks for secret handling — establishes the benchmark harness used across the project.

#![allow(missing_docs)] // criterion_group! expands to a public, undocumented fn

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use shared::secret::SecretString;

fn bench_redaction(c: &mut Criterion) {
    let secret = SecretString::from("super-secret-value");
    c.bench_function("secret_string_debug_redaction", |b| {
        b.iter(|| black_box(format!("{secret:?}")));
    });
}

criterion_group!(benches, bench_redaction);
criterion_main!(benches);
