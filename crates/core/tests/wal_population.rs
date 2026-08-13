// Integration tests for WAL population (issue #74).
//
// Verifies that production write handlers populate the application WAL directory
// with JSONL mutation lines, and that WAL replay reconstructs the DB state.

use std::collections::HashMap;
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
    replay::WalReplayer,
    telemetry::{NoopSink, TelemetrySink},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

// ── helpers ───────────────────────────────────────────────────────────────────

const EMB_DIM: usize = 4;

fn make_db() -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("wal_pop_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }
    (db, dir)
}

/// `wal_root` becomes `AppState.wal_root` (issue #378): a WAL **root** containing one
/// subdirectory per `group_id`, not a single shared stream. Per-group writers/directories are
/// created lazily on first write (FR-003) — callers that need to inspect a particular group's
/// WAL files directly should look under `wal_root.join(<that group's dir name>)`, e.g. via
/// [`group_wal_dir`].
fn make_state_with_wal(db: Arc<Db>, wal_root: std::path::PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
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
    })
}

/// A `group_id` used by this file's tests always already satisfies
/// `checkpoint::validate_name`'s charset, so its WAL directory name is the `group_id` itself,
/// unchanged (`wal_group::encode_group_dir_name`'s "already safe" case).
fn group_wal_dir(wal_root: &std::path::Path, group_id: &str) -> std::path::PathBuf {
    wal_root.join(group_id)
}

fn make_state_no_wal(db: Arc<Db>) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
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

/// Counts the total number of JSONL lines across all `.jsonl` files in `dir`.
fn count_wal_lines(dir: &std::path::Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    let mut total = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            total += content.lines().filter(|l| !l.trim().is_empty()).count();
        }
    }
    total
}

fn has_wal_files(dir: &std::path::Path) -> bool {
    if !dir.exists() {
        return false;
    }
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        })
        .unwrap_or(false)
}

// ── User Story 1a: add_episode with WAL dir → WAL files created ───────────────

/// After knowledge_add_episode, the WAL directory must contain at least one JSONL
/// file with at least one mutation line.
#[tokio::test]
async fn test_add_episode_populates_wal() {
    let (db, _db_dir) = make_db();
    let wal_dir = TempDir::new().unwrap();

    let state = make_state_with_wal(db, wal_dir.path().to_path_buf());

    let v = dispatch(
        1,
        "knowledge_add_episode",
        json!({
            "name": "test-chunk",
            "episode_body": "Alice works at Acme Corp.",
            "source": "test",
            "source_description": "test/source",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "test"
        }),
        Arc::clone(&state),
    )
    .await;

    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(
        v["result"]["episode_uuid"].as_str().is_some(),
        "expected episode_uuid: {v}"
    );

    // WAL must be populated, under this episode's own group's subdirectory (issue #378).
    let group_dir = group_wal_dir(wal_dir.path(), "test");
    assert!(
        has_wal_files(&group_dir),
        "WAL directory must contain at least one JSONL file after add_episode"
    );
    let line_count = count_wal_lines(&group_dir);
    assert!(
        line_count >= 1,
        "WAL must contain at least one mutation line, got {line_count}"
    );
}

/// Returns the max `seq` across all WAL lines in `dir`, or `None` if none are found.
fn max_wal_seq(dir: &std::path::Path) -> Option<u64> {
    let mut max: Option<u64> = None;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let val: Value = serde_json::from_str(line).unwrap();
            let seq = val["seq"].as_u64().unwrap();
            max = Some(max.map_or(seq, |m: u64| m.max(seq)));
        }
    }
    max
}

/// SC-001 (partial, live-write half): after `knowledge_add_episode`, the persisted
/// `applied_seq` must equal the max `seq` of the WAL lines just written for that chunk.
#[tokio::test]
async fn test_add_episode_advances_applied_seq_to_chunk_max_seq() {
    let (db, _db_dir) = make_db();
    let wal_dir = TempDir::new().unwrap();

    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    let v = dispatch(
        1,
        "knowledge_add_episode",
        json!({
            "name": "applied-seq-chunk",
            "episode_body": "Alice works at Acme Corp.",
            "source": "test",
            "source_description": "test/source",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "test"
        }),
        Arc::clone(&state),
    )
    .await;
    assert!(v.get("result").is_some(), "expected result, got: {v}");

    let group_dir = group_wal_dir(wal_dir.path(), "test");
    let expected_seq = max_wal_seq(&group_dir).expect("WAL must contain at least one line");

    let conn = db.connect().unwrap();
    assert_eq!(
        conn.get_applied_seq("test").unwrap(),
        Some(expected_seq),
        "applied_seq must equal the chunk's max WAL seq"
    );
}

// ── User Story 1c: no WAL dir → writes succeed, no WAL dir created ────────────

/// When no WAL directory is configured, writes succeed normally and no WAL
/// directory is created (WAL is opt-in, never blocking).
#[tokio::test]
async fn test_add_episode_without_wal_dir_succeeds() {
    let (db, _db_dir) = make_db();
    let state = make_state_no_wal(db.clone());

    let v = dispatch(
        2,
        "knowledge_add_episode",
        json!({
            "name": "no-wal-chunk",
            "episode_body": "Bob manages the project.",
            "source": "test",
            "source_description": "test/source",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "test"
        }),
        state,
    )
    .await;

    assert!(
        v.get("result").is_some(),
        "write must succeed without WAL: {v}"
    );
    assert!(
        v["result"]["episode_uuid"].as_str().is_some(),
        "expected episode_uuid: {v}"
    );

    // DB must have the episodic node
    let conn = db.connect().unwrap();
    let ep_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(ep_count, 1, "episodic node must exist in DB");
}

// ── Mutation WAL content: delete handler logs DELETE cypher ───────────────────

/// After knowledge_delete_episode, a DELETE mutation must appear in the WAL.
#[tokio::test]
async fn test_delete_episode_appends_to_wal() {
    let (db, _db_dir) = make_db();
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    // First ingest an episode so we have something to delete.
    let add_v = dispatch(
        3,
        "knowledge_add_episode",
        json!({
            "name": "to-delete-chunk",
            "episode_body": "Carol is an engineer.",
            "source": "test",
            "source_description": "test/delete",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "test"
        }),
        Arc::clone(&state),
    )
    .await;
    let ep_uuid = add_v["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid");

    // knowledge_delete_episode deletes by UUID with no group_id in the request, but the target
    // episode's own group is looked up before the delete runs — the delete mutation lands in
    // "test" (the episode's own group), not the default "liminis" group.
    let owning_group_dir = group_wal_dir(wal_dir.path(), "test");
    let lines_before = count_wal_lines(&owning_group_dir);

    // Delete the episode.
    let del_v = dispatch(
        4,
        "knowledge_delete_episode",
        json!({"episode_uuid": ep_uuid}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(del_v["result"]["status"], "deleted", "{del_v}");

    let lines_after = count_wal_lines(&owning_group_dir);
    assert!(
        lines_after > lines_before,
        "WAL must grow after delete_episode (before={lines_before}, after={lines_after})"
    );

    // At least one WAL line must contain a DELETE or DETACH keyword.
    let all_content: String = std::fs::read_dir(&owning_group_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect();
    let has_delete_line = all_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .any(|l| {
            let upper = l.to_uppercase();
            upper.contains("DELETE") || upper.contains("DETACH")
        });
    assert!(
        has_delete_line,
        "WAL must contain a DELETE mutation after delete_episode"
    );
}

// ── User Story 2: WAL rebuild reproduces DB counts ────────────────────────────

/// Ingest episodes, populate WAL. Open a fresh empty DB. Replay WAL.
/// Entity and Episodic counts must match the post-ingestion baseline.
#[tokio::test]
async fn test_wal_rebuild_reproduces_counts() {
    let (db, _db_dir) = make_db();
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    // Ingest two episodes.
    for i in 0..2 {
        let v = dispatch(
            10 + i,
            "knowledge_add_episode",
            json!({
                "name": format!("rebuild-chunk-{i}"),
                "episode_body": format!("Episode {i} body text about Alice and Acme Corp."),
                "source": "test",
                "source_description": format!("test/rebuild/{i}"),
                "reference_time": "2026-01-01 00:00:00",
                "group_id": "rebuild_test"
            }),
            Arc::clone(&state),
        )
        .await;
        assert!(v.get("result").is_some(), "add_episode {i} failed: {v}");
    }

    // Record baseline counts from original DB.
    let entity_count_orig = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    let episodic_count_orig = {
        let conn = db.connect().unwrap();
        conn.count_nodes("Episodic").unwrap()
    };
    let edge_count_orig = {
        let conn = db.connect().unwrap();
        conn.count_relates_to_edges().unwrap()
    };

    assert!(entity_count_orig > 0, "original DB must have entities");
    assert_eq!(
        episodic_count_orig, 2,
        "original DB must have 2 episodic nodes"
    );
    assert!(
        edge_count_orig > 0,
        "original DB must have RELATES_TO edges"
    );

    let group_dir = group_wal_dir(wal_dir.path(), "rebuild_test");

    // WAL must be populated before attempting rebuild.
    assert!(
        has_wal_files(&group_dir),
        "WAL must be populated before rebuild test"
    );

    // Create a fresh empty DB with schema.
    let rebuild_dir = TempDir::new().unwrap();
    let rebuild_db =
        Arc::new(Db::open(rebuild_dir.path().join("rebuild.db").to_str().unwrap()).unwrap());
    {
        let conn = rebuild_db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }

    // Replay the WAL into the fresh DB.
    let replayer = WalReplayer::new(&group_dir);
    let conn = rebuild_db.connect().unwrap();
    let stats = replayer.replay(&conn).unwrap();

    assert!(
        stats.lines_replayed > 0,
        "WAL replay must process at least one mutation line"
    );

    // Drop conn before counting (Conn holds a borrow on rebuild_db).
    drop(conn);

    // Counts in the rebuilt DB must match the original.
    let entity_count_rebuilt = {
        let conn = rebuild_db.connect().unwrap();
        conn.count_nodes("Entity").unwrap()
    };
    let episodic_count_rebuilt = {
        let conn = rebuild_db.connect().unwrap();
        conn.count_nodes("Episodic").unwrap()
    };
    let edge_count_rebuilt = {
        let conn = rebuild_db.connect().unwrap();
        conn.count_relates_to_edges().unwrap()
    };

    assert_eq!(
        entity_count_orig, entity_count_rebuilt,
        "rebuilt DB entity count must match original (orig={entity_count_orig}, rebuilt={entity_count_rebuilt})"
    );
    assert_eq!(
        episodic_count_orig, episodic_count_rebuilt,
        "rebuilt DB episodic count must match original (orig={episodic_count_orig}, rebuilt={episodic_count_rebuilt})"
    );
    assert_eq!(
        edge_count_orig, edge_count_rebuilt,
        "rebuilt DB RELATES_TO edge count must match original (orig={edge_count_orig}, rebuilt={edge_count_rebuilt})"
    );
}

// ── SC-004/FR-008/FR-009: relation_type survives WAL round-trip ───────────────

// SC-004: relation_type written to the WAL (via INSERT Cypher) and reproduced
// faithfully after a full wipe + replay cycle.
#[tokio::test]
async fn relation_type_survives_wal_round_trip() {
    let (db, _db_dir) = make_db();
    let wal_dir = TempDir::new().unwrap();

    let state = make_state_with_wal(db.clone(), wal_dir.path().to_path_buf());

    // Ingest one episode; MockExtractor produces Alice --WORKS_AT--> Acme Corp.
    let v = dispatch(
        100,
        "knowledge_add_episode",
        json!({
            "name": "rt-chunk",
            "episode_body": "Alice works at Acme Corp.",
            "source": "test",
            "source_description": "test/rt",
            "reference_time": "2026-01-01 00:00:00",
            "group_id": "rt_group"
        }),
        Arc::clone(&state),
    )
    .await;
    assert!(v.get("result").is_some(), "add_episode failed: {v}");

    // Verify the original DB has edges with WORKS_AT.
    let orig_edges = {
        let conn = db.connect().unwrap();
        conn.list_relationships(None, 100).unwrap()
    };
    assert!(
        !orig_edges.is_empty(),
        "original DB must have at least one edge"
    );
    for e in &orig_edges {
        assert_eq!(
            e.relation_type.as_deref(),
            Some("WORKS_AT"),
            "original edge must have relation_type=WORKS_AT"
        );
    }

    let group_dir = group_wal_dir(wal_dir.path(), "rt_group");

    // WAL must be populated before attempting rebuild.
    assert!(
        has_wal_files(&group_dir),
        "WAL must be populated before round-trip test"
    );

    // Create a fresh empty DB and replay the WAL into it.
    let rebuild_dir = TempDir::new().unwrap();
    let rebuild_db =
        Arc::new(Db::open(rebuild_dir.path().join("rt_rebuild.db").to_str().unwrap()).unwrap());
    {
        let conn = rebuild_db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }

    let replayer = WalReplayer::new(&group_dir);
    let conn = rebuild_db.connect().unwrap();
    let stats = replayer.replay(&conn).unwrap();
    assert!(
        stats.lines_replayed > 0,
        "WAL replay must process at least one mutation line"
    );
    drop(conn);

    // Edges in the rebuilt DB must have the same relation_type as the originals.
    let rebuilt_edges = {
        let conn = rebuild_db.connect().unwrap();
        conn.list_relationships(None, 100).unwrap()
    };
    assert_eq!(
        orig_edges.len(),
        rebuilt_edges.len(),
        "rebuilt DB must have same edge count as original"
    );
    for e in &rebuilt_edges {
        assert_eq!(
            e.relation_type.as_deref(),
            Some("WORKS_AT"),
            "replayed edge must have relation_type=WORKS_AT; got {:?}",
            e.relation_type
        );
    }
}

// ── Issue #378, User Story 1 / SC-004: writes to two groups never cross streams ────────────────

/// SC-004 / US1 AS1-3: episodes interleaved between two groups land in two independent WAL
/// directories, and every line in each directory names only its own group — inspected via WAL
/// file content directly, not just graph state. Also covers AS1/AS2: neither group's directory
/// or writer exists until that group's first write, and creating B's writer never disturbs A's
/// already-flushed content.
#[tokio::test]
async fn add_episode_to_two_groups_never_crosses_wal_streams() {
    let (db, _db_dir) = make_db();
    let wal_root = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_root.path().to_path_buf());

    let group_a_dir = group_wal_dir(wal_root.path(), "group-a");
    let group_b_dir = group_wal_dir(wal_root.path(), "group-b");

    // AS1: neither group has a directory yet.
    assert!(
        !group_a_dir.exists() && !group_b_dir.exists(),
        "no group directory should exist before any write"
    );

    async fn add(id: i64, group_id: &str, body: &str, state: Arc<AppState>) -> Value {
        dispatch(
            id,
            "knowledge_add_episode",
            json!({
                "name": format!("{group_id}-chunk-{id}"),
                "episode_body": body,
                "source": "test",
                "source_description": format!("test/{group_id}"),
                "reference_time": "2026-01-01 00:00:00",
                "group_id": group_id,
            }),
            state,
        )
        .await
    }

    // AS1: the first write to group A creates only A's directory and writer.
    let a1 = add(
        1,
        "group-a",
        "Alice works at Acme Corp.",
        Arc::clone(&state),
    )
    .await;
    assert!(a1.get("result").is_some(), "group-a write 1 failed: {a1}");
    assert!(group_a_dir.exists(), "group-a's directory must now exist");
    assert!(
        !group_b_dir.exists(),
        "group-b must remain untouched by group-a's first write"
    );

    // AS2: writing to a new group B creates a second, independent writer/directory; A is
    // unaffected (its file content, checked below, must not change).
    let a_lines_after_first_write = count_wal_lines(&group_a_dir);
    let b1 = add(
        2,
        "group-b",
        "Carol leads the design team.",
        Arc::clone(&state),
    )
    .await;
    assert!(b1.get("result").is_some(), "group-b write 1 failed: {b1}");
    assert!(group_b_dir.exists(), "group-b's directory must now exist");
    assert_eq!(
        count_wal_lines(&group_a_dir),
        a_lines_after_first_write,
        "group-a's WAL content must be unaffected by group-b's writer being created"
    );

    // Interleave further writes across both groups.
    let a2 = add(3, "group-a", "Bob manages the project.", Arc::clone(&state)).await;
    assert!(a2.get("result").is_some(), "group-a write 2 failed: {a2}");
    let b2 = add(4, "group-b", "Dave reviews the design.", Arc::clone(&state)).await;
    assert!(b2.get("result").is_some(), "group-b write 2 failed: {b2}");

    // AS3/SC-004: every line under group A's directory names group A, and every line under
    // group B's directory names group B — inspected via raw WAL file content.
    for (dir, expected_group, other_group) in [
        (&group_a_dir, "group-a", "group-b"),
        (&group_b_dir, "group-b", "group-a"),
    ] {
        assert!(
            has_wal_files(dir),
            "{expected_group}'s directory must contain WAL files"
        );
        let mut saw_a_mutation_line = false;
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            for line in content.lines().filter(|l| !l.trim().is_empty()) {
                let v: Value = serde_json::from_str(line).unwrap();
                // Every mutation line's params carry the episode/entity's own group_id
                // somewhere in the recorded Cypher params (e.g. "group_id":"group-a"). A line
                // that mentions the *other* group's id anywhere would indicate a cross-stream
                // leak; a line naming neither (e.g. an index-DDL/no-param line, which
                // log_mutation filters out already) is not expected here since every recorded
                // line comes from an actual mutation with group_id bound.
                let text = line.to_string();
                if text.contains(&format!("\"{expected_group}\"")) {
                    saw_a_mutation_line = true;
                }
                assert!(
                    !text.contains(&format!("\"{other_group}\"")),
                    "{expected_group}'s WAL directory must never contain a line naming \
                     {other_group} — no mutation may cross a stream boundary (SC-004): {v}"
                );
            }
        }
        assert!(
            saw_a_mutation_line,
            "{expected_group}'s directory must contain at least one line naming its own group"
        );
    }
}
