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

use arc_swap::ArcSwapOption;
use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    episode,
    extractor::MockExtractor,
    ontology::{group_ontology_path, load_ontology, Ontology, OntologyMode},
    ontology_sidecar::read_wal_ontology_sidecar,
    telemetry::{NoopSink, TelemetrySink},
    types::SourceType,
    wal_group::group_wal_dir,
};
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
    )
    .await
    .unwrap();

    assert_eq!(result.nodes_extracted, 2);
}
