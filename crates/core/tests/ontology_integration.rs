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
                },
                ExtractedEntity {
                    name: "Alice".to_string(),
                    entity_type: "Person".to_string(),
                    summary: "A person".to_string(),
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
                },
                ExtractedEntity {
                    name: "Alice".to_string(),
                    entity_type: "Person".to_string(),
                    summary: "A person".to_string(),
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
        },
        ExtractedEntity {
            name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
        },
        ExtractedEntity {
            name: "Carol".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
        },
        ExtractedEntity {
            name: "Dave".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
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
        },
        ExtractedEntity {
            name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
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
        },
        ExtractedEntity {
            name: "Bob".to_string(),
            entity_type: "Person".to_string(),
            summary: "A person".to_string(),
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
}

// FR-006 regression: the entity-side strict-mode filter still hard-drops out-of-vocabulary
// entities rather than reclassifying them — unlike the edge-side filter, this is unchanged by
// issue #310 because EntityTypeDef has no aliases/keywords concept to be alias-blind about (see
// the doc comment at the entity filter in episode.rs).
#[tokio::test]
async fn strict_mode_entity_type_still_drops_not_reclassifies() {
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
        "test-ep-entity-drop",
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
        "Acme Corp (Organization, out of vocabulary) must still be dropped outright, not reclassified"
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
