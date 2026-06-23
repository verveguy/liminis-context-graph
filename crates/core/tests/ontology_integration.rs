// Integration tests for FR-009: ontology-guided extraction (issue #83).
//
// Covers: ontology-honored extraction, no-ontology-unchanged behavior,
// strict-mode entity filtering, malformed-ontology graceful-degrade,
// knowledge_status ontology field.

use std::collections::HashMap;
use std::io::Write;
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
    handlers,
    ipc::IpcRequest,
    ontology::{content_hash, load_ontology, EntityTypeDef, Ontology, OntologyMode},
    ontology_sidecar,
    telemetry::{NoopSink, TelemetrySink},
    types::SourceType,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const EMB_DIM: usize = 4;

fn make_db() -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("ontology_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }
    (db, dir)
}

fn make_state(db: Arc<Db>, ontology: Option<Ontology>) -> Arc<AppState> {
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
        ontology: ontology.map(Arc::new),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
    })
}

fn write_ontology_file(dir: &TempDir, content: &str) {
    let lcg_dir = dir.path().join(".lcg");
    std::fs::create_dir_all(&lcg_dir).unwrap();
    let mut f = std::fs::File::create(lcg_dir.join("ontology.yaml")).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

fn req(id: i64, method: &str, params: Value) -> IpcRequest {
    IpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(id),
        method: method.to_string(),
        params,
    }
}

// ── add_episode ontology tests ────────────────────────────────────────────────

// SC-003: With no ontology, extraction is identical to free-form baseline.
// MockExtractor returns Alice (Person) + Acme Corp (Organization) — both should survive.
#[tokio::test]
async fn no_ontology_all_entities_pass() {
    let (db, _dir) = make_db();
    let state = make_state(db, None);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep",
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
        "no ontology: expected 2 entities (Alice + Acme Corp), got {}",
        result.nodes_extracted
    );
}

// SC-001: Strict-mode with {Person} drops Acme Corp (Organization not in vocabulary).
#[tokio::test]
async fn strict_mode_entity_filtering_drops_out_of_vocab() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    let state = make_state(db, Some(ontology));

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep",
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
        result.nodes_extracted, 1,
        "strict mode with {{Person}}: expected 1 entity (Alice only), got {}",
        result.nodes_extracted
    );
}

// SC-001: Open-mode with {Person} does not filter — both entities pass through.
#[tokio::test]
async fn open_mode_no_filtering() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    let state = make_state(db, Some(ontology));

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep",
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
        "open mode: expected 2 entities (no filtering), got {}",
        result.nodes_extracted
    );
}

// ── knowledge_status ontology field tests ─────────────────────────────────────

// SC-005: knowledge_status always includes ontology field — present=false when no ontology.
#[tokio::test]
async fn knowledge_status_ontology_field_present_false() {
    let (db, _dir) = make_db();
    let state = make_state(db, None);

    let resp = handlers::dispatch(req(1, "knowledge_status", json!({})), state, None).await;
    let resp_val = serde_json::to_value(resp).unwrap();
    let result = &resp_val["result"];

    assert!(
        result.get("ontology").is_some(),
        "knowledge_status must always include 'ontology' field"
    );
    assert_eq!(result["ontology"]["present"], json!(false));
    assert_eq!(result["ontology"]["loaded"], json!(false));
    assert_eq!(result["ontology"]["entity_type_count"], json!(0));
    assert_eq!(result["ontology"]["relation_type_count"], json!(0));
    assert_eq!(
        result["ontology"]["drifted"],
        json!(false),
        "no drift when no ontology and workspace_root is None"
    );
}

// SC-005: knowledge_status includes ontology field — present=true with correct counts.
#[tokio::test]
async fn knowledge_status_ontology_field_populated() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![
            EntityTypeDef {
                name: "Person".to_string(),
                description: None,
                parent: None,
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: None,
                parent: None,
            },
        ],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    let state = make_state(db, Some(ontology));

    let resp = handlers::dispatch(req(1, "knowledge_status", json!({})), state, None).await;
    let resp_val = serde_json::to_value(resp).unwrap();
    let result = &resp_val["result"];

    assert_eq!(result["ontology"]["present"], json!(true));
    assert_eq!(result["ontology"]["loaded"], json!(true));
    assert_eq!(result["ontology"]["mode"], json!("strict"));
    assert_eq!(result["ontology"]["entity_type_count"], json!(2));
    assert_eq!(result["ontology"]["relation_type_count"], json!(0));
    assert_eq!(
        result["ontology"]["drifted"],
        json!(false),
        "no drift when workspace_root is None"
    );
}

// ── load_ontology graceful degradation tests ──────────────────────────────────

// SC-004: Malformed YAML does not panic; returns None.
#[test]
fn load_ontology_malformed_returns_none() {
    let dir = TempDir::new().unwrap();
    write_ontology_file(&dir, "not: valid: yaml: [{{\n");
    let result = load_ontology(Some(dir.path()));
    assert!(
        result.is_none(),
        "malformed YAML ontology should return None without panicking"
    );
}

// SC-003: Valid YAML with no types coerces to None (free-form behavior).
#[test]
fn load_ontology_empty_returns_none() {
    let dir = TempDir::new().unwrap();
    write_ontology_file(&dir, "mode: open\nentity_types: []\nrelation_types: []\n");
    let result = load_ontology(Some(dir.path()));
    assert!(result.is_none(), "empty ontology file should return None");
}

// ── SC-003/FR-006: strict-mode relation_type filtering ───────────────────────

use lcg_core::ontology::RelationTypeDef;

// SC-003(a): Strict-mode ontology with {AUTHORED} declared — edges with WORKS_AT are dropped.
// MockExtractor returns Alice --WORKS_AT--> Acme Corp; the relation_type is not in vocabulary.
#[tokio::test]
async fn strict_mode_relation_type_drops_non_matching_edges() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![
            EntityTypeDef {
                name: "Person".to_string(),
                description: None,
                parent: None,
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: None,
                parent: None,
            },
        ],
        relation_types: vec![RelationTypeDef {
            name: "AUTHORED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let state = make_state(db, Some(ontology));

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-rt-strict",
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
        "both entities (Alice + Acme Corp) should pass through entity filtering"
    );
    assert_eq!(
        result.edges_extracted, 0,
        "strict mode: WORKS_AT edge must be dropped when ontology only declares AUTHORED; got {} edges",
        result.edges_extracted
    );
}

// SC-003(b): Open-mode ontology with {AUTHORED} declared — edges with WORKS_AT survive.
// MockExtractor returns Alice --WORKS_AT--> Acme Corp; open mode keeps LLM-derived relation_type.
#[tokio::test]
async fn open_mode_relation_type_keeps_llm_derived_edges() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![
            EntityTypeDef {
                name: "Person".to_string(),
                description: None,
                parent: None,
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: None,
                parent: None,
            },
        ],
        relation_types: vec![RelationTypeDef {
            name: "AUTHORED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let state = make_state(db, Some(ontology));

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-rt-open",
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
        "open mode: expected 2 entities (Alice + Acme Corp)"
    );
    assert_eq!(
        result.edges_extracted, 1,
        "open mode: WORKS_AT edge must survive even when ontology declares only AUTHORED; got {} edges",
        result.edges_extracted
    );
}

// ── FR-008: drift detection regression tests ─────────────────────────────────

fn make_ontology_with_entities(mode: OntologyMode, names: &[&str]) -> Ontology {
    let entity_types: Vec<EntityTypeDef> = names
        .iter()
        .map(|n| EntityTypeDef {
            name: n.to_string(),
            description: None,
            parent: None,
        })
        .collect();
    Ontology {
        mode,
        ancestor_map: HashMap::new(),
        entity_types,
        relation_types: vec![],
    }
}

#[test]
fn drift_detected_after_entity_type_addition() {
    let o1 = make_ontology_with_entities(OntologyMode::Open, &["Person"]);
    let o2 = make_ontology_with_entities(OntologyMode::Open, &["Person", "Equipment"]);
    assert_ne!(
        content_hash(Some(&o1)),
        content_hash(Some(&o2)),
        "adding an entity type must change the hash"
    );
}

#[test]
fn drift_detected_after_relation_type_rename() {
    let o1 = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "AUTHORED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let o2 = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "WROTE".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    assert_ne!(
        content_hash(Some(&o1)),
        content_hash(Some(&o2)),
        "renaming a relation type must change the hash"
    );
}

#[test]
fn cosmetic_edit_produces_same_hash() {
    // Parse the same logical ontology twice via load_ontology to verify the hash
    // is derived from parsed struct, not raw bytes.
    let dir1 = TempDir::new().unwrap();
    write_ontology_file(&dir1, "mode: open\nentity_types:\n  - name: Person\n");
    let dir2 = TempDir::new().unwrap();
    // Extra blank line is a cosmetic-only change
    write_ontology_file(&dir2, "mode: open\n\nentity_types:\n  - name: Person\n\n");
    let o1 = load_ontology(Some(dir1.path())).unwrap();
    let o2 = load_ontology(Some(dir2.path())).unwrap();
    assert_eq!(
        content_hash(Some(&o1)),
        content_hash(Some(&o2)),
        "whitespace-only differences must produce the same hash"
    );
}

#[test]
fn no_drift_when_sidecar_matches_loaded_ontology() {
    let dir = TempDir::new().unwrap();
    let ontology = make_ontology_with_entities(OntologyMode::Open, &["Person"]);
    ontology_sidecar::write_sidecar(dir.path(), Some(&ontology)).unwrap();
    let (drifted, summary) =
        ontology_sidecar::compute_drift(Some(dir.path()), Some(&ontology), false);
    assert!(
        !drifted,
        "drift must be false when sidecar hash matches current ontology"
    );
    assert!(summary.is_none());
}

#[test]
fn drift_clears_after_write_sidecar() {
    let dir = TempDir::new().unwrap();
    let o1 = make_ontology_with_entities(OntologyMode::Open, &["Person"]);
    let o2 = make_ontology_with_entities(OntologyMode::Open, &["Person", "Equipment"]);
    // Write sidecar with o1
    ontology_sidecar::write_sidecar(dir.path(), Some(&o1)).unwrap();
    // Drift detected for o2
    let (drifted_before, _) = ontology_sidecar::compute_drift(Some(dir.path()), Some(&o2), false);
    assert!(drifted_before, "drift must be true before sidecar update");
    // Write sidecar with o2 to "clear" drift
    ontology_sidecar::write_sidecar(dir.path(), Some(&o2)).unwrap();
    let (drifted_after, _) = ontology_sidecar::compute_drift(Some(dir.path()), Some(&o2), false);
    assert!(
        !drifted_after,
        "drift must clear after sidecar is updated to current ontology"
    );
}

#[test]
fn no_ontology_to_no_ontology_no_drift() {
    let dir = TempDir::new().unwrap();
    // Write sidecar with no ontology (sentinel "none")
    ontology_sidecar::write_sidecar(dir.path(), None).unwrap();
    let (drifted, _) = ontology_sidecar::compute_drift(Some(dir.path()), None, false);
    assert!(
        !drifted,
        "no drift when both sidecar and current ontology are None"
    );
}

#[test]
fn no_sidecar_no_prior_data_means_no_drift() {
    let dir = TempDir::new().unwrap();
    let ontology = make_ontology_with_entities(OntologyMode::Open, &["Person"]);
    let (drifted, _) = ontology_sidecar::compute_drift(Some(dir.path()), Some(&ontology), false);
    assert!(
        !drifted,
        "no drift on first run (no sidecar, no prior DB data)"
    );
}

#[test]
fn drift_summary_names_added_and_removed_types() {
    let dir = TempDir::new().unwrap();
    let old = make_ontology_with_entities(OntologyMode::Open, &["Person", "OldType"]);
    ontology_sidecar::write_sidecar(dir.path(), Some(&old)).unwrap();
    let new_ontology = make_ontology_with_entities(OntologyMode::Open, &["Person", "Equipment"]);
    let (drifted, summary) =
        ontology_sidecar::compute_drift(Some(dir.path()), Some(&new_ontology), false);
    assert!(drifted);
    let s = summary.unwrap();
    assert!(
        s.contains("Equipment"),
        "drift summary should mention added type: {s}"
    );
    assert!(
        s.contains("OldType"),
        "drift summary should mention removed type: {s}"
    );
}

// FR-002/User Story 1: sidecar present with hash "none" + ontology now loaded → drift (addition).
#[test]
fn sidecar_none_hash_plus_loaded_ontology_reports_drift() {
    let dir = TempDir::new().unwrap();
    ontology_sidecar::write_sidecar(dir.path(), None).unwrap();
    let ontology = make_ontology_with_entities(OntologyMode::Open, &["Person", "Organization"]);
    let (drifted, summary) =
        ontology_sidecar::compute_drift(Some(dir.path()), Some(&ontology), false);
    assert!(
        drifted,
        "drift must be true: sidecar=none, current=has ontology"
    );
    let s = summary.unwrap();
    assert!(
        s.contains("ontology added"),
        "summary must mention 'ontology added': {s}"
    );
}

// FR-002/User Story 2: sidecar present with real hash + no ontology now → drift (removal).
#[test]
fn sidecar_real_hash_plus_no_ontology_reports_drift() {
    let dir = TempDir::new().unwrap();
    let ontology = make_ontology_with_entities(OntologyMode::Open, &["Person", "Organization"]);
    ontology_sidecar::write_sidecar(dir.path(), Some(&ontology)).unwrap();
    let (drifted, summary) = ontology_sidecar::compute_drift(Some(dir.path()), None, false);
    assert!(
        drifted,
        "drift must be true: sidecar=has ontology, current=none"
    );
    let s = summary.unwrap();
    assert!(
        s.contains("ontology removed"),
        "summary must mention 'ontology removed': {s}"
    );
}

// FR-002/User Story 3: no sidecar + DB has prior data + ontology loaded → drift (pre-upgrade workspace).
#[test]
fn no_sidecar_with_prior_data_and_ontology_reports_drift() {
    let dir = TempDir::new().unwrap();
    let ontology = make_ontology_with_entities(OntologyMode::Open, &["Person", "Organization"]);
    let (drifted, summary) =
        ontology_sidecar::compute_drift(Some(dir.path()), Some(&ontology), true);
    assert!(
        drifted,
        "drift must be true for pre-upgrade workspace with ontology now loaded"
    );
    let s = summary.unwrap();
    assert!(
        s.contains("ontology added"),
        "summary must mention 'ontology added': {s}"
    );
}

// Regression: knowledge_status must surface drift even when ontology is None (removed-ontology
// scenario — User Story 1, acceptance scenario 3). The handler previously hardcoded
// "drifted: false" in the None branch, ignoring the drift state from AppState.
#[tokio::test]
async fn knowledge_status_surfaces_drift_when_ontology_is_none() {
    let (db, _dir) = make_db();
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
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
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState {
            drifted: true,
            drift_summary: Some("entity types removed: [Person]".to_string()),
        })),
    });

    let resp = handlers::dispatch(req(1, "knowledge_status", json!({})), state, None).await;
    let resp_val = serde_json::to_value(resp).unwrap();
    let result = &resp_val["result"];

    assert_eq!(result["ontology"]["loaded"], json!(false));
    assert_eq!(
        result["ontology"]["drifted"],
        json!(true),
        "drift must be surfaced even when no ontology is loaded"
    );
    assert_eq!(
        result["ontology"]["drift_summary"],
        json!("entity types removed: [Person]")
    );
}
