// Integration tests for per-group ontology resolution (issue #446).
//
// Covers: SC-001 (zero cross-group leakage between a per-group-ontology group and a co-resident
// group governed by something else), US2 (fallback to the workspace-wide ontology when no
// per-group file exists), the ontology-less edge case, malformed-per-group-file fallback, and
// percent-encoded group_id path resolution (FR-004).

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    episode,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    ontology::{group_ontology_path, load_ontology, EntityTypeDef, Ontology, OntologyMode},
    ontology_sidecar::{self, read_wal_ontology_sidecar},
    telemetry::{NoopSink, TelemetrySink},
    types::SourceType,
    wal_group::group_wal_dir,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const EMB_DIM: usize = 4;

fn make_db(dir: &TempDir) -> Arc<Db> {
    let db = Arc::new(Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }
    db
}

/// Mirrors `ontology_integration.rs`'s `make_state`, but additionally accepts a workspace root
/// so `AppState::resolve_ontology` can look for per-group files.
fn make_state(
    db: Arc<Db>,
    workspace_root: Option<&Path>,
    ontology: Option<Ontology>,
) -> Arc<AppState> {
    make_state_with_wal(db, workspace_root, None, ontology)
}

/// Like `make_state`, but also accepts a WAL root so `add_episode`'s FR-007 published-ontology
/// sidecar write has somewhere to write to.
fn make_state_with_wal(
    db: Arc<Db>,
    workspace_root: Option<&Path>,
    wal_root: Option<&Path>,
    ontology: Option<Ontology>,
) -> Arc<AppState> {
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
        wal_root: wal_root.map(|p| p.to_path_buf()),
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(HashMap::new())),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: workspace_root.map(|p| p.to_path_buf()),
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: ontology.map(Arc::new),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        embedding_cache: std::sync::Arc::new(lcg_core::EmbeddingCache::new()),
    })
}

fn write_group_ontology(workspace_root: &Path, group_id: &str, content: &str) {
    let path = group_ontology_path(workspace_root, group_id).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

fn write_workspace_ontology(workspace_root: &Path, content: &str) {
    let lcg_dir = workspace_root.join(".lcg");
    std::fs::create_dir_all(&lcg_dir).unwrap();
    let mut f = std::fs::File::create(lcg_dir.join("ontology.yaml")).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

// ── SC-001: zero cross-group leakage ───────────────────────────────────────────

// Group "catalog" has its own strict {Person}-only ontology file; group "content" has neither a
// per-group file nor a workspace ontology. MockExtractor returns Alice(Person)/Acme
// Corp(Organization) unconditionally regardless of group_id, so any difference in the two
// groups' persisted labels can only come from per-group ontology resolution, not extraction
// input.
#[tokio::test]
async fn per_group_ontology_governs_extraction_for_that_group_only() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    write_group_ontology(
        dir.path(),
        "catalog",
        "mode: strict\nentity_types:\n  - name: Person\n",
    );
    let state = make_state(db.clone(), Some(dir.path()), None);

    episode::add_episode(
        Arc::clone(&state),
        "catalog-ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "catalog",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    episode::add_episode(
        Arc::clone(&state),
        "content-ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "content",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    let conn = db.connect().unwrap();

    let catalog_acme = conn
        .get_entity_by_name_ci("Acme Corp", "catalog")
        .unwrap()
        .expect("Acme Corp must be persisted in group catalog");
    assert!(
        catalog_acme.labels.contains(&"Unclassified".to_string()),
        "group catalog's own strict {{Person}} ontology must reclassify Acme Corp: {:?}",
        catalog_acme.labels
    );

    let content_acme = conn
        .get_entity_by_name_ci("Acme Corp", "content")
        .unwrap()
        .expect("Acme Corp must be persisted in group content");
    assert!(
        content_acme.labels.contains(&"Organization".to_string()),
        "group content has no ontology at all and must not be affected by group catalog's \
         strict vocabulary: {:?}",
        content_acme.labels
    );
    assert!(
        !content_acme.labels.contains(&"Unclassified".to_string()),
        "group catalog's strict mode must not leak into group content: {:?}",
        content_acme.labels
    );
}

// ── US2: fallback to workspace-wide ontology ───────────────────────────────────

#[tokio::test]
async fn group_without_per_group_file_falls_back_to_workspace_ontology() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    write_workspace_ontology(
        dir.path(),
        "mode: strict\nentity_types:\n  - name: Person\n",
    );
    // Mirrors what `AppState::from_env` does at startup: load the workspace-wide ontology once
    // and hand it to AppState.ontology — resolve_ontology's fallback reads that in-memory field,
    // not the disk, so the workspace file must be loaded up front just like production startup.
    let workspace_ontology = load_ontology(Some(dir.path()));
    assert!(
        workspace_ontology.is_some(),
        "workspace ontology file must load"
    );
    // No per-group file for "grp" — must fall back to the workspace ontology above.
    let state = make_state(db.clone(), Some(dir.path()), workspace_ontology);

    let resolved = state.resolve_ontology("grp");
    let resolved = resolved.expect("must fall back to the workspace-wide ontology file");
    assert_eq!(resolved.mode, OntologyMode::Strict);
    assert_eq!(resolved.entity_types.len(), 1);
    assert_eq!(resolved.entity_types[0].name, "Person");
}

#[tokio::test]
async fn group_with_per_group_file_does_not_use_workspace_fallback() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    write_workspace_ontology(
        dir.path(),
        "mode: strict\nentity_types:\n  - name: Person\n",
    );
    write_group_ontology(
        dir.path(),
        "grp-open",
        "mode: open\nentity_types:\n  - name: Person\n",
    );
    let workspace_ontology = load_ontology(Some(dir.path()));
    let state = make_state(db, Some(dir.path()), workspace_ontology);

    let resolved = state
        .resolve_ontology("grp-open")
        .expect("per-group file must resolve");
    assert_eq!(
        resolved.mode,
        OntologyMode::Open,
        "the group's own file (open) must win over the workspace file (strict)"
    );
}

// ── Edge case: neither per-group nor workspace ontology exists ────────────────

#[tokio::test]
async fn group_with_neither_ontology_extracts_free_form() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    let state = make_state(db.clone(), Some(dir.path()), None);

    assert!(
        state.resolve_ontology("grp").is_none(),
        "no per-group file and no workspace ontology must resolve to None"
    );

    let result = episode::add_episode(
        Arc::clone(&state),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "grp",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    assert_eq!(
        result.nodes_extracted, 2,
        "with no ontology at all, extraction must be unfiltered (free-form)"
    );
}

// ── Malformed per-group file falls back to the workspace ontology, not to None ────

#[tokio::test]
async fn malformed_per_group_file_falls_back_to_workspace_ontology_no_panic() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    write_workspace_ontology(
        dir.path(),
        "mode: strict\nentity_types:\n  - name: Person\n",
    );
    write_group_ontology(dir.path(), "grp", "not: valid: yaml: [{{\n");
    let workspace_ontology = load_ontology(Some(dir.path()));
    let state = make_state(db, Some(dir.path()), workspace_ontology);

    let resolved = state.resolve_ontology("grp");
    let resolved = resolved
        .expect("a malformed per-group file must fall back to the workspace ontology, not to None");
    assert_eq!(resolved.mode, OntologyMode::Strict);
    assert_eq!(resolved.entity_types[0].name, "Person");
}

// ── FR-004: percent-encoded group_id resolves correctly and does not collide ──────

#[tokio::test]
async fn percent_encoded_group_id_resolves_its_own_file_without_collision() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    // "acme/prod" is unsafe as a bare path component and must be percent-encoded (FR-004).
    write_group_ontology(
        dir.path(),
        "acme/prod",
        "mode: open\nentity_types:\n  - name: KnowledgeChannel\n",
    );
    let state = make_state(db, Some(dir.path()), None);

    let resolved = state
        .resolve_ontology("acme/prod")
        .expect("percent-encoded group_id must resolve to its own file");
    assert_eq!(resolved.entity_types[0].name, "KnowledgeChannel");

    // A different, unsafe group_id that happens to share the same encoded prefix must not
    // collide with "acme/prod"'s file.
    assert!(
        state.resolve_ontology("acme").is_none(),
        "a differently-encoded group_id must not see another group's per-group file"
    );
}

// ── Lazy-load-and-cache: a second resolve_ontology call is served from cache ──────

#[tokio::test]
async fn resolve_ontology_caches_after_first_lookup() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    write_group_ontology(dir.path(), "grp", "entity_types:\n  - name: Person\n");
    let state = make_state(db, Some(dir.path()), None);

    let first = state
        .resolve_ontology("grp")
        .expect("first resolution should find the file");
    assert_eq!(first.entity_types[0].name, "Person");

    // Mutate the file on disk after the first (caching) lookup. Since hot-reload is explicitly
    // out of scope for this issue, a second lookup for the same group must still return the
    // originally cached value, not re-read the changed file.
    write_group_ontology(dir.path(), "grp", "entity_types:\n  - name: Organization\n");

    let second = state
        .resolve_ontology("grp")
        .expect("second resolution should hit the cache");
    assert_eq!(
        second.entity_types[0].name, "Person",
        "resolve_ontology must be cached per group_id, not re-read from disk on every call"
    );
    assert!(
        Arc::ptr_eq(&first, &second),
        "the cached value must be the same Arc, not a freshly loaded one"
    );
}

// ── FR-007/FR-009: published ontology sidecar is written per group, absence is safe ──

#[tokio::test]
async fn add_episode_writes_published_ontology_sidecar_for_the_extracting_group() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    let wal_root = dir.path().join("wal");
    write_group_ontology(
        dir.path(),
        "catalog",
        "mode: strict\nentity_types:\n  - name: Person\n",
    );
    let state = make_state_with_wal(db, Some(dir.path()), Some(&wal_root), None);

    episode::add_episode(
        Arc::clone(&state),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "catalog",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    let gid_dir = group_wal_dir(&wal_root, "catalog").unwrap();
    let sidecar = read_wal_ontology_sidecar(&gid_dir)
        .expect("FR-007: .wal-ontology.json must be written into the group's own WAL directory");
    assert_eq!(sidecar.mode.as_deref(), Some("strict"));
    assert_eq!(sidecar.entity_types, vec!["Person".to_string()]);

    // A co-resident group with no ontology gets its own sidecar recording "no ontology" — never
    // "catalog"'s (FR-007 must not leak across groups any more than extraction itself does).
    episode::add_episode(
        Arc::clone(&state),
        "ep2",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "content",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();
    let content_gid_dir = group_wal_dir(&wal_root, "content").unwrap();
    let content_sidecar = read_wal_ontology_sidecar(&content_gid_dir)
        .expect("group content must get its own sidecar");
    assert_eq!(content_sidecar.mode, None);
    assert!(content_sidecar.entity_types.is_empty());
}

// FR-009: a missing wal_root (so the sidecar write is skipped entirely) must not affect
// extraction/replay correctness — add_episode still succeeds normally.
#[tokio::test]
async fn add_episode_succeeds_without_wal_root_configured() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    let state = make_state_with_wal(db, Some(dir.path()), None, None);

    let result = episode::add_episode(
        Arc::clone(&state),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "grp",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    assert_eq!(result.nodes_extracted, 2);
}

// ── issue #451: per-group ontology drift detection ─────────────────────────────

fn ontology_with(mode: OntologyMode, names: &[&str]) -> Ontology {
    Ontology {
        mode,
        entity_types: names
            .iter()
            .map(|n| EntityTypeDef {
                name: n.to_string(),
                description: None,
                parent: None,
            })
            .collect(),
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    }
}

async fn dispatch(id: i64, method: &str, params: Value, state: Arc<AppState>) -> Value {
    let req = IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    };
    let resp = handlers::dispatch(req, state, None).await;
    serde_json::to_value(resp).unwrap()
}

fn entity_wal_line(seq: u64, uuid: &str, group_id: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{uuid}', n.group_id = '{group_id}', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

// User Story 1, Scenario 1 + User Story 2 (isolation): changing group A's own per-group file is
// reported as drift for A only — a sibling group B, whose own per-group file is unaffected, must
// show no drift.
#[tokio::test]
async fn per_group_file_change_reports_drift_for_that_group_only() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);

    // Group A's per-group file on disk now declares {Person, Organization} — but its recorded
    // sidecar (from a "previous run") only reflects {Person}, simulating an operator edit
    // between restarts.
    write_group_ontology(
        dir.path(),
        "group-a",
        "mode: open\nentity_types:\n  - name: Person\n  - name: Organization\n",
    );
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-a",
        Some(&ontology_with(OntologyMode::Open, &["Person"])),
    )
    .unwrap();

    // Group B's per-group file and recorded sidecar agree — unaffected by A's change.
    write_group_ontology(
        dir.path(),
        "group-b",
        "mode: open\nentity_types:\n  - name: Equipment\n",
    );
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-b",
        Some(&ontology_with(OntologyMode::Open, &["Equipment"])),
    )
    .unwrap();

    let state = make_state(db, Some(dir.path()), None);

    state.resolve_ontology("group-a");
    state.resolve_ontology("group-b");

    let a_status = state
        .group_drift_status("group-a")
        .expect("group-a must have a computed status after resolve_ontology");
    assert!(
        a_status.drifted,
        "group-a's per-group file change must be detected as drift"
    );

    let b_status = state
        .group_drift_status("group-b")
        .expect("group-b must have a computed status after resolve_ontology");
    assert!(
        !b_status.drifted,
        "group-b's unaffected per-group file must not be reported as drifted just because \
         group-a's file changed"
    );
}

// User Story 1, Scenario 2: a group that falls back to the workspace ontology detects drift when
// the workspace ontology changes.
#[tokio::test]
async fn workspace_fallback_group_reports_drift_when_workspace_ontology_changes() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);

    // "Now": the workspace ontology declares {Person, Organization}.
    write_workspace_ontology(
        dir.path(),
        "mode: open\nentity_types:\n  - name: Person\n  - name: Organization\n",
    );
    let workspace_ontology = load_ontology(Some(dir.path()));

    // "Previously": group-b (no per-group file of its own) had its drift sidecar recorded
    // against an older workspace ontology that only had {Person}.
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-b",
        Some(&ontology_with(OntologyMode::Open, &["Person"])),
    )
    .unwrap();

    let state = make_state(db, Some(dir.path()), workspace_ontology);
    state.resolve_ontology("group-b");

    let status = state.group_drift_status("group-b").unwrap();
    assert!(
        status.drifted,
        "a workspace-fallback group must detect drift when the workspace ontology it falls \
         back to changes"
    );
}

// User Story 2, Scenario 2 + edge case: two groups that both fall back to the workspace ontology
// both drift when it changes; a co-resident group with its own unaffected per-group file does not.
#[tokio::test]
async fn workspace_ontology_change_drifts_every_fallback_group_but_not_a_group_with_its_own_file() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);

    write_workspace_ontology(
        dir.path(),
        "mode: open\nentity_types:\n  - name: Person\n  - name: Organization\n",
    );
    let workspace_ontology = load_ontology(Some(dir.path()));

    // group-c and group-d both fall back to the workspace ontology; both have stale sidecars.
    for gid in ["group-c", "group-d"] {
        ontology_sidecar::write_group_sidecar(
            dir.path(),
            gid,
            Some(&ontology_with(OntologyMode::Open, &["Person"])),
        )
        .unwrap();
    }

    // group-a has its own per-group file, unaffected by the workspace change, with a matching
    // sidecar recorded against that file.
    write_group_ontology(
        dir.path(),
        "group-a",
        "mode: open\nentity_types:\n  - name: Equipment\n",
    );
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-a",
        Some(&ontology_with(OntologyMode::Open, &["Equipment"])),
    )
    .unwrap();

    let state = make_state(db, Some(dir.path()), workspace_ontology);
    state.resolve_ontology("group-c");
    state.resolve_ontology("group-d");
    state.resolve_ontology("group-a");

    assert!(
        state.group_drift_status("group-c").unwrap().drifted,
        "group-c falls back to the changed workspace ontology and must drift"
    );
    assert!(
        state.group_drift_status("group-d").unwrap().drifted,
        "group-d falls back to the changed workspace ontology and must drift"
    );
    assert!(
        !state.group_drift_status("group-a").unwrap().drifted,
        "group-a has its own unaffected per-group file and must not drift just because the \
         workspace ontology, which it doesn't use, changed"
    );
}

// Edge case: a group's per-group file is deleted, so it now falls back to the workspace ontology
// — a change to which source it resolves through, even though neither file's content changed on
// its own — MUST be detected as drift.
#[tokio::test]
async fn per_group_file_deleted_falls_back_and_reports_drift() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);

    write_workspace_ontology(dir.path(), "mode: open\nentity_types:\n  - name: Person\n");
    let workspace_ontology = load_ontology(Some(dir.path()));

    // group-a previously had its own per-group file with {Equipment}; its sidecar matches that.
    let per_group_path = group_ontology_path(dir.path(), "group-a").unwrap();
    write_group_ontology(
        dir.path(),
        "group-a",
        "mode: open\nentity_types:\n  - name: Equipment\n",
    );
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-a",
        Some(&ontology_with(OntologyMode::Open, &["Equipment"])),
    )
    .unwrap();

    // The operator removes group-a's per-group file — it now falls back to the workspace
    // ontology ({Person}), a different resolved ontology than what the sidecar recorded.
    std::fs::remove_file(&per_group_path).unwrap();

    let state = make_state(db, Some(dir.path()), workspace_ontology);
    let resolved = state
        .resolve_ontology("group-a")
        .expect("must fall back to the workspace ontology now that the per-group file is gone");
    assert_eq!(resolved.entity_types[0].name, "Person");

    let status = state.group_drift_status("group-a").unwrap();
    assert!(
        status.drifted,
        "falling back to a different source (workspace, after per-group file removal) must be \
         detected as drift, even though neither file's own content changed"
    );
}

// FR-010: a group whose data already exists in the DB, but which has never had a per-group drift
// sidecar recorded (mirrors the pre-#98 workspace-level `has_prior_data` migration case), must be
// treated as drifted the first time its ontology is resolved under per-group tracking.
#[tokio::test]
async fn group_with_prior_data_and_no_recorded_sidecar_reports_drift() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);

    // Ingest once for group "legacy" — this is "before" per-group drift tracking existed (in
    // effect, since the just-written group sidecar is removed below to simulate a workspace that
    // was ingested by a pre-#451 binary that never wrote one).
    let state1 = make_state(db.clone(), Some(dir.path()), None);
    episode::add_episode(
        Arc::clone(&state1),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "legacy",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    let sidecar_path = ontology_sidecar::group_sidecar_path(dir.path(), "legacy").unwrap();
    assert!(
        sidecar_path.exists(),
        "add_episode must have written a group sidecar for 'legacy'"
    );
    std::fs::remove_file(&sidecar_path).unwrap();

    // "Restart": a fresh AppState pointed at the same DB and workspace root, now with an
    // ontology loaded for the first time.
    let ontology = ontology_with(OntologyMode::Open, &["Person", "Organization"]);
    let state2 = make_state(db, Some(dir.path()), Some(ontology));
    state2.resolve_ontology("legacy");

    let status = state2.group_drift_status("legacy").unwrap();
    assert!(
        status.drifted,
        "a group with existing data but no recorded per-group sidecar must be treated as \
         drifted (FR-010), mirroring the workspace-level has_prior_data case"
    );
    let summary = status.drift_summary.unwrap();
    assert!(
        summary.contains("ontology added"),
        "summary should read like the workspace-level pre-upgrade case: {summary}"
    );
}

// User Story 5: drift clears after the documented "Recreate + re-ingest" remediation, which
// routes through add_episode — scoped to the group actually re-ingested.
#[tokio::test]
async fn drift_clears_after_add_episode_reingest_for_that_group_only() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);

    // group-a's per-group file is now {Person, Organization} but its recorded sidecar only has
    // {Person} — drift, until re-ingested.
    write_group_ontology(
        dir.path(),
        "group-a",
        "mode: open\nentity_types:\n  - name: Person\n  - name: Organization\n",
    );
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-a",
        Some(&ontology_with(OntologyMode::Open, &["Person"])),
    )
    .unwrap();

    // A sibling group-b is independently drifted and must stay drifted throughout.
    ontology_sidecar::write_group_sidecar(
        dir.path(),
        "group-b",
        Some(&ontology_with(OntologyMode::Open, &["Person"])),
    )
    .unwrap();
    write_group_ontology(
        dir.path(),
        "group-b",
        "mode: open\nentity_types:\n  - name: Equipment\n",
    );

    let state = make_state(db, Some(dir.path()), None);
    state.resolve_ontology("group-a");
    state.resolve_ontology("group-b");
    assert!(state.group_drift_status("group-a").unwrap().drifted);
    assert!(state.group_drift_status("group-b").unwrap().drifted);

    episode::add_episode(
        Arc::clone(&state),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "group-a",
        SourceType::Text,
        None,
        "",
    )
    .await
    .unwrap();

    assert!(
        !state.group_drift_status("group-a").unwrap().drifted,
        "group-a's drift must clear after a successful re-ingest under its current ontology"
    );
    assert!(
        state.group_drift_status("group-b").unwrap().drifted,
        "group-b was never re-ingested and must remain drifted — a re-ingest for group-a must \
         not clear an unrelated group's drift"
    );
}

// User Story 5 (WAL rebuild path) + FR-009: drift clears after a successful WAL rebuild for the
// remediated group only — a sibling group's legitimately-drifted state must survive untouched
// (the ADR-0451 clear-scope decision).
#[tokio::test]
async fn drift_clears_after_wal_rebuild_for_that_group_only() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    let wal_root = dir.path().join("wal");

    write_workspace_ontology(dir.path(), "mode: open\nentity_types:\n  - name: Person\n");
    let workspace_ontology = load_ontology(Some(dir.path()));

    // Both "rebuilt-group" and "other-group" fall back to the workspace ontology and are both
    // drifted going in.
    for gid in ["rebuilt-group", "other-group"] {
        ontology_sidecar::write_group_sidecar(
            dir.path(),
            gid,
            Some(&ontology_with(OntologyMode::Open, &["StaleType"])),
        )
        .unwrap();
    }

    let state = make_state_with_wal(db, Some(dir.path()), Some(&wal_root), workspace_ontology);
    state.resolve_ontology("rebuilt-group");
    state.resolve_ontology("other-group");
    assert!(state.group_drift_status("rebuilt-group").unwrap().drifted);
    assert!(state.group_drift_status("other-group").unwrap().drifted);

    let group_wal = group_wal_dir(&wal_root, "rebuilt-group").unwrap();
    std::fs::create_dir_all(&group_wal).unwrap();
    std::fs::write(
        group_wal.join("20260522_000000_aaa111_0000.jsonl"),
        entity_wal_line(0, "rebuilt-entity", "rebuilt-group") + "\n",
    )
    .unwrap();

    let v = dispatch(
        1,
        "knowledge_rebuild_from_wal",
        json!({"group_id": "rebuilt-group"}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(v["result"]["success"], json!(true), "{v}");
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            2,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        match status_v["result"]["status"].as_str().unwrap_or("?") {
            "completed" => break,
            "failed" => panic!("rebuild job failed: {status_v}"),
            "running" => {
                if std::time::Instant::now() > deadline {
                    panic!("rebuild did not complete within 5s: {status_v}");
                }
            }
            other => panic!("unexpected status: {other}: {status_v}"),
        }
    }

    assert!(
        !state.group_drift_status("rebuilt-group").unwrap().drifted,
        "rebuilt-group's drift must clear after its own successful WAL rebuild"
    );
    assert!(
        state.group_drift_status("other-group").unwrap().drifted,
        "other-group was not rebuilt and must remain drifted — FR-009's clear must not leak \
         across groups (ADR-0451)"
    );
}

// Regression: a WAL rebuild for a group this process has never resolved before (the realistic
// degraded-mode-recovery case — an admin issues a rebuild before any other use of the group)
// must not populate that group's drift cache at all, and must leave the on-disk sidecar in a
// state that reports "not drifted" whenever the group is genuinely resolved afterward. Before
// the `peek_or_load_ontology` fix, the clear-site called `resolve_ontology` to fetch the value
// to write into the sidecar, which — for a never-before-resolved group with data already
// present (post-replay) and no prior sidecar — computed drift=true and printed a false-alarm
// "drift detected ... recommend Recreate + re-ingest" warning in the middle of the very
// operation performing that remediation, then immediately cached the group as resolved (even
// though FR-007 says a group's status becomes available only on genuine first use).
#[tokio::test]
async fn wal_rebuild_of_never_resolved_group_does_not_populate_drift_cache() {
    let dir = TempDir::new().unwrap();
    let db = make_db(&dir);
    let wal_root = dir.path().join("wal");

    write_workspace_ontology(dir.path(), "mode: open\nentity_types:\n  - name: Person\n");
    let workspace_ontology = load_ontology(Some(dir.path()));

    let state = make_state_with_wal(db, Some(dir.path()), Some(&wal_root), workspace_ontology);
    // Deliberately no `state.resolve_ontology("fresh-group")` call here — this group has never
    // been resolved in this process, mirroring a freshly-started/degraded service whose first
    // action on this group is an admin-triggered rebuild.
    assert!(
        state.group_drift_status("fresh-group").is_none(),
        "precondition: group must start as not-yet-computed"
    );

    let group_wal = group_wal_dir(&wal_root, "fresh-group").unwrap();
    std::fs::create_dir_all(&group_wal).unwrap();
    std::fs::write(
        group_wal.join("20260522_000000_aaa111_0000.jsonl"),
        entity_wal_line(0, "fresh-entity", "fresh-group") + "\n",
    )
    .unwrap();

    let v = dispatch(
        1,
        "knowledge_rebuild_from_wal",
        json!({"group_id": "fresh-group"}),
        Arc::clone(&state),
    )
    .await;
    assert_eq!(v["result"]["success"], json!(true), "{v}");
    let job_id = v["result"]["job_id"]
        .as_str()
        .expect("expected job_id")
        .to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status_v = dispatch(
            2,
            "knowledge_rebuild_status",
            json!({"job_id": job_id.as_str()}),
            Arc::clone(&state),
        )
        .await;
        match status_v["result"]["status"].as_str().unwrap_or("?") {
            "completed" => break,
            "failed" => panic!("rebuild job failed: {status_v}"),
            "running" => {
                if std::time::Instant::now() > deadline {
                    panic!("rebuild did not complete within 5s: {status_v}");
                }
            }
            other => panic!("unexpected status: {other}: {status_v}"),
        }
    }

    assert!(
        state.group_drift_status("fresh-group").is_none(),
        "a rebuild of a group this process never resolved must not populate its drift cache \
         (FR-007: status becomes available only on genuine first use, not as a side effect of \
         the clear-site fetching a value to write into the sidecar)"
    );

    // Now genuinely resolve the group for the first time — the sidecar the rebuild just wrote
    // must already match, so this reports "not drifted", not a stale false positive.
    state.resolve_ontology("fresh-group");
    assert!(
        !state.group_drift_status("fresh-group").unwrap().drifted,
        "the sidecar written by the rebuild must already be consistent with the resolved \
         ontology, so the group's genuine first resolution afterward reports no drift"
    );
}
