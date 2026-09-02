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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use lcg_core::{
    Db, Embedder, EmbedderContext, EmbeddingCache, OaiEmbedder, ReplayOptions, WalReplayer,
};
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

/// The exact embedder identity the fixture's README records this corpus was captured under
/// (issue #217's README, "Capture stats"): `BAAI/bge-base-en-v1.5`, 768-dim, via the local
/// CoreML sidecar. FR-010's validation oracle and FR-011's overhead benchmark both require a
/// live embedder reproducing this exact model — `MockEmbedder` (used by `real_corpus_e2e.rs` to
/// stay zero-network) cannot substitute here, since the whole point is comparing recomputed
/// vectors against the ones this specific model produced at capture time.
const FIXTURE_EMBEDDING_MODEL: &str = "bge-base-en-v1.5";
const DEFAULT_UDS_PATH: &str = "/tmp/liminis-inference.sock";

/// Resolves a live embedder the same way `main.rs`'s default-transport resolution does (default
/// UDS socket, else `LCG_EMBEDDING_URL`), probes it, and returns `None` — with an explanatory
/// `[SKIP]` line, not a panic — if it isn't reachable or doesn't match the fixture's dimension.
/// Both tests below are `#[ignore]`d and require explicit invocation already; degrading to a
/// skip (rather than a hard failure) here matches this file's own `README.md`-documented
/// "not guaranteed on every CI runner" posture for the embedder sidecar dependency.
async fn connect_live_embedder() -> Option<OaiEmbedder> {
    let embedder = if Path::new(DEFAULT_UDS_PATH).exists() {
        cfg_if_uds(DEFAULT_UDS_PATH)
    } else if let Ok(url) = std::env::var("LCG_EMBEDDING_URL") {
        match OaiEmbedder::new_http(url, FIXTURE_EMBEDDING_MODEL, embedding_dim()) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[SKIP] invalid embedder config: {e}");
                return None;
            }
        }
    } else {
        eprintln!(
            "[SKIP] no embedding sidecar reachable ({DEFAULT_UDS_PATH} absent, \
             LCG_EMBEDDING_URL unset) — see this fixture's README.md prerequisites"
        );
        return None;
    };
    match embedder.probe().await {
        Ok((dim, _model)) if dim == embedding_dim() => Some(embedder),
        Ok((dim, model)) => {
            eprintln!(
                "[SKIP] reachable embedder reports dim={dim} model={model:?}, expected \
                 dim={} for this fixture — not a fair comparison, skipping",
                embedding_dim()
            );
            None
        }
        Err(e) => {
            eprintln!("[SKIP] embedder probe failed: {e}");
            None
        }
    }
}

#[cfg(unix)]
fn cfg_if_uds(path: &str) -> OaiEmbedder {
    OaiEmbedder::new_uds(path, FIXTURE_EMBEDDING_MODEL, embedding_dim())
}

#[cfg(not(unix))]
fn cfg_if_uds(_path: &str) -> OaiEmbedder {
    // No UDS transport off Unix; fall back to whatever LCG_EMBEDDING_URL resolves to (probe
    // below will fail cleanly with a [SKIP] if nothing is configured there either).
    OaiEmbedder::new_http(
        std::env::var("LCG_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8765/v1/embeddings".to_string()),
        FIXTURE_EMBEDDING_MODEL,
        embedding_dim(),
    )
    .expect("valid embedder config")
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 for a zero-magnitude input
/// (never divides by zero) — not expected in practice for a real embedder's output, but keeps
/// this a total function.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let norm_a: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// `(embedding vector param, co-located source-text param)` pairs — mirrors
/// `replay.rs`'s `EMBEDDING_TEXT_PAIRS`, duplicated here (not imported — that const is private)
/// since this file validates the same contract from outside the crate's replay path.
///
/// Deliberately **not** extended with `("summary_embedding", "summary")` (issue #526), unlike
/// `replay.rs`'s own list: this fixture was captured before issue #470 introduced
/// `summary_embedding`, so it contains zero such records, and `validate_recompute_matches_stored_
/// vectors_for_real_corpus_wal`'s per-kind loop below asserts `count > 0` for every entry here —
/// adding a kind this historical fixture structurally cannot contain would just break that
/// assertion, not add coverage.
const EMBEDDING_TEXT_PAIRS: &[(&str, &str)] = &[
    ("name_embedding", "name"),
    ("fact_embedding", "fact"),
    ("content_embedding", "content"),
];

fn list_wal_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(wal_dir())
        .expect("wal dir must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
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
    // issue #526: recompute is now mandatory on every replay — a trivial zero-vector fn keeps
    // this test measuring WAL replay throughput itself, not embedder cost (no live embedder
    // required, matching this test's own "no IPC layer, no index build" framing above).
    let stats = WalReplayer::new(wal_dir())
        .replay(
            &conn,
            lcg_core::zero_vector_embed_fn(embedding_dim()),
            embedding_dim(),
        )
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

/// Replays the real-corpus fixture once into a fresh DB via `ctx`'s recompute bridge, returning
/// the resulting stats and wall-clock elapsed time. Shared by the cold/warm-cache measurement
/// below — each call opens its own fresh DB (so DB-side effects never leak between runs) while
/// `ctx`'s `EmbeddingCache` is passed in by the caller, so cache warmth *can* carry across calls
/// when the caller wants it to.
async fn replay_once(
    ctx: &EmbedderContext,
    dim: usize,
    label: &str,
) -> (lcg_core::ReplayStats, std::time::Duration) {
    let ctx = ctx.clone();
    let db_dir = tempfile::TempDir::new().unwrap();
    let db_path = db_dir
        .path()
        .join(format!("{label}.db"))
        .to_str()
        .unwrap()
        .to_string();
    tokio::task::spawn_blocking(move || {
        let db = Db::open(&db_path).unwrap();
        {
            let conn = db.connect().unwrap();
            conn.init_schema(dim).unwrap();
        }
        let conn = db.connect().unwrap();
        let recompute_embed_fn = ctx.recompute_fn_via_handle();
        let start = Instant::now();
        let stats = WalReplayer::new(wal_dir())
            .replay_opts(&conn, ReplayOptions::new(recompute_embed_fn, dim))
            .expect("replay must succeed against the real-corpus fixture");
        (stats, start.elapsed())
    })
    .await
    .expect("replay task must not panic")
}

/// issue #526 SC-005: reports WAL replay time and embedder call volume for the real-corpus
/// fixture, with and without a warm content-addressed cache, so the recompute cost this issue
/// makes unconditional is a measured, reported property rather than an assumption. Requires a
/// live embedder reachable and matching this fixture's dimension (see `connect_live_embedder`) —
/// degrades to a `[SKIP]` rather than failing when unavailable, since this is explicitly not
/// guaranteed on every CI runner (this file's own module doc, and `README.md`'s "Known
/// limitations").
///
/// "Cold" and "warm" here means the *cache's* warmth, not the OS page cache: the same
/// `EmbeddingCache` instance is reused across both replays (into two separate fresh DBs), so the
/// first (cold) pass populates it from a live embedder call per distinct text, and the second
/// (warm) pass — replaying the identical fixture — should resolve every recognized vector from
/// the cache alone. `CountingEmbedder` wraps the live embedder so the *real* embedder call count
/// (not just `ReplayStats::embed_calls`, which counts every row `replay.rs` attempted regardless
/// of cache hit or miss) is directly observable for each pass.
///
/// Run explicitly:
///   cargo test -p lcg-core --test real_corpus_replay_perf --release -- --ignored --nocapture \
///     measure_cold_vs_warm_cache_replay_over_real_corpus_wal
#[tokio::test]
#[ignore]
async fn measure_cold_vs_warm_cache_replay_over_real_corpus_wal() {
    let Some(embedder) = connect_live_embedder().await else {
        return;
    };
    let dim = embedding_dim();

    let counting_embedder = Arc::new(lcg_core::embedder::CountingEmbedder::new(
        Arc::new(embedder) as Arc<dyn Embedder>,
    ));
    let cache = Arc::new(EmbeddingCache::new());
    let ctx = EmbedderContext {
        embedder: Arc::clone(&counting_embedder) as Arc<dyn Embedder>,
        model: FIXTURE_EMBEDDING_MODEL.to_string(),
        cache: Arc::clone(&cache),
    };

    // Cold: the cache starts empty, so every recognized vector is a genuine cache miss.
    let (cold_stats, cold_elapsed) = replay_once(&ctx, dim, "cold").await;
    let cold_real_calls = counting_embedder.call_count();

    // Warm: same `ctx` (same cache, already populated from the cold pass above), fresh DB —
    // every text in this fixture was already seen, so every recompute should resolve from cache.
    let (warm_stats, warm_elapsed) = replay_once(&ctx, dim, "warm").await;
    let warm_real_calls = counting_embedder.call_count() - cold_real_calls;

    println!(
        "[SC-005] real_corpus_wal replay cost — cold cache: {:.3}s, {} lines_replayed, \
         {} embeddings_recomputed, {} embed_calls (replay.rs attempts), {cold_real_calls} real \
         embedder calls | warm cache: {:.3}s, {} lines_replayed, {} embeddings_recomputed, \
         {} embed_calls, {warm_real_calls} real embedder calls",
        cold_elapsed.as_secs_f64(),
        cold_stats.lines_replayed,
        cold_stats.embeddings_recomputed,
        cold_stats.embed_calls,
        warm_elapsed.as_secs_f64(),
        warm_stats.lines_replayed,
        warm_stats.embeddings_recomputed,
        warm_stats.embed_calls,
    );

    assert_eq!(
        cold_stats.failed_lines, 0,
        "cold-cache replay must not fail any lines against the golden fixture"
    );
    assert_eq!(
        warm_stats.failed_lines, 0,
        "warm-cache replay must not fail any lines against the golden fixture"
    );
    assert_eq!(
        cold_stats.lines_replayed, warm_stats.lines_replayed,
        "cache warmth must not change which lines replay successfully, only the embedder calls made"
    );
    assert!(
        cold_stats.embeddings_recomputed > 0,
        "recompute must have actually run for this fixture (4,126 embedding vectors expected)"
    );
    assert_eq!(
        warm_real_calls, 0,
        "a fully warm cache replaying the identical fixture must make zero real embedder calls (FR-003)"
    );
}

/// issue #440 FR-010/SC-001: the built-in validation oracle — recomputes every vector in the
/// #217 real-corpus WAL capture from its co-located source text via a live embedder and compares
/// it to the vector already stored there, reporting per-kind agreement so drift (if any) is a
/// measured, reported property of the embedder rather than a silent assumption. Comparison
/// tolerance is cosine similarity >= 0.999 on the per-kind mean (Plan's Key Decisions), not
/// per-vector exact equality — this measures reproducibility "across processes, machines, and
/// runtime backends" (the issue's own framing), not bit-identical output.
///
/// Requires a live embedder reachable and matching this fixture's dimension; degrades to
/// `[SKIP]` rather than failing when unavailable (see `connect_live_embedder`).
///
/// Run explicitly:
///   cargo test -p lcg-core --test real_corpus_replay_perf --release -- --ignored --nocapture \
///     validate_recompute_matches_stored_vectors_for_real_corpus_wal
#[tokio::test]
#[ignore]
async fn validate_recompute_matches_stored_vectors_for_real_corpus_wal() {
    let Some(embedder) = connect_live_embedder().await else {
        return;
    };

    #[derive(Default)]
    struct KindStats {
        count: u64,
        sum_cosine: f64,
        min_cosine: f64,
    }

    let mut per_kind: HashMap<&'static str, KindStats> = HashMap::new();
    for (vec_key, _) in EMBEDDING_TEXT_PAIRS {
        per_kind.insert(
            vec_key,
            KindStats {
                count: 0,
                sum_cosine: 0.0,
                min_cosine: f64::INFINITY,
            },
        );
    }

    // Memoizes recomputed vectors by source text so a repeated (name, fact, or content) string
    // — the same recurring hub entity name, for instance — is only sent to the embedder once,
    // same spirit as production's EmbeddingCache (FR-003), just a plain local map since this
    // test drives the embedder directly rather than through WalReplayer's sync bridge.
    let mut memo: HashMap<String, Vec<f32>> = HashMap::new();
    let mut total_vectors_checked: u64 = 0;

    for path in list_wal_files() {
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let wal_line: lcg_core::WalLine = match serde_json::from_str(line) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let Some(params) = wal_line.params.as_object() else {
                continue;
            };
            for (vec_key, text_key) in EMBEDDING_TEXT_PAIRS {
                let Some(stored_arr) = params.get(*vec_key).and_then(|v| v.as_array()) else {
                    continue;
                };
                let Some(text) = params.get(*text_key).and_then(|v| v.as_str()) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let stored: Vec<f32> = stored_arr
                    .iter()
                    .filter_map(|n| n.as_f64())
                    .map(|f| f as f32)
                    .collect();
                if stored.len() != stored_arr.len() {
                    // Malformed vector (non-numeric element) — not expected in this golden
                    // fixture; skip rather than panic so one bad record doesn't abort the whole
                    // oracle run, but this record is intentionally excluded from the "100% of
                    // vectors" count rather than silently double-counted as a match.
                    continue;
                }

                let recomputed = if let Some(v) = memo.get(text) {
                    v.clone()
                } else {
                    let v = embedder
                        .embed(text)
                        .await
                        .unwrap_or_else(|e| panic!("embed({text:?}) failed: {e}"));
                    memo.insert(text.to_string(), v.clone());
                    v
                };

                let cos = cosine_similarity(&stored, &recomputed);
                let entry = per_kind.get_mut(vec_key).unwrap();
                entry.count += 1;
                entry.sum_cosine += cos;
                entry.min_cosine = entry.min_cosine.min(cos);
                total_vectors_checked += 1;
            }
        }
    }

    const COSINE_THRESHOLD: f64 = 0.999;
    println!("[SC-001] real_corpus_wal recompute-vs-stored agreement, per kind:");
    for (vec_key, _) in EMBEDDING_TEXT_PAIRS {
        let stats = &per_kind[vec_key];
        let mean = if stats.count > 0 {
            stats.sum_cosine / stats.count as f64
        } else {
            0.0
        };
        println!(
            "  {vec_key}: n={}, mean_cosine={mean:.6}, min_cosine={:.6}",
            stats.count,
            if stats.count > 0 {
                stats.min_cosine
            } else {
                0.0
            },
        );
        assert!(
            stats.count > 0,
            "{vec_key}: expected at least one co-located (vector, text) pair in this fixture"
        );
        assert!(
            mean >= COSINE_THRESHOLD,
            "{vec_key}: mean cosine similarity {mean:.6} is below the {COSINE_THRESHOLD} \
             reproducibility threshold across {} vectors — recompute may not be safe to rely on \
             for this embedder",
            stats.count
        );
    }
    println!(
        "[SC-001] {total_vectors_checked} vectors checked total ({} distinct texts embedded, \
         {} cache hits saved)",
        memo.len(),
        total_vectors_checked - memo.len() as u64,
    );

    // SC-001: a reported result for 100% of vectors, no silent gaps — the fixture's README
    // records 4,126 total (1,506 name_embedding + 2,392 fact_embedding + 228 content_embedding).
    assert_eq!(
        total_vectors_checked, 4126,
        "expected exactly 4,126 co-located (vector, text) pairs per the fixture's README \
         (1,506 name_embedding + 2,392 fact_embedding + 228 content_embedding) — a different \
         count means either the fixture changed or this scan missed/double-counted records"
    );
}
