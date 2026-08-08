// IPC parity tests: structural JSON-RPC 2.0 correctness for all 11 wire methods.
//
// Each test calls handlers::dispatch() in-process and checks that:
//   1. The response is valid JSON-RPC 2.0 (has "jsonrpc":"2.0" and matching "id")
//   2. The result has the expected shape for that method
//
// Methods that require external embedding/extraction services (find_entities,
// find_relationships, add_episode) are exercised only for error-shape correctness —
// the embedder points at an unreachable address so HTTP fails with a wrapped -32000 error.
//
// To enable exact Python-vs-Rust parity comparison, capture fixtures with
// scripts/record_corpus.py and set PARITY_GOLDEN=1 (see tests/fixtures/README.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::{Conn, Db},
    dedup_adapter::PassthroughDedupAdapter,
    embedder::{MockEmbedder, OaiEmbedder},
    error::Error as LcgError,
    extractor::{ExtractOptions, Extractor, MockExtractor},
    handlers,
    ipc::IpcRequest,
    ontology::{EntityTypeDef, OntologyMode, RelationTypeDef},
    telemetry::{NoopSink, TelemetrySink},
    types::{ExtractedEntity, ExtractionOutcome, ExtractionResult},
    EntityRow, Ontology, RelatesToEdge, WalWriter,
};
use regex::Regex;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("parity.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(dim).unwrap();
        conn.create_vector_indexes().unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>) -> Arc<AppState> {
    // MockExtractor + PassthroughDedupAdapter + default Embedder (unreachable URL in CI)
    // Methods that call embed() will fail with -32000 — that's expected for those tests.
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

/// Same as `make_state`, but with `wal_dir` configured — required for
/// `knowledge_rebuild_from_wal` (issue #239's `force_clear` wire-contract parity tests).
///
/// Takes a real, resolvable `db_path` (not the placeholder `"test.db"` most of this file's
/// other `make_state*` helpers use) because `force_clear: true` invokes `clear_db_for_rebuild`,
/// which deletes and reopens the file at `db_path` — a placeholder path would either no-op
/// against a nonexistent file or, worse, create a stray `test.db` in the crate's working
/// directory (mirrors `handlers_wal_admin.rs`'s `make_state_with_wal_and_path`).
fn make_state_with_wal(db: Arc<Db>, wal_dir: PathBuf, db_path: String) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_dir: Some(wal_dir),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

/// Like `make_state_with_wal`, but with a real, live `WalWriter` (issue #353's `applied_seq`
/// tests need `knowledge_process_chunk` to actually append WAL lines through the normal
/// write path, unlike `make_state_with_wal`'s callers, which write WAL files directly to
/// disk and never go through `AppState.wal_writer`).
fn make_state_with_live_wal(db: Arc<Db>, wal_dir: PathBuf, db_path: String) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let wal_writer = WalWriter::new(&wal_dir, 10_000, 5 * 1024 * 1024).ok();
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path,
        wal_dir: Some(wal_dir),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(wal_writer)),
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

/// A standalone `Entity` WAL line, param-bound (mirrors `handlers_wal_admin.rs`'s helper of the
/// same shape) — used only to give `knowledge_rebuild_from_wal` valid content to replay after a
/// `force_clear`.
fn entity_wal_line(seq: u64, uuid: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{uuid}', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

fn make_state_with_ontology(db: Arc<Db>, ontology: Arc<Ontology>) -> Arc<AppState> {
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
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: Some(ontology),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

#[allow(dead_code)]
fn make_degraded_state(reason: &str) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some(reason.to_string()))),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test-degraded.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

fn assert_ok_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("result").is_some(), "expected result, got: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

fn assert_err_resp(v: &Value, id: i64, expected_code: i32) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_some(), "expected error field: {v}");
    assert_eq!(v["error"]["code"], expected_code, "wrong error code: {v}");
}

async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn parity_build_indices() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(1, "knowledge_build_indices", json!({}), state).await;
    assert_ok_resp(&v, 1);
    assert_eq!(v["result"]["status"], "ok");
}

#[tokio::test]
async fn parity_get_episodes_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        2,
        "knowledge_get_episodes",
        json!({"group_id": "parity_group", "last_n": 10}),
        state,
    )
    .await;
    assert_ok_resp(&v, 2);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(
        v["result"]["episodes"].is_array(),
        "expected episodes array: {v}"
    );
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_get_nodes_by_group_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        3,
        "knowledge_get_nodes_by_group",
        json!({"group_ids": ["parity_group"]}),
        state,
    )
    .await;
    assert_ok_resp(&v, 3);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(v["result"]["nodes"].is_array(), "expected nodes array: {v}");
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_get_edges_by_group_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        4,
        "knowledge_get_edges_by_group",
        json!({"group_ids": ["parity_group"]}),
        state,
    )
    .await;
    assert_ok_resp(&v, 4);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(v["result"]["edges"].is_array(), "expected edges array: {v}");
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_get_edges_by_uuids_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        5,
        "knowledge_get_edges_by_uuids",
        json!({"uuids": []}),
        state,
    )
    .await;
    assert_ok_resp(&v, 5);
    assert!(v["result"].is_object(), "expected object envelope: {v}");
    assert!(v["result"]["edges"].is_array(), "expected edges array: {v}");
    assert_eq!(v["result"]["count"], 0);
}

#[tokio::test]
async fn parity_query_cypher() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        6,
        "knowledge_query_cypher",
        json!({"query": "MATCH (n:Entity) RETURN n.uuid LIMIT 1"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 6);
    assert!(v["result"]["rows"].is_array(), "expected rows array: {v}");
}

#[tokio::test]
async fn parity_delete_episode_noop() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        7,
        "knowledge_delete_episode",
        json!({"episode_uuid": "00000000-0000-0000-0000-000000000001"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 7);
    assert_eq!(v["result"]["status"], "deleted");
}

#[tokio::test]
async fn parity_close() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(8, "knowledge_close", json!({}), state).await;
    assert_ok_resp(&v, 8);
    assert_eq!(v["result"]["status"], "closed");
}

#[tokio::test]
async fn parity_unknown_method_returns_error() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(9, "no_such_method", json!({}), state).await;
    assert_err_resp(&v, 9, -32000);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("no_such_method"),
        "error message should name the method: {v}"
    );
}

#[tokio::test]
async fn parity_find_entities_requires_embedder() {
    // Embedding call fails (no server at default URL) → -32000 error with an HTTP message.
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        10,
        "knowledge_find_entities",
        json!({"query": "Alice", "group_ids": ["g"], "num_results": 5}),
        state,
    )
    .await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 10);
    assert!(
        v.get("result").is_some() || v["error"]["code"] == -32000,
        "unexpected response shape: {v}"
    );
}

#[tokio::test]
async fn parity_find_relationships_requires_embedder() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        11,
        "knowledge_find_relationships",
        json!({"query": "works at", "group_ids": ["g"], "num_results": 5}),
        state,
    )
    .await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 11);
    assert!(
        v.get("result").is_some() || v["error"]["code"] == -32000,
        "unexpected response shape: {v}"
    );
}

// ── Helpers for Tier 1a handshake tests ──────────────────────────────────────

fn make_state_with_mock_embed(db: Arc<Db>) -> Arc<AppState> {
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
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

fn make_state_with_workspace(db: Arc<Db>, workspace_root: PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: Some(workspace_root),
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: None,
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

/// Test extractor that returns a fixed `type_label` for every entity in `classify_entities`.
/// Used to drive `scope=off_ontology` / `scope=all` tests without a real LLM.
struct ClassifyingExtractor {
    type_label: String,
}

impl ClassifyingExtractor {
    fn new(type_label: &str) -> Self {
        Self {
            type_label: type_label.to_string(),
        }
    }
}

impl Extractor for ClassifyingExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, LcgError>> {
        Box::pin(async { Ok(ExtractionOutcome::from(ExtractionResult::default())) })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        let label = self.type_label.clone();
        let count = entities.len();
        Box::pin(async move { Ok(vec![label; count]) })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        let count = edges.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

fn make_state_with_ontology_and_extractor(
    db: Arc<Db>,
    ontology: Arc<Ontology>,
    extractor: Arc<dyn Extractor>,
    workspace_root: PathBuf,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor,
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: Some(workspace_root),
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: Some(ontology),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

/// Builds a minimal `Ontology` with a single entity type `Person` (no parent).
fn make_person_ontology() -> Arc<Ontology> {
    let entity_types = vec![EntityTypeDef {
        name: "Person".to_string(),
        description: None,
        parent: None,
    }];
    let ancestor_map = std::collections::HashMap::from([("Person".to_string(), vec![])]);
    Arc::new(Ontology {
        mode: OntologyMode::Strict,
        entity_types,
        relation_types: vec![],
        ancestor_map,
    })
}

/// Inserts an entity with the given name, group, labels, and uuid.
fn insert_test_entity(db: &Arc<Db>, uuid: &str, name: &str, group: &str, labels: Vec<String>) {
    let conn = db.connect().unwrap();
    conn.insert_entity(&EntityRow {
        uuid: uuid.to_string(),
        name: name.to_string(),
        group_id: group.to_string(),
        labels,
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("{name} summary"),
        attributes: "{}".to_string(),
        ..Default::default()
    })
    .unwrap();
}

// ── #210: reprocess_relation_types helpers ────────────────────────────────────

/// Test extractor that classifies edges by looking up `fact` in a fixed verdict map. A fact
/// with no entry abstains (returns an empty string). Drives `knowledge_reprocess_relation_types`
/// scope/dry_run tests without a real LLM.
struct RelationClassifyingExtractor {
    verdicts: HashMap<String, String>,
}

impl RelationClassifyingExtractor {
    fn new(verdicts: HashMap<String, String>) -> Self {
        Self { verdicts }
    }
}

impl Extractor for RelationClassifyingExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, LcgError>> {
        Box::pin(async { Ok(ExtractionOutcome::from(ExtractionResult::default())) })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        let result: Vec<String> = edges
            .iter()
            .map(|(fact, _current)| self.verdicts.get(*fact).cloned().unwrap_or_default())
            .collect();
        Box::pin(async move { Ok(result) })
    }
}

/// Builds a minimal `Ontology` declaring two relation types (no entity types).
fn make_relation_ontology() -> Arc<Ontology> {
    Arc::new(Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![],
        relation_types: vec![
            RelationTypeDef {
                name: "AUTHORED".to_string(),
                description: Some("a person authored something".to_string()),
                source_type: None,
                target_type: None,
                aliases: vec![],
                keywords: vec![],
            },
            RelationTypeDef {
                name: "AFFILIATED_WITH".to_string(),
                description: Some("a person is affiliated with an organization".to_string()),
                source_type: None,
                target_type: None,
                aliases: vec![],
                keywords: vec![],
            },
        ],
        ancestor_map: std::collections::HashMap::new(),
    })
}

/// Inserts a RELATES_TO edge between `source`/`target` with the given `fact`/`relation_type`.
fn insert_test_edge(
    db: &Arc<Db>,
    uuid: &str,
    source: &str,
    target: &str,
    group: &str,
    fact: &str,
    relation_type: Option<&str>,
) {
    let conn = db.connect().unwrap();
    conn.insert_relates_to_edge(&RelatesToEdge {
        uuid: uuid.to_string(),
        name: fact.to_string(),
        source_node_uuid: source.to_string(),
        target_node_uuid: target.to_string(),
        group_id: group.to_string(),
        fact: fact.to_string(),
        fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
        created_at: "2026-01-01T00:00:00Z".to_string(),
        valid_at: None,
        invalid_at: None,
        attributes: "{}".to_string(),
        relation_type: relation_type.map(|s| s.to_string()),
        episode_uuids: vec![],
        source_descriptions: vec![],
    })
    .unwrap();
}

// ── Tier 1a: health_check ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_health_check_ok() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(20, "health_check", json!({}), state).await;
    assert_ok_resp(&v, 20);
    assert_eq!(v["result"]["ok"], true, "expected ok:true: {v}");
    assert_eq!(v["result"]["healthy"], true, "expected healthy:true: {v}");
}

// ── Tier 1a: knowledge_status ─────────────────────────────────────────────────

#[tokio::test]
async fn test_knowledge_status_empty_db() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(21, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&v, 21);
    let r = &v["result"];
    assert_eq!(r["entity_count"], 0, "expected 0 entities: {v}");
    assert_eq!(r["relationship_count"], 0, "expected 0 relationships: {v}");
    assert_eq!(r["episode_count"], 0, "expected 0 episodes: {v}");
    assert_eq!(r["wal"]["exists"], false, "expected wal.exists:false: {v}");
    assert!(
        r["database_path"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "expected non-empty database_path: {v}"
    );
    assert!(
        r["embedding_model"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "expected non-empty embedding_model: {v}"
    );
    assert!(
        r["embedding_dim"].as_u64().is_some(),
        "expected numeric embedding_dim: {v}"
    );
    assert_eq!(
        r["context_graph_initialized"], true,
        "expected context_graph_initialized:true: {v}"
    );
    assert_eq!(r["connected"], true, "expected connected:true: {v}");
    assert_eq!(r["initializing"], false, "expected initializing:false: {v}");
    assert!(
        r["last_index_time"].is_null(),
        "expected last_index_time:null on empty db: {v}"
    );
    assert!(
        r.get("index_created_at").is_none(),
        "expected index_created_at to be absent from empty-DB response: {v}"
    );
}

#[tokio::test]
async fn test_knowledge_status_counts() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    // Insert one episode via knowledge_process_chunk; MockExtractor yields 2 entities, 1 edge.
    let ingest = dispatch_val(
        22,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-001",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 22);

    let v = dispatch_val(23, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 23);
    let r = &v["result"];
    assert_eq!(r["entity_count"], 2, "expected 2 entities: {v}");
    assert_eq!(r["episode_count"], 1, "expected 1 episode: {v}");
    assert_eq!(
        r["relationship_count"], 1,
        "expected 1 RELATES_TO relationship: {v}"
    );
    assert_eq!(
        r["context_graph_initialized"], true,
        "expected context_graph_initialized:true: {v}"
    );
    assert!(
        r["last_index_time"].as_str().is_some(),
        "expected non-null last_index_time after ingestion: {v}"
    );
    let ica = r["index_created_at"]
        .as_str()
        .expect("expected index_created_at to be a string");
    let iso8601 = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
    assert!(
        iso8601.is_match(ica),
        "expected index_created_at to be ISO 8601 UTC, got: {ica}"
    );
}

/// Regression for issue #325: `knowledge_status` must degrade to a status response, not a
/// JSON-RPC error, when a core table (`Entity`) is missing on an otherwise-open database
/// (FR-001). `indices_built` must report `false` (FR-002), and counts must be `null` — not
/// `0` — so the caller can never mistake a broken graph for an empty one (FR-003).
#[tokio::test]
async fn test_knowledge_status_missing_entity_table_reports_not_queryable() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);

    let rename_away = dispatch_val(
        24,
        "knowledge_query_cypher",
        json!({"query": "ALTER TABLE Entity RENAME TO EntityTmp"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rename_away, 24);

    let v = dispatch_val(25, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 25);
    let r = &v["result"];
    assert_eq!(
        r["queryable"], false,
        "expected queryable:false while Entity is renamed away: {v}"
    );
    assert_eq!(r["connected"], true, "expected connected:true: {v}");
    assert_eq!(
        r["context_graph_initialized"], true,
        "expected context_graph_initialized:true: {v}"
    );
    assert!(
        r["entity_count"].is_null(),
        "expected entity_count:null, not 0, so a broken graph can't be mistaken for an \
         empty one: {v}"
    );
    assert!(
        r["relationship_count"].is_null(),
        "expected relationship_count:null: {v}"
    );
    assert!(
        r["episode_count"].is_null(),
        "expected episode_count:null: {v}"
    );
    assert!(
        r["reason"]
            .as_str()
            .map(|s| s.contains("Entity"))
            .unwrap_or(false),
        "expected reason to mention the missing table: {v}"
    );
    assert_eq!(
        r["indices_built"], false,
        "expected indices_built:false: {v}"
    );
}

/// Regression for issue #325 FR-002: `indices_built` must report `false` while a core table is
/// missing even if it was `true` *before* the table broke and no intervening
/// `knowledge_build_indices` call has stored a fresh `false` — otherwise a caller could see
/// `queryable:false` alongside a stale `indices_built:true`, the same class of staleness bug
/// #297 fixed for a different code path.
#[tokio::test]
async fn test_knowledge_status_missing_entity_table_forces_indices_built_false() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    state.indices_built.store(true, Ordering::Release);

    let rename_away = dispatch_val(
        32,
        "knowledge_query_cypher",
        json!({"query": "ALTER TABLE Entity RENAME TO EntityTmp"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rename_away, 32);

    let v = dispatch_val(33, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 33);
    let r = &v["result"];
    assert_eq!(
        r["queryable"], false,
        "expected queryable:false while Entity is renamed away: {v}"
    );
    assert_eq!(
        r["indices_built"], false,
        "expected indices_built:false even though the atomic was pre-set true, since indices \
         on a missing table are meaningless: {v}"
    );
}

/// Regression for issue #325 Edge Cases: no caching of schema state must survive a rename-back
/// — `knowledge_status` re-queries live every call, so restoring the table must self-heal the
/// very next status call without any extra invalidation step.
#[tokio::test]
async fn test_knowledge_status_rename_back_self_heals() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);

    dispatch_val(
        26,
        "knowledge_query_cypher",
        json!({"query": "ALTER TABLE Entity RENAME TO EntityTmp"}),
        Arc::clone(&state),
    )
    .await;
    let broken = dispatch_val(27, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_eq!(
        broken["result"]["queryable"], false,
        "expected queryable:false immediately after rename: {broken}"
    );

    let rename_back = dispatch_val(
        28,
        "knowledge_query_cypher",
        json!({"query": "ALTER TABLE EntityTmp RENAME TO Entity"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rename_back, 28);

    let healed = dispatch_val(29, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&healed, 29);
    let r = &healed["result"];
    assert_eq!(
        r["queryable"], true,
        "expected queryable:true after rename-back: {healed}"
    );
    assert_eq!(
        r["entity_count"], 0,
        "expected entity_count:0 (numeric, not null) after rename-back: {healed}"
    );
}

/// Regression for issue #325 FR-006 (SC-003): a query failure for a reason *other* than a
/// missing table must still surface as a genuine JSON-RPC error, proving the missing-table
/// classifier doesn't mask unrelated failures.
#[tokio::test]
async fn test_knowledge_status_other_query_failure_still_errors() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);

    let drop_col = dispatch_val(
        31,
        "knowledge_query_cypher",
        json!({"query": "ALTER TABLE Episodic DROP created_at"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&drop_col, 31);

    let v = dispatch_val(32, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_err_resp(&v, 32, -32000);
}

/// issue #353: an empty DB with no WAL configured reports `applied_seq: null` (row absent —
/// nothing ever written, no backfill trigger since `make_db`/`make_state` never call the
/// backfill path a real service startup would) and `max_seq: null` (no WAL dir at all).
#[tokio::test]
async fn test_knowledge_status_wal_seq_fields_null_with_no_wal_configured() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(40, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&v, 40);
    let r = &v["result"];
    assert!(
        r["wal"]["applied_seq"].is_null(),
        "expected wal.applied_seq:null (no position ever recorded): {v}"
    );
    assert!(
        r["wal"]["max_seq"].is_null(),
        "expected wal.max_seq:null (no WAL dir configured): {v}"
    );
}

/// SC-001: after `knowledge_process_chunk`, `wal.applied_seq` equals the max seq of the WAL
/// lines just written, and `wal.applied_seq == wal.max_seq` (User Story 1, scenario 1 — the
/// "caught up" case). Also covers SC-006 (`wal.max_seq` derived from the same on-disk WAL
/// content `scan_max_seq` reads).
#[tokio::test]
async fn test_knowledge_status_applied_seq_matches_max_seq_after_ingest() {
    let (db, dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let db_path = dir.path().join("parity.db").to_str().unwrap().to_string();
    let state = make_state_with_live_wal(db, wal_dir.path().to_path_buf(), db_path);

    let ingest = dispatch_val(
        41,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-applied-seq",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 41);

    let v = dispatch_val(42, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 42);
    let r = &v["result"];
    let applied_seq = r["wal"]["applied_seq"]
        .as_u64()
        .expect("expected an integer wal.applied_seq after ingest");
    let max_seq = r["wal"]["max_seq"]
        .as_u64()
        .expect("expected an integer wal.max_seq after ingest");
    assert_eq!(
        applied_seq, max_seq,
        "a DB just caught up to its own writes must report applied_seq == max_seq: {v}"
    );
}

/// SC-001 (restart half): `applied_seq` must survive a service restart — reopening the same
/// DB file in a fresh `AppState`/`Db` must still report the position, proving persistence
/// rather than in-process memoisation (FR-001 explicitly forbids caching this on `AppState`).
#[tokio::test]
async fn test_knowledge_status_applied_seq_persists_across_restart() {
    let (db, dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let db_path = dir.path().join("parity.db").to_str().unwrap().to_string();
    let state = make_state_with_live_wal(db, wal_dir.path().to_path_buf(), db_path.clone());

    let ingest = dispatch_val(
        43,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Bob manages the project.",
            "chunk_id": "chunk-restart",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        state,
    )
    .await;
    assert_ok_resp(&ingest, 43);

    // Simulate a restart: open a brand-new Db handle (and AppState) against the same on-disk
    // database file, dropping every in-process value the first AppState/Db held. Mirrors
    // main.rs's real startup sequence, which always calls init_schema (idempotent) after
    // Db::open on every boot, not just a fresh DB.
    let restarted_db = Db::open(&db_path).unwrap();
    restarted_db.connect().unwrap().init_schema(4).unwrap();
    let restarted_db = Arc::new(restarted_db);
    let restarted_state =
        make_state_with_live_wal(restarted_db, wal_dir.path().to_path_buf(), db_path.clone());
    let v = dispatch_val(44, "knowledge_status", json!({}), restarted_state).await;
    assert_ok_resp(&v, 44);
    assert!(
        v["result"]["wal"]["applied_seq"].as_u64().is_some(),
        "applied_seq must survive reopening the DB, not just persist within one process: {v}"
    );
}

/// SC-003: after `knowledge_clear_all`, `wal.applied_seq` resets to `0` (known position,
/// nothing applied) — not left at its pre-clear value, and not `null`.
#[tokio::test]
async fn test_knowledge_status_clear_all_resets_applied_seq() {
    let (db, dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let db_path = dir.path().join("parity.db").to_str().unwrap().to_string();
    let state = make_state_with_live_wal(db, wal_dir.path().to_path_buf(), db_path);

    let ingest = dispatch_val(
        45,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Carol leads the team.",
            "chunk_id": "chunk-clear",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 45);
    let before = dispatch_val(46, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert!(
        before["result"]["wal"]["applied_seq"].as_u64().is_some(),
        "sanity check: applied_seq must be set before clearing: {before}"
    );

    let cleared = dispatch_val(
        47,
        "knowledge_clear_all",
        json!({"confirm": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&cleared, 47);

    let after = dispatch_val(48, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&after, 48);
    assert_eq!(
        after["result"]["wal"]["applied_seq"], 0,
        "expected wal.applied_seq:0 after knowledge_clear_all: {after}"
    );
}

/// `wal.applied_seq`/`wal.max_seq` must also appear (as `null`, not omitted) in the
/// `NotQueryable` branch — FR-006 is additive to the whole `wal` object, not just the
/// healthy-path response.
#[tokio::test]
async fn test_knowledge_status_not_queryable_includes_wal_seq_fields() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);

    let rename_away = dispatch_val(
        49,
        "knowledge_query_cypher",
        json!({"query": "ALTER TABLE Entity RENAME TO EntityTmp"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&rename_away, 49);

    let v = dispatch_val(50, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&v, 50);
    let r = &v["result"];
    assert!(
        r["wal"].get("applied_seq").is_some(),
        "expected wal.applied_seq key present (even if null) in NotQueryable branch: {v}"
    );
    assert!(
        r["wal"].get("max_seq").is_some(),
        "expected wal.max_seq key present (even if null) in NotQueryable branch: {v}"
    );
}

// ── Tier 1a: knowledge_process_chunk ─────────────────────────────────────────

#[tokio::test]
async fn test_knowledge_process_chunk_ok() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let v = dispatch_val(
        30,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "test-chunk-1",
            "source_file": "test.txt",
            "reference_time": "2024-06-01T12:00:00Z",
        }),
        state,
    )
    .await;
    assert_ok_resp(&v, 30);
    let r = &v["result"];
    assert_eq!(r["success"], true, "expected success:true: {v}");
    assert_eq!(r["chunk_id"], "test-chunk-1");
    assert_eq!(r["source_file"], "test.txt");
    assert!(
        r["episode_uuid"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "expected non-empty episode_uuid: {v}"
    );
    assert!(
        r["nodes_extracted"].as_u64().is_some(),
        "expected numeric nodes_extracted: {v}"
    );
    assert!(
        r["edges_extracted"].as_u64().is_some(),
        "expected numeric edges_extracted: {v}"
    );
    assert!(
        r["edges_dropped_unresolvable"].as_u64().is_some(),
        "expected numeric edges_dropped_unresolvable: {v}"
    );
    assert!(
        r["edges_reclassified_unclassified"].as_u64().is_some(),
        "expected numeric edges_reclassified_unclassified (FR-005): {v}"
    );
    assert!(
        r["entities_reclassified_unclassified"].as_u64().is_some(),
        "expected numeric entities_reclassified_unclassified (issue #312 FR-004): {v}"
    );
    assert!(
        r["entities_dropped_malformed"].as_u64().is_some(),
        "expected numeric entities_dropped_malformed (issue #342 FR-003): {v}"
    );
    assert!(
        r["edges_dropped_malformed"].as_u64().is_some(),
        "expected numeric edges_dropped_malformed (issue #342 FR-003): {v}"
    );
    assert!(
        r["duration_seconds"].as_f64().is_some(),
        "expected numeric duration_seconds: {v}"
    );
}

// ── #342 US1 AS4 / SC-004: one malformed item must not sink a multi-chunk ingest ────

/// Test extractor whose `extract()` reports a malformed-item drop for chunks whose
/// `episode_body` contains `malformed_marker`, and a clean result otherwise. Standing in for
/// the parse-time salvage that `extractor.rs`'s own unit tests already exercise directly
/// (T2/T3/T5) — this drives the `episode.rs`/`handlers.rs` layers `knowledge_process_chunk`
/// actually returns over the wire, which is what #340's reported impact (a whole multi-chunk
/// document lost because one chunk erred) depends on.
struct PartiallyMalformedExtractor {
    malformed_marker: &'static str,
}

impl Extractor for PartiallyMalformedExtractor {
    fn extract<'a>(
        &'a self,
        opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionOutcome, LcgError>> {
        let is_malformed_chunk = opts.episode_body.contains(self.malformed_marker);
        let chunk_key = opts.chunk_key.unwrap_or("chunk").to_string();
        Box::pin(async move {
            let entities = vec![
                ExtractedEntity {
                    name: format!("Alice-{chunk_key}"),
                    entity_type: "Person".to_string(),
                    summary: "A person".to_string(),
                    original_entity_type: None,
                },
                ExtractedEntity {
                    name: format!("Acme-{chunk_key}"),
                    entity_type: "Organization".to_string(),
                    summary: "A company".to_string(),
                    original_entity_type: None,
                },
            ];
            Ok(ExtractionOutcome {
                result: ExtractionResult {
                    entities,
                    edges: vec![],
                },
                // Simulates one entity in this chunk's raw response having been dropped by
                // parse-time salvage (e.g. a missing `name`) — the N-valid-plus-1-malformed
                // shape User Story 1's Independent Test describes.
                entities_dropped_malformed: if is_malformed_chunk { 1 } else { 0 },
                edges_dropped_malformed: 0,
            })
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }

    fn classify_relations<'a>(
        &'a self,
        edges: &'a [(&'a str, &'a str)],
        _allowed_types: &'a [(String, Option<String>)],
    ) -> BoxFuture<'a, Result<Vec<String>, LcgError>> {
        let count = edges.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

fn make_state_with_extractor(db: Arc<Db>, extractor: Arc<dyn Extractor>) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor,
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test.db".to_string(),
        wal_dir: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

/// #342 US1 AS4 / SC-004: a multi-chunk document where exactly one chunk's extraction response
/// contains a malformed item must still complete for every chunk — the regression test for
/// #340's actual reported impact (a ~40-chunk document lost in full over one chunk's error).
#[tokio::test]
async fn test_knowledge_process_chunk_multi_chunk_document_survives_one_malformed_chunk() {
    let (db, _dir) = make_db(4);
    let extractor = Arc::new(PartiallyMalformedExtractor {
        malformed_marker: "BAD_CHUNK_MARKER",
    });
    let state = make_state_with_extractor(db, extractor);

    let chunk_bodies = [
        "Chunk 1: Alice works at Acme Corp.",
        "Chunk 2: Alice works at Acme Corp.",
        "Chunk 3 BAD_CHUNK_MARKER: Alice works at Acme Corp.",
        "Chunk 4: Alice works at Acme Corp.",
        "Chunk 5: Alice works at Acme Corp.",
    ];

    for (i, body) in chunk_bodies.iter().enumerate() {
        let v = dispatch_val(
            i as i64,
            "knowledge_process_chunk",
            json!({
                "chunk_text": body,
                "chunk_id": format!("chunk-{i}"),
                "source_file": "document.txt",
                "reference_time": "2024-06-01T12:00:00Z",
            }),
            Arc::clone(&state),
        )
        .await;
        assert_ok_resp(&v, i as i64);
        let r = &v["result"];
        assert_eq!(
            r["success"], true,
            "chunk {i} must succeed even when chunk 2 (0-indexed) has a malformed item: {v}"
        );
        let expected_dropped = if body.contains("BAD_CHUNK_MARKER") {
            1
        } else {
            0
        };
        assert_eq!(
            r["entities_dropped_malformed"], expected_dropped,
            "chunk {i} entities_dropped_malformed mismatch: {v}"
        );
    }
}

#[tokio::test]
async fn test_knowledge_process_chunk_duplicate_chunk_id() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let params = json!({
        "chunk_text": "Alice works at Acme Corp.",
        "chunk_id": "dup-chunk",
        "source_file": "test.txt",
        "reference_time": "2024-06-01T12:00:00Z",
    });
    let v1 = dispatch_val(
        31,
        "knowledge_process_chunk",
        params.clone(),
        Arc::clone(&state),
    )
    .await;
    let v2 = dispatch_val(32, "knowledge_process_chunk", params, Arc::clone(&state)).await;
    assert_ok_resp(&v1, 31);
    assert_ok_resp(&v2, 32);
    let uuid1 = v1["result"]["episode_uuid"].as_str().unwrap();
    let uuid2 = v2["result"]["episode_uuid"].as_str().unwrap();
    assert_ne!(
        uuid1, uuid2,
        "duplicate chunk_id must produce distinct episode_uuid values"
    );
}

#[tokio::test]
async fn test_knowledge_process_chunk_rejects_empty_chunk_text() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        33,
        "knowledge_process_chunk",
        json!({ "chunk_text": "", "chunk_id": "c1", "source_file": "f.txt" }),
        state,
    )
    .await;
    assert_err_resp(&v, 33, -32000);
}

#[tokio::test]
async fn test_knowledge_process_chunk_rejects_missing_chunk_id() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        34,
        "knowledge_process_chunk",
        json!({ "chunk_text": "some text", "source_file": "f.txt" }),
        state,
    )
    .await;
    assert_err_resp(&v, 34, -32000);
}

// ── Tier 1b: knowledge_search_passages ───────────────────────────────────────

#[tokio::test]
async fn parity_search_passages_empty_db() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let v = dispatch_val(
        40,
        "knowledge_search_passages",
        serde_json::json!({"query": "test passage", "num_results": 5, "min_score": 0.0}),
        state,
    )
    .await;
    assert_ok_resp(&v, 40);
    assert!(
        v["result"]["passages"].is_array(),
        "expected passages array: {v}"
    );
    assert_eq!(
        v["result"]["count"], 0,
        "empty db should yield 0 passages: {v}"
    );
}

#[tokio::test]
async fn parity_search_passages_empty_query() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);
    let v = dispatch_val(
        41,
        "knowledge_search_passages",
        serde_json::json!({"query": "", "num_results": 5}),
        state,
    )
    .await;
    assert_err_resp(&v, 41, -32000);
}

// ── Tier 1b: knowledge_list_entities ─────────────────────────────────────────

#[tokio::test]
async fn parity_list_entities_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(42, "knowledge_list_entities", json!({}), state).await;
    assert_ok_resp(&v, 42);
    assert!(v["result"]["nodes"].is_array(), "expected nodes array: {v}");
    assert_eq!(v["result"]["count"], 0, "empty db: {v}");
}

#[tokio::test]
async fn parity_list_entities_invalid_num_results() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        43,
        "knowledge_list_entities",
        json!({"num_results": 0}),
        state,
    )
    .await;
    assert_err_resp(&v, 43, -32000);
}

// ── Tier 1b: knowledge_list_relationships ────────────────────────────────────

#[tokio::test]
async fn parity_list_relationships_empty() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(44, "knowledge_list_relationships", json!({}), state).await;
    assert_ok_resp(&v, 44);
    assert!(v["result"]["facts"].is_array(), "expected facts array: {v}");
    assert_eq!(v["result"]["count"], 0, "empty db: {v}");
}

// ── Tier 1b: knowledge_get_entity_neighbors ───────────────────────────────────

#[tokio::test]
async fn parity_get_entity_neighbors_missing_uuid() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(45, "knowledge_get_entity_neighbors", json!({}), state).await;
    assert_err_resp(&v, 45, -32000);
}

#[tokio::test]
async fn parity_get_entity_neighbors_nonexistent() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        46,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": "00000000-0000-0000-0000-000000000099"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 46);
    assert!(v["result"]["nodes"].is_array(), "expected nodes: {v}");
    assert!(v["result"]["edges"].is_array(), "expected edges: {v}");
    assert_eq!(
        v["result"]["node_count"], 0,
        "no neighbors for nonexistent uuid: {v}"
    );
    assert_eq!(
        v["result"]["edge_count"], 0,
        "no edges for nonexistent uuid: {v}"
    );
}

// ── Tier 1b: knowledge_get_entities_by_source ────────────────────────────────

#[tokio::test]
async fn parity_get_entities_by_source_empty_source() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        47,
        "knowledge_get_entities_by_source",
        json!({"source": ""}),
        state,
    )
    .await;
    assert_err_resp(&v, 47, -32000);
}

#[tokio::test]
async fn parity_get_entities_by_source_no_match() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        48,
        "knowledge_get_entities_by_source",
        json!({"source": "nonexistent-source-xyz"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 48);
    assert!(v["result"]["nodes"].is_array(), "expected nodes: {v}");
    assert_eq!(v["result"]["count"], 0, "no match: {v}");
}

#[tokio::test]
async fn test_knowledge_process_chunk_rejects_bad_reference_time() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        35,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "some text",
            "chunk_id": "c1",
            "source_file": "f.txt",
            "reference_time": "not-a-date",
        }),
        state,
    )
    .await;
    assert_err_resp(&v, 35, -32000);
}

// ── Tier 3: corrections ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_validate_corrections_no_workspace() {
    // workspace_root is None — all corrections methods should return a structured error
    let (db, _dir) = make_db(4);
    let state = make_state(db); // workspace_root: None
    let v = dispatch_val(50, "knowledge_validate_corrections", json!({}), state).await;
    assert_err_resp(&v, 50, -32000);
}

#[tokio::test]
async fn test_validate_corrections_no_file() {
    // workspace_root set but no .liminis/knowledge-corrections.yaml exists
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(51, "knowledge_validate_corrections", json!({}), state).await;
    assert_ok_resp(&v, 51);
    let r = &v["result"];
    assert_eq!(r["valid"], true, "no file should be valid:true: {v}");
    assert_eq!(r["total_corrections"], 0, "should be 0: {v}");
    assert_eq!(r["unapplied_corrections"], 0, "should be 0: {v}");
    assert!(
        r["issues"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "no issues: {v}"
    );
}

#[tokio::test]
async fn test_apply_corrections_no_file() {
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(52, "knowledge_apply_corrections", json!({}), state).await;
    assert_ok_resp(&v, 52);
    let r = &v["result"];
    assert_eq!(r["success"], true, "no file should succeed: {v}");
    assert_eq!(r["applied"], 0, "nothing applied: {v}");
}

#[tokio::test]
async fn test_apply_corrections_dry_run() {
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();

    // Create .liminis/knowledge-corrections.yaml with two unapplied retract entries
    let liminis_dir = workspace_dir.path().join(".liminis");
    std::fs::create_dir_all(&liminis_dir).unwrap();
    let corrections_path = liminis_dir.join("knowledge-corrections.yaml");
    std::fs::write(
        &corrections_path,
        "corrections:\n  - id: r1\n    type: retract\n    edge_uuid: nonexistent-uuid-1\n  - id: r2\n    type: retract\n    edge_uuid: nonexistent-uuid-2\n",
    )
    .unwrap();

    let before = std::fs::read_to_string(&corrections_path).unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(
        53,
        "knowledge_apply_corrections",
        json!({"dry_run": true}),
        state,
    )
    .await;
    assert_ok_resp(&v, 53);
    let r = &v["result"];
    // Edge existence is validated even in dry_run (FR-015). Both retract entries reference
    // nonexistent edge UUIDs, so success is false and errors has one entry per failing correction.
    assert_eq!(
        r["success"], false,
        "dry_run with nonexistent edges must fail: {v}"
    );
    assert_eq!(r["applied"], 0, "dry_run must not apply: {v}");
    let errs = r["errors"].as_array().expect("errors must be an array");
    assert_eq!(
        errs.len(),
        2,
        "expected one error per nonexistent edge: {v}"
    );

    // File must be byte-identical after dry_run — patch_applied_at is not called in dry_run
    let after = std::fs::read_to_string(&corrections_path).unwrap();
    assert_eq!(
        before, after,
        "dry_run must not modify the corrections file"
    );
}

#[tokio::test]
async fn test_reprocess_entity_types_no_entities() {
    let (db, _dir) = make_db(4);
    let workspace_dir = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace_dir.path().to_path_buf());
    let v = dispatch_val(
        54,
        "knowledge_reprocess_entity_types",
        json!({"group_id": "test_group"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 54);
    let r = &v["result"];
    assert_eq!(r["success"], true, "no entities to reprocess: {v}");
    assert_eq!(r["reclassified_count"], 0, "nothing to reclassify: {v}");
}

// ── Tier 1b regression: two-hop RELATES_TO traversal ─────────────────────────
//
// These tests verify that list_relationships and get_entity_neighbors return
// populated results after ingestion via add_episode (the Rust write path).
// They guard against regressions where the two-hop write (Entity→RelatesToNode_→Entity)
// or two-hop read (MATCH ...→rn:RelatesToNode_→...) is accidentally removed.

#[tokio::test]
async fn test_list_relationships_after_ingest() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    // Ingest one episode; MockExtractor yields Alice-[works_at]->Acme Corp.
    let ingest = dispatch_val(
        60,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-list-rel",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 60);

    let v = dispatch_val(
        61,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 61);
    let facts = v["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(
        !facts.is_empty(),
        "expected ≥1 relationship after ingest, got 0 — two-hop write/read may be broken: {v}"
    );
    let fact = &facts[0];
    assert!(
        fact["uuid"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "fact uuid should be non-empty: {v}"
    );
    assert!(
        fact["fact"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "fact.fact should be non-empty: {v}"
    );
}

#[tokio::test]
async fn test_get_entity_neighbors_after_ingest() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    // Ingest one episode; MockExtractor yields Alice-[works_at]->Acme Corp.
    let ingest = dispatch_val(
        62,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-neighbors",
            "source_file": "doc.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 62);

    // Get the source entity UUID from list_relationships.
    let lr = dispatch_val(
        63,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&lr, 63);
    let facts = lr["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(!facts.is_empty(), "expected ≥1 relationship: {lr}");
    let src_uuid = facts[0]["source_node_uuid"]
        .as_str()
        .expect("expected source_node_uuid")
        .to_string();
    assert!(!src_uuid.is_empty(), "source_node_uuid must be non-empty");

    let v = dispatch_val(
        64,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": src_uuid}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 64);
    let edge_count = v["result"]["edge_count"].as_u64().unwrap_or(0);
    assert!(
        edge_count >= 1,
        "expected ≥1 neighbor edge for entity {src_uuid}, got {edge_count} — \
         two-hop write/read may be broken: {v}"
    );
}

// ── Tier 1b: source-info enrichment (episode_uuids / source_descriptions) ────
//
// These tests ingest an episode with a known source_description, then call all
// four Tier 1b list/neighbor methods and assert that each returned node and edge
// carries non-empty episode_uuids and source_descriptions arrays that include the
// expected episode UUID and source_description value.

#[tokio::test]
async fn test_source_info_enrichment_list_entities() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        70,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-01",
            "source_file": "enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 70);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    // source_description is "<source_file>:<chunk_id>"
    let expected_src_desc = "enrich.txt:chunk-src-01";

    let v = dispatch_val(71, "knowledge_list_entities", json!({}), Arc::clone(&state)).await;
    assert_ok_resp(&v, 71);
    let nodes = v["result"]["nodes"]
        .as_array()
        .expect("expected nodes array");
    assert!(!nodes.is_empty(), "expected ≥1 node after ingest: {v}");
    for node in nodes {
        let ep_uuids = node["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = node["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert!(
            !ep_uuids.is_empty(),
            "expected non-empty episode_uuids for node: {node}"
        );
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "episode_uuids and source_descriptions must be same length: {node}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in node episode_uuids: {node}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description {expected_src_desc} in node: {node}"
        );
    }
}

#[tokio::test]
async fn test_source_info_enrichment_list_relationships() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        72,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-02",
            "source_file": "enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 72);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    let expected_src_desc = "enrich.txt:chunk-src-02";

    let v = dispatch_val(
        73,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 73);
    let facts = v["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(
        !facts.is_empty(),
        "expected ≥1 relationship after ingest: {v}"
    );
    for fact in facts {
        let ep_uuids = fact["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = fact["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert!(
            !ep_uuids.is_empty(),
            "expected non-empty episode_uuids for edge: {fact}"
        );
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "episode_uuids and source_descriptions must be same length: {fact}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in edge episode_uuids: {fact}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description {expected_src_desc} in edge: {fact}"
        );
    }
}

#[tokio::test]
async fn test_source_info_enrichment_get_entity_neighbors() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        74,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-03",
            "source_file": "enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 74);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    let expected_src_desc = "enrich.txt:chunk-src-03";

    // Get a source entity UUID via list_relationships.
    let lr = dispatch_val(
        75,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&lr, 75);
    let facts = lr["result"]["facts"].as_array().expect("expected facts");
    assert!(!facts.is_empty(), "expected ≥1 relationship: {lr}");
    let src_uuid = facts[0]["source_node_uuid"]
        .as_str()
        .expect("expected source_node_uuid")
        .to_string();

    let v = dispatch_val(
        76,
        "knowledge_get_entity_neighbors",
        json!({"entity_uuid": src_uuid}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 76);
    let nodes = v["result"]["nodes"].as_array().expect("expected nodes");
    let edges = v["result"]["edges"].as_array().expect("expected edges");
    assert!(
        !nodes.is_empty() || !edges.is_empty(),
        "expected results after ingest: {v}"
    );

    for node in nodes {
        let ep_uuids = node["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = node["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "positional alignment: {node}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in neighbor node: {node}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description in neighbor node: {node}"
        );
    }

    for edge in edges {
        let ep_uuids = edge["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = edge["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "positional alignment: {edge}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in neighbor edge: {edge}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description in neighbor edge: {edge}"
        );
    }
}

#[tokio::test]
async fn test_source_info_enrichment_get_entities_by_source() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        77,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-src-04",
            "source_file": "unique-enrich.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 77);
    let ep_uuid = ingest["result"]["episode_uuid"]
        .as_str()
        .expect("expected episode_uuid")
        .to_string();
    let expected_src_desc = "unique-enrich.txt:chunk-src-04";

    let v = dispatch_val(
        78,
        "knowledge_get_entities_by_source",
        json!({"source": "unique-enrich.txt"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 78);
    let nodes = v["result"]["nodes"].as_array().expect("expected nodes");
    assert!(!nodes.is_empty(), "expected ≥1 node for source match: {v}");
    for node in nodes {
        let ep_uuids = node["episode_uuids"]
            .as_array()
            .expect("episode_uuids must be an array");
        let src_descs = node["source_descriptions"]
            .as_array()
            .expect("source_descriptions must be an array");
        assert!(
            !ep_uuids.is_empty(),
            "expected non-empty episode_uuids: {node}"
        );
        assert_eq!(
            ep_uuids.len(),
            src_descs.len(),
            "positional alignment: {node}"
        );
        assert!(
            ep_uuids.iter().any(|u| u.as_str() == Some(&ep_uuid)),
            "expected episode_uuid {ep_uuid} in node: {node}"
        );
        assert!(
            src_descs
                .iter()
                .any(|d| d.as_str() == Some(expected_src_desc)),
            "expected source_description {expected_src_desc} in node: {node}"
        );
    }
}

// ── Python-DB index name regression tests (FR-005) ───────────────────────────
//
// These tests open the Python-populated baseline_db fixture without any schema
// init or index creation, then call every method that queries an index by name.
// They guard against the class of bug in issue #49: Rust using a different index
// name than the upstream Python graphiti-core service used when creating the DB.
//
// The fixture at tests/fixtures/baseline_db/liminis.db is NOT committed to git.
// These tests skip gracefully when the file is absent. To populate it, run
// scripts/record_corpus.py against a live upstream Python graphiti-core service
// (see tests/fixtures/README.md).

/// Copies the baseline_db fixture into a fresh TempDir and returns the path
/// inside the copy alongside the TempDir (which must stay alive for the test).
/// Protects the original fixture from the write transactions that Db::open
/// issues (INSTALL / LOAD EXTENSION are write transactions in lbug).
fn open_baseline_db() -> Option<(PathBuf, TempDir)> {
    let src =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/baseline_db/liminis.db");
    if !src.exists() {
        return None;
    }
    let tmp = TempDir::new().ok()?;
    let dst = tmp.path().join("liminis.db");
    copy_path(&src, &dst).ok()?;
    Some((dst, tmp))
}

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

#[test]
fn python_db_index_names_fts_entities() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_fts_entities: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    let result = conn.fts_search_entities("test", &["*"], 5);
    assert!(
        result.is_ok(),
        "fts_search_entities failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

#[test]
fn python_db_index_names_fts_edges() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_fts_edges: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    let result = conn.fts_search_edges("test", &["*"], 5);
    assert!(
        result.is_ok(),
        "fts_search_edges failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

#[test]
fn python_db_index_names_vector_entities() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_vector_entities: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    // Python DBs use 768-dim bge-base-en-v1.5 embeddings; zero-vector confirms index resolves.
    let result = conn.vector_search_entities(&vec![0.0_f32; 768], &["*"], 5);
    assert!(
        result.is_ok(),
        "vector_search_entities failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

#[test]
fn python_db_index_names_vector_edges() {
    let Some((path, _tmp)) = open_baseline_db() else {
        eprintln!(
            "SKIP python_db_index_names_vector_edges: \
             tests/fixtures/baseline_db/liminis.db absent — \
             run scripts/record_corpus.py to populate it"
        );
        return;
    };
    let db = Db::open(path.to_str().expect("baseline_db path is not valid UTF-8"))
        .expect("open baseline_db copy");
    let conn = db.connect().expect("connect");
    // Python DBs use 768-dim bge-base-en-v1.5 embeddings; zero-vector confirms index resolves.
    let result = conn.vector_search_edges(&vec![0.0_f32; 768], &["*"], 5);
    assert!(
        result.is_ok(),
        "vector_search_edges failed against Python DB (index name mismatch?): {:?}",
        result.err()
    );
}

// ── FR-007/SC-001: relation_type surfaces in knowledge_list_relationships ─────

// After ingestion via MockExtractor (which returns WORKS_AT), every edge in the
// knowledge_list_relationships response must include a non-null relation_type field.
#[tokio::test]
async fn list_relationships_includes_relation_type() {
    let (db, _dir) = make_db(4);
    let state = make_state_with_mock_embed(db);

    let ingest = dispatch_val(
        200,
        "knowledge_process_chunk",
        json!({
            "chunk_text": "Alice works at Acme Corp.",
            "chunk_id": "chunk-rt-ipc",
            "source_file": "rt_test.txt",
            "reference_time": "2024-01-01T00:00:00Z",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&ingest, 200);

    let v = dispatch_val(
        201,
        "knowledge_list_relationships",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 201);

    let facts = v["result"]["facts"]
        .as_array()
        .expect("expected facts array");
    assert!(
        !facts.is_empty(),
        "expected ≥1 relationship after ingest: {v}"
    );

    for fact in facts {
        let rt = fact["relation_type"]
            .as_str()
            .expect("every fact must have a string relation_type field");
        assert_eq!(
            rt, "WORKS_AT",
            "MockExtractor always returns WORKS_AT; got '{rt}'"
        );
    }
}

// ── knowledge_dump_wal ────────────────────────────────────────────────────────

/// SC-004: dump on an empty graph returns success with zero counts.
#[tokio::test]
async fn parity_dump_wal_empty_graph() {
    let (db, dir) = make_db(4);
    let state = make_state(db);

    // Use an explicit target_dir inside the TempDir so the test is self-contained.
    let target_dir = dir.path().join("dump-out");
    let v = dispatch_val(
        50,
        "knowledge_dump_wal",
        json!({ "target_dir": target_dir.to_str().unwrap() }),
        state,
    )
    .await;

    assert_ok_resp(&v, 50);
    let r = &v["result"];
    assert_eq!(r["success"], true, "success field: {v}");
    assert_eq!(r["nodes_dumped"], 0, "nodes_dumped: {v}");
    assert_eq!(r["edges_dumped"], 0, "edges_dumped: {v}");
    assert_eq!(r["files_written"], 0, "files_written: {v}");
    assert!(
        r["target_dir"].is_string(),
        "target_dir must be a string: {v}"
    );
}

// ── knowledge_merge_entities ──────────────────────────────────────────────────

/// Validation error: neither canonical_uuid nor canonical_name provided → success: false.
#[tokio::test]
async fn parity_merge_entities_missing_canonical() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        55,
        "knowledge_merge_entities",
        json!({ "merge_all_by_name": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 55);
    let r = &v["result"];
    assert_eq!(
        r["success"], false,
        "must fail when no canonical provided: {v}"
    );
    assert!(
        r["errors"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "errors must be non-empty: {v}"
    );
}

/// Canonical not found on empty graph → success: false with canonical error.
#[tokio::test]
async fn parity_merge_entities_canonical_not_found() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        56,
        "knowledge_merge_entities",
        json!({ "canonical_name": "Brett", "merge_all_by_name": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 56);
    let r = &v["result"];
    assert_eq!(
        r["success"], false,
        "must fail when canonical not found: {v}"
    );
    assert!(
        r["errors"]
            .as_array()
            .map(|a| a
                .iter()
                .any(|e| e.as_str().map(|s| s.contains("not found")).unwrap_or(false)))
            .unwrap_or(false),
        "error must mention 'not found': {v}"
    );
}

/// Single entity with given name → merged_count: 0, success: true (noop through handler).
#[tokio::test]
async fn parity_merge_entities_noop_single_entity() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "brett-parity-001".to_string(),
            name: "Brett".to_string(),
            group_id: "liminis".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-01-01 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "parity test entity".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }
    let state = make_state(db);
    let v = dispatch_val(
        57,
        "knowledge_merge_entities",
        json!({ "canonical_name": "Brett", "merge_all_by_name": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 57);
    let r = &v["result"];
    assert_eq!(r["success"], true, "single entity must succeed: {v}");
    assert_eq!(r["merged_count"], 0, "nothing to merge: {v}");
    assert_eq!(r["skipped"], 0, "nothing skipped: {v}");
    assert!(
        r["canonical_uuid"].is_string(),
        "canonical_uuid must be present: {v}"
    );
    assert!(
        r["edges_rewritten"].is_number(),
        "edges_rewritten must be numeric: {v}"
    );
    assert!(
        r["edges_deduplicated"].is_number(),
        "edges_deduplicated must be numeric: {v}"
    );
    assert!(r["errors"].is_array(), "errors must be an array: {v}");
}

// ── FR-011, SC-004: RELATES_TO / MENTIONS edge type correctness ───────────────

/// Inserts a RELATES_TO edge with a known `created_at` timestamp, queries it back, and asserts
/// the returned `created_at` is a non-empty, valid datetime string — not a TYPE_MISMATCH error.
///
/// SC-004: zero TYPE_MISMATCH errors produced by direct-write paths.
#[tokio::test]
async fn test_relates_to_edge_timestamp_type() {
    let (db, _dir) = make_db(4);

    // Insert two entities and a RELATES_TO edge directly via the Conn API.
    // This exercises `insert_relates_to_edge` → `exec_params` → `json_value_for_param`.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "irt-src-001".to_string(),
            name: "SourceEntity".to_string(),
            group_id: "irt-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-03-01T08:00:00Z".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "source entity".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "irt-dst-001".to_string(),
            name: "TargetEntity".to_string(),
            group_id: "irt-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-03-01T08:00:01Z".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "target entity".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_relates_to_edge(&RelatesToEdge {
            uuid: "irt-edge-001".to_string(),
            name: "relates to".to_string(),
            source_node_uuid: "irt-src-001".to_string(),
            target_node_uuid: "irt-dst-001".to_string(),
            group_id: "irt-group".to_string(),
            fact: "SourceEntity relates to TargetEntity".to_string(),
            fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
            created_at: "2024-03-01T08:00:02Z".to_string(),
            valid_at: None,
            invalid_at: None,
            attributes: "{}".to_string(),
            relation_type: None,
            episode_uuids: vec![],
            source_descriptions: vec![],
        })
        .unwrap();
    }

    // Query the RelatesToNode_ created_at directly via Cypher to verify correct type storage.
    // A TYPE_MISMATCH during insert would cause lbug to store the wrong type; reading back via
    // cypher_query() would then return an error string or empty value (SC-004).
    let conn = db.connect().unwrap();
    assert_eq!(
        conn.count_nodes("RelatesToNode_").unwrap(),
        1,
        "must have exactly one RelatesToNode_ shadow node"
    );
    let rows = conn
        .cypher_query("MATCH (rn:RelatesToNode_ {uuid: 'irt-edge-001'}) RETURN rn.created_at")
        .expect("querying created_at on RelatesToNode_ must succeed (SC-004)");
    assert_eq!(rows.len(), 1, "must return exactly one row");
    let created_at = &rows[0][0];
    // Check for the specific date we inserted: "2024-03-01T08:00:02Z" → stored as TIMESTAMP.
    // lbug returns TIMESTAMP as "YYYY-MM-DD HH:MM:SS[.ffffff]" or RFC-3339; either way it
    // contains the date portion. TYPE_MISMATCH or an error string won't contain "2024-03-01".
    assert!(
        created_at.contains("2024-03-01"),
        "created_at must contain the expected date '2024-03-01' — \
         a TYPE_MISMATCH or wrong type would not match (SC-004): {created_at}"
    );
}

// ── FR-012: same_as correction timestamp safety ───────────────────────────────

/// Applies a `same_as` correction between two real entities and verifies the correction
/// completes without error. A TYPE_MISMATCH on any timestamp written by `apply_same_as`
/// would cause the correction to fail or return an error (FR-012).
#[tokio::test]
async fn test_same_as_correction_timestamp_type() {
    let (db, _dir) = make_db(4);

    // Insert two entities: one canonical, one to be merged as an alias.
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "samc-canonical-001".to_string(),
            name: "CanonicalPerson".to_string(),
            group_id: "samc-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-04-01T09:00:00Z".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "the canonical person".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "samc-alias-001".to_string(),
            name: "AliasPerson".to_string(),
            group_id: "samc-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-04-01T09:00:01Z".to_string(),
            name_embedding: vec![0.9, 0.1, 0.0, 0.0],
            summary: "an alias for the canonical person".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    // Write a corrections YAML with a same_as entry.
    let workspace_dir = tempfile::TempDir::new().unwrap();
    let liminis_dir = workspace_dir.path().join(".liminis");
    std::fs::create_dir_all(&liminis_dir).unwrap();
    let corrections_path = liminis_dir.join("knowledge-corrections.yaml");
    std::fs::write(
        &corrections_path,
        "corrections:\n  - id: samc-001\n    type: same_as\n    canonical: \"CanonicalPerson\"\n    aliases:\n      - \"AliasPerson\"\n",
    )
    .unwrap();

    let state = make_state_with_workspace(db.clone(), workspace_dir.path().to_path_buf());
    let v = dispatch_val(72, "knowledge_apply_corrections", json!({}), state).await;

    // The correction must succeed. A TYPE_MISMATCH on any timestamp write would propagate as
    // an error in the result (FR-012).
    assert_ok_resp(&v, 72);
    let r = &v["result"];
    assert_eq!(
        r["success"], true,
        "same_as correction must succeed without TYPE_MISMATCH (FR-012): {v}"
    );
    assert!(
        r["errors"].as_array().map(|a| a.is_empty()).unwrap_or(true),
        "same_as correction must produce zero errors: {v}"
    );

    // Verify the canonical entity still has a valid created_at after the correction.
    let canonical = db
        .connect()
        .unwrap()
        .get_entity_by_uuid("samc-canonical-001")
        .expect("canonical entity must be queryable after same_as correction");
    if let Some(e) = canonical {
        let created_at = &e.created_at;
        // Check for the specific date we inserted: "2024-04-01T09:00:00Z" → stored and read back
        // as "2024-04-01 09:00:00" (space-format). A TYPE_MISMATCH artifact won't contain this.
        assert!(
            created_at.contains("2024-04-01"),
            "canonical entity created_at must contain the expected date '2024-04-01' \
             after same_as correction (FR-012): {created_at}"
        );
    }
}

/// Without an ontology in AppState → -32000 error mentioning relation_types.
#[tokio::test]
async fn parity_canonicalize_no_ontology_error_shape() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        60,
        "knowledge_canonicalize_relations",
        json!({ "dry_run": true }),
        state,
    )
    .await;
    assert_err_resp(&v, 60, -32000);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("relation_type") || msg.contains("ontology"),
        "error must mention relation_types or ontology: {v}"
    );
}

/// Regression: canonicalize_relations MUST NOT delete arrow-named edges (FR-016–FR-019, SC-007).
///
/// Before ADR-0033, EdgeClass::Noise edges were DETACH DELETE'd. After the fix they are
/// reclassified to UNCLASSIFIED. This test inserts 10 ALL-CAPS arrow-named edges with a
/// populated relation_type and verifies all 10 survive a live canonicalize pass.
#[tokio::test]
async fn parity_canonicalize_no_deletion_of_arrow_edges() {
    let (db, _dir) = make_db(4);

    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "cnde-src-001".to_string(),
            name: "BRETT".to_string(),
            group_id: "cnde-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-05-01T00:00:00Z".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "source entity".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "cnde-dst-001".to_string(),
            name: "RAJI".to_string(),
            group_id: "cnde-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-05-01T00:00:01Z".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "target entity".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        // 10 ALL-CAPS arrow-named edges with populated relation_type.
        // These match is_noise_edge() and would have been deleted before ADR-0033.
        for i in 0..10usize {
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: format!("cnde-edge-{i:03}"),
                name: "BRETT → RAJI".to_string(),
                source_node_uuid: "cnde-src-001".to_string(),
                target_node_uuid: "cnde-dst-001".to_string(),
                group_id: "cnde-group".to_string(),
                fact: format!("Brett knows Raji (fact {i})"),
                fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
                created_at: format!("2024-05-01T00:00:{:02}Z", i + 2),
                valid_at: None,
                invalid_at: None,
                attributes: "{}".to_string(),
                relation_type: Some("KNOWS".to_string()),
                episode_uuids: vec![],
                source_descriptions: vec![],
            })
            .unwrap();
        }
    }

    // Ontology with no keywords that match "BRETT → RAJI" → all 10 edges go through Noise path.
    let ontology = Arc::new(Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Entity".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "AFFILIATED_WITH".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec!["affiliat".to_string()],
        }],
        ancestor_map: std::collections::HashMap::new(),
    });
    let state = make_state_with_ontology(db.clone(), ontology);

    let v = dispatch_val(
        70,
        "knowledge_canonicalize_relations",
        json!({ "dry_run": false }),
        state,
    )
    .await;
    assert_ok_resp(&v, 70);

    // All 10 edges must survive — canonicalize must not delete arrow-named edges (FR-016).
    let edge_count = db.connect().unwrap().count_relates_to_edges().unwrap();
    assert_eq!(
        edge_count, 10,
        "canonicalize must not delete arrow-named edges (ADR-0033): only {edge_count} of 10 remain"
    );

    // noise_count should be 10 (classified as noise, but not deleted)
    let r = &v["result"];
    assert_eq!(
        r["noise_count"], 10,
        "noise_count must reflect 10 noise-classified edges: {v}"
    );

    // Pre-existing relation_type values on noise edges must be preserved — the Noise branch
    // must NOT overwrite a populated relation_type with UNCLASSIFIED (Copilot review fix).
    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'BRETT → RAJI' \
             RETURN n.relation_type ORDER BY n.uuid",
        )
        .unwrap();
    assert_eq!(rows.len(), 10, "all 10 noise edges must still exist");
    for row in &rows {
        assert_eq!(
            row[0], "KNOWS",
            "noise edge relation_type must not be overwritten by canonicalize (ADR-0033): {:?}",
            row[0]
        );
    }
}

/// With a valid ontology + empty DB + dry_run:true → result has expected shape.
#[tokio::test]
async fn parity_canonicalize_relations_shape() {
    let (db, _dir) = make_db(4);
    let ontology = Arc::new(Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Entity".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "RELATES_TO".to_string(),
            description: Some("generic relation".to_string()),
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec!["relat".to_string()],
        }],
        ancestor_map: std::collections::HashMap::new(),
    });
    let state = make_state_with_ontology(db, ontology);
    let v = dispatch_val(
        61,
        "knowledge_canonicalize_relations",
        json!({ "dry_run": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 61);
    let r = &v["result"];
    assert_eq!(r["dry_run"], true, "dry_run must be true: {v}");
    assert!(
        r["total_edges"].is_number(),
        "total_edges must be numeric: {v}"
    );
    assert!(
        r["mapped_count"].is_number(),
        "mapped_count must be numeric: {v}"
    );
    assert!(
        r["noise_count"].is_number(),
        "noise_count must be numeric: {v}"
    );
    assert!(
        r["residual_count"].is_number(),
        "residual_count must be numeric: {v}"
    );
}

// ── Backfill IPC parity tests (FR-005–FR-015) ─────────────────────────────────

/// Empty DB + dry_run:true → response shape is correct (FR-006, SC-006).
#[tokio::test]
async fn parity_backfill_relation_types_shape() {
    let (db, _dir) = make_db(4);
    let state = make_state(db);
    let v = dispatch_val(
        80,
        "knowledge_backfill_relation_types",
        json!({ "dry_run": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 80);
    let r = &v["result"];
    assert_eq!(r["total_edges"], 0, "empty DB must have total_edges=0: {v}");
    assert_eq!(r["backfilled"], 0, "empty DB must have backfilled=0: {v}");
    assert_eq!(r["dry_run"], true, "dry_run flag must be reflected: {v}");
}

/// dry_run:true on a graph with 3 empty + 2 populated edges → backfilled=3, no mutations (FR-006, SC-006).
#[tokio::test]
async fn parity_backfill_dry_run_counts() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bfdr-src-001".to_string(),
            name: "Alice".to_string(),
            group_id: "bfdr-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-06-01T00:00:00Z".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "source".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bfdr-dst-001".to_string(),
            name: "Bob".to_string(),
            group_id: "bfdr-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-06-01T00:00:01Z".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "target".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        // 3 edges with empty relation_type
        for i in 0..3usize {
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: format!("bfdr-empty-{i:03}"),
                name: "Alice → Bob".to_string(),
                source_node_uuid: "bfdr-src-001".to_string(),
                target_node_uuid: "bfdr-dst-001".to_string(),
                group_id: "bfdr-group".to_string(),
                fact: "Alice knows Bob".to_string(),
                fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
                created_at: format!("2024-06-01T00:00:{:02}Z", i + 2),
                valid_at: None,
                invalid_at: None,
                attributes: "{}".to_string(),
                relation_type: None,
                episode_uuids: vec![],
                source_descriptions: vec![],
            })
            .unwrap();
        }
        // 2 edges with populated relation_type
        for i in 0..2usize {
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: format!("bfdr-pop-{i:03}"),
                name: "Alice → Bob".to_string(),
                source_node_uuid: "bfdr-src-001".to_string(),
                target_node_uuid: "bfdr-dst-001".to_string(),
                group_id: "bfdr-group".to_string(),
                fact: "Alice knows Bob well".to_string(),
                fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
                created_at: format!("2024-06-01T00:00:{:02}Z", i + 5),
                valid_at: None,
                invalid_at: None,
                attributes: "{}".to_string(),
                relation_type: Some("KNOWS".to_string()),
                episode_uuids: vec![],
                source_descriptions: vec![],
            })
            .unwrap();
        }
    }

    let state = make_state(db.clone());
    let v = dispatch_val(
        81,
        "knowledge_backfill_relation_types",
        json!({ "dry_run": true }),
        state,
    )
    .await;
    assert_ok_resp(&v, 81);
    let r = &v["result"];
    assert_eq!(r["total_edges"], 5, "must count all 5 edges: {v}");
    assert_eq!(r["backfilled"], 3, "dry_run must count 3 empty edges: {v}");
    assert_eq!(r["dry_run"], true, "dry_run flag must be reflected: {v}");

    // No mutations: all 5 edges should still have their original relation_type
    let rows = db
        .connect()
        .unwrap()
        .cypher_query("MATCH (n:RelatesToNode_) WHERE n.uuid STARTS WITH 'bfdr-empty-' RETURN n.relation_type ORDER BY n.uuid")
        .unwrap();
    assert_eq!(rows.len(), 3, "must have 3 empty edges");
    for row in &rows {
        assert!(
            row[0].is_empty(),
            "dry_run must not modify edges: relation_type should still be empty/null"
        );
    }
}

/// Live mode: 3 empty edges get relation_type, 2 populated are unchanged, no edge deleted (FR-007–FR-011, SC-002–SC-004).
#[tokio::test]
async fn parity_backfill_live_fills_empty() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bflv-src-001".to_string(),
            name: "Alice".to_string(),
            group_id: "bflv-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-07-01T00:00:00Z".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "source".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bflv-dst-001".to_string(),
            name: "Bob".to_string(),
            group_id: "bflv-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-07-01T00:00:01Z".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "target".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        for i in 0..3usize {
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: format!("bflv-empty-{i:03}"),
                name: "Alice → Bob".to_string(),
                source_node_uuid: "bflv-src-001".to_string(),
                target_node_uuid: "bflv-dst-001".to_string(),
                group_id: "bflv-group".to_string(),
                fact: "Alice knows Bob".to_string(),
                fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
                created_at: format!("2024-07-01T00:00:{:02}Z", i + 2),
                valid_at: None,
                invalid_at: None,
                attributes: "{}".to_string(),
                relation_type: None,
                episode_uuids: vec![],
                source_descriptions: vec![],
            })
            .unwrap();
        }
        for i in 0..2usize {
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: format!("bflv-pop-{i:03}"),
                name: "Alice → Bob".to_string(),
                source_node_uuid: "bflv-src-001".to_string(),
                target_node_uuid: "bflv-dst-001".to_string(),
                group_id: "bflv-group".to_string(),
                fact: "Alice knows Bob well".to_string(),
                fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
                created_at: format!("2024-07-01T00:00:{:02}Z", i + 5),
                valid_at: None,
                invalid_at: None,
                attributes: "{}".to_string(),
                relation_type: Some("KNOWS".to_string()),
                episode_uuids: vec![],
                source_descriptions: vec![],
            })
            .unwrap();
        }
    }

    let state = make_state(db.clone());
    let v = dispatch_val(
        82,
        "knowledge_backfill_relation_types",
        json!({ "dry_run": false }),
        state,
    )
    .await;
    assert_ok_resp(&v, 82);
    let r = &v["result"];
    assert_eq!(r["total_edges"], 5, "must count all 5 edges: {v}");
    assert_eq!(r["backfilled"], 3, "must report 3 backfilled edges: {v}");
    assert_eq!(r["dry_run"], false, "must report live mode: {v}");

    // All 5 edges must still exist (FR-010, SC-004)
    let edge_count = db.connect().unwrap().count_relates_to_edges().unwrap();
    assert_eq!(
        edge_count, 5,
        "no edges may be deleted by backfill: {edge_count}"
    );

    // The 3 empty edges now have a non-empty relation_type (FR-007)
    let conn = db.connect().unwrap();
    let empty_rows = conn
        .cypher_query("MATCH (n:RelatesToNode_) WHERE n.uuid STARTS WITH 'bflv-empty-' RETURN n.relation_type ORDER BY n.uuid")
        .unwrap();
    assert_eq!(
        empty_rows.len(),
        3,
        "must query back 3 formerly-empty edges"
    );
    for row in &empty_rows {
        assert!(
            !row[0].is_empty(),
            "formerly-empty edge must have non-empty relation_type after live backfill: {:?}",
            row[0]
        );
    }

    // The 2 populated edges are unchanged (FR-007: must NOT overwrite existing values)
    let pop_rows = conn
        .cypher_query("MATCH (n:RelatesToNode_) WHERE n.uuid STARTS WITH 'bflv-pop-' RETURN n.relation_type ORDER BY n.uuid")
        .unwrap();
    assert_eq!(pop_rows.len(), 2, "must query back 2 populated edges");
    for row in &pop_rows {
        assert_eq!(
            row[0], "KNOWS",
            "populated relation_type must be unchanged after backfill: {:?}",
            row[0]
        );
    }
}

/// Idempotency: running live backfill twice produces zero new mutations on second run (FR-013, SC-006).
#[tokio::test]
async fn parity_backfill_idempotent() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bfid-src-001".to_string(),
            name: "Alice".to_string(),
            group_id: "bfid-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-08-01T00:00:00Z".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "source".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.insert_entity(&EntityRow {
            uuid: "bfid-dst-001".to_string(),
            name: "Bob".to_string(),
            group_id: "bfid-group".to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2024-08-01T00:00:01Z".to_string(),
            name_embedding: vec![0.0, 1.0, 0.0, 0.0],
            summary: "target".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        for i in 0..3usize {
            conn.insert_relates_to_edge(&RelatesToEdge {
                uuid: format!("bfid-empty-{i:03}"),
                name: "Alice → Bob".to_string(),
                source_node_uuid: "bfid-src-001".to_string(),
                target_node_uuid: "bfid-dst-001".to_string(),
                group_id: "bfid-group".to_string(),
                fact: "Alice knows Bob".to_string(),
                fact_embedding: vec![0.5, 0.5, 0.0, 0.0],
                created_at: format!("2024-08-01T00:00:{:02}Z", i + 2),
                valid_at: None,
                invalid_at: None,
                attributes: "{}".to_string(),
                relation_type: None,
                episode_uuids: vec![],
                source_descriptions: vec![],
            })
            .unwrap();
        }
    }

    // First run — fills 3 empty edges
    let state1 = make_state(db.clone());
    let v1 = dispatch_val(
        83,
        "knowledge_backfill_relation_types",
        json!({ "dry_run": false }),
        state1,
    )
    .await;
    assert_ok_resp(&v1, 83);
    assert_eq!(
        v1["result"]["backfilled"], 3,
        "first run must backfill 3: {v1}"
    );

    // Second run — must find zero empty edges and produce no new WAL mutations
    let state2 = make_state(db.clone());
    let v2 = dispatch_val(
        84,
        "knowledge_backfill_relation_types",
        json!({ "dry_run": false }),
        state2,
    )
    .await;
    assert_ok_resp(&v2, 84);
    assert_eq!(
        v2["result"]["backfilled"], 0,
        "second run on already-backfilled graph must report backfilled=0 (FR-013): {v2}"
    );
    assert_eq!(
        v2["result"]["total_edges"], 3,
        "total_edges unchanged: {v2}"
    );
}

// ── #177: reprocess_entity_types scope / dry_run ──────────────────────────────

/// Backward-compat: calling with no `scope` param must behave identically to pre-#177.
#[tokio::test]
async fn test_reprocess_scope_untyped_default() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        90,
        "knowledge_reprocess_entity_types",
        json!({"group_id": "liminis"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 90);
    assert_eq!(v["result"]["success"], true, "no scope → success: {v}");
    assert_eq!(
        v["result"]["reclassified_count"], 0,
        "no entities → 0 reclassified: {v}"
    );
}

/// `scope=off_ontology` without an ontology loaded → structured error, not a crash.
#[tokio::test]
async fn test_reprocess_scope_off_ontology_no_ontology() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        91,
        "knowledge_reprocess_entity_types",
        json!({"scope": "off_ontology"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 91);
    assert_eq!(
        v["result"]["success"], false,
        "off_ontology without ontology must fail: {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// `scope=all` without an ontology loaded → structured error.
#[tokio::test]
async fn test_reprocess_scope_all_requires_ontology() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        92,
        "knowledge_reprocess_entity_types",
        json!({"scope": "all"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 92);
    assert_eq!(
        v["result"]["success"], false,
        "scope=all without ontology must fail: {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// Unknown scope value → structured error, not a crash.
#[tokio::test]
async fn test_reprocess_scope_invalid() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        93,
        "knowledge_reprocess_entity_types",
        json!({"scope": "bad_value"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 93);
    assert_eq!(
        v["result"]["success"], false,
        "invalid scope must fail: {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// `dry_run: true` with `scope=off_ontology` returns a plan but mutates nothing.
#[tokio::test]
async fn test_reprocess_dry_run_returns_plan() {
    let (db, _dir) = make_db(4);
    // Seed 2 entities with an off-ontology label ("Council" is not in the Person ontology).
    insert_test_entity(
        &db,
        "dry-run-001",
        "Alice",
        "liminis",
        vec!["Entity".to_string(), "Council".to_string()],
    );
    insert_test_entity(
        &db,
        "dry-run-002",
        "Bob",
        "liminis",
        vec!["Entity".to_string(), "Council".to_string()],
    );

    let ontology = make_person_ontology();
    let extractor = Arc::new(ClassifyingExtractor::new("Person"));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        94,
        "knowledge_reprocess_entity_types",
        json!({"scope": "off_ontology", "dry_run": true}),
        state,
    )
    .await;
    assert_ok_resp(&v, 94);
    let r = &v["result"];
    assert_eq!(
        r["would_reclassify_count"], 2,
        "dry_run must report 2 planned reclassifications: {v}"
    );
    assert!(r["plan"].is_array(), "plan must be an array: {v}");
    assert_eq!(
        r["plan"].as_array().unwrap().len(),
        2,
        "plan must have 2 entries: {v}"
    );

    // Verify labels are unchanged after dry_run.
    let conn = db.connect().unwrap();
    let e1 = conn.get_entity_by_uuid("dry-run-001").unwrap().unwrap();
    assert_eq!(
        e1.labels,
        vec!["Entity".to_string(), "Council".to_string()],
        "dry_run must not mutate labels: {:?}",
        e1.labels
    );
    let e2 = conn.get_entity_by_uuid("dry-run-002").unwrap().unwrap();
    assert_eq!(
        e2.labels,
        vec!["Entity".to_string(), "Council".to_string()],
        "dry_run must not mutate labels: {:?}",
        e2.labels
    );
}

/// Two consecutive `scope=off_ontology` runs: second run produces `reclassified_count: 0`.
#[tokio::test]
async fn test_reprocess_scope_off_ontology_idempotency() {
    let (db, _dir) = make_db(4);
    // Seed 2 entities with off-ontology label "Council".
    insert_test_entity(
        &db,
        "idempotent-001",
        "Carol",
        "liminis",
        vec!["Entity".to_string(), "Council".to_string()],
    );
    insert_test_entity(
        &db,
        "idempotent-002",
        "Dave",
        "liminis",
        vec!["Entity".to_string(), "Council".to_string()],
    );

    let ontology = make_person_ontology();
    let extractor = Arc::new(ClassifyingExtractor::new("Person"));
    let workspace = TempDir::new().unwrap();

    // First run: should reclassify both entities from Council → Person.
    let state1 = make_state_with_ontology_and_extractor(
        db.clone(),
        Arc::clone(&ontology),
        Arc::clone(&extractor) as Arc<dyn Extractor>,
        workspace.path().to_path_buf(),
    );
    let v1 = dispatch_val(
        95,
        "knowledge_reprocess_entity_types",
        json!({"scope": "off_ontology"}),
        state1,
    )
    .await;
    assert_ok_resp(&v1, 95);
    assert_eq!(
        v1["result"]["reclassified_count"], 2,
        "first run must reclassify 2 entities: {v1}"
    );

    // Second run: entities are now ontology-aligned; nothing to reclassify.
    let state2 = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );
    let v2 = dispatch_val(
        96,
        "knowledge_reprocess_entity_types",
        json!({"scope": "off_ontology"}),
        state2,
    )
    .await;
    assert_ok_resp(&v2, 96);
    assert_eq!(
        v2["result"]["reclassified_count"], 0,
        "second run on corrected graph must report 0 (FR-016): {v2}"
    );
}

/// #213 FR-002: a progress-tracked, non-dry-run reclassification emits at least one
/// `{"type":"progress"}` event (the unconditional write-phase-entry send), proving
/// `handle_reprocess_entity_types` actually threads `progress_tx` through rather than
/// silently ignoring it when a caller supplies a progress token.
#[tokio::test]
async fn test_reprocess_entity_types_emits_progress_when_tracked() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "progress-001",
        "Erin",
        "liminis",
        vec!["Entity".to_string(), "Council".to_string()],
    );

    let ontology = make_person_ontology();
    let extractor = Arc::new(ClassifyingExtractor::new("Person"));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db,
        ontology,
        extractor as Arc<dyn Extractor>,
        workspace.path().to_path_buf(),
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let resp = handlers::dispatch(
        req(
            97,
            "knowledge_reprocess_entity_types",
            json!({"scope": "off_ontology"}),
        ),
        state,
        Some(tx),
    )
    .await;
    let v = serde_json::to_value(resp).unwrap();
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["reclassified_count"], 1, "{v}");

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        !events.is_empty(),
        "expected at least one progress event when a progress_tx is supplied"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "progress" && e["phase"] == "classifying"),
        "expected a classifying-phase progress event even for a run smaller than \
         REPROCESS_PROGRESS_EVERY (Copilot review finding on PR #224 — the classify loop's \
         periodic send alone never fires for small runs): {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "progress" && e["phase"] == "writing"),
        "expected a writing-phase progress event: {events:?}"
    );
}

/// #213 FR-004: without a progress token (`progress_tx: None`), behavior is unchanged —
/// no progress events, plain blocking call. `dispatch_val` always passes `None`, so this
/// is exercised implicitly by every other `reprocess_entity_types` test above; this test
/// makes the "no progress_tx → no progress channel touched" invariant explicit.
#[tokio::test]
async fn test_reprocess_entity_types_no_progress_tx_unchanged_behavior() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "progress-002",
        "Frank",
        "liminis",
        vec!["Entity".to_string(), "Council".to_string()],
    );

    let ontology = make_person_ontology();
    let extractor = Arc::new(ClassifyingExtractor::new("Person"));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db,
        ontology,
        extractor as Arc<dyn Extractor>,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        98,
        "knowledge_reprocess_entity_types",
        json!({"scope": "off_ontology"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 98);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["reclassified_count"], 1, "{v}");
}

// ── #210: reprocess_relation_types scope / dry_run ─────────────────────────────

/// `scope=untyped` (default) on an empty DB → success, 0 reclassified.
#[tokio::test]
async fn test_reprocess_relation_scope_untyped_default() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let ontology = make_relation_ontology();
    let extractor = Arc::new(RelationClassifyingExtractor::new(HashMap::new()));
    let state = make_state_with_ontology_and_extractor(
        db,
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );
    let v = dispatch_val(
        100,
        "knowledge_reprocess_relation_types",
        json!({"group_id": "liminis"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 100);
    assert_eq!(v["result"]["success"], true, "no edges → success: {v}");
    assert_eq!(
        v["result"]["reclassified_count"], 0,
        "no edges → 0 reclassified: {v}"
    );
    assert_eq!(
        v["result"]["breakdown"],
        json!({}),
        "zero-candidate apply → breakdown: {{}}: {v}"
    );
}

/// `scope=off_ontology` without an ontology loaded → structured error, not a crash (FR-002, SC-002).
#[tokio::test]
async fn test_reprocess_relation_scope_off_ontology_no_ontology() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        101,
        "knowledge_reprocess_relation_types",
        json!({"scope": "off_ontology"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 101);
    assert_eq!(
        v["result"]["success"], false,
        "off_ontology without ontology must fail: {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// `scope=all` without an ontology loaded → structured error (FR-002, SC-002).
#[tokio::test]
async fn test_reprocess_relation_scope_all_requires_ontology() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        102,
        "knowledge_reprocess_relation_types",
        json!({"scope": "all"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 102);
    assert_eq!(
        v["result"]["success"], false,
        "scope=all without ontology must fail: {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// `scope=untyped` without an ontology loaded → also a structured error (A1: unlike
/// reprocess_entity_types, relation classification has no open-ended fallback for any scope).
#[tokio::test]
async fn test_reprocess_relation_scope_untyped_requires_ontology() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_workspace(db, workspace.path().to_path_buf());
    let v = dispatch_val(
        103,
        "knowledge_reprocess_relation_types",
        json!({"scope": "untyped"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 103);
    assert_eq!(
        v["result"]["success"], false,
        "scope=untyped without ontology must fail (A1): {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// Unknown scope value → structured error, not a crash.
#[tokio::test]
async fn test_reprocess_relation_scope_invalid() {
    let (db, _dir) = make_db(4);
    let workspace = TempDir::new().unwrap();
    let ontology = make_relation_ontology();
    let extractor = Arc::new(RelationClassifyingExtractor::new(HashMap::new()));
    let state = make_state_with_ontology_and_extractor(
        db,
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );
    let v = dispatch_val(
        104,
        "knowledge_reprocess_relation_types",
        json!({"scope": "bad_value"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 104);
    assert_eq!(
        v["result"]["success"], false,
        "invalid scope must fail: {v}"
    );
    assert!(
        v["result"]["error"].is_string(),
        "error field must be a string: {v}"
    );
}

/// `dry_run: true` with `scope=off_ontology` returns a plan + breakdown, mutates nothing (FR-011, FR-012, SC-003).
#[tokio::test]
async fn test_reprocess_relation_dry_run_returns_plan_and_breakdown() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "rrdr-src",
        "Alice",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_entity(
        &db,
        "rrdr-dst",
        "Report",
        "liminis",
        vec!["Entity".to_string()],
    );
    // One edge classifies to AUTHORED, one abstains → UNCLASSIFIED.
    insert_test_edge(
        &db,
        "rrdr-edge-1",
        "rrdr-src",
        "rrdr-dst",
        "liminis",
        "Alice authored the report",
        None,
    );
    insert_test_edge(
        &db,
        "rrdr-edge-2",
        "rrdr-src",
        "rrdr-dst",
        "liminis",
        "Alice has an unrelated fact",
        None,
    );

    let ontology = make_relation_ontology();
    let mut verdicts = HashMap::new();
    verdicts.insert(
        "Alice authored the report".to_string(),
        "AUTHORED".to_string(),
    );
    let extractor = Arc::new(RelationClassifyingExtractor::new(verdicts));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        105,
        "knowledge_reprocess_relation_types",
        json!({"scope": "untyped", "dry_run": true}),
        state,
    )
    .await;
    assert_ok_resp(&v, 105);
    let r = &v["result"];
    assert_eq!(
        r["would_reclassify_count"], 2,
        "dry_run must report 2 planned reclassifications: {v}"
    );
    assert_eq!(
        r["plan"].as_array().unwrap().len(),
        2,
        "plan must have 2 entries: {v}"
    );
    assert_eq!(
        r["breakdown"]["AUTHORED"], 1,
        "breakdown must count 1 AUTHORED: {v}"
    );
    assert_eq!(
        r["breakdown"]["UNCLASSIFIED"], 1,
        "breakdown must count 1 UNCLASSIFIED (abstention): {v}"
    );

    // Verify relation_type is unchanged after dry_run.
    let conn = db.connect().unwrap();
    let e1 = conn.get_edge_by_uuid("rrdr-edge-1").unwrap().unwrap();
    assert!(
        e1.relation_type.unwrap_or_default().is_empty(),
        "dry_run must not mutate relation_type"
    );
}

/// A whitespace-only `relation_type` is a candidate under `scope=untyped` (matching
/// `is_untyped`'s `trim().is_empty()` predicate), and its `old_type` in the dry_run plan must be
/// `null`, not the literal whitespace string — the candidate's recorded `current_type` must use
/// the same untyped predicate as the scope filter (Copilot review finding on PR #222).
#[tokio::test]
async fn test_reprocess_relation_whitespace_only_type_reports_null_old_type() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "rrws-src",
        "Alice",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_entity(
        &db,
        "rrws-dst",
        "Report",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_edge(
        &db,
        "rrws-edge-1",
        "rrws-src",
        "rrws-dst",
        "liminis",
        "Alice authored the report",
        Some("   "),
    );

    let ontology = make_relation_ontology();
    let mut verdicts = HashMap::new();
    verdicts.insert(
        "Alice authored the report".to_string(),
        "AUTHORED".to_string(),
    );
    let extractor = Arc::new(RelationClassifyingExtractor::new(verdicts));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        110,
        "knowledge_reprocess_relation_types",
        json!({"scope": "untyped", "dry_run": true}),
        state,
    )
    .await;
    assert_ok_resp(&v, 110);
    let r = &v["result"];
    assert_eq!(
        r["would_reclassify_count"], 1,
        "whitespace-only relation_type must be a scope=untyped candidate: {v}"
    );
    assert_eq!(
        r["plan"][0]["old_type"],
        Value::Null,
        "old_type must be null for a whitespace-only current relation_type, not the literal \
         whitespace string: {v}"
    );
}

/// Two consecutive `scope=off_ontology` runs: second run produces `reclassified_count: 0` (FR-009, SC-004).
#[tokio::test]
async fn test_reprocess_relation_scope_off_ontology_idempotency() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "rrid-src",
        "Bob",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_entity(
        &db,
        "rrid-dst",
        "Acme",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_edge(
        &db,
        "rrid-edge-1",
        "rrid-src",
        "rrid-dst",
        "liminis",
        "Bob is affiliated with Acme",
        Some("HOLDS"), // off-ontology predicate from a prior bad canonicalize pass
    );

    let ontology = make_relation_ontology();
    let mut verdicts = HashMap::new();
    verdicts.insert(
        "Bob is affiliated with Acme".to_string(),
        "AFFILIATED_WITH".to_string(),
    );
    let extractor = Arc::new(RelationClassifyingExtractor::new(verdicts));
    let workspace = TempDir::new().unwrap();

    // First run: HOLDS → AFFILIATED_WITH.
    let state1 = make_state_with_ontology_and_extractor(
        db.clone(),
        Arc::clone(&ontology),
        Arc::clone(&extractor) as Arc<dyn Extractor>,
        workspace.path().to_path_buf(),
    );
    let v1 = dispatch_val(
        106,
        "knowledge_reprocess_relation_types",
        json!({"scope": "off_ontology"}),
        state1,
    )
    .await;
    assert_ok_resp(&v1, 106);
    assert_eq!(
        v1["result"]["reclassified_count"], 1,
        "first run must reclassify 1 edge: {v1}"
    );
    assert_eq!(
        v1["result"]["breakdown"],
        json!({"AFFILIATED_WITH": 1}),
        "first run breakdown must reflect the single reclassified edge: {v1}"
    );

    let conn = db.connect().unwrap();
    let edge = conn.get_edge_by_uuid("rrid-edge-1").unwrap().unwrap();
    assert_eq!(edge.relation_type.as_deref(), Some("AFFILIATED_WITH"));

    // Second run: edge is now ontology-aligned; nothing to reclassify.
    let state2 = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );
    let v2 = dispatch_val(
        107,
        "knowledge_reprocess_relation_types",
        json!({"scope": "off_ontology"}),
        state2,
    )
    .await;
    assert_ok_resp(&v2, 107);
    assert_eq!(
        v2["result"]["reclassified_count"], 0,
        "second run on corrected graph must report 0 (FR-009): {v2}"
    );
}

/// US1: `scope=untyped` over a mix of edges that classify cleanly and edges that abstain —
/// all abstained edges must land on UNCLASSIFIED, never left NULL/empty (FR-004, FR-008).
#[tokio::test]
async fn test_reprocess_relation_scope_untyped_mixed_classify_and_abstain() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "us1-src",
        "Carol",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_entity(&db, "us1-dst", "Org", "liminis", vec!["Entity".to_string()]);

    let mut verdicts = HashMap::new();
    // 4 edges classify to a declared type.
    for i in 0..4 {
        let fact = format!("Carol authored document {i}");
        insert_test_edge(
            &db,
            &format!("us1-authored-{i}"),
            "us1-src",
            "us1-dst",
            "liminis",
            &fact,
            None,
        );
        verdicts.insert(fact, "AUTHORED".to_string());
    }
    // 3 edges have facts the extractor cannot map — abstain.
    for i in 0..3 {
        let fact = format!("Carol has unrelated fact {i}");
        insert_test_edge(
            &db,
            &format!("us1-unclassified-{i}"),
            "us1-src",
            "us1-dst",
            "liminis",
            &fact,
            None,
        );
        // No verdict entry inserted → RelationClassifyingExtractor abstains.
    }

    let ontology = make_relation_ontology();
    let extractor = Arc::new(RelationClassifyingExtractor::new(verdicts));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        108,
        "knowledge_reprocess_relation_types",
        json!({"scope": "untyped"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 108);
    assert_eq!(
        v["result"]["reclassified_count"], 7,
        "all 7 untyped edges must be reclassified: {v}"
    );

    let conn = db.connect().unwrap();
    for i in 0..4 {
        let edge = conn
            .get_edge_by_uuid(&format!("us1-authored-{i}"))
            .unwrap()
            .unwrap();
        assert_eq!(
            edge.relation_type.as_deref(),
            Some("AUTHORED"),
            "edge {i} must classify to AUTHORED"
        );
    }
    for i in 0..3 {
        let edge = conn
            .get_edge_by_uuid(&format!("us1-unclassified-{i}"))
            .unwrap()
            .unwrap();
        assert_eq!(
            edge.relation_type.as_deref(),
            Some("UNCLASSIFIED"),
            "abstained edge {i} must be UNCLASSIFIED, never NULL/empty"
        );
    }
}

/// Issue #332 (FR-006): an apply run whose candidates *all* abstain must report a `breakdown`
/// distinguishing it from an all-confident run of the same size — the case the reporter
/// identified as indistinguishable from `reclassified_count` alone today.
#[tokio::test]
async fn test_reprocess_relation_apply_all_abstain_breakdown() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "abstain-src",
        "Erin",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_entity(
        &db,
        "abstain-dst",
        "Org",
        "liminis",
        vec!["Entity".to_string()],
    );

    // 3 edges, none with a verdict entry → RelationClassifyingExtractor abstains on every one.
    for i in 0..3 {
        insert_test_edge(
            &db,
            &format!("abstain-edge-{i}"),
            "abstain-src",
            "abstain-dst",
            "liminis",
            &format!("Erin has unclassifiable fact {i}"),
            None,
        );
    }

    let ontology = make_relation_ontology();
    let extractor = Arc::new(RelationClassifyingExtractor::new(HashMap::new()));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        110,
        "knowledge_reprocess_relation_types",
        json!({"scope": "untyped"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 110);
    assert_eq!(
        v["result"]["reclassified_count"], 3,
        "all 3 abstained edges are still real writes to UNCLASSIFIED (ADR-0037): {v}"
    );
    assert_eq!(
        v["result"]["breakdown"],
        json!({"UNCLASSIFIED": 3}),
        "all-abstain breakdown must show UNCLASSIFIED == reclassified_count, \
         distinguishing this from an all-confident apply of the same size: {v}"
    );
}

/// US2: `scope=off_ontology` picks up untyped, fact-prefix-pseudo-typed, and prior-UNCLASSIFIED
/// edges while leaving correctly-typed edges untouched (FR-005).
#[tokio::test]
async fn test_reprocess_relation_scope_off_ontology_mixed_candidates() {
    let (db, _dir) = make_db(4);
    insert_test_entity(
        &db,
        "us2-src",
        "Dave",
        "liminis",
        vec!["Entity".to_string()],
    );
    insert_test_entity(
        &db,
        "us2-dst",
        "Acme",
        "liminis",
        vec!["Entity".to_string()],
    );

    let mut verdicts = HashMap::new();

    // (a) untyped edge → classifies to AFFILIATED_WITH.
    insert_test_edge(
        &db,
        "us2-untyped",
        "us2-src",
        "us2-dst",
        "liminis",
        "Dave is affiliated with Acme",
        None,
    );
    verdicts.insert(
        "Dave is affiliated with Acme".to_string(),
        "AFFILIATED_WITH".to_string(),
    );

    // (b) fact-prefix pseudo-typed edge (from backfill) → reclassified to AUTHORED.
    insert_test_edge(
        &db,
        "us2-pseudo",
        "us2-src",
        "us2-dst",
        "liminis",
        "Dave authored the spec",
        Some("DAVE_AUTHORED_THE"),
    );
    verdicts.insert("Dave authored the spec".to_string(), "AUTHORED".to_string());

    // (c) prior UNCLASSIFIED sentinel → still abstains (no verdict entry).
    insert_test_edge(
        &db,
        "us2-prior-unclassified",
        "us2-src",
        "us2-dst",
        "liminis",
        "Dave has a vague fact",
        Some("UNCLASSIFIED"),
    );

    // (d) already correctly-typed edge → must be untouched.
    insert_test_edge(
        &db,
        "us2-correct",
        "us2-src",
        "us2-dst",
        "liminis",
        "Dave authored another document",
        Some("AUTHORED"),
    );
    verdicts.insert(
        "Dave authored another document".to_string(),
        "AUTHORED".to_string(),
    );

    let ontology = make_relation_ontology();
    let extractor = Arc::new(RelationClassifyingExtractor::new(verdicts));
    let workspace = TempDir::new().unwrap();
    let state = make_state_with_ontology_and_extractor(
        db.clone(),
        ontology,
        extractor,
        workspace.path().to_path_buf(),
    );

    let v = dispatch_val(
        109,
        "knowledge_reprocess_relation_types",
        json!({"scope": "off_ontology"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 109);
    // Candidates are (a) untyped, (b) pseudo-typed, (c) prior-UNCLASSIFIED — (d) is already a
    // declared type, so it is on-ontology and never becomes a candidate at all.
    // (a) and (b) get a new type (reclassified); (c) abstains again → UNCLASSIFIED == UNCLASSIFIED,
    // a no-op per FR-009 (idempotent abstention), so it counts as unchanged, not reclassified.
    assert_eq!(
        v["result"]["reclassified_count"], 2,
        "(a) and (b) reclassify to a declared type: {v}"
    );
    assert_eq!(
        v["result"]["unchanged_count"], 1,
        "(c) re-abstaining to UNCLASSIFIED is a no-op (FR-009): {v}"
    );
    assert_eq!(
        v["result"]["breakdown"],
        json!({"AFFILIATED_WITH": 1, "AUTHORED": 1}),
        "breakdown counts only newly-written types (a) and (b); (c)'s idempotent no-op is excluded: {v}"
    );

    let conn = db.connect().unwrap();
    assert_eq!(
        conn.get_edge_by_uuid("us2-untyped")
            .unwrap()
            .unwrap()
            .relation_type
            .as_deref(),
        Some("AFFILIATED_WITH")
    );
    assert_eq!(
        conn.get_edge_by_uuid("us2-pseudo")
            .unwrap()
            .unwrap()
            .relation_type
            .as_deref(),
        Some("AUTHORED")
    );
    assert_eq!(
        conn.get_edge_by_uuid("us2-prior-unclassified")
            .unwrap()
            .unwrap()
            .relation_type
            .as_deref(),
        Some("UNCLASSIFIED"),
        "still-abstained edge must remain UNCLASSIFIED"
    );
    assert_eq!(
        conn.get_edge_by_uuid("us2-correct")
            .unwrap()
            .unwrap()
            .relation_type
            .as_deref(),
        Some("AUTHORED"),
        "already-correct edge must be untouched"
    );
}

// ── knowledge_rebuild_from_wal: force_clear wire contract (issue #239, FR-005/FR-006) ─────────

/// A `from_seq: 0` (default) rebuild against a non-empty database must fail fast over the wire
/// with a structural JSON-RPC error (-32000) rather than silently succeeding with a flood of
/// duplicate-primary-key `failed_samples` — this is the wire-contract shape CodeRabbit flagged
/// as untested in this file (behavioral coverage already exists in handlers_wal_admin.rs).
#[tokio::test]
async fn parity_rebuild_from_wal_non_empty_db_fails_fast_by_default() {
    let (db, dir) = make_db(4);
    insert_test_entity(
        &db,
        "parity-rebuild-001",
        "Pre-existing",
        "g",
        vec!["Entity".to_string()],
    );

    let wal_dir = TempDir::new().unwrap();
    std::fs::write(
        wal_dir.path().join("20260726_000000_parity.jsonl"),
        entity_wal_line(0, "parity-rebuild-001") + "\n",
    )
    .unwrap();
    let db_path = dir.path().join("parity.db").to_str().unwrap().to_string();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), db_path);

    let v = dispatch_val(100, "knowledge_rebuild_from_wal", json!({}), state).await;

    assert_err_resp(&v, 100, -32000);
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("force_clear") || msg.contains("knowledge_clear_all"),
        "error must name the corrective action: {v}"
    );
}

/// `force_clear: true` on the same non-empty-database scenario clears the database and lets the
/// rebuild proceed to completion, returning a normal JSON-RPC success result over the wire.
#[tokio::test]
async fn parity_rebuild_from_wal_force_clear_succeeds() {
    let (db, dir) = make_db(4);
    insert_test_entity(
        &db,
        "parity-rebuild-002",
        "Pre-existing",
        "g",
        vec!["Entity".to_string()],
    );

    let wal_dir = TempDir::new().unwrap();
    std::fs::write(
        wal_dir.path().join("20260726_000000_parity.jsonl"),
        entity_wal_line(0, "parity-rebuild-002") + "\n",
    )
    .unwrap();
    let db_path = dir.path().join("parity.db").to_str().unwrap().to_string();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), db_path);

    // Use the streaming path (progress_tx: Some) so the rebuild completes synchronously within
    // this call, instead of needing to poll knowledge_rebuild_status for a background job.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let resp = handlers::dispatch(
        req(
            101,
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
        ),
        Arc::clone(&state),
        Some(tx),
    )
    .await;
    while rx.try_recv().is_ok() {}
    let v = serde_json::to_value(resp).unwrap();

    assert_ok_resp(&v, 101);
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(
        v["result"]["failed_samples"],
        json!([]),
        "force_clear must have removed the stale entity, leaving zero duplicate-key failures: {v}"
    );

    // SC-002: after knowledge_rebuild_from_wal {force_clear: true} over a WAL whose max seq is
    // N (here, the single line's seq 0), wal.applied_seq == N.
    let status = dispatch_val(102, "knowledge_status", json!({}), state).await;
    assert_ok_resp(&status, 102);
    assert_eq!(
        status["result"]["wal"]["applied_seq"], 0,
        "applied_seq must equal the replay's last_committed_seq (the WAL's only line, seq 0): {status}"
    );
}

// ── knowledge_wal_mark_create / _list / _delete (issue #365: WAL checkpoints) ─────────────

/// Directly inserts a minimal, valid `Entity` row via the normal `insert_entity` Rust API
/// (a raw `MERGE ... SET` on `name_embedding` conflicts with the HNSW vector index `make_db`
/// already built). Used to give a DB real content so FR-005's `applied_seq == 0` emptiness
/// check has something non-empty to detect (`graph_has_no_content` must return `false`).
fn seed_entity(conn: &Conn<'_>, uuid: &str) {
    conn.insert_entity(&EntityRow {
        uuid: uuid.to_string(),
        name: uuid.to_string(),
        group_id: "g".to_string(),
        labels: vec!["Entity".to_string()],
        created_at: "2026-05-22 00:00:00".to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: "s".to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
    })
    .unwrap();
}

/// Like `make_degraded_state`, but with `wal_dir` configured — needed for the FR-011 exemption
/// tests, since `knowledge_wal_mark_list`/`_delete` operate solely on `.checkpoints/` under the
/// WAL directory and must work even with no open DB.
fn make_degraded_state_with_wal(reason: &str, wal_dir: PathBuf) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some(reason.to_string()))),
        embedder: Arc::new(OaiEmbedder::from_env()),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: "test-degraded.db".to_string(),
        wal_dir: Some(wal_dir),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
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

#[tokio::test]
async fn wal_mark_create_succeeds_against_nonzero_applied_seq() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(42).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    // Give the WAL directory content covering seq 42, so `reachable` reports true below.
    std::fs::write(
        wal_dir.path().join("0000.jsonl"),
        entity_wal_line(42, "e42") + "\n",
    )
    .unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "pre-migration"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 1);
    assert_eq!(v["result"]["seq"], 42, "{v}");

    let list_v = dispatch_val(2, "knowledge_wal_mark_list", json!({}), state).await;
    assert_ok_resp(&list_v, 2);
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0]["name"], "pre-migration");
    assert_eq!(checkpoints[0]["seq"], 42);
    assert_eq!(checkpoints[0]["reachable"], true);
}

#[tokio::test]
async fn wal_mark_create_fails_when_applied_seq_is_null() {
    let (db, _dir) = make_db(4); // applied_seq row never written -> get_applied_seq() == None
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v = dispatch_val(1, "knowledge_wal_mark_create", json!({"name": "x"}), state).await;
    assert_err_resp(&v, 1, -32000);

    // No checkpoint record — not even a placeholder — must be written (FR-004).
    assert!(!wal_dir.path().join(".checkpoints").exists());
}

#[tokio::test]
async fn wal_mark_create_rejects_duplicate_active_name() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(1).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v1 = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "dup"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v1, 1);

    let v2 = dispatch_val(
        2,
        "knowledge_wal_mark_create",
        json!({"name": "dup"}),
        Arc::clone(&state),
    )
    .await;
    assert_err_resp(&v2, 2, -32000);

    let list_v = dispatch_val(3, "knowledge_wal_mark_list", json!({}), state).await;
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(
        checkpoints[0]["seq"], 1,
        "the original record must be unmodified by the rejected duplicate: {list_v}"
    );
}

#[tokio::test]
async fn wal_mark_create_applied_seq_zero_empty_graph_records_seq_none() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(0).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "fresh"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v, 1);
    assert!(
        v["result"]["seq"].is_null(),
        "a genuinely fresh, empty graph must record seq:null, not seq:0: {v}"
    );

    let list_v = dispatch_val(2, "knowledge_wal_mark_list", json!({}), state).await;
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    assert!(checkpoints[0]["seq"].is_null());
    assert_eq!(
        checkpoints[0]["reachable"], true,
        "a seq:null checkpoint is always reachable (restore is knowledge_clear_all): {list_v}"
    );
}

#[tokio::test]
async fn wal_mark_create_applied_seq_zero_nonempty_graph_records_seq_some_zero() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        seed_entity(&conn, "first-chunk-entity");
        conn.set_applied_seq(0).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "first-chunk"}),
        state,
    )
    .await;
    assert_ok_resp(&v, 1);
    assert_eq!(
        v["result"]["seq"], 0,
        "applied_seq==0 with real graph content must record seq:0, distinct from seq:null: {v}"
    );
}

#[tokio::test]
async fn wal_mark_list_on_empty_store_returns_empty_not_error() {
    let (db, _dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v = dispatch_val(1, "knowledge_wal_mark_list", json!({}), state).await;
    assert_ok_resp(&v, 1);
    assert_eq!(v["result"]["checkpoints"], json!([]));
}

#[tokio::test]
async fn wal_mark_list_reachability_after_deleting_covering_wal_files() {
    let (db, _dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    // Two WAL files: one covers seq 0, the other covers seq 10.
    std::fs::write(
        wal_dir.path().join("a_0000.jsonl"),
        entity_wal_line(0, "e0") + "\n",
    )
    .unwrap();
    std::fs::write(
        wal_dir.path().join("b_0000.jsonl"),
        entity_wal_line(10, "e10") + "\n",
    )
    .unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    {
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        seed_entity(&conn, "low-marker");
        conn.set_applied_seq(0).unwrap();
    }
    let v_low = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "low"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v_low, 1);

    {
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        conn.set_applied_seq(10).unwrap();
    }
    let v_high = dispatch_val(
        2,
        "knowledge_wal_mark_create",
        json!({"name": "high"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&v_high, 2);

    let list_before =
        dispatch_val(3, "knowledge_wal_mark_list", json!({}), Arc::clone(&state)).await;
    let cps_before = list_before["result"]["checkpoints"].as_array().unwrap();
    assert!(
        cps_before.iter().all(|c| c["reachable"] == true),
        "both checkpoints must be reachable while their WAL content is on disk: {list_before}"
    );

    // Externally remove the file covering seq 0 — "low" is no longer reachable via the cheap
    // min/max bound (its seq now falls below the WAL's lowest surviving seq).
    std::fs::remove_file(wal_dir.path().join("a_0000.jsonl")).unwrap();

    let list_after = dispatch_val(4, "knowledge_wal_mark_list", json!({}), state).await;
    let cps_after = list_after["result"]["checkpoints"].as_array().unwrap();
    let low = cps_after.iter().find(|c| c["name"] == "low").unwrap();
    let high = cps_after.iter().find(|c| c["name"] == "high").unwrap();
    assert_eq!(low["reachable"], false, "{list_after}");
    assert_eq!(high["reachable"], true, "{list_after}");
}

#[tokio::test]
async fn wal_mark_list_excludes_deleted_checkpoints() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(3).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let created = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "gone"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 1);
    let deleted = dispatch_val(
        2,
        "knowledge_wal_mark_delete",
        json!({"name": "gone"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&deleted, 2);

    let list_v = dispatch_val(3, "knowledge_wal_mark_list", json!({}), state).await;
    assert_eq!(list_v["result"]["checkpoints"], json!([]), "{list_v}");
}

#[tokio::test]
async fn wal_mark_delete_of_nonexistent_name_fails() {
    let (db, _dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let v = dispatch_val(
        1,
        "knowledge_wal_mark_delete",
        json!({"name": "never-created"}),
        state,
    )
    .await;
    assert_err_resp(&v, 1, -32000);
}

#[tokio::test]
async fn wal_mark_delete_of_already_deleted_name_fails() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(1).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());
    dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "x"}),
        Arc::clone(&state),
    )
    .await;
    let first_delete = dispatch_val(
        2,
        "knowledge_wal_mark_delete",
        json!({"name": "x"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&first_delete, 2);

    let v = dispatch_val(3, "knowledge_wal_mark_delete", json!({"name": "x"}), state).await;
    assert_err_resp(&v, 3, -32000);
}

#[tokio::test]
async fn wal_mark_delete_never_rewrites_create_record_and_name_is_reusable() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(1).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let created = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "reuse"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&created, 1);

    let name_dir = wal_dir.path().join(".checkpoints").join("reuse");
    let create_record_before = std::fs::read_to_string(name_dir.join("g1.create.json")).unwrap();

    let deleted = dispatch_val(
        2,
        "knowledge_wal_mark_delete",
        json!({"name": "reuse"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&deleted, 2);

    let create_record_after = std::fs::read_to_string(name_dir.join("g1.create.json")).unwrap();
    assert_eq!(
        create_record_before, create_record_after,
        "delete must never rewrite the create record in place (FR-007)"
    );
    assert!(
        name_dir.join("g1.delete.json").exists(),
        "delete must add a separate tombstone marker"
    );

    // Bump applied_seq and re-create under the same, now-inactive name.
    {
        let db = state.db.load_full().unwrap();
        let conn = db.connect().unwrap();
        conn.set_applied_seq(77).unwrap();
    }
    let recreated = dispatch_val(
        3,
        "knowledge_wal_mark_create",
        json!({"name": "reuse"}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&recreated, 3);
    assert_eq!(recreated["result"]["seq"], 77, "{recreated}");
    assert!(
        name_dir.join("g2.create.json").exists(),
        "reuse after delete must land in a new generation, not overwrite g1"
    );

    let list_v = dispatch_val(4, "knowledge_wal_mark_list", json!({}), state).await;
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0]["seq"], 77);
}

#[tokio::test]
async fn wal_mark_list_and_delete_work_when_db_degraded_but_create_does_not() {
    let wal_dir = TempDir::new().unwrap();

    // Seed a checkpoint via a healthy state first (create needs an open DB); then simulate the
    // DB becoming unavailable and confirm list/delete still see the store on disk (FR-011).
    let (db, _dbdir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(1).unwrap();
    }
    let healthy_state =
        make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());
    let created = dispatch_val(
        1,
        "knowledge_wal_mark_create",
        json!({"name": "survives-degradation"}),
        healthy_state,
    )
    .await;
    assert_ok_resp(&created, 1);

    let degraded = make_degraded_state_with_wal("simulated failure", wal_dir.path().to_path_buf());

    let list_v = dispatch_val(
        2,
        "knowledge_wal_mark_list",
        json!({}),
        Arc::clone(&degraded),
    )
    .await;
    assert_ok_resp(&list_v, 2);
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0]["name"], "survives-degradation");

    let delete_v = dispatch_val(
        3,
        "knowledge_wal_mark_delete",
        json!({"name": "survives-degradation"}),
        Arc::clone(&degraded),
    )
    .await;
    assert_ok_resp(&delete_v, 3);

    // create, by contrast, correctly depends on an open DB (applied_seq) and must still fail.
    let create_v = dispatch_val(
        4,
        "knowledge_wal_mark_create",
        json!({"name": "x"}),
        degraded,
    )
    .await;
    assert_err_resp(&create_v, 4, -32001);
}

#[tokio::test]
async fn wal_mark_create_concurrent_same_name_exactly_one_wins() {
    let (db, _dir) = make_db(4);
    {
        let conn = db.connect().unwrap();
        conn.set_applied_seq(1).unwrap();
    }
    let wal_dir = TempDir::new().unwrap();
    let state = make_state_with_wal(db, wal_dir.path().to_path_buf(), "test.db".to_string());

    let handles: Vec<_> = (0..8i64)
        .map(|i| {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                dispatch_val(
                    i,
                    "knowledge_wal_mark_create",
                    json!({"name": "race"}),
                    state,
                )
                .await
            })
        })
        .collect();

    let mut successes = 0;
    for h in handles {
        let v = h.await.unwrap();
        if v.get("error").is_none() {
            successes += 1;
        }
    }
    assert_eq!(
        successes, 1,
        "exactly one concurrent create must succeed (FR-011/FR-012)"
    );

    let list_v = dispatch_val(100, "knowledge_wal_mark_list", json!({}), state).await;
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 1);
}
