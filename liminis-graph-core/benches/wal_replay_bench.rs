use criterion::{criterion_group, criterion_main, Criterion};

// Compile-only stub. Timing assertions are deferred to a [HOT] issue.
fn wal_replay_bench(_c: &mut Criterion) {}

criterion_group!(benches, wal_replay_bench);
criterion_main!(benches);
