// Integration tests for WAL stream generation identity (issue #387): detecting a publisher
// reset instead of replaying incrementally across it (Stories 1-6, SC-001-008).
//
// A "reset" is simulated by deleting a group's existing `.jsonl` files and writing a fresh set
// restarting at `seq: 0`, then either leaving the `.wal-generation.json` sidecar untouched (no
// generation change — not a reset) or overwriting it with a different value (a genuine reset).
// This mirrors what an external publisher (orac/zen) does when it re-extracts a corpus and
// republishes from scratch, per the issue's Background.

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
    wal_generation, wal_group, EntityRow, WalWriter,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db(dim: usize) -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("gen_reset_test.db").to_str().unwrap()).unwrap());
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

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

/// Non-streaming dispatch — `knowledge_rebuild_from_wal` returns a `job_id` and replays in the
/// background; other methods return their result directly.
async fn dispatch_val(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let resp = handlers::dispatch(req(id, method, params), state, None).await;
    serde_json::to_value(resp).unwrap()
}

/// Streaming dispatch (a progress sender present takes the synchronous code path) — see
/// `cross_group_incremental_replay.rs`'s identical helper.
async fn dispatch_rebuild_sync(id: i64, params: Value, state: Arc<AppState>) -> Value {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let resp = handlers::dispatch(
        req(id, "knowledge_rebuild_from_wal", params),
        state,
        Some(tx),
    )
    .await;
    serde_json::to_value(resp).unwrap()
}

fn assert_ok_resp(v: &Value, id: i64) {
    assert_eq!(v["jsonrpc"], "2.0", "jsonrpc field wrong: {v}");
    assert_eq!(v["id"], id, "id mismatch: {v}");
    assert!(v.get("error").is_none(), "unexpected error: {v}");
}

/// A standalone `Entity` WAL line, named after its own uuid unless `name` is given.
fn entity_wal_line(seq: u64, uuid: &str, name: &str, group_id: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{name}', n.group_id = '{group_id}', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

fn group_dir(wal_root: &std::path::Path, group_id: &str) -> std::path::PathBuf {
    wal_group::group_wal_dir(wal_root, group_id).unwrap()
}

/// Writes `lines` into a fresh `.jsonl` file in `group_id`'s directory. Never deletes or touches
/// the generation sidecar — used both for a group's very first content and for ordinary
/// continued-ingest appends (Story 1 AC4 / SC-003).
fn write_group_wal(wal_root: &std::path::Path, group_id: &str, file_name: &str, lines: &[String]) {
    let dir = group_dir(wal_root, group_id);
    std::fs::create_dir_all(&dir).unwrap();
    let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(dir.join(file_name), content).unwrap();
}

fn set_group_generation(wal_root: &std::path::Path, group_id: &str, generation: &str) {
    let dir = group_dir(wal_root, group_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".wal-generation.json"),
        format!(r#"{{"generation":"{generation}"}}"#),
    )
    .unwrap();
}

fn read_group_generation(wal_root: &std::path::Path, group_id: &str) -> Option<String> {
    wal_generation::read_generation(&group_dir(wal_root, group_id))
}

/// Simulates an external publisher resetting `group_id`'s stream (Background section): every
/// existing `.jsonl` file is removed and replaced by a fresh set restarting at `seq: 0`, and the
/// generation sidecar is overwritten with `new_generation` — a genuinely different stream, not a
/// continuation of the old one.
fn reset_group_stream(
    wal_root: &std::path::Path,
    group_id: &str,
    file_name: &str,
    lines: &[String],
    new_generation: &str,
) {
    let dir = group_dir(wal_root, group_id);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    write_group_wal(wal_root, group_id, file_name, lines);
    set_group_generation(wal_root, group_id, new_generation);
}

// ── SC-001 / Story 1 AC1: longer reset stream, streaming path ─────────────────────────────────

#[tokio::test]
async fn sc001_longer_reset_stream_is_detected_and_purges_old_generation() {
    const G: &str = "g-longer";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "old-0", "old-0", G),
            entity_wal_line(1, "old-1", "old-1", G),
            entity_wal_line(2, "old-2", "old-2", G),
        ],
    );
    set_group_generation(&wal_root, G, "gen-1");

    let state = make_state_with_wal(db, wal_root.clone());

    // Bootstrap: ordinary first replay, no prior recorded position — must adopt, not reset.
    let boot_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_ok_resp(&boot_v, 1);
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");
    assert_ne!(
        boot_v["result"]["reset_detected"], true,
        "the very first replay of a stream must never itself be reported as a reset: {boot_v}"
    );

    {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        assert!(conn.get_entity_by_uuid("old-0").unwrap().is_some());
        let pos = conn.get_wal_position(G).unwrap();
        assert_eq!(pos.applied_seq, Some(2));
        assert_eq!(pos.generation.as_deref(), Some("gen-1"));
    }

    // Reset: a longer replacement stream under a new generation.
    reset_group_stream(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "new-0", "new-0", G),
            entity_wal_line(1, "new-1", "new-1", G),
            entity_wal_line(2, "new-2", "new-2", G),
            entity_wal_line(3, "new-3", "new-3", G),
            entity_wal_line(4, "new-4", "new-4", G),
        ],
        "gen-2",
    );

    // The caller innocently asks for an incremental catch-up (from_seq: 3) — detection must
    // override this with a full purge-and-replay regardless (FR-005).
    let reset_v =
        dispatch_rebuild_sync(2, json!({"group_id": G, "from_seq": 3}), Arc::clone(&state)).await;
    assert_ok_resp(&reset_v, 2);
    assert_eq!(reset_v["result"]["success"], true, "{reset_v}");
    assert_eq!(
        reset_v["result"]["reset_detected"], true,
        "a longer republished stream under a new generation must be detected: {reset_v}"
    );
    assert_eq!(
        reset_v["result"]["previous_generation"], "gen-1",
        "{reset_v}"
    );
    assert_eq!(reset_v["result"]["generation"], "gen-2", "{reset_v}");
    assert_eq!(
        reset_v["result"]["from_seq"], 0,
        "detection must override the caller's requested from_seq with a full replay: {reset_v}"
    );

    let db3 = state.db.load_full().unwrap();
    let conn = db3.connect().unwrap();
    for old in ["old-0", "old-1", "old-2"] {
        assert!(
            conn.get_entity_by_uuid(old).unwrap().is_none(),
            "old generation's entity {old:?} must be purged, not co-residing: {reset_v}"
        );
    }
    for new in ["new-0", "new-1", "new-2", "new-3", "new-4"] {
        assert!(
            conn.get_entity_by_uuid(new).unwrap().is_some(),
            "new generation's entity {new:?} must be present after the self-heal"
        );
    }
    let pos = conn.get_wal_position(G).unwrap();
    assert_eq!(pos.applied_seq, Some(4));
    assert_eq!(pos.generation.as_deref(), Some("gen-2"));
}

// ── SC-002: shorter reset stream, background-job path — same detection code path ──────────────

#[tokio::test]
async fn sc002_shorter_reset_stream_takes_same_detection_path_via_background_job() {
    const G: &str = "g-shorter";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &(0..5)
            .map(|i| entity_wal_line(i, &format!("old-{i}"), &format!("old-{i}"), G))
            .collect::<Vec<_>>(),
    );
    set_group_generation(&wal_root, G, "gen-1");

    let state = make_state_with_wal(db, wal_root.clone());
    let boot_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");

    // Reset: a SHORTER replacement stream (the case a bare applied_seq > max_seq comparison
    // would already catch) under a new generation — must take the identical detection path.
    reset_group_stream(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "new-0", "new-0", G),
            entity_wal_line(1, "new-1", "new-1", G),
        ],
        "gen-2",
    );

    let submit_v = dispatch_val(
        2,
        "knowledge_rebuild_from_wal",
        json!({"group_id": G, "from_seq": 5}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&submit_v, 2);
    let job_id = submit_v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let job_result = loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let status_v = dispatch_val(
            3,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        match status_v["result"]["status"].as_str().unwrap_or("?") {
            "completed" => break status_v["result"]["result"].clone(),
            "failed" => panic!("rebuild job failed: {status_v}"),
            _ => assert!(
                std::time::Instant::now() < deadline,
                "job did not complete in time: {status_v}"
            ),
        }
    };

    assert_eq!(
        job_result["reset_detected"], true,
        "a shorter republished stream must be detected via the same code path: {job_result}"
    );
    assert_eq!(job_result["previous_generation"], "gen-1", "{job_result}");
    assert_eq!(job_result["generation"], "gen-2", "{job_result}");

    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    for i in 0..5 {
        assert!(conn
            .get_entity_by_uuid(&format!("old-{i}"))
            .unwrap()
            .is_none());
    }
    assert!(conn.get_entity_by_uuid("new-0").unwrap().is_some());
    assert!(conn.get_entity_by_uuid("new-1").unwrap().is_some());
    let pos = conn.get_wal_position(G).unwrap();
    assert_eq!(pos.applied_seq, Some(1));
    assert_eq!(pos.generation.as_deref(), Some("gen-2"));
}

// ── SC-003: ordinary forward progress is unaffected ────────────────────────────────────────────

#[tokio::test]
async fn sc003_forward_progress_on_unchanged_generation_is_not_flagged_as_reset() {
    const G: &str = "g-forward";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "e0", "e0", G),
            entity_wal_line(1, "e1", "e1", G),
        ],
    );
    set_group_generation(&wal_root, G, "gen-1");

    let state = make_state_with_wal(db, wal_root.clone());
    let boot_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");

    // Ordinary continued ingest: append more lines to the SAME generation, no reset.
    write_group_wal(
        &wal_root,
        G,
        "0001.jsonl",
        &[
            entity_wal_line(2, "e2", "e2", G),
            entity_wal_line(3, "e3", "e3", G),
        ],
    );
    assert_eq!(
        read_group_generation(&wal_root, G).as_deref(),
        Some("gen-1"),
        "appending must never change the generation"
    );

    let cont_v =
        dispatch_rebuild_sync(2, json!({"group_id": G, "from_seq": 2}), Arc::clone(&state)).await;
    assert_ok_resp(&cont_v, 2);
    assert_eq!(cont_v["result"]["success"], true, "{cont_v}");
    assert_ne!(
        cont_v["result"]["reset_detected"], true,
        "ordinary forward progress on the same generation must never be flagged: {cont_v}"
    );

    // No purge happened — e0/e1 from the first replay must still be present alongside e2/e3.
    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    for uuid in ["e0", "e1", "e2", "e3"] {
        assert!(
            conn.get_entity_by_uuid(uuid).unwrap().is_some(),
            "{uuid} must survive an incremental (non-reset) replay"
        );
    }
    let pos = conn.get_wal_position(G).unwrap();
    assert_eq!(pos.applied_seq, Some(3));
    assert_eq!(pos.generation.as_deref(), Some("gen-1"));
}

// ── Story 5 / SC-006: a pre-existing, never-generationed stream ───────────────────────────────

#[tokio::test]
async fn story5_legacy_no_generation_stream_across_two_boots_then_later_reset_is_detected() {
    const G: &str = "g-legacy";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    // A pre-#387 stream: content on disk, no generation sidecar at all.
    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "legacy-0", "legacy-0", G),
            entity_wal_line(1, "legacy-1", "legacy-1", G),
        ],
    );
    assert_eq!(read_group_generation(&wal_root, G), None);

    let state = make_state_with_wal(db, wal_root.clone());

    // Boot 1: first-ever check. Must not report a reset (nothing to compare against).
    let boot1_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_eq!(boot1_v["result"]["success"], true, "{boot1_v}");
    assert_ne!(boot1_v["result"]["reset_detected"], true, "{boot1_v}");
    {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        let pos = conn.get_wal_position(G).unwrap();
        assert_eq!(pos.applied_seq, Some(1));
        assert_eq!(
            pos.generation, None,
            "adopting an ungenerationed stream must record generation: None, not force one"
        );
    }

    // Boot 2: a later boot, stream unchanged (still no generation), but a position was already
    // recorded by Boot 1 — issue #414 FR-002 reverses ADR-0387's original "tolerated forever"
    // design for exactly this case: this must now hard-fail rather than silently proceed as an
    // ordinary replay with reset detection unable to run.
    let boot2_v =
        dispatch_rebuild_sync(2, json!({"group_id": G, "from_seq": 2}), Arc::clone(&state)).await;
    assert!(
        boot2_v.get("error").is_some(),
        "a recorded position with an unknown current generation must refuse to replay: {boot2_v}"
    );
    assert_eq!(boot2_v["error"]["code"], -32000, "{boot2_v}");
    let boot2_msg = boot2_v["error"]["message"].as_str().unwrap_or("");
    assert!(
        boot2_msg.contains(G) && boot2_msg.contains(".wal-generation.json"),
        "error should name the group and the missing sidecar: {boot2_v}"
    );
    {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        assert!(
            conn.get_entity_by_uuid("legacy-0").unwrap().is_some(),
            "the refused boot 2 call must not have purged anything"
        );
        let pos = conn.get_wal_position(G).unwrap();
        assert_eq!(
            pos.applied_seq,
            Some(1),
            "the refused boot 2 call must not have advanced the recorded position"
        );
    }

    // A genuine reset afterward: republished from scratch, now carrying a generation for the
    // first time. The no-generation upgrade path must not have permanently disabled detection.
    reset_group_stream(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "post-upgrade-0", "post-upgrade-0", G),
            entity_wal_line(1, "post-upgrade-1", "post-upgrade-1", G),
        ],
        "gen-x",
    );
    let boot3_v =
        dispatch_rebuild_sync(3, json!({"group_id": G, "from_seq": 2}), Arc::clone(&state)).await;
    assert_ok_resp(&boot3_v, 3);
    assert_eq!(boot3_v["result"]["success"], true, "{boot3_v}");
    assert_eq!(
        boot3_v["result"]["reset_detected"], true,
        "a stream that had no generation and is later genuinely reset — now carrying one for \
         the first time — must still be detected: {boot3_v}"
    );
    assert_eq!(
        boot3_v["result"]["previous_generation"],
        Value::Null,
        "{boot3_v}"
    );
    assert_eq!(boot3_v["result"]["generation"], "gen-x", "{boot3_v}");

    let db3 = state.db.load_full().unwrap();
    let conn = db3.connect().unwrap();
    assert!(conn.get_entity_by_uuid("legacy-0").unwrap().is_none());
    assert!(conn.get_entity_by_uuid("post-upgrade-0").unwrap().is_some());
}

// ── issue #414 FR-001: knowledge_status's three-state generation_status field ──────────────────

#[tokio::test]
async fn fr001_knowledge_status_distinguishes_not_applicable_unknown_and_known() {
    const NEVER: &str = "g-never-hydrated";
    const UNKNOWN: &str = "g-content-no-generation";
    const KNOWN: &str = "g-content-with-generation";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    // NEVER: no directory at all — the ordinary "not yet hydrated" state.
    // UNKNOWN: content on disk, no .wal-generation.json — the reproduction's case.
    write_group_wal(
        &wal_root,
        UNKNOWN,
        "0000.jsonl",
        &[entity_wal_line(0, "u-0", "u-0", UNKNOWN)],
    );
    // KNOWN: content on disk, generation sidecar present.
    write_group_wal(
        &wal_root,
        KNOWN,
        "0000.jsonl",
        &[entity_wal_line(0, "k-0", "k-0", KNOWN)],
    );
    set_group_generation(&wal_root, KNOWN, "gen-known");

    let state = make_state_with_wal(db, wal_root.clone());
    let status_v = dispatch_val(1, "knowledge_status", json!({}), Arc::clone(&state)).await;
    assert!(status_v.get("error").is_none(), "{status_v}");

    let groups = &status_v["result"]["wal_groups"];
    assert!(
        groups.get(NEVER).is_none(),
        "a group with no WAL directory at all must not appear in wal_groups: {status_v}"
    );
    assert_eq!(
        groups[UNKNOWN]["generation"],
        Value::Null,
        "generation itself stays null for backward compatibility: {status_v}"
    );
    assert_eq!(
        groups[UNKNOWN]["generation_status"], "unknown",
        "content with no generation sidecar must be distinguishable from never-hydrated: {status_v}"
    );
    assert_eq!(groups[KNOWN]["generation"], "gen-known", "{status_v}");
    assert_eq!(groups[KNOWN]["generation_status"], "known", "{status_v}");
}

// ── issue #414 FR-002 / SC-005: refusal is scoped to the affected group only ───────────────────

#[tokio::test]
async fn fr002_refusal_is_scoped_to_the_affected_group_only() {
    const BAD: &str = "g-unknown-generation";
    const GOOD: &str = "g-known-generation";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        BAD,
        "0000.jsonl",
        &[entity_wal_line(0, "bad-0", "bad-0", BAD)],
    );
    write_group_wal(
        &wal_root,
        GOOD,
        "0000.jsonl",
        &[entity_wal_line(0, "good-0", "good-0", GOOD)],
    );
    set_group_generation(&wal_root, GOOD, "gen-good");

    let state = make_state_with_wal(db, wal_root.clone());

    // First-ever call for BAD adopts (applied_seq: None -> no comparison possible yet).
    let boot_v = dispatch_rebuild_sync(
        1,
        json!({"group_id": BAD, "from_seq": 0}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");

    // A later call against BAD, now that a position is recorded and the on-disk generation is
    // still unknown, must refuse — including under dry_run (no silent bypass, FR-002).
    let dry_v = dispatch_rebuild_sync(
        2,
        json!({"group_id": BAD, "from_seq": 1, "dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert!(
        dry_v.get("error").is_some(),
        "dry_run must not bypass the unknown-generation refusal: {dry_v}"
    );

    // GOOD, sharing the same WAL root, must remain independently replayable (SC-005).
    let good_v = dispatch_rebuild_sync(
        3,
        json!({"group_id": GOOD, "from_seq": 0}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(
        good_v["result"]["success"], true,
        "a sibling group with a known generation must not be blocked by BAD's refusal: {good_v}"
    );
}

// ── issue #428 FR-002/FR-006/SC-004: a group with demonstrably nothing previously applied to
//    protect is exempted from the unknown-generation guard, and the exemption mints a generation
//    so the workspace isn't left permanently re-hitting it ────────────────────────────────────

#[tokio::test]
async fn fr002_428_no_prior_content_at_risk_exemption_lets_rebuild_succeed_and_stamps_a_generation(
) {
    const G: &str = "g-migrated-unstamped";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    // Content on disk, no generation sidecar — the shape a workspace migrated by 0.13.0/0.13.1
    // is left in (issue #428 Story 2), before this issue's migration-time stamp (FR-001) existed.
    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "mig-0", "mig-0", G),
            entity_wal_line(1, "mig-1", "mig-1", G),
        ],
    );
    assert_eq!(read_group_generation(&wal_root, G), None);

    let state = make_state_with_wal(Arc::clone(&db), wal_root.clone());

    // Simulate `knowledge_status`'s own backfill having already recorded a position at
    // `applied_seq: 0` against an empty database — the exact "demonstrably nothing at risk"
    // state issue #428's narrow exemption targets. Written directly (not via a prior rebuild
    // call), since the whole point of this scenario is that this row can arrive without ever
    // having gone through a rebuild.
    {
        let conn = db.connect().unwrap();
        conn.set_wal_position(G, 0, None).unwrap();
    }

    // Without issue #428's fix, this call would be refused outright by #414's guard
    // (`applied_seq.is_some() && current_generation.is_none()`) — the exact regression this
    // issue exists to fix.
    let rebuild_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_ok_resp(&rebuild_v, 1);
    assert_eq!(rebuild_v["result"]["success"], true, "{rebuild_v}");

    // The exemption mints a generation immediately (not left permanently unstamped) — visible
    // both on disk and to the completion write.
    let minted = read_group_generation(&wal_root, G);
    assert!(
        minted.is_some(),
        "the exemption must stamp a generation, not leave the stream permanently unstamped"
    );

    {
        let conn = db.connect().unwrap();
        assert!(conn.get_entity_by_uuid("mig-0").unwrap().is_some());
        assert!(conn.get_entity_by_uuid("mig-1").unwrap().is_some());
        let pos = conn.get_wal_position(G).unwrap();
        assert_eq!(pos.applied_seq, Some(1));
        assert_eq!(
            pos.generation, minted,
            "the completion write must record the newly minted generation"
        );
    }

    // A follow-up call now behaves as an ordinary, generation-known incremental replay — no more
    // special-casing, no repeated refusal.
    write_group_wal(
        &wal_root,
        G,
        "0001.jsonl",
        &[entity_wal_line(2, "mig-2", "mig-2", G)],
    );
    let followup_v =
        dispatch_rebuild_sync(2, json!({"group_id": G, "from_seq": 2}), Arc::clone(&state)).await;
    assert_ok_resp(&followup_v, 2);
    assert_eq!(followup_v["result"]["success"], true, "{followup_v}");
    assert_ne!(
        followup_v["result"]["reset_detected"], true,
        "ordinary forward progress on the now-known generation must never be flagged: {followup_v}"
    );

    let conn = db.connect().unwrap();
    assert!(conn.get_entity_by_uuid("mig-2").unwrap().is_some());
    let pos = conn.get_wal_position(G).unwrap();
    assert_eq!(pos.applied_seq, Some(2));
    assert_eq!(pos.generation, minted, "{pos:?}");
}

/// dry_run under the narrow exemption must remain a true no-op: no generation minted, no
/// database mutation, consistent with every other dry-run path in this handler.
#[tokio::test]
async fn fr002_428_no_prior_content_at_risk_exemption_is_not_mutated_by_dry_run() {
    const G: &str = "g-migrated-unstamped-dry";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[entity_wal_line(0, "dry-0", "dry-0", G)],
    );

    let state = make_state_with_wal(Arc::clone(&db), wal_root.clone());
    {
        let conn = db.connect().unwrap();
        conn.set_wal_position(G, 0, None).unwrap();
    }

    let dry_v = dispatch_rebuild_sync(
        1,
        json!({"group_id": G, "from_seq": 0, "dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&dry_v, 1);
    assert_eq!(dry_v["result"]["success"], true, "{dry_v}");
    assert_eq!(
        read_group_generation(&wal_root, G),
        None,
        "a dry-run preview must never write a generation sidecar to disk"
    );
    let conn = db.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid("dry-0").unwrap().is_none(),
        "a dry-run preview must never mutate the database"
    );
}

// ── SC-008: multi-group isolation ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn sc008_multi_group_isolation_reset_of_a_leaves_b_and_c_untouched() {
    const A: &str = "g-iso-a";
    const B: &str = "g-iso-b";
    const C: &str = "g-iso-c";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    for (gid, gen) in [(A, "a-gen-1"), (B, "b-gen-1"), (C, "c-gen-1")] {
        write_group_wal(
            &wal_root,
            gid,
            "0000.jsonl",
            &[entity_wal_line(
                0,
                &format!("{gid}-e0"),
                &format!("{gid}-e0"),
                gid,
            )],
        );
        set_group_generation(&wal_root, gid, gen);
    }

    let state = make_state_with_wal(db, wal_root.clone());
    for (i, gid) in [A, B, C].into_iter().enumerate() {
        let v = dispatch_rebuild_sync(
            i as i64,
            json!({"group_id": gid, "from_seq": 0}),
            Arc::clone(&state),
        )
        .await;
        assert_eq!(v["result"]["success"], true, "{v}");
    }

    let (b_pos_before, c_pos_before) = {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        (
            conn.get_wal_position(B).unwrap(),
            conn.get_wal_position(C).unwrap(),
        )
    };

    // Reset only group A.
    reset_group_stream(
        &wal_root,
        A,
        "0000.jsonl",
        &[
            entity_wal_line(0, "a-new-0", "a-new-0", A),
            entity_wal_line(1, "a-new-1", "a-new-1", A),
        ],
        "a-gen-2",
    );
    let reset_v = dispatch_rebuild_sync(
        10,
        json!({"group_id": A, "from_seq": 1}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(reset_v["result"]["reset_detected"], true, "{reset_v}");

    let db3 = state.db.load_full().unwrap();
    let conn = db3.connect().unwrap();
    let (b_pos_after, c_pos_after) = (
        conn.get_wal_position(B).unwrap(),
        conn.get_wal_position(C).unwrap(),
    );
    assert_eq!(
        b_pos_before, b_pos_after,
        "group B's position must be byte-for-byte unchanged by A's reset and self-heal"
    );
    assert_eq!(
        c_pos_before, c_pos_after,
        "group C's position must be byte-for-byte unchanged by A's reset and self-heal"
    );
    assert!(
        conn.get_entity_by_uuid(&format!("{B}-e0"))
            .unwrap()
            .is_some(),
        "group B's data must be untouched"
    );
    assert!(
        conn.get_entity_by_uuid(&format!("{C}-e0"))
            .unwrap()
            .is_some(),
        "group C's data must be untouched"
    );
}

// ── Story 4 / SC-005: a checkpoint taken before a reset reports unreachable afterward ─────────

#[tokio::test]
async fn story4_checkpoint_taken_before_reset_reports_unreachable_after() {
    const G: &str = "g-checkpoint";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "e0", "e0", G),
            entity_wal_line(1, "e1", "e1", G),
        ],
    );
    set_group_generation(&wal_root, G, "gen-1");

    let state = make_state_with_wal(db, wal_root.clone());
    let boot_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");

    let checkpoint_v = dispatch_val(
        2,
        "knowledge_wal_mark_create",
        json!({"name": "pre-reset", "group_id": G}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&checkpoint_v, 2);
    assert_eq!(
        checkpoint_v["result"]["generation"], "gen-1",
        "{checkpoint_v}"
    );

    // Reset: a longer stream so the checkpoint's seq would still fall inside the new
    // [wal_min_seq, wal_max_seq] bounds — proving the generation check, not the bounds check,
    // is what makes it unreachable.
    reset_group_stream(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "n0", "n0", G),
            entity_wal_line(1, "n1", "n1", G),
            entity_wal_line(2, "n2", "n2", G),
        ],
        "gen-2",
    );

    let list_v = dispatch_val(
        3,
        "knowledge_wal_mark_list",
        json!({"group_id": G}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&list_v, 3);
    let checkpoints = list_v["result"]["checkpoints"].as_array().unwrap();
    let pre_reset = checkpoints
        .iter()
        .find(|c| c["name"] == "pre-reset")
        .unwrap_or_else(|| panic!("expected 'pre-reset' checkpoint in {list_v}"));
    assert_eq!(pre_reset["generation"], "gen-1", "{pre_reset}");
    assert_eq!(
        pre_reset["reachable"], false,
        "a checkpoint recorded against a generation that no longer matches the stream's \
         current on-disk generation must be unreachable even though its seq is within bounds: \
         {pre_reset}"
    );

    // A checkpoint taken against the CURRENT generation must be reachable as normal (this
    // feature narrows reachability, it does not change it for the unaffected case — AC2).
    let checkpoint2_v = dispatch_val(
        4,
        "knowledge_wal_mark_create",
        json!({"name": "post-reset", "group_id": G}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&checkpoint2_v, 4);
    assert_eq!(
        checkpoint2_v["result"]["generation"], "gen-2",
        "{checkpoint2_v}"
    );
    let list2_v = dispatch_val(5, "knowledge_wal_mark_list", json!({"group_id": G}), state).await;
    let checkpoints2 = list2_v["result"]["checkpoints"].as_array().unwrap();
    let post_reset = checkpoints2
        .iter()
        .find(|c| c["name"] == "post-reset")
        .unwrap();
    assert_eq!(post_reset["reachable"], true, "{post_reset}");
}

// ── Story 3 / SC-004: cross-group pointers re-bind to the new generation after a reset ────────

#[tokio::test]
async fn story3_cross_group_pointer_rebinds_to_new_generation_after_reset() {
    const LAYER: &str = "layer-reset";
    const TARGET: &str = "g-target-reset";
    let (db, _db_dir) = make_db(4);
    let hub_uuid = uuid::Uuid::new_v4().to_string();
    {
        let conn = db.connect().unwrap();
        conn.insert_entity(&EntityRow {
            uuid: hub_uuid.clone(),
            name: "layer-hub".to_string(),
            group_id: LAYER.to_string(),
            labels: vec!["Entity".to_string()],
            created_at: "2026-05-22 00:00:00".to_string(),
            name_embedding: vec![1.0, 0.0, 0.0, 0.0],
            summary: "s".to_string(),
            attributes: "{}".to_string(),
            ..Default::default()
        })
        .unwrap();
        conn.set_wal_position(LAYER, 999, None).unwrap();
    }

    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();
    let state = make_state_with_wal(db, wal_root.clone());

    let add_v = dispatch_val(
        1,
        "knowledge_add_cross_group_edge",
        json!({
            "name": "REFERENCES",
            "group_id": LAYER,
            "source": {"uuid": hub_uuid},
            "target": {"source_group_id": TARGET, "endpoint_name": "widget"},
            "fact": "layer-hub references widget",
        }),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&add_v, 1);
    let edge_uuid = add_v["result"]["uuid"].as_str().unwrap().to_string();
    assert_eq!(
        add_v["result"]["cross_group_pointers"]["dst"]["binding_state"],
        "unbound"
    );

    // Target group's first content, generation g1.
    let widget_1 = uuid::Uuid::new_v4().to_string();
    write_group_wal(
        &wal_root,
        TARGET,
        "0000.jsonl",
        &[entity_wal_line(0, &widget_1, "widget", TARGET)],
    );
    set_group_generation(&wal_root, TARGET, "target-gen-1");

    let boot_v = dispatch_rebuild_sync(
        2,
        json!({"group_id": TARGET, "from_seq": 0}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");
    // First bind is an ordinary, explicit rebind call (no reset — FR-010's auto-rebind only
    // fires on a detected reset).
    let rebind_v = dispatch_val(
        3,
        "knowledge_rebind_pointers",
        json!({"source_group_id": TARGET}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(rebind_v["result"]["bound"], 1, "{rebind_v}");

    let bound_before = {
        let db2 = state.db.load_full().unwrap();
        let conn = db2.connect().unwrap();
        conn.get_relates_to_by_uuids(std::slice::from_ref(&edge_uuid))
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .target_node_uuid
    };
    assert_eq!(bound_before, widget_1);

    // Reset the target group: a new "widget" entity under a new uuid and generation, plus a
    // decoy so the replacement stream is longer than the original.
    let widget_2 = uuid::Uuid::new_v4().to_string();
    reset_group_stream(
        &wal_root,
        TARGET,
        "0000.jsonl",
        &[
            entity_wal_line(0, &widget_2, "widget", TARGET),
            entity_wal_line(1, "decoy", "decoy", TARGET),
        ],
        "target-gen-2",
    );

    let reset_v = dispatch_rebuild_sync(
        4,
        json!({"group_id": TARGET, "from_seq": 1}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&reset_v, 4);
    assert_eq!(reset_v["result"]["reset_detected"], true, "{reset_v}");
    assert_eq!(
        reset_v["result"]["cross_group_rebind"]["bound"], 1,
        "FR-010: the reset-triggered self-heal must re-bind cross-group pointers into the reset \
         group as part of the same operation, without a separate knowledge_rebind_pointers call: \
         {reset_v}"
    );

    let db3 = state.db.load_full().unwrap();
    let conn = db3.connect().unwrap();
    let edge_after = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge_uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        edge_after.target_node_uuid, widget_2,
        "the pointer must re-bind to the new generation's entity"
    );
    assert!(
        conn.get_entity_by_uuid(&widget_1).unwrap().is_none(),
        "the old generation's widget entity must be purged, not left dangling"
    );
}

// ── Story 6: knowledge_dump_wal always mints a new generation ─────────────────────────────────

#[tokio::test]
async fn story6_dump_wal_always_mints_a_new_generation() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("dump_gen_test.db");
    let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(4).unwrap();
    }

    let seed_wal_dir = dir.path().join("seed-wal");
    {
        let mut writer = WalWriter::new(&seed_wal_dir, 10_000, 0).unwrap();
        writer
            .with_chunk(|w| {
                w.log_mutation(
                    "MERGE (n:Entity {uuid: 'seed-1'}) ON CREATE SET n.name = 'seed', \
                     n.group_id = 'liminis'",
                    json!({}),
                    "",
                )
            })
            .unwrap();
    }
    let source_generation = wal_generation::read_generation(&seed_wal_dir);
    assert!(
        source_generation.is_some(),
        "the seed writer must have minted a generation for its own fresh directory"
    );
    {
        let conn = db.connect().unwrap();
        lcg_core::replay::WalReplayer::new(&seed_wal_dir)
            .replay(&conn)
            .unwrap();
    }

    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(4)),
        extractor: Arc::new(MockExtractor),
        dedup: Arc::new(PassthroughDedupAdapter),
        write_lock: Arc::new(RwLock::new(())),
        sink,
        db_path: db_path.to_str().unwrap().to_string(),
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
    });

    let target_dir = dir.path().join("dump-out");
    let dump_v = dispatch_val(
        1,
        "knowledge_dump_wal",
        json!({"target_dir": target_dir.to_str().unwrap()}),
        state,
    )
    .await;
    assert_ok_resp(&dump_v, 1);
    assert_eq!(dump_v["result"]["success"], true, "{dump_v}");

    let dumped_generation = wal_generation::read_generation(&target_dir);
    assert!(
        dumped_generation.is_some(),
        "a dumped stream must carry its own newly minted generation"
    );
    assert_ne!(
        dumped_generation, source_generation,
        "a dumped stream's generation must differ from the source's — it is a new stream, not \
         a copy of the source's identity"
    );
}

// ── Dry-run mismatch is report-only ─────────────────────────────────────────────────────────

#[tokio::test]
async fn dry_run_mismatch_is_report_only_and_does_not_mutate() {
    const G: &str = "g-dry-run";
    let (db, _db_dir) = make_db(4);
    let wal_dir = TempDir::new().unwrap();
    let wal_root = wal_dir.path().to_path_buf();

    write_group_wal(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "e0", "e0", G),
            entity_wal_line(1, "e1", "e1", G),
        ],
    );
    set_group_generation(&wal_root, G, "gen-1");

    let state = make_state_with_wal(db, wal_root.clone());
    let boot_v =
        dispatch_rebuild_sync(1, json!({"group_id": G, "from_seq": 0}), Arc::clone(&state)).await;
    assert_eq!(boot_v["result"]["success"], true, "{boot_v}");

    reset_group_stream(
        &wal_root,
        G,
        "0000.jsonl",
        &[
            entity_wal_line(0, "n0", "n0", G),
            entity_wal_line(1, "n1", "n1", G),
            entity_wal_line(2, "n2", "n2", G),
        ],
        "gen-2",
    );

    let dry_v = dispatch_val(
        2,
        "knowledge_rebuild_from_wal",
        json!({"group_id": G, "from_seq": 2, "dry_run": true}),
        Arc::clone(&state),
    )
    .await;
    assert_ok_resp(&dry_v, 2);
    assert_eq!(dry_v["result"]["success"], true, "{dry_v}");
    assert_eq!(dry_v["result"]["dry_run"], true, "{dry_v}");
    assert_eq!(dry_v["result"]["reset_detected"], true, "{dry_v}");
    assert_eq!(dry_v["result"]["previous_generation"], "gen-1", "{dry_v}");
    assert_eq!(dry_v["result"]["generation"], "gen-2", "{dry_v}");
    assert!(
        dry_v["result"].get("job_id").is_none(),
        "a dry-run mismatch must return synchronously, report-only — never spawn a background \
         job: {dry_v}"
    );

    // Nothing was mutated: the old generation's entities are still present, and the recorded
    // position is untouched.
    let db2 = state.db.load_full().unwrap();
    let conn = db2.connect().unwrap();
    assert!(
        conn.get_entity_by_uuid("e0").unwrap().is_some(),
        "a dry run must never purge anything, even on a detected mismatch"
    );
    assert!(conn.get_entity_by_uuid("n0").unwrap().is_none());
    let pos = conn.get_wal_position(G).unwrap();
    assert_eq!(pos.applied_seq, Some(1));
    assert_eq!(pos.generation.as_deref(), Some("gen-1"));
}
