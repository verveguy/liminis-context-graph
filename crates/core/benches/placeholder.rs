use criterion::{criterion_group, criterion_main, Criterion};

// Placeholder — real benchmarks will be added in hot-path issues.
fn placeholder_bench(_c: &mut Criterion) {}

criterion_group!(benches, placeholder_bench);
criterion_main!(benches);
