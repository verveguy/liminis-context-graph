// Issue #428, SC-001/SC-003: a genuine pre-0.13.0 flat WAL layout, migrated by the fixed
// binary, ends up with a readable `.wal-generation.json` and a working knowledge_rebuild_from_wal
// — reproducing the exact counts 0.13.1 produced against the same fixture before the regression
// shipped in 0.13.2.
//
// `real_corpus_e2e.rs` (#217) deliberately builds `AppState` directly against the fixture's
// `wal/` directory rather than via a migration path, so it never exercises
// `wal_group::migrate_wal_root_if_needed` — that gap is exactly what let #428's regression ship
// unnoticed (see the issue's Out of Scope section; fixing that stale-fixture gap itself is filed
// separately). This file adds the missing migration-path coverage: the fixture's 16 `*.jsonl`
// files are copied into a fresh temp directory at the WAL root's top level (the genuine pre-378
// flat shape — no `liminis/` subdirectory), migrated, then replayed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    telemetry::{NoopSink, TelemetrySink},
    wal_generation, wal_group,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_corpus_wal")
}

fn fixture_wal_dir() -> PathBuf {
    fixture_dir().join("wal")
}

fn expected_results() -> Value {
    let path = fixture_dir().join("expected_results.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("expected_results.json must be valid JSON")
}

/// Copies the fixture's 16 `*.jsonl` files directly into `dest` (no `liminis/` subdirectory) —
/// the genuine pre-378 flat `LCG_WAL_DIR` shape a ≤0.12.x install left on disk.
fn seed_flat_legacy_layout(dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    for entry in std::fs::read_dir(fixture_wal_dir()).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let dest_path = dest.join(path.file_name().unwrap());
            std::fs::copy(&path, &dest_path).unwrap();
        }
    }
}

fn make_state(wal_root: PathBuf, dim: usize) -> (Arc<AppState>, TempDir) {
    let db_dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(db_dir.path().join("migrated.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }

    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(dim)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_dir.path().join("migrated.db").display().to_string(),
        wal_root: Some(wal_root),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(HashMap::new())),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    });
    (state, db_dir)
}

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

async fn dispatch(id: i64, method: &str, params: Value, state: &Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), Arc::clone(state), None).await;
    let v = serde_json::to_value(resp).unwrap();
    assert!(v.get("error").is_none(), "{method} returned an error: {v}");
    v["result"].clone()
}

/// Synchronous rebuild via the streaming path (a `progress_tx` present takes the blocking code
/// path instead of spawning a background job) — same rationale as `real_corpus_e2e.rs`'s
/// identical helper.
async fn rebuild(state: &Arc<AppState>) -> Value {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let resp = handlers::dispatch(
        req(1, "knowledge_rebuild_from_wal", json!({})),
        Arc::clone(state),
        Some(tx),
    )
    .await;
    let v = serde_json::to_value(resp).unwrap();
    assert!(
        v.get("error").is_none(),
        "knowledge_rebuild_from_wal returned an error: {v}"
    );
    v["result"].clone()
}

// #[ignore]: a full replay + HNSW/FTS index build over the fixture's 1,506 entities takes
// roughly a minute (see real_corpus_wal/README.md, and real_corpus_e2e.rs's identical
// rationale) — excluded from the default `cargo test` run, exercised by CI's dedicated
// `real-corpus-e2e` job (`--ignored`, see .github/workflows/ci.yml) and locally via
// `cargo test -p lcg-core --test real_corpus_wal_migration_e2e --release -- --ignored`.
#[tokio::test]
#[ignore]
async fn migrated_flat_legacy_layout_stamps_a_generation_and_rebuild_succeeds() {
    let expected = expected_results();
    let dim = expected["embedding_dim"].as_u64().unwrap() as usize;

    let wal_root_dir = TempDir::new().unwrap();
    let wal_root = wal_root_dir.path().to_path_buf();
    seed_flat_legacy_layout(&wal_root);

    // The migration this issue fixes: relocate the flat layout into `<wal_root>/liminis/` and
    // (FR-001) stamp its generation as part of that same pass.
    wal_group::migrate_wal_root_if_needed(&wal_root).unwrap();

    let default_dir = wal_group::group_wal_dir(&wal_root, wal_group::DEFAULT_GROUP_ID).unwrap();
    assert!(
        wal_generation::read_generation(&default_dir).is_some(),
        "SC-001: a legacy flat-WAL workspace migrated by the fixed build must have a readable \
         .wal-generation.json for its group"
    );

    let (state, _db_dir) = make_state(wal_root.clone(), dim);

    // Without issue #428's fix, this call would refuse outright: the migration would have
    // relocated the files but left no generation, and the very first replay's completion write
    // would record a position, permanently tripping #414's unknown-generation guard on every
    // later call. Confirmed reachable here as a single first call, matching the reproduction.
    let rebuild_result = rebuild(&state).await;
    assert_eq!(rebuild_result["success"], true, "{rebuild_result}");

    let status = dispatch(2, "knowledge_status", json!({}), &state).await;
    assert_eq!(
        status["entity_count"], expected["entity_count"],
        "SC-003: entity_count mismatch: {status}"
    );
    assert_eq!(
        status["relationship_count"], expected["relationship_count"],
        "SC-003: relationship_count mismatch: {status}"
    );
    assert_eq!(
        status["episode_count"], expected["episode_count"],
        "SC-003: episode_count mismatch: {status}"
    );

    let group_positions = &status["wal_groups"][wal_group::DEFAULT_GROUP_ID];
    let applied_seq = group_positions["applied_seq"].as_u64().unwrap();
    let max_seq = group_positions["max_seq"].as_u64().unwrap();
    assert_eq!(
        applied_seq, max_seq,
        "SC-003: applied_seq must equal max_seq after a full replay: {group_positions}"
    );
    assert_eq!(
        applied_seq, 12481,
        "SC-003: applied_seq must reproduce 0.13.1's reported max_seq for this fixture: \
         {group_positions}"
    );
    assert_eq!(
        group_positions["generation_status"], "known",
        "the migrated-and-replayed group's generation must now be known: {group_positions}"
    );
}
