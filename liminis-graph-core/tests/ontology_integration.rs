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
use liminis_graph_core::{
    app_state::AppState,
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    episode,
    extractor::MockExtractor,
    handlers,
    ipc::IpcRequest,
    ontology::{load_ontology, EntityTypeDef, Ontology, OntologyMode},
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
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writer: Arc::new(Mutex::new(None)),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: None,
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: ontology.map(Arc::new),
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
        }],
        relation_types: vec![],
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
        }],
        relation_types: vec![],
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
    assert_eq!(result["ontology"]["entity_type_count"], json!(0));
    assert_eq!(result["ontology"]["relation_type_count"], json!(0));
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
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: None,
            },
        ],
        relation_types: vec![],
    };
    let state = make_state(db, Some(ontology));

    let resp = handlers::dispatch(req(1, "knowledge_status", json!({})), state, None).await;
    let resp_val = serde_json::to_value(resp).unwrap();
    let result = &resp_val["result"];

    assert_eq!(result["ontology"]["present"], json!(true));
    assert_eq!(result["ontology"]["mode"], json!("strict"));
    assert_eq!(result["ontology"]["entity_type_count"], json!(2));
    assert_eq!(result["ontology"]["relation_type_count"], json!(0));
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

use liminis_graph_core::ontology::RelationTypeDef;

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
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: None,
            },
        ],
        relation_types: vec![RelationTypeDef {
            name: "AUTHORED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
        }],
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
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: None,
            },
        ],
        relation_types: vec![RelationTypeDef {
            name: "AUTHORED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
        }],
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
