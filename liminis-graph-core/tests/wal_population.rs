// Integration tests for WAL population (issue #74).
//
// Verifies that production write handlers populate the application WAL directory
// with JSONL mutation lines, and that WAL replay reconstructs the DB state.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    replay::WalReplayer,
    telemetry::{NoopSink, TelemetrySink},
    WalWriter,
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

fn make_state_with_wal(db: Arc<Db>, wal_dir: std::path::PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(&wal_dir, 10_000, 0).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: Some(wal_dir),
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(wal_writer)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
    })
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
        wal_dir: None,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
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

    // WAL must be populated
    assert!(
        has_wal_files(wal_dir.path()),
        "WAL directory must contain at least one JSONL file after add_episode"
    );
    let line_count = count_wal_lines(wal_dir.path());
    assert!(
        line_count >= 1,
        "WAL must contain at least one mutation line, got {line_count}"
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

    let lines_before = count_wal_lines(wal_dir.path());

    // Delete the episode.
    let del_v = dispatch(
        4,
        "knowledge_delete_episode",
        json!({"episode_uuid": ep_uuid}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(del_v["result"]["status"], "deleted", "{del_v}");

    let lines_after = count_wal_lines(wal_dir.path());
    assert!(
        lines_after > lines_before,
        "WAL must grow after delete_episode (before={lines_before}, after={lines_after})"
    );

    // At least one WAL line must contain a DELETE or DETACH keyword.
    let all_content: String = std::fs::read_dir(wal_dir.path())
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

    // WAL must be populated before attempting rebuild.
    assert!(
        has_wal_files(wal_dir.path()),
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
    let replayer = WalReplayer::new(wal_dir.path());
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
