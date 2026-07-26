// Issue #240 (SC-004/SC-005): ad hoc throughput/memory measurement for WAL replay's new
// explicit-transaction design, replaying the real-corpus WAL fixture
// (crates/core/tests/fixtures/real_corpus_wal/) directly through `WalReplayer::replay` — no IPC
// layer, no HNSW/FTS index build — so the measured time/memory reflects replay itself, not the
// unrelated index-build cost `real_corpus_e2e.rs`'s full-rebuild tests also pay.
//
// `#[ignore]`d for the same reason as `real_corpus_e2e.rs`'s rebuild tests: a full replay of this
// fixture takes on the order of a minute, too slow for every `cargo test --release` run. Run
// explicitly with:
//   cargo test -p lcg-core --test real_corpus_replay_perf --release -- --ignored --nocapture
//
// Peak memory (SC-004) is not measured from inside this test — no memory-profiling crate is a
// dependency of `lcg-core` today, and adding one just for an ad hoc measurement is out of scope
// for this issue (User Story 4's boundedness argument is structural: transaction size is capped
// by `batch_size`, independent of total WAL size — see ADR-0047). Measure externally instead,
// e.g. on macOS/Linux:
//   /usr/bin/time -l cargo test -p lcg-core --test real_corpus_replay_perf --release -- --ignored --nocapture
// (macOS reports "maximum resident set size" in bytes; Linux's `/usr/bin/time -v` reports
// "Maximum resident set size" in KB.)

use std::path::{Path, PathBuf};
use std::time::Instant;

use lcg_core::{Db, WalReplayer};
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_corpus_wal")
}

fn wal_dir() -> PathBuf {
    fixture_dir().join("wal")
}

fn embedding_dim() -> usize {
    let path = fixture_dir().join("expected_results.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let v: Value = serde_json::from_str(&raw).expect("expected_results.json must be valid JSON");
    v["embedding_dim"].as_u64().unwrap() as usize
}

#[test]
#[ignore]
fn measure_replay_throughput_over_real_corpus_wal() {
    let db_dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(db_dir.path().join("real_corpus_perf.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(embedding_dim()).unwrap();
    }

    let conn = db.connect().unwrap();
    let start = Instant::now();
    let stats = WalReplayer::new(wal_dir())
        .replay(&conn)
        .expect("replay must succeed against the real-corpus fixture");
    let elapsed = start.elapsed();

    let total_mutations = stats.lines_replayed + stats.failed_lines + stats.legacy_skipped_lines;
    println!(
        "[SC-005] real_corpus_wal replay: {:.3}s total, {:.1} mutations/s ({} lines_replayed, \
         {} failed_lines, {} rolled_back_lines, {} transactions_committed, \
         {} transactions_rolled_back)",
        elapsed.as_secs_f64(),
        total_mutations as f64 / elapsed.as_secs_f64().max(0.001),
        stats.lines_replayed,
        stats.failed_lines,
        stats.rolled_back_lines,
        stats.transactions_committed,
        stats.transactions_rolled_back,
    );

    assert_eq!(
        stats.failed_lines, 0,
        "the golden fixture must replay cleanly"
    );
    assert_eq!(
        stats.rolled_back_lines, 0,
        "no transaction should roll back on this fixture"
    );
}
