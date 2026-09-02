// Issue #526, SC-001: a freshly written WAL contains no embedding vector params, and its size
// drops by approximately the previously-measured 89.9% relative to the #217 capture's original,
// vector-bearing shape — with the actual figure reported, not merely asserted.
//
// Rather than re-running a live ingest workload (which would need a real extractor + embedder,
// out of scope for a fast, network-free test), this replays the already-committed #217 capture
// fixture (crates/core/tests/fixtures/real_corpus_wal/wal/ — the *original*, pre-#526,
// vector-bearing shape measured in the issue) into a fresh DB, then re-dumps it via
// `knowledge_dump_wal`. The dump path funnels through the exact same `WalWriter::log_mutation`
// choke point the live ingest write path does (see `wal.rs`'s doc comment), so the freshly
// written WAL this produces is representative of what a live ingest run would write today —
// just without needing one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{Embedder, HashEmbedder},
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    replay::WalReplayer,
    telemetry::{NoopSink, TelemetrySink},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Every embedding vector param key this issue removes from the WAL (FR-001) — deliberately a
/// plain local literal, not imported from `wal::VECTOR_PARAM_KEYS`, since that list is
/// `pub(crate)` and this test validates the observable contract from outside the crate.
const VECTOR_PARAM_KEYS: &[&str] = &[
    "name_embedding",
    "fact_embedding",
    "content_embedding",
    "summary_embedding",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_corpus_wal")
}

fn original_wal_dir() -> PathBuf {
    fixture_dir().join("wal")
}

fn embedding_dim() -> usize {
    let path = fixture_dir().join("expected_results.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let v: Value = serde_json::from_str(&raw).expect("expected_results.json must be valid JSON");
    v["embedding_dim"].as_u64().unwrap() as usize
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect()
}

fn total_bytes(dir: &Path) -> u64 {
    jsonl_files(dir)
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn make_state(db: Arc<Db>, db_path: &str, dim: usize) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(HashEmbedder::new(dim)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
        wal_root: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "hash-embedder-test".to_string(),
        wal_writers: Arc::new(Mutex::new(HashMap::new())),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        embedding_cache: std::sync::Arc::new(lcg_core::EmbeddingCache::new()),
    })
}

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

async fn dispatch(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

// #[ignore]: replays the full #217 capture (12,482 records) — not as slow as the full
// `real_corpus_e2e.rs` rebuild (no HNSW/FTS index build here, just replay + a plain node/edge
// dump), but still slow enough to exclude from the default `cargo test` run, matching every
// other real-corpus-fixture test in this crate. Run explicitly:
//   cargo test -p lcg-core --test wal_vector_stripping -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn fresh_wal_dump_has_no_vector_params_and_is_dramatically_smaller() {
    let dim = embedding_dim();
    let original_bytes = total_bytes(&original_wal_dir());
    assert!(
        original_bytes > 0,
        "the #217 capture fixture must be non-empty"
    );

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("fresh.db");
    let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }

    // Replay the original, vector-bearing capture into a fresh DB. The embed fn's fidelity
    // doesn't matter for this test (only byte counts and param-key absence do) — HashEmbedder
    // is used anyway to exercise a real (if offline) recompute path rather than a degenerate
    // all-zero one.
    let embedder = HashEmbedder::new(dim);
    let embed_fn: lcg_core::RecomputeEmbedFn =
        Box::new(move |texts: &[&str]| futures::executor::block_on(embedder.embed_batch(texts)));
    {
        let conn = db.connect().unwrap();
        let stats = WalReplayer::new(original_wal_dir())
            .replay(&conn, embed_fn, dim)
            .expect("replay of the #217 capture must succeed");
        assert_eq!(
            stats.failed_lines, 0,
            "the golden fixture must replay cleanly"
        );
    }

    // Re-dump through knowledge_dump_wal — the live write path's exact choke point
    // (WalWriter::log_mutation) for stripping vector params (FR-001).
    let fresh_dir = dir.path().join("fresh-wal");
    let state = make_state(Arc::clone(&db), db_path.to_str().unwrap(), dim);
    let dump_v = dispatch(
        1,
        "knowledge_dump_wal",
        json!({ "target_dir": fresh_dir.to_str().unwrap() }),
        state,
    )
    .await;
    assert_eq!(
        dump_v["result"]["success"], true,
        "dump must succeed: {dump_v}"
    );

    // FR-001: no line in the freshly-dumped WAL carries any embedding vector param.
    let mut lines_checked = 0u64;
    for path in jsonl_files(&fresh_dir) {
        let content = std::fs::read_to_string(&path).unwrap();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let wal_line: lcg_core::WalLine =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("bad WAL line: {e}: {line}"));
            if let Some(params) = wal_line.params.as_object() {
                for key in VECTOR_PARAM_KEYS {
                    assert!(
                        !params.contains_key(*key),
                        "freshly-dumped WAL line carries a stripped vector param {key:?}: {line}"
                    );
                }
            }
            lines_checked += 1;
        }
    }
    assert!(lines_checked > 0, "the fresh dump must contain WAL lines");

    // SC-001: report and verify the measured byte-size reduction.
    let fresh_bytes = total_bytes(&fresh_dir);
    let reduction = 1.0 - (fresh_bytes as f64 / original_bytes as f64);
    println!(
        "[SC-001] WAL size: original (vector-bearing) = {original_bytes} bytes, fresh \
         (stripped) = {fresh_bytes} bytes, reduction = {:.1}% (previously measured on the #217 \
         capture: 89.9%)",
        reduction * 100.0,
    );
    assert!(
        reduction > 0.5,
        "expected a dramatic size reduction from stripping embedding vectors, got only \
         {:.1}% (original={original_bytes}, fresh={fresh_bytes})",
        reduction * 100.0
    );
}
