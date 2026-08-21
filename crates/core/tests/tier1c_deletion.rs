// Tier 1c deletion tests: delete_by_source, delete_chunk_episode, clear_all.
//
// Each test exercises the IPC handler via handlers::dispatch() in-process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

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
    wal_group::DEFAULT_GROUP_ID,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.db");
    let db = Arc::new(Db::open(path.to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>, db_path: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
        wal_root: None,
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
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
    })
}

/// `wal_root` becomes `AppState.wal_root` (issue #378): a WAL root containing one subdirectory
/// per group_id. `wal_writers` starts empty — per-group writers are created lazily on first
/// write (FR-003), matching a fresh install.
fn make_state_with_wal(db: Arc<Db>, wal_root: std::path::PathBuf, db_path: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_string(),
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
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
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

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn assert_ok(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

fn assert_err(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_some(), "expected error field: {v}");
}

async fn process_chunk(id: i64, chunk_id: &str, source_file: &str, state: Arc<AppState>) -> Value {
    dispatch_val(
        id,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": chunk_id,
            "source_file": source_file,
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        state,
    )
    .await
}

async fn status_counts(id: i64, state: Arc<AppState>) -> (u64, u64, u64) {
    let v = dispatch_val(id, "knowledge_status", json!({}), state).await;
    let r = &v["result"];
    (
        r["entity_count"].as_u64().unwrap_or(0),
        r["episode_count"].as_u64().unwrap_or(0),
        r["relationship_count"].as_u64().unwrap_or(0),
    )
}

/// Adds an episode directly via `knowledge_add_episode`, naming both `name` (matched by
/// `knowledge_delete_chunk_episode`) and `source_description` (matched by
/// `knowledge_delete_by_source`) explicitly, so cross-group fixtures can share either value
/// across groups without going through `process_chunk` (which always lands in
/// `DEFAULT_GROUP_ID`).
async fn add_episode(
    id: i64,
    name: &str,
    source_description: &str,
    group_id: &str,
    state: Arc<AppState>,
) -> Value {
    dispatch_val(
        id,
        "knowledge_add_episode",
        json!({
            "name": name,
            "episode_body": "Alice works at Acme Corp.",
            "source": "text",
            "source_description": source_description,
            "reference_time": "2024-01-01T00:00:00Z",
            "group_id": group_id,
        }),
        state,
    )
    .await
}

async fn episode_count_in_group(id: i64, group_id: &str, state: Arc<AppState>) -> u64 {
    let v = dispatch_val(
        id,
        "knowledge_get_episodes",
        json!({"group_id": group_id, "last_n": 100}),
        state,
    )
    .await;
    v["result"]["count"].as_u64().unwrap_or(0)
}

// ── delete_by_source ──────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_by_source_basic() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Index 2 chunks under docs/a.md and 1 under docs/b.md
    assert_ok(
        &process_chunk(1, "chunk-a1", "docs/a.md", Arc::clone(&state)).await,
        1,
    );
    assert_ok(
        &process_chunk(2, "chunk-a2", "docs/a.md", Arc::clone(&state)).await,
        2,
    );
    assert_ok(
        &process_chunk(3, "chunk-b1", "docs/b.md", Arc::clone(&state)).await,
        3,
    );

    let (_, ep_before, _) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(ep_before, 3, "expected 3 episodes before delete");

    let v = dispatch_val(
        5,
        "knowledge_delete_by_source",
        json!({"source_file": "docs/a.md", "group_ids": [DEFAULT_GROUP_ID]}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 5);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 2, "expected 2 deleted: {v}");
    let uuids = v["result"]["deleted_uuids"].as_array().unwrap();
    assert_eq!(uuids.len(), 2, "expected 2 deleted_uuids: {v}");

    let (_, ep_after, _) = status_counts(6, Arc::clone(&state)).await;
    assert_eq!(ep_after, 1, "docs/b.md episode must survive");
}

#[tokio::test]
async fn delete_by_source_no_match() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    let v = dispatch_val(
        1,
        "knowledge_delete_by_source",
        json!({"source_file": "no-such-file.md", "group_ids": [DEFAULT_GROUP_ID]}),
        state,
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 0, "{v}");
    let uuids = v["result"]["deleted_uuids"].as_array().unwrap();
    assert!(uuids.is_empty(), "{v}");
}

#[tokio::test]
async fn delete_by_source_missing_param() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    let v = dispatch_val(1, "knowledge_delete_by_source", json!({}), state).await;
    assert_err(&v, 1);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("source_file"),
        "error should mention source_file: {v}"
    );
}

/// FR-001/FR-002, SC-004: an unscoped `knowledge_delete_by_source` call — group_ids omitted,
/// null, or `[]` — must be rejected with an error naming `group_ids`, and must not delete
/// anything in any group.
#[tokio::test]
async fn delete_by_source_requires_group_ids() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    assert_ok(
        &add_episode(1, "ep-a", "doc.md", "group-a", Arc::clone(&state)).await,
        1,
    );
    assert_ok(
        &add_episode(2, "ep-b", "doc.md", "group-b", Arc::clone(&state)).await,
        2,
    );

    for (id, params) in [
        (3, json!({"source_file": "doc.md"})),
        (4, json!({"source_file": "doc.md", "group_ids": null})),
        (5, json!({"source_file": "doc.md", "group_ids": []})),
    ] {
        let v = dispatch_val(id, "knowledge_delete_by_source", params, Arc::clone(&state)).await;
        assert_err(&v, id);
        let msg = v["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("group_ids"),
            "error should mention group_ids: {v}"
        );
    }

    assert_eq!(
        episode_count_in_group(6, "group-a", Arc::clone(&state)).await,
        1,
        "group-a row must survive an unscoped call"
    );
    assert_eq!(
        episode_count_in_group(7, "group-b", Arc::clone(&state)).await,
        1,
        "group-b row must survive an unscoped call"
    );
}

/// FR-003/FR-007, SC-005: a scoped `knowledge_delete_by_source` call touches only the named
/// group's rows, both for an exact `source_description` match and a `source_file:` prefix
/// match; a same-source row in another group is untouched.
#[tokio::test]
async fn delete_by_source_scoped_to_one_group() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Exact match in both groups.
    assert_ok(
        &add_episode(1, "ep-a1", "doc.md", "group-a", Arc::clone(&state)).await,
        1,
    );
    assert_ok(
        &add_episode(2, "ep-b1", "doc.md", "group-b", Arc::clone(&state)).await,
        2,
    );
    // Prefix match in both groups.
    assert_ok(
        &add_episode(3, "ep-a2", "doc.md:section1", "group-a", Arc::clone(&state)).await,
        3,
    );
    assert_ok(
        &add_episode(4, "ep-b2", "doc.md:section1", "group-b", Arc::clone(&state)).await,
        4,
    );

    let v = dispatch_val(
        5,
        "knowledge_delete_by_source",
        json!({"source_file": "doc.md", "group_ids": ["group-a"]}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 5);
    assert_eq!(
        v["result"]["deleted_count"], 2,
        "both exact and prefix matches in group-a must be deleted: {v}"
    );

    assert_eq!(
        episode_count_in_group(6, "group-a", Arc::clone(&state)).await,
        0,
        "group-a rows must be gone"
    );
    assert_eq!(
        episode_count_in_group(7, "group-b", Arc::clone(&state)).await,
        2,
        "group-b rows (exact and prefix) must survive"
    );
}

// ── delete_chunk_episode ──────────────────────────────────────────────────────

#[tokio::test]
async fn delete_chunk_episode_basic() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    assert_ok(
        &process_chunk(1, "my-chunk", "file.txt", Arc::clone(&state)).await,
        1,
    );
    let (_, ep_before, _) = status_counts(2, Arc::clone(&state)).await;
    assert_eq!(ep_before, 1, "expected 1 episode before delete");

    let v = dispatch_val(
        3,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "my-chunk", "group_ids": [DEFAULT_GROUP_ID]}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 3);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 1, "{v}");

    let (_, ep_after, _) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(ep_after, 0, "episode should be gone");
}

#[tokio::test]
async fn delete_chunk_episode_all_revisions() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Process the same chunk_id twice — append-on-revision creates 2 episodes
    assert_ok(
        &process_chunk(1, "rev-chunk", "file.txt", Arc::clone(&state)).await,
        1,
    );
    assert_ok(
        &process_chunk(2, "rev-chunk", "file.txt", Arc::clone(&state)).await,
        2,
    );

    let (_, ep_before, _) = status_counts(3, Arc::clone(&state)).await;
    assert_eq!(ep_before, 2, "expected 2 episodes (2 revisions)");

    let v = dispatch_val(
        4,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "rev-chunk", "group_ids": [DEFAULT_GROUP_ID]}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 4);
    assert_eq!(
        v["result"]["deleted_count"], 2,
        "both revisions must be deleted: {v}"
    );

    let (_, ep_after, _) = status_counts(5, Arc::clone(&state)).await;
    assert_eq!(ep_after, 0, "all revisions should be gone");
}

#[tokio::test]
async fn delete_chunk_episode_no_match() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    let v = dispatch_val(
        1,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "nonexistent-chunk", "group_ids": [DEFAULT_GROUP_ID]}),
        state,
    )
    .await;
    assert_ok(&v, 1);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["deleted_count"], 0, "{v}");
}

/// FR-001/FR-002, SC-001: an unscoped `knowledge_delete_chunk_episode` call — group_ids
/// omitted, null, or `[]` — must be rejected with an error naming `group_ids`, and must not
/// delete anything in any group, even when the same `chunk_id` collides across groups.
#[tokio::test]
async fn delete_chunk_episode_requires_group_ids() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    assert_ok(
        &add_episode(
            1,
            "shared-chunk",
            "docs/shared.md",
            "group-a",
            Arc::clone(&state),
        )
        .await,
        1,
    );
    assert_ok(
        &add_episode(
            2,
            "shared-chunk",
            "docs/shared.md",
            "group-b",
            Arc::clone(&state),
        )
        .await,
        2,
    );

    for (id, params) in [
        (3, json!({"chunk_id": "shared-chunk"})),
        (4, json!({"chunk_id": "shared-chunk", "group_ids": null})),
        (5, json!({"chunk_id": "shared-chunk", "group_ids": []})),
    ] {
        let v = dispatch_val(
            id,
            "knowledge_delete_chunk_episode",
            params,
            Arc::clone(&state),
        )
        .await;
        assert_err(&v, id);
        let msg = v["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("group_ids"),
            "error should mention group_ids: {v}"
        );
    }

    assert_eq!(
        episode_count_in_group(6, "group-a", Arc::clone(&state)).await,
        1,
        "group-a row must survive an unscoped call"
    );
    assert_eq!(
        episode_count_in_group(7, "group-b", Arc::clone(&state)).await,
        1,
        "group-b row must survive an unscoped call"
    );
}

/// FR-003/FR-006, SC-002: a scoped `knowledge_delete_chunk_episode` call touches only the
/// named group's row(s); a same-name row in another group is untouched.
#[tokio::test]
async fn delete_chunk_episode_scoped_to_one_group() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    assert_ok(
        &add_episode(
            1,
            "shared-chunk",
            "docs/shared.md",
            "group-a",
            Arc::clone(&state),
        )
        .await,
        1,
    );
    assert_ok(
        &add_episode(
            2,
            "shared-chunk",
            "docs/shared.md",
            "group-b",
            Arc::clone(&state),
        )
        .await,
        2,
    );

    let v = dispatch_val(
        3,
        "knowledge_delete_chunk_episode",
        json!({"chunk_id": "shared-chunk", "group_ids": ["group-a"]}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 3);
    assert_eq!(v["result"]["deleted_count"], 1, "{v}");

    assert_eq!(
        episode_count_in_group(4, "group-a", Arc::clone(&state)).await,
        0,
        "group-a row must be gone"
    );
    assert_eq!(
        episode_count_in_group(5, "group-b", Arc::clone(&state)).await,
        1,
        "group-b row must survive"
    );
}

// ── clear_all ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn clear_all_rejected_without_confirm() {
    let (db, _dir) = make_db(4);
    let state = make_state(db, "test.db");

    // Populate with known content
    assert_ok(
        &process_chunk(1, "c1", "f.txt", Arc::clone(&state)).await,
        1,
    );
    let (_, ep_before, _) = status_counts(2, Arc::clone(&state)).await;
    assert_eq!(ep_before, 1, "need content to verify no-op");

    // Call without confirm
    let v = dispatch_val(
        3,
        "knowledge_clear_all",
        json!({"confirm": false}),
        Arc::clone(&state),
    )
    .await;
    assert_err(&v, 3);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("confirm"),
        "error should mention 'confirm': {v}"
    );

    // DB unchanged
    let (_, ep_after, _) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(ep_after, 1, "DB must be unchanged after rejected clear_all");
}

#[tokio::test]
async fn clear_all_wipes_and_reinitializes() {
    let (db, dir) = make_db(4);
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let state = make_state(db, &db_path);

    // Populate
    assert_ok(
        &process_chunk(1, "c1", "f.txt", Arc::clone(&state)).await,
        1,
    );
    let (_, ep_before, _) = status_counts(2, Arc::clone(&state)).await;
    assert_eq!(ep_before, 1, "need content before clear");

    // Clear
    let v = dispatch_val(
        3,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 3);
    assert_eq!(v["result"]["success"], true, "{v}");

    // All counts should be zero
    let (entities, episodes, edges) = status_counts(4, Arc::clone(&state)).await;
    assert_eq!(entities, 0, "entity_count must be 0 after clear_all");
    assert_eq!(episodes, 0, "episode_count must be 0 after clear_all");
    assert_eq!(edges, 0, "relationship_count must be 0 after clear_all");
}

#[tokio::test]
async fn clear_all_followed_by_process_chunk() {
    let (db, dir) = make_db(4);
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let state = make_state(db, &db_path);

    // Clear the DB
    let clear = dispatch_val(
        1,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&clear, 1);

    // Service must still accept new writes after clear
    let ingest = process_chunk(2, "post-clear-chunk", "fresh.txt", Arc::clone(&state)).await;
    assert_ok(&ingest, 2);
    assert_eq!(ingest["result"]["success"], true, "{ingest}");

    let (_, ep_count, _) = status_counts(3, Arc::clone(&state)).await;
    assert_eq!(ep_count, 1, "new episode should appear after clear_all");
}

#[tokio::test]
async fn clear_all_reinits_wal_writer() {
    // Regression test for #100, updated for issue #378's per-group writer map: post-Recreate
    // writes must still be captured in the WAL. Unlike the pre-378 eager re-init, the fix is now
    // lazy per-group creation (FR-003) — clear_all clears the whole wal_writers map and creates
    // nothing eagerly, but the very next write to any group must still create its writer and
    // capture the mutation, exactly as it would on a fresh install.
    let (db, dir) = make_db(4);
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let wal_root = dir.path().join("wal");
    std::fs::create_dir_all(&wal_root).unwrap();
    let state = make_state_with_wal(db, wal_root.clone(), &db_path);

    let v = dispatch_val(
        1,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&v, 1);

    // clear_all clears the whole map — nothing is eagerly recreated (issue #378).
    assert!(
        state.wal_writers.lock().unwrap().is_empty(),
        "wal_writers must be empty immediately after clear_all — lazy creation handles the rest"
    );

    // The next write must still lazily create a writer and capture the mutation (the #100 fix,
    // preserved under lazy per-group creation).
    let ingest = dispatch_val(
        2,
        "knowledge_add_episode",
        json!({
            "name": "post-clear-chunk",
            "episode_body": "Alice works at Acme Corp.",
            "source": "test",
            "source_description": "test/source",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "liminis"
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok(&ingest, 2);

    assert!(
        state.wal_writers.lock().unwrap().contains_key("liminis"),
        "wal_writers must contain a lazily-created 'liminis' writer after the post-clear write"
    );
    let liminis_dir = wal_root.join("liminis");
    let has_jsonl = std::fs::read_dir(&liminis_dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        })
        .unwrap_or(false);
    assert!(
        has_jsonl,
        "post-clear write must land on disk under the group's own WAL directory (#100 fix)"
    );
}
