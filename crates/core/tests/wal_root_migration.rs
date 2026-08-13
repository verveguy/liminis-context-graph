// User Story 3 / SC-005 (issue #378): a pre-378, single-stream `LCG_WAL_DIR` — loose `*.jsonl`
// files, a `.checkpoints/` store (#365), and a `.wal-bounds.json` manifest (#375), all directly
// at the WAL directory's top level, with no `liminis/` subdirectory — must open under the
// upgraded binary exactly as it did before: same applied position, every existing checkpoint's
// `reachable` flag preserved. This is the end-to-end counterpart to `wal_group.rs`'s unit tests,
// which cover the filesystem relocation mechanics in isolation; this file drives the same
// fixture through `wal_group::migrate_wal_root_if_needed` and then through real IPC dispatch
// (`knowledge_wal_mark_list`, `knowledge_status`) to confirm the migration is invisible to a
// caller that only knows the pre-378 shape.

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
    telemetry::{NoopSink, TelemetrySink},
    wal_group, EntityRow,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("migration_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
    }
    (db, dir)
}

fn make_state_with_wal(db: Arc<Db>, wal_root: std::path::PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
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

/// Writes a checkpoint's on-disk record directly, in the exact shape `checkpoint::create`
/// produces (`<wal_dir>/.checkpoints/<name>/g1.create.json`, `checkpoint.rs`'s private
/// `CreateRecord`), since `checkpoint` is `pub(crate)` and not reachable from an integration
/// test — this fixture stands in for a checkpoint that was already created by a pre-378 binary.
fn write_legacy_checkpoint(wal_dir: &std::path::Path, name: &str, seq: Option<u64>) {
    let dir = wal_dir.join(".checkpoints").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("g1.create.json"),
        json!({"name": name, "seq": seq, "created_at_ms": 0}).to_string(),
    )
    .unwrap();
}

fn entity_wal_line(seq: u64, uuid: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{uuid}', n.group_id = 'liminis', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    };
    let resp = handlers::dispatch(req, state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn assert_ok_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

/// SC-005 end to end: a pre-378 flat `LCG_WAL_DIR` (loose `.jsonl` + `.checkpoints/` +
/// `.wal-bounds.json`, no `liminis/` subdir) is opened by the upgraded binary. Migration runs
/// automatically, the graph's applied position is unchanged, and the pre-existing checkpoint's
/// `reachable` flag survives — both checked through real IPC dispatch, not just the filesystem.
#[tokio::test]
async fn pre_378_flat_wal_dir_migrates_and_preserves_position_and_checkpoint_reachability() {
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    // Pre-378 shape: two WAL files sitting directly at the WAL directory's top level, covering
    // seq 0 and seq 1 — no "liminis" subdirectory exists yet.
    std::fs::write(
        wal_root.join("20260101_000000_aaaaaa_0000.jsonl"),
        entity_wal_line(0, "pre-migration-entity-0") + "\n",
    )
    .unwrap();
    std::fs::write(
        wal_root.join("20260101_000100_bbbbbb_0000.jsonl"),
        entity_wal_line(1, "pre-migration-entity-1") + "\n",
    )
    .unwrap();

    // A pre-existing named checkpoint (#365), also at the pre-378 top level.
    write_legacy_checkpoint(&wal_root, "before-upgrade", Some(1));

    // The graph already has content and an applied position recorded under the default group —
    // this is what "position unchanged" means: the DB row is untouched by the (purely
    // filesystem) migration.
    let (db, _db_dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "pre-migration-entity-1".to_string(),
            name: "pre-migration-entity-1".to_string(),
            group_id: "liminis".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-05-22 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "s".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.set_applied_seq(wal_group::DEFAULT_GROUP_ID, 1)
            .unwrap();
    }

    // This is what the upgraded binary's startup does before constructing AppState/any
    // WalWriter (see AppState::from_env / main.rs) — run it explicitly here to mirror that path.
    wal_group::migrate_wal_root_if_needed(&wal_root).unwrap();

    // The pre-378 top-level artifacts are gone; everything now lives under "liminis/".
    assert!(!wal_root.join("20260101_000000_aaaaaa_0000.jsonl").exists());
    assert!(!wal_root.join(".checkpoints").exists());
    let default_dir = wal_root.join(wal_group::DEFAULT_GROUP_ID);
    assert!(default_dir
        .join("20260101_000000_aaaaaa_0000.jsonl")
        .is_file());
    assert!(default_dir
        .join("20260101_000100_bbbbbb_0000.jsonl")
        .is_file());

    let state = make_state_with_wal(db, wal_root.clone());

    // The checkpoint that existed before migration is still listed, at the same seq, still
    // reachable — its `wal_min_seq`/`wal_max_seq` bounds are computed against the migrated
    // directory's now-present WAL files, not an emptied one.
    let list_v = dispatch_val(1, "knowledge_wal_mark_list", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&list_v, 1);
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    let after = checkpoints
        .iter()
        .find(|c| c["name"] == "before-upgrade")
        .unwrap_or_else(|| panic!("checkpoint must survive migration: {list_v}"));
    assert_eq!(after["seq"], 1, "{list_v}");
    assert_eq!(
        after["reachable"], true,
        "a pre-existing checkpoint must remain reachable post-migration: {list_v}"
    );

    // The default group's applied position is unchanged (SC-005) and matches the WAL's own
    // max_seq, exactly as a pre-378 caller reading only the flat fields would expect.
    let status_v = dispatch_val(2, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&status_v, 2);
    assert_eq!(status_v["result"]["wal"]["applied_seq"], 1, "{status_v}");
    assert_eq!(status_v["result"]["wal"]["max_seq"], 1, "{status_v}");
}

/// A migration re-run against an already-migrated root (e.g. a second startup after the first
/// upgrade boot) must be a no-op — no double-migration, no loss of the checkpoint or WAL content
/// that already made it into `liminis/`.
#[tokio::test]
async fn migration_is_a_noop_on_second_startup() {
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    std::fs::write(
        wal_root.join("20260101_000000_aaaaaa_0000.jsonl"),
        entity_wal_line(0, "entity-0") + "\n",
    )
    .unwrap();
    write_legacy_checkpoint(&wal_root, "cp", Some(0));

    wal_group::migrate_wal_root_if_needed(&wal_root).unwrap();
    // Simulates the second startup's own migration call.
    wal_group::migrate_wal_root_if_needed(&wal_root).unwrap();

    let (db, _db_dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(wal_group::DEFAULT_GROUP_ID, 0)
            .unwrap();
    }
    let state = make_state_with_wal(db, wal_root);

    let list_v = dispatch_val(1, "knowledge_wal_mark_list", json!({}), state).await;
    assert_ok_resp(&list_v, 1);
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    let cp = checkpoints
        .iter()
        .find(|c| c["name"] == "cp")
        .unwrap_or_else(|| panic!("checkpoint must survive a second, no-op migration: {list_v}"));
    assert_eq!(cp["seq"], 0, "{list_v}");
    assert_eq!(cp["reachable"], true, "{list_v}");
}
