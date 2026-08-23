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
    extractor::{ConfigurableExtractor, Extractor, MockExtractor},
    handlers,
    ipc::IpcRequest,
    ontology::{content_hash, load_ontology, EntityTypeDef, Ontology, OntologyMode},
    ontology_sidecar,
    telemetry::{NoopSink, TelemetrySink},
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult, SourceType},
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
        ontology: ontology.map(Arc::new),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
    })
}

/// Like `make_state`, but with a `workspace_root` configured (issue #451) — needed for any test
/// that exercises `.lcg/ontology-hash.json` / `.lcg/ontology-hash/<group>.json` on disk, or
/// per-group drift via `AppState::resolve_ontology`.
fn make_state_with_root(
    db: Arc<Db>,
    workspace_root: &std::path::Path,
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
        wal_root: None,
        wal_max_events_per_file: 10_000,
        wal_max_bytes_per_file: 5 * 1024 * 1024,
        embedding_model: "bge-base-en-v1.5".to_string(),
        wal_writers: Arc::new(Mutex::new(HashMap::new())),
        active_writes: Arc::new(AtomicUsize::new(0)),
        rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        workspace_root: Some(workspace_root.to_path_buf()),
        indices_built: Arc::new(AtomicBool::new(false)),
        cancel_token: CancellationToken::new(),
        cancelled_chunks: Arc::new(AtomicUsize::new(0)),
        ontology: ontology.map(Arc::new),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
    })
}

fn make_state_with_extractor(
    db: Arc<Db>,
    ontology: Option<Ontology>,
    extractor: Arc<dyn Extractor>,
) -> Arc<AppState> {
    let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
    Arc::new(AppState {
        db: ArcSwapOption::from(Some(db)),
        degraded_reason: Arc::new(Mutex::new(None)),
        embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
        extractor,
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
        ontology: ontology.map(Arc::new),
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
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

// SC-001 (issue #312 supersedes the old drop expectation): Strict-mode with {Person}
// reclassifies Acme Corp (Organization not in vocabulary) to Unclassified rather than dropping
// it. See `strict_mode_entity_type_reclassifies_not_drops` below for the full disposition
// assertions (labels, attributes).
#[tokio::test]
async fn strict_mode_entity_filtering_reclassifies_out_of_vocab() {
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
        result.nodes_extracted, 2,
        "strict mode with {{Person}}: expected 2 entities (Alice + reclassified Acme Corp), got {}",
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

// SC-003(a)/FR-004/SC-005: Strict-mode ontology with {AUTHORED} declared — an edge with
// WORKS_AT (outside the vocabulary and not a declared alias) is retained and reclassified to
// UNCLASSIFIED rather than dropped; the original relation_type is preserved in `attributes`.
// MockExtractor returns Alice --WORKS_AT--> Acme Corp.
#[tokio::test]
async fn strict_mode_relation_type_reclassifies_non_matching_edges_to_unclassified() {
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
    let state = make_state(db.clone(), Some(ontology));

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
        result.edges_extracted, 1,
        "strict mode: WORKS_AT edge must be retained (reclassified), not dropped, when ontology only declares AUTHORED; got {} edges",
        result.edges_extracted
    );
    assert_eq!(
        result.edges_reclassified_unclassified, 1,
        "the out-of-vocabulary edge must be counted in the per-run tally (FR-005)"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'Alice → Acme Corp' \
             RETURN n.relation_type, n.attributes",
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one edge must be stored");
    assert_eq!(
        rows[0][0], "UNCLASSIFIED",
        "relation_type must be closed to UNCLASSIFIED, not the raw out-of-vocab label"
    );
    let attrs = rows[0][1].as_str();
    assert!(
        attrs.contains("WORKS_AT"),
        "original relation type must be recoverable from attributes (SC-005): {attrs}"
    );
}

// User Story 1/SC-001: an edge whose relation_type is a declared alias (LAUNCHED_BY → LAUNCHED)
// is retained under the canonical name, not dropped.
#[tokio::test]
async fn strict_mode_alias_edge_retained_under_canonical_name() {
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
                name: "Vehicle".to_string(),
                description: None,
                parent: None,
            },
        ],
        relation_types: vec![RelationTypeDef {
            name: "LAUNCHED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec!["LAUNCHED_BY".to_string()],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities: vec![
                ExtractedEntity {
                    name: "Rocket".to_string(),
                    entity_type: "Vehicle".to_string(),
                    summary: "A rocket".to_string(),
                    original_entity_type: None,
                },
                ExtractedEntity {
                    name: "Alice".to_string(),
                    entity_type: "Person".to_string(),
                    summary: "A person".to_string(),
                    original_entity_type: None,
                },
            ],
            edges: vec![ExtractedEdge {
                source_name: "Rocket".to_string(),
                target_name: "Alice".to_string(),
                fact: "Rocket launched by Alice".to_string(),
                relation_type: Some("LAUNCHED_BY".to_string()),
                valid_at: None,
                invalid_at: None,
                original_relation_type: None,
            }],
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-alias",
        "Rocket launched by Alice",
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
        result.edges_extracted, 1,
        "declared-alias edge must be retained, not dropped"
    );
    assert_eq!(
        result.edges_reclassified_unclassified, 0,
        "a declared alias must not count as an out-of-vocabulary reclassification"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'Rocket → Alice' RETURN n.relation_type",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0], "LAUNCHED",
        "LAUNCHED_BY must be normalised to its canonical relation type LAUNCHED"
    );
}

// Edge Case: a case/separator variant of a declared alias (`launched_by`) is still recognized,
// because `normalize_relation_type` is the single normalisation point consulted before the
// alias-map lookup (defense-in-depth — the real extractor already normalizes upstream, but the
// filter must not assume that).
#[tokio::test]
async fn strict_mode_alias_case_variant_recognized() {
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
                name: "Vehicle".to_string(),
                description: None,
                parent: None,
            },
        ],
        relation_types: vec![RelationTypeDef {
            name: "LAUNCHED".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec!["LAUNCHED_BY".to_string()],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities: vec![
                ExtractedEntity {
                    name: "Rocket".to_string(),
                    entity_type: "Vehicle".to_string(),
                    summary: "A rocket".to_string(),
                    original_entity_type: None,
                },
                ExtractedEntity {
                    name: "Alice".to_string(),
                    entity_type: "Person".to_string(),
                    summary: "A person".to_string(),
                    original_entity_type: None,
                },
            ],
            edges: vec![ExtractedEdge {
                source_name: "Rocket".to_string(),
                target_name: "Alice".to_string(),
                fact: "Rocket launched by Alice".to_string(),
                relation_type: Some("launched by".to_string()),
                valid_at: None,
                invalid_at: None,
                original_relation_type: None,
            }],
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-alias-case",
        "Rocket launched by Alice",
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
        result.edges_extracted, 1,
        "a case/separator variant of a declared alias must still be retained"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'Rocket → Alice' RETURN n.relation_type",
        )
        .unwrap();
    assert_eq!(
        rows[0][0], "LAUNCHED",
        "'launched by' must normalise to LAUNCHED_BY and resolve via the alias map to LAUNCHED"
    );
}

// Edge Case: an ontology with relation types but no declared aliases behaves unchanged for
// edges whose relation_type already matches a canonical name exactly.
#[tokio::test]
async fn strict_mode_no_aliases_declared_canonical_edge_unchanged() {
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
            name: "WORKS_AT".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let state = make_state(db.clone(), Some(ontology));

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-no-alias",
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

    assert_eq!(result.edges_extracted, 1);
    assert_eq!(
        result.edges_reclassified_unclassified, 0,
        "a canonical-name match with no aliases declared must behave exactly as before"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'Alice → Acme Corp' RETURN n.relation_type",
        )
        .unwrap();
    assert_eq!(rows[0][0], "WORKS_AT");
}

// SC-003/FR-005: a run with N out-of-vocabulary edges surfaces edges_reclassified_unclassified
// == N via AddEpisodeResult, not merely per-edge log lines.
#[tokio::test]
async fn strict_mode_reclassify_count_reflects_n_out_of_vocab_edges() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "KNOWS".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let entities = vec![
        ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Carol".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Dave".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
    ];
    let edges = vec![
        ExtractedEdge {
            source_name: "Alice".to_string(),
            target_name: "Bob".to_string(),
            fact: "Alice also known as Bob".to_string(),
            relation_type: Some("ALSO_KNOWN_AS".to_string()),
            valid_at: None,
            invalid_at: None,
            original_relation_type: None,
        },
        ExtractedEdge {
            source_name: "Bob".to_string(),
            target_name: "Carol".to_string(),
            fact: "Bob mentored Carol".to_string(),
            relation_type: Some("MENTORED".to_string()),
            valid_at: None,
            invalid_at: None,
            original_relation_type: None,
        },
        ExtractedEdge {
            source_name: "Carol".to_string(),
            target_name: "Dave".to_string(),
            fact: "Carol knows Dave".to_string(),
            relation_type: Some("KNOWS".to_string()),
            valid_at: None,
            invalid_at: None,
            original_relation_type: None,
        },
    ];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges,
        }]));
    let state = make_state_with_extractor(db, Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-count",
        "irrelevant body — extraction is mocked",
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
        result.edges_extracted, 3,
        "all 3 edges must be retained (reclassify-not-drop)"
    );
    assert_eq!(
        result.edges_reclassified_unclassified, 2,
        "exactly the 2 out-of-vocabulary edges (ALSO_KNOWN_AS, MENTORED) must be counted; KNOWS is in-vocabulary"
    );
}

// PR #311 review finding: an edge whose incoming relation_type is None/empty has
// `original_relation_type` deliberately left as None (nothing to preserve) even though
// `relation_type` is set to UNCLASSIFIED. The tally must not use
// `original_relation_type.is_some()` as a proxy for "was reclassified" — it must count off
// `relation_type == UNCLASSIFIED` directly, so an edge with no original label to preserve is
// still counted (FR-005 must reflect what's actually persisted).
#[tokio::test]
async fn strict_mode_reclassify_count_includes_edges_with_no_original_relation_type() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "KNOWS".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let entities = vec![
        ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
    ];
    let edges = vec![ExtractedEdge {
        source_name: "Alice".to_string(),
        target_name: "Bob".to_string(),
        fact: "Alice and Bob are connected somehow".to_string(),
        relation_type: None,
        valid_at: None,
        invalid_at: None,
        original_relation_type: None,
    }];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges,
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-empty-rt",
        "irrelevant body — extraction is mocked",
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
        result.edges_extracted, 1,
        "edge with no relation_type must be retained (reclassified), not dropped"
    );
    assert_eq!(
        result.edges_reclassified_unclassified, 1,
        "edge reclassified to UNCLASSIFIED must be counted even though it has no \
         original_relation_type to preserve"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'Alice → Bob' \
             RETURN n.relation_type, n.attributes",
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one edge must be stored");
    assert_eq!(rows[0][0], "UNCLASSIFIED");
    assert_eq!(
        rows[0][1], "{}",
        "no original relation type to preserve, attributes must remain the empty-object default"
    );
}

// PR #311 review finding: an edge reclassified to UNCLASSIFIED by the pre-lock strict-mode pass
// must NOT be counted in edges_reclassified_unclassified if it's subsequently dropped as
// self-referential — the tally must reflect what's actually persisted (mirroring how
// edges_dropped_unresolvable is only ever counted authoritatively at Phase C, per ADR-0051).
// This out-of-vocabulary, self-referential edge must be dropped and must not inflate the count.
#[tokio::test]
async fn strict_mode_reclassified_self_referential_edge_not_counted() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "KNOWS".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Person".to_string(),
                summary: "A person".to_string(),
                original_entity_type: None,
            }],
            edges: vec![ExtractedEdge {
                source_name: "Alice".to_string(),
                target_name: "Alice".to_string(),
                fact: "Alice also known as Alice".to_string(),
                relation_type: Some("ALSO_KNOWN_AS".to_string()),
                valid_at: None,
                invalid_at: None,
                original_relation_type: None,
            }],
        }]));
    let state = make_state_with_extractor(db, Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-reclassify-self-ref",
        "irrelevant body — extraction is mocked",
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
        result.edges_extracted, 0,
        "the self-referential edge must still be dropped, even though it was also reclassified"
    );
    assert_eq!(
        result.edges_reclassified_unclassified, 0,
        "a reclassified edge that's later dropped as self-referential must not inflate the tally"
    );
}

// PR #311 review finding: edges_reclassified_unclassified must only ever be incremented for
// edges that actually passed through the strict-mode reclassify filter. Under `open` mode that
// filter never runs at all, so even if the extractor happens to emit the literal string
// "UNCLASSIFIED" as relation_type (an LLM output collision, not an ontology decision), the tally
// must stay at zero — otherwise the count would falsely suggest strict-mode reclassification
// occurred for a workspace where strict mode isn't even active.
#[tokio::test]
async fn open_mode_literal_unclassified_relation_type_not_counted_as_reclassified() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Person".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![RelationTypeDef {
            name: "KNOWS".to_string(),
            description: None,
            source_type: None,
            target_type: None,
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let entities = vec![
        ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
    ];
    let edges = vec![ExtractedEdge {
        source_name: "Alice".to_string(),
        target_name: "Bob".to_string(),
        fact: "Alice and Bob are connected somehow".to_string(),
        relation_type: Some("UNCLASSIFIED".to_string()),
        valid_at: None,
        invalid_at: None,
        original_relation_type: None,
    }];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges,
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-open-literal-unclassified",
        "irrelevant body — extraction is mocked",
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
        result.edges_extracted, 1,
        "open mode: the edge must be retained regardless of its relation_type"
    );
    assert_eq!(
        result.edges_reclassified_unclassified, 0,
        "open mode never runs the strict-mode reclassify filter, so this must stay zero even \
         though the edge's relation_type happens to be the literal string UNCLASSIFIED"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query(
            "MATCH (n:RelatesToNode_) WHERE n.name = 'Alice → Bob' \
             RETURN n.relation_type, n.attributes",
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one edge must be stored");
    assert_eq!(
        rows[0][0], "UNCLASSIFIED",
        "persisted relation_type must reflect the extractor's literal output, not a rewrite \
         by the (never-run, in open mode) strict-mode filter"
    );
    let attrs = rows[0][1].as_str();
    assert_eq!(
        attrs, "{}",
        "no strict-mode reclassification occurred, so attributes must remain the empty-object \
         default, not carry a spurious original_relation_type"
    );
}

// FR-001/FR-002/FR-003 (issue #312): the entity-side strict-mode filter reclassifies
// out-of-vocabulary entities to `Unclassified` rather than dropping them, preserving the
// original type in `attributes.original_entity_type` — mirroring ADR-0310's edge-side
// treatment. Supersedes the old `strict_mode_entity_type_still_drops_not_reclassifies`, which
// locked in the drop behavior this issue changes.
#[tokio::test]
async fn strict_mode_entity_type_reclassifies_not_drops() {
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
    let state = make_state(db.clone(), Some(ontology));

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-entity-reclassify",
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
        "Acme Corp (Organization, out of vocabulary) must be retained, not dropped"
    );
    assert_eq!(
        result.entities_reclassified_unclassified, 1,
        "exactly Acme Corp must be counted as reclassified"
    );

    let conn = db.connect().unwrap();
    let acme = conn
        .get_entity_by_name_ci("Acme Corp", "grp")
        .unwrap()
        .expect("Acme Corp must be persisted");
    assert!(
        acme.labels.contains(&"Unclassified".to_string()),
        "Acme Corp's labels must include Unclassified, not Organization: {:?}",
        acme.labels
    );
    assert!(
        !acme.labels.contains(&"Organization".to_string()),
        "the raw out-of-vocabulary type must not leak into labels: {:?}",
        acme.labels
    );
    let attrs: Value = serde_json::from_str(&acme.attributes).unwrap();
    assert_eq!(
        attrs["original_entity_type"], "Organization",
        "original type must be recoverable from attributes: {}",
        acme.attributes
    );

    let alice = conn
        .get_entity_by_name_ci("Alice", "grp")
        .unwrap()
        .expect("Alice must be persisted");
    assert!(
        !alice.labels.contains(&"Unclassified".to_string()),
        "an in-vocabulary entity must not be reclassified: {:?}",
        alice.labels
    );
}

// User Story 1 (issue #312): the extractor returning the same out-of-vocabulary type across
// multiple entities in one run — every one of them is retained under Unclassified, none dropped.
#[tokio::test]
async fn strict_mode_reclassify_count_reflects_n_out_of_vocab_entities() {
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
    let entities = vec![
        ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Rocket".to_string(),
            entity_type: "Spacecraft".to_string(),
            summary: "A rocket".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Capsule".to_string(),
            entity_type: "Spacecraft".to_string(),
            summary: "A capsule".to_string(),
            original_entity_type: None,
        },
    ];
    let extractor: Arc<dyn Extractor> = Arc::new(ConfigurableExtractor::new(vec![
        ExtractionResult {
            entities,
            edges: vec![],
        },
        ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Bob".to_string(),
                entity_type: "Person".to_string(),
                summary: "A person".to_string(),
                original_entity_type: None,
            }],
            edges: vec![],
        },
    ]));
    let state = make_state_with_extractor(db, Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-count",
        "irrelevant body — extraction is mocked",
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
        result.nodes_extracted, 3,
        "all 3 entities must be retained (reclassify-not-drop)"
    );
    assert_eq!(
        result.entities_reclassified_unclassified, 2,
        "exactly the 2 out-of-vocabulary entities (Rocket, Capsule) must be counted"
    );

    let result2 = episode::add_episode(
        Arc::clone(&state),
        "test-ep-count-zero",
        "irrelevant body — extraction is mocked",
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
        result2.entities_reclassified_unclassified, 0,
        "a run with zero out-of-vocabulary entities must report 0, distinguishable from absent"
    );
}

// User Story 2 (issue #312): an edge referencing an out-of-vocabulary-typed entity is inserted
// with both endpoints resolved, since the entity now survives reclassification instead of being
// dropped — and the edge is not counted in edges_dropped_unresolvable.
#[tokio::test]
async fn strict_mode_edge_survives_when_endpoint_entity_is_reclassified() {
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
    let entities = vec![
        ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Rocket".to_string(),
            entity_type: "Spacecraft".to_string(),
            summary: "A rocket".to_string(),
            original_entity_type: None,
        },
    ];
    let edges = vec![ExtractedEdge {
        source_name: "Alice".to_string(),
        target_name: "Rocket".to_string(),
        fact: "Alice piloted Rocket".to_string(),
        relation_type: None,
        valid_at: None,
        invalid_at: None,
        original_relation_type: None,
    }];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges,
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-edge-survives",
        "irrelevant body — extraction is mocked",
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
        result.edges_extracted, 1,
        "the edge must be inserted; its out-of-vocabulary-typed endpoint is not treated as missing"
    );
    assert_eq!(
        result.edges_dropped_unresolvable, 0,
        "the reclassified entity is a resolvable endpoint, not an unresolvable one"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query("MATCH (n:RelatesToNode_) WHERE n.name = 'Alice → Rocket' RETURN n.name")
        .unwrap();
    assert_eq!(rows.len(), 1, "the edge must resolve to both endpoints");
}

// FR-007 (issue #312): an entity with an empty/absent type must continue to resolve as a plain,
// untyped Entity under strict — not routed through the Unclassified reclassification path.
#[tokio::test]
async fn strict_mode_empty_entity_type_resolves_as_plain_entity() {
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
    let entities = vec![
        ExtractedEntity {
            name: "Mystery".to_string(),
            entity_type: "".to_string(),
            summary: "An untyped thing".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "AlsoMystery".to_string(),
            entity_type: "Entity".to_string(),
            summary: "Another untyped thing".to_string(),
            original_entity_type: None,
        },
    ];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges: vec![],
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-empty-type",
        "irrelevant body — extraction is mocked",
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
    assert_eq!(
        result.entities_reclassified_unclassified, 0,
        "empty/'Entity' type must not be reclassified"
    );

    let conn = db.connect().unwrap();
    for name in ["Mystery", "AlsoMystery"] {
        let row = conn
            .get_entity_by_name_ci(name, "grp")
            .unwrap()
            .unwrap_or_else(|| panic!("{name} must be persisted"));
        assert_eq!(
            row.labels,
            vec!["Entity".to_string()],
            "{name} must carry only the base Entity label, no Unclassified: {:?}",
            row.labels
        );
        assert_eq!(
            row.attributes, "{}",
            "{name} must not carry a spurious original_entity_type: {}",
            row.attributes
        );
    }
}

// FR-008/SC-006 (issue #312): a reclassified entity must dedup correctly against a
// pre-existing entity of the same subject already stored under its correct declared type — no
// second, permanently-separate copy is created.
#[tokio::test]
async fn strict_mode_reclassified_entity_dedups_against_declared_type() {
    let (db, _dir) = make_db();
    let ontology = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![EntityTypeDef {
            name: "Spacecraft".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    let extractor: Arc<dyn Extractor> = Arc::new(ConfigurableExtractor::new(vec![
        ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Rocket".to_string(),
                entity_type: "Spacecraft".to_string(),
                summary: "A rocket".to_string(),
                original_entity_type: None,
            }],
            edges: vec![],
        },
        ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Rocket".to_string(),
                entity_type: "Vehicle".to_string(),
                summary: "Also a rocket".to_string(),
                original_entity_type: None,
            }],
            edges: vec![],
        },
    ]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    episode::add_episode(
        Arc::clone(&state),
        "test-ep-dedup-1",
        "irrelevant body — extraction is mocked",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "grp",
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    let result2 = episode::add_episode(
        Arc::clone(&state),
        "test-ep-dedup-2",
        "irrelevant body — extraction is mocked",
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
        result2.nodes_extracted, 1,
        "the second extraction must dedup-merge against the existing Rocket, not create a new node"
    );
    assert_eq!(
        result2.entities_reclassified_unclassified, 1,
        "the second extraction's out-of-vocabulary type still counts toward the tally even though it merges"
    );

    let conn = db.connect().unwrap();
    let rows = conn
        .cypher_query("MATCH (n:Entity) WHERE n.name = 'Rocket' RETURN n.uuid")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "there must be exactly one Rocket entity, not a second permanently-separate copy"
    );
}

// Review finding (issue #312): an out-of-vocabulary entity with an empty/whitespace-only name
// is dropped by the empty-name filter and must never be counted toward
// entities_reclassified_unclassified — counting it and then dropping it later would desync the
// tally from what's actually persisted.
#[tokio::test]
async fn strict_mode_empty_name_out_of_vocab_entity_not_counted_in_tally() {
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
    let entities = vec![
        ExtractedEntity {
            name: "   ".to_string(),
            entity_type: "Spacecraft".to_string(),
            summary: "an unnamed rocket".to_string(),
            original_entity_type: None,
        },
        ExtractedEntity {
            name: "Alice".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
            original_entity_type: None,
        },
    ];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges: vec![],
        }]));
    let state = make_state_with_extractor(db, Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-empty-name-oov",
        "irrelevant body — extraction is mocked",
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
        "only Alice must be persisted; the empty-named entity is dropped"
    );
    assert_eq!(
        result.entities_reclassified_unclassified, 0,
        "the empty-named out-of-vocabulary entity must never be counted — it never reaches storage"
    );
}

// Review finding (issue #312): a case/separator variant of "Entity" (e.g. lowercase "entity")
// normalizes to "Entity" and must not be reclassified — but `entity_type` must also be rewritten
// to its normalized form on this passthrough path, otherwise the raw out-of-vocabulary-looking
// string leaks into `EntityRow.labels` via `make_insert_row`'s raw-string (non-normalized) check.
#[tokio::test]
async fn strict_mode_entity_type_case_variant_of_entity_not_leaked_into_labels() {
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
    let entities = vec![ExtractedEntity {
        name: "Mystery".to_string(),
        entity_type: "entity".to_string(),
        summary: "lowercase entity type variant".to_string(),
        original_entity_type: None,
    }];
    let extractor: Arc<dyn Extractor> =
        Arc::new(ConfigurableExtractor::new(vec![ExtractionResult {
            entities,
            edges: vec![],
        }]));
    let state = make_state_with_extractor(db.clone(), Some(ontology), extractor);

    let result = episode::add_episode(
        Arc::clone(&state),
        "test-ep-entity-case-variant",
        "irrelevant body — extraction is mocked",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "grp",
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    assert_eq!(result.nodes_extracted, 1);
    assert_eq!(
        result.entities_reclassified_unclassified, 0,
        "a case variant of 'Entity' is not a reclassification"
    );

    let conn = db.connect().unwrap();
    let mystery = conn
        .get_entity_by_name_ci("Mystery", "grp")
        .unwrap()
        .expect("Mystery must be persisted");
    assert_eq!(
        mystery.labels,
        vec!["Entity".to_string()],
        "the raw 'entity' string must not leak into labels: {:?}",
        mystery.labels
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
        ontology_drift: Arc::new(Mutex::new(OntologyDriftState {
            drifted: true,
            drift_summary: Some("entity types removed: [Person]".to_string()),
        })),
        group_ontologies: Arc::new(Mutex::new(HashMap::new())),
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

// ── issue #451, FR-004/SC-003: single-ontology workspaces are unaffected ────────────────────

// FR-004: `.lcg/ontology-hash.json`'s content/shape must stay byte-identical for an existing
// single-ontology workspace, even once a group's per-group drift is also computed and stored
// (additively, under a separate `.lcg/ontology-hash/` directory) alongside it.
#[tokio::test]
async fn workspace_ontology_hash_json_unchanged_when_group_drift_is_also_tracked() {
    let dir = TempDir::new().unwrap();
    write_ontology_file(
        &dir,
        "mode: strict\nentity_types:\n  - name: Person\n  - name: Organization\n",
    );
    let ontology = load_ontology(Some(dir.path()));
    let (db, _db_dir) = make_db();
    let state = make_state_with_root(db, dir.path(), ontology.clone());

    episode::add_episode(
        Arc::clone(&state),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "g1",
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    let workspace_sidecar = ontology_sidecar::read_sidecar(dir.path())
        .expect(".lcg/ontology-hash.json must exist, exactly as before issue #451");
    assert_eq!(workspace_sidecar.hash, content_hash(ontology.as_ref()));
    assert_eq!(workspace_sidecar.mode.as_deref(), Some("strict"));
    assert_eq!(
        workspace_sidecar.entity_types,
        vec!["Person".to_string(), "Organization".to_string()]
    );
    assert!(workspace_sidecar.relation_types.is_empty());

    // The raw JSON on disk has exactly the same shape (same keys) as before this issue — no
    // group_id, no map, no new top-level field folded into the single-valued file.
    let raw = std::fs::read_to_string(ontology_sidecar::sidecar_path(dir.path())).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["entity_types", "hash", "mode", "relation_types"]);

    // Additive only: per-group tracking lives in a separate directory, never inside the
    // workspace file above.
    assert!(
        ontology_sidecar::group_sidecar_path(dir.path(), "g1")
            .unwrap()
            .exists(),
        "issue #451's per-group sidecar must exist alongside the unchanged workspace file"
    );
}

// SC-003: a workspace with no `.lcg/ontology/` directory (no group has ever had a per-group
// file) reports an empty `group_ontology_drift` in knowledge_status until a group is actually
// used in this process, then reports exactly that group.
#[tokio::test]
async fn knowledge_status_group_ontology_drift_empty_until_a_group_is_used() {
    let dir = TempDir::new().unwrap();
    write_ontology_file(&dir, "mode: open\nentity_types:\n  - name: Person\n");
    let ontology = load_ontology(Some(dir.path()));
    let (db, _db_dir) = make_db();
    let state = make_state_with_root(db, dir.path(), ontology);

    let resp = handlers::dispatch(
        req(1, "knowledge_status", json!({})),
        Arc::clone(&state),
        None,
    )
    .await;
    let resp_val = serde_json::to_value(resp).unwrap();
    assert_eq!(
        resp_val["result"]["group_ontology_drift"],
        json!([]),
        "no group has been resolved in this process yet — must be empty, not a false negative \
         for any on-disk group"
    );

    episode::add_episode(
        Arc::clone(&state),
        "ep",
        "Alice works at Acme Corp",
        "test",
        "test source",
        "2026-01-01T00:00:00Z",
        "g1",
        SourceType::Text,
        None,
    )
    .await
    .unwrap();

    let resp2 = handlers::dispatch(req(2, "knowledge_status", json!({})), state, None).await;
    let resp2_val = serde_json::to_value(resp2).unwrap();
    let breakdown = resp2_val["result"]["group_ontology_drift"]
        .as_array()
        .expect("group_ontology_drift must be an array");
    assert_eq!(breakdown.len(), 1, "only g1 has been used: {breakdown:?}");
    assert_eq!(breakdown[0]["group_id"], json!("g1"));
    assert_eq!(
        breakdown[0]["drifted"],
        json!(false),
        "g1 was just freshly ingested under the current ontology — no drift"
    );
}

// User Story 4, Scenario 1: knowledge_status's group_ontology_drift array must distinguish a
// drifted group from a non-drifted sibling in the actual JSON response, including drift_summary
// — not just via the internal AppState::group_drift_status accessor other tests exercise.
#[tokio::test]
async fn knowledge_status_group_ontology_drift_distinguishes_drifted_and_clean_groups() {
    let dir = TempDir::new().unwrap();
    write_ontology_file(&dir, "mode: open\nentity_types:\n  - name: Person\n");
    let ontology = load_ontology(Some(dir.path()));
    let (db, _db_dir) = make_db();

    // group-a's recorded sidecar reflects a stale ontology, so its first resolution drifts.
    let stale = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "StaleType".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    ontology_sidecar::write_group_sidecar(dir.path(), "group-a", Some(&stale)).unwrap();

    let state = make_state_with_root(db, dir.path(), ontology);
    state.resolve_ontology("group-a");
    state.resolve_ontology("group-b");

    let resp = handlers::dispatch(
        req(1, "knowledge_status", json!({})),
        Arc::clone(&state),
        None,
    )
    .await;
    let resp_val = serde_json::to_value(resp).unwrap();
    let breakdown = resp_val["result"]["group_ontology_drift"]
        .as_array()
        .expect("group_ontology_drift must be an array");
    assert_eq!(breakdown.len(), 2, "{breakdown:?}");

    let a = breakdown
        .iter()
        .find(|e| e["group_id"] == json!("group-a"))
        .expect("group-a entry present");
    assert_eq!(
        a["drifted"],
        json!(true),
        "group-a's sidecar was stale: {a:?}"
    );
    assert!(
        a["drift_summary"].as_str().is_some_and(|s| !s.is_empty()),
        "a drifted group's summary must be a non-empty string: {a:?}"
    );

    let b = breakdown
        .iter()
        .find(|e| e["group_id"] == json!("group-b"))
        .expect("group-b entry present");
    assert_eq!(
        b["drifted"],
        json!(false),
        "group-b has no prior sidecar or data — first use, not drift: {b:?}"
    );
    assert_eq!(b["drift_summary"], json!(null));
}
