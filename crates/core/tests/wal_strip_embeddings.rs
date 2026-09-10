// Integration tests for `knowledge_strip_wal_embeddings` (issue #577), dispatched through the
// real IPC handler (`handlers::dispatch`) so they exercise param parsing, the write_lock,
// degraded-mode reachability, and the FR-006 response shape end-to-end. Coverage of the
// underlying two-pass streaming algorithm itself (byte-preserving pass-through, malformed-value
// validation, atomic tmp+rename) lives in `crates/core/src/wal_strip.rs`'s own `#[cfg(test)]`
// module — these tests deliberately don't re-derive that here.

use std::collections::HashMap;
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
    replay::WalReplayer,
    telemetry::NoopSink,
    WalLine,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const VECTOR_PARAM_KEYS: &[&str] = &[
    "name_embedding",
    "fact_embedding",
    "content_embedding",
    "summary_embedding",
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wal_strip")
}

/// Deliberately DB-less (`db: ArcSwapOption::from(None)`, a non-empty `degraded_reason`): this
/// operation touches only `state.wal_root`/`state.write_lock`, never the DB (per this issue's
/// FR-001 and `handlers.rs`'s `exempt_in_degraded` list), so every test in this file that
/// succeeds against this state is simultaneously proof the operation is reachable while the
/// service is degraded — see `reachable_while_db_is_degraded` below for the test that asserts
/// this explicitly.
fn make_state(wal_root: Option<PathBuf>) -> Arc<AppState> {
    Arc::new(AppState {
        db: ArcSwapOption::from(None),
        degraded_reason: Arc::new(Mutex::new(Some("test: no DB opened".to_string()))),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink: Arc::new(NoopSink),
        db_path: "test.db".to_string(),
        wal_root,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "mock-embedder-test".to_string(),
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
        embedding_cache: Arc::new(lcg_core::EmbeddingCache::new()),
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

fn group_dir(wal_root: &Path, group_id: &str) -> PathBuf {
    wal_root.join(group_id)
}

fn copy_fixture_into(name: &str, dest_dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dest_dir).unwrap();
    let dest = dest_dir.join(name);
    std::fs::copy(fixtures_dir().join(name), &dest).unwrap();
    dest
}

// ── User Story 1 / FR-002 / FR-006 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn strips_embeddings_preserves_other_fields_and_reports_stats() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    let path = copy_fixture_into("mixed_embedding_params.jsonl", &group);
    let before_bytes = std::fs::metadata(&path).unwrap().len();

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;

    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["dry_run"], false, "{v}");
    assert_eq!(v["result"]["files_processed"], 1, "{v}");
    assert_eq!(v["result"]["files_rewritten"], 1, "{v}");
    assert_eq!(v["result"]["files_unchanged"], 0, "{v}");
    // 3 of the fixture's 4 lines carry exactly one embedding-vector key each; the 4th (a plain
    // attribute update) has none.
    assert_eq!(v["result"]["records_rewritten"], 3, "{v}");
    assert_eq!(v["result"]["bytes_before"], before_bytes, "{v}");
    assert!(
        v["result"]["bytes_after"].as_u64().unwrap() < before_bytes,
        "{v}"
    );
    assert!(v["result"]["errors"].as_array().unwrap().is_empty(), "{v}");

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 4, "record count/ordering must be preserved");
    for (i, line) in lines.iter().enumerate() {
        let wal_line: WalLine = serde_json::from_str(line).unwrap();
        assert_eq!(
            wal_line.seq, i as u64,
            "sequence numbers must be preserved exactly"
        );
        let params = wal_line.params.as_object().unwrap();
        for key in VECTOR_PARAM_KEYS {
            assert!(
                !params.contains_key(*key),
                "line {i} still carries {key:?}: {line}"
            );
        }
    }
    // Every non-embedding field survives untouched.
    let first: WalLine = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first.db, "");
    assert_eq!(first.params["uuid"], "entity-0");
    assert_eq!(first.params["summary"], "S0");
    assert_eq!(first.params["attributes"], "{}");
}

// ── User Story 2 / FR-004 / SC-003: idempotency ─────────────────────────────────────────────

#[tokio::test]
async fn rerun_after_strip_is_a_byte_identical_noop() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    let path = copy_fixture_into("mixed_embedding_params.jsonl", &group);

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let first = dispatch_val(
        1,
        "knowledge_strip_wal_embeddings",
        json!({}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(first["result"]["files_rewritten"], 1, "{first}");

    let bytes_after_first = std::fs::read(&path).unwrap();
    let mtime_after_first = std::fs::metadata(&path).unwrap().modified().unwrap();

    let second = dispatch_val(2, "knowledge_strip_wal_embeddings", json!({}), state).await;

    assert_eq!(second["result"]["files_rewritten"], 0, "{second}");
    assert_eq!(second["result"]["files_unchanged"], 1, "{second}");
    assert_eq!(second["result"]["records_rewritten"], 0, "{second}");
    assert!(
        second["result"]["errors"].as_array().unwrap().is_empty(),
        "{second}"
    );

    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes_after_first,
        "re-run must leave the file byte-identical"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        mtime_after_first,
        "re-run must not touch the file's mtime"
    );
}

// ── User Story 2, Acceptance Scenario 2: mixed group (pre-0.14 alongside already-clean) ────

#[tokio::test]
async fn mixed_group_only_rewrites_the_file_that_needs_it() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    copy_fixture_into("mixed_embedding_params.jsonl", &group);
    let clean_path = copy_fixture_into("already_clean.jsonl", &group);
    let clean_before = std::fs::read(&clean_path).unwrap();

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;

    assert_eq!(v["result"]["files_processed"], 2, "{v}");
    assert_eq!(v["result"]["files_rewritten"], 1, "{v}");
    assert_eq!(v["result"]["files_unchanged"], 1, "{v}");
    assert_eq!(
        std::fs::read(&clean_path).unwrap(),
        clean_before,
        "an already-clean sibling file must be left untouched"
    );
}

// ── Edge Cases / FR-009: malformed embedding value ──────────────────────────────────────────

#[tokio::test]
async fn malformed_embedding_value_is_a_per_file_error_and_other_files_still_process() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    let bad_path = copy_fixture_into("malformed_embedding_value.jsonl", &group);
    let bad_before = std::fs::read(&bad_path).unwrap();
    copy_fixture_into("mixed_embedding_params.jsonl", &group);

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;

    // The overall IPC call still succeeds — a per-file error must not fail the whole run.
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["files_processed"], 2, "{v}");
    assert_eq!(
        v["result"]["files_rewritten"], 1,
        "the good file must still be rewritten: {v}"
    );

    let errors = v["result"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{v}");
    assert!(
        errors[0]["path"]
            .as_str()
            .unwrap()
            .contains("malformed_embedding_value.jsonl"),
        "{v}"
    );
    assert!(
        errors[0]["error"]
            .as_str()
            .unwrap()
            .contains("name_embedding"),
        "{v}"
    );
    assert_eq!(
        std::fs::read(&bad_path).unwrap(),
        bad_before,
        "a file with a malformed value must be left completely untouched"
    );
}

// ── Edge Cases: an interspersed unparseable line never aborts the file ──────────────────────

#[tokio::test]
async fn unparseable_line_passes_through_and_is_counted_separately_from_errors() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    let path = copy_fixture_into("interspersed_unparseable_line.jsonl", &group);

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;

    assert_eq!(v["result"]["files_rewritten"], 1, "{v}");
    assert_eq!(v["result"]["records_rewritten"], 2, "{v}");
    assert_eq!(v["result"]["unparseable_lines"], 1, "{v}");
    assert!(v["result"]["errors"].as_array().unwrap().is_empty(), "{v}");

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[1], "this line is truncated garbage, not JSON at all {\"seq\":",
        "the unparseable line must pass through byte-for-byte unchanged"
    );
}

// ── FR-010: dry_run ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dry_run_reports_would_be_stats_without_mutating_anything() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    let path = copy_fixture_into("mixed_embedding_params.jsonl", &group);
    let before_bytes = std::fs::read(&path).unwrap();
    let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(
        1,
        "knowledge_strip_wal_embeddings",
        json!({"dry_run": true}),
        state,
    )
    .await;

    assert_eq!(v["result"]["dry_run"], true, "{v}");
    assert_eq!(v["result"]["files_rewritten"], 1, "{v}");
    assert_eq!(v["result"]["records_rewritten"], 3, "{v}");
    assert!(
        v["result"]["bytes_after"].as_u64().unwrap()
            < v["result"]["bytes_before"].as_u64().unwrap(),
        "{v}"
    );

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before_bytes,
        "dry_run must not mutate the file"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before_mtime,
        "dry_run must not touch the file's mtime"
    );
}

// ── FR-007: group_id scoping across the multi-stream layout (ADR-0378) ─────────────────────

#[tokio::test]
async fn group_id_param_scopes_to_a_single_groups_wal_directory() {
    let wal_dir = TempDir::new().unwrap();
    let group_a = group_dir(wal_dir.path(), "group-a");
    let group_b = group_dir(wal_dir.path(), "group-b");
    copy_fixture_into("mixed_embedding_params.jsonl", &group_a);
    let b_path = copy_fixture_into("mixed_embedding_params.jsonl", &group_b);
    let b_before = std::fs::read(&b_path).unwrap();

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(
        1,
        "knowledge_strip_wal_embeddings",
        json!({"group_id": "group-a"}),
        state,
    )
    .await;

    assert_eq!(v["result"]["files_processed"], 1, "{v}");
    assert_eq!(v["result"]["files_rewritten"], 1, "{v}");
    assert_eq!(
        std::fs::read(&b_path).unwrap(),
        b_before,
        "a group_id filter must leave every other group's WAL directory untouched"
    );
}

#[tokio::test]
async fn missing_group_id_directory_is_an_empty_report_not_an_error() {
    let wal_dir = TempDir::new().unwrap();
    let state = make_state(Some(wal_dir.path().to_path_buf()));
    let v = dispatch_val(
        1,
        "knowledge_strip_wal_embeddings",
        json!({"group_id": "nonexistent-group"}),
        state,
    )
    .await;

    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["files_processed"], 0, "{v}");
}

#[tokio::test]
async fn no_wal_root_configured_is_an_error() {
    let state = make_state(None);
    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;
    assert!(
        v.get("error").is_some(),
        "expected an error when no WAL root is configured: {v}"
    );
}

// ── Degraded-mode reachability (issue #577, mirrors knowledge_wal_mark_list/_delete) ───────

/// FR-001: this operation touches only `state.wal_root`/`state.write_lock`, never the DB, so it
/// must remain reachable when the service is degraded (DB unavailable) — same exemption
/// `knowledge_wal_mark_list`/`knowledge_wal_mark_delete` already have in `handlers.rs`'s
/// `exempt_in_degraded` list. Every other test in this file already runs against a DB-less
/// `AppState` (see `make_state`'s doc comment); this test asserts that explicitly and confirms
/// the call does not surface `Error::DbUnavailable` (JSON-RPC code -32001).
#[tokio::test]
async fn reachable_while_db_is_degraded() {
    let wal_dir = TempDir::new().unwrap();
    let group = group_dir(wal_dir.path(), "liminis");
    copy_fixture_into("mixed_embedding_params.jsonl", &group);

    let state = make_state(Some(wal_dir.path().to_path_buf()));
    assert!(
        state.db.load_full().is_none(),
        "state must actually be degraded for this test to be meaningful"
    );

    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;

    assert!(
        v.get("error").is_none(),
        "must not fail with DbUnavailable while degraded: {v}"
    );
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["files_rewritten"], 1, "{v}");
}

// ── SC-001 / SC-002: real-corpus byte reduction and replay equivalence ──────────────────────
//
// #[ignore]: replays the full #217 capture fixture twice (once original, once stripped) — slow
// enough to exclude from the default `cargo test` run, matching every other real-corpus-fixture
// test in this crate. Run explicitly:
//   cargo test -p lcg-core --test wal_strip_embeddings -- --ignored --nocapture

fn real_corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_corpus_wal")
}

fn real_corpus_wal_dir() -> PathBuf {
    real_corpus_dir().join("wal")
}

fn real_corpus_embedding_dim() -> usize {
    let path = real_corpus_dir().join("expected_results.json");
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

fn copy_dir_jsonl_files(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    for path in jsonl_files(src) {
        let name = path.file_name().unwrap();
        std::fs::copy(&path, dest.join(name)).unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn strip_reduces_real_corpus_bytes_by_close_to_89_9_percent_and_replays_identically() {
    let dim = real_corpus_embedding_dim();
    let original_dir = real_corpus_wal_dir();
    let original_bytes = total_bytes(&original_dir);
    assert!(
        original_bytes > 0,
        "the #217 capture fixture must be non-empty"
    );

    // Two independent copies: one left untouched (replayed as "original"), one stripped in
    // place by the operation under test (replayed as "stripped"). Never mutates the
    // checked-in fixture itself.
    let untouched_dir = TempDir::new().unwrap();
    copy_dir_jsonl_files(&original_dir, untouched_dir.path());

    let wal_root = TempDir::new().unwrap();
    let stripped_group_dir = group_dir(wal_root.path(), "liminis");
    copy_dir_jsonl_files(&original_dir, &stripped_group_dir);

    let state = make_state(Some(wal_root.path().to_path_buf()));
    let v = dispatch_val(1, "knowledge_strip_wal_embeddings", json!({}), state).await;
    assert_eq!(v["result"]["success"], true, "{v}");
    assert!(v["result"]["errors"].as_array().unwrap().is_empty(), "{v}");

    let stripped_bytes = total_bytes(&stripped_group_dir);
    let reduction = 1.0 - (stripped_bytes as f64 / original_bytes as f64);
    println!(
        "[SC-001] WAL size: original = {original_bytes} bytes, stripped = {stripped_bytes} \
         bytes, reduction = {:.1}% (documented figure: 89.9%)",
        reduction * 100.0,
    );
    assert!(
        reduction > 0.85,
        "expected a reduction close to the documented 89.9%, got only {:.1}% \
         (original={original_bytes}, stripped={stripped_bytes})",
        reduction * 100.0
    );

    // SC-002: replaying the stripped WAL must produce an identical DB to replaying the
    // original — 0.14.0's "a vector found in an older WAL is ignored" behavior (#526/#440) is
    // exactly what makes this safe.
    let embed_fn = || lcg_core::zero_vector_embed_fn(dim);

    let original_db_dir = TempDir::new().unwrap();
    let original_db =
        Db::open(original_db_dir.path().join("original.db").to_str().unwrap()).unwrap();
    let original_conn = original_db.connect().unwrap();
    original_conn.init_schema(dim).unwrap();
    let original_stats = WalReplayer::new(untouched_dir.path())
        .replay(&original_conn, embed_fn(), dim)
        .expect("replay of the original capture must succeed");

    let stripped_db_dir = TempDir::new().unwrap();
    let stripped_db =
        Db::open(stripped_db_dir.path().join("stripped.db").to_str().unwrap()).unwrap();
    let stripped_conn = stripped_db.connect().unwrap();
    stripped_conn.init_schema(dim).unwrap();
    let stripped_stats = WalReplayer::new(&stripped_group_dir)
        .replay(&stripped_conn, embed_fn(), dim)
        .expect("replay of the stripped capture must succeed");

    assert_eq!(
        original_stats.failed_lines, 0,
        "original replay must be clean"
    );
    assert_eq!(
        stripped_stats.failed_lines, 0,
        "stripped replay must be clean"
    );
    assert_eq!(
        original_stats.lines_replayed, stripped_stats.lines_replayed,
        "stripping must not change how many mutations replay applies"
    );

    for label in ["Entity", "Episodic", "RelatesToNode_"] {
        let original_count = original_conn.count_nodes(label).unwrap();
        let stripped_count = stripped_conn.count_nodes(label).unwrap();
        assert_eq!(
            original_count, stripped_count,
            "{label} count must match between original and stripped replay"
        );
    }
}
