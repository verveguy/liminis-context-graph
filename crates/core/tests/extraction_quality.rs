// Extraction quality tests (FR-009, issue #92).
//
// Covers the four verification targets from the spec:
//   SC-004: ontology entity types appear in the system prompt sent to the LLM
//   SC-005: without an ontology the system prompt has no <ENTITY_TYPES> section
//   SC-006: SourceType::Json and SourceType::Text produce distinct system prompts
//   SC-003: self-referential edges and edges with unresolvable endpoints are dropped

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use lcg_core::{
    app_state::{AppState, OntologyDriftState},
    db::Db,
    dedup_adapter::PassthroughDedupAdapter,
    embedder::MockEmbedder,
    episode,
    error::Error,
    extractor::{ExtractOptions, MockExtractor},
    ontology::{EntityTypeDef, Ontology, OntologyMode},
    prompts,
    telemetry::{NoopSink, TelemetrySink},
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult, SourceType},
    Extractor,
};
use tempfile::TempDir;

const EMB_DIM: usize = 4;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_db() -> (Arc<Db>, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Db::open(dir.path().join("eq_test.db").to_str().unwrap()).unwrap());
    {
        let conn = db.connect().unwrap();
        conn.init_schema(EMB_DIM).unwrap();
    }
    (db, dir)
}

fn make_state(
    db: Arc<Db>,
    extractor: Arc<dyn Extractor>,
    ontology: Option<Ontology>,
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

fn strict_person_ontology() -> Ontology {
    Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![
            EntityTypeDef {
                name: "Person".to_string(),
                description: Some("A human individual".to_string()),
                parent: None,
            },
            EntityTypeDef {
                name: "Organization".to_string(),
                description: Some("A company or institution".to_string()),
                parent: None,
            },
            EntityTypeDef {
                name: "Paper".to_string(),
                description: None,
                parent: None,
            },
        ],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    }
}

// ── SC-004/SC-005: Prompt content tests ───────────────────────────────────────

// SC-004: With an ontology loaded, entity_system_prompt contains <ENTITY_TYPES>
// with the workspace types and the strict-mode instruction sentence.
#[test]
fn ontology_entity_types_injected_into_system_prompt() {
    let onto = strict_person_ontology();
    let prompt = prompts::entity_system_prompt(SourceType::Text, Some(&onto));

    assert!(
        prompt.contains("<ENTITY_TYPES>"),
        "system prompt must contain <ENTITY_TYPES> opening tag when ontology is set"
    );
    assert!(
        prompt.contains("Person"),
        "system prompt must list Person from the ontology"
    );
    assert!(
        prompt.contains("Organization"),
        "system prompt must list Organization from the ontology"
    );
    assert!(
        prompt.contains("Paper"),
        "system prompt must list Paper from the ontology"
    );
    assert!(
        prompt.contains("Only extract entities whose type is exactly one of the listed types"),
        "strict-mode instruction sentence must appear in system prompt"
    );
    assert!(
        prompt.contains("</ENTITY_TYPES>"),
        "system prompt must contain </ENTITY_TYPES> closing tag"
    );
    // The hardcoded default types must NOT appear in strict-mode ontology output.
    assert!(
        !prompt.contains("Person: An individual human being"),
        "default type list must not appear when workspace ontology is present"
    );
}

// SC-004 (open mode): Open-mode ontology uses the right instruction sentence.
#[test]
fn open_mode_ontology_uses_prefer_sentence() {
    let onto = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![EntityTypeDef {
            name: "Concept".to_string(),
            description: None,
            parent: None,
        }],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    let prompt = prompts::entity_system_prompt(SourceType::Text, Some(&onto));

    assert!(
        prompt.contains("Prefer the listed entity types when they apply"),
        "open-mode instruction sentence must appear in system prompt"
    );
    assert!(
        !prompt.contains("Only extract entities whose type is exactly one of the listed types"),
        "strict-mode sentence must not appear in open-mode prompt"
    );
}

// SC-005: Without an ontology the system prompt has no <ENTITY_TYPES> section.
#[test]
fn no_ontology_produces_no_entity_types_section() {
    let prompt = prompts::entity_system_prompt(SourceType::Text, None);

    assert!(
        !prompt.contains("<ENTITY_TYPES>"),
        "system prompt must not contain <ENTITY_TYPES> section when no ontology is set"
    );
    // The default 16-type list should be present instead.
    assert!(
        prompt.contains("Person: An individual human being"),
        "default entity type list must appear when no ontology is set"
    );
    assert!(
        !prompt.contains("{{ENTITY_TYPES_SECTION}}"),
        "placeholder token must not appear in rendered prompt"
    );
}

// SC-005: Empty ontology behaves as no ontology.
#[test]
fn empty_ontology_produces_no_entity_types_section() {
    let onto = Ontology {
        mode: OntologyMode::Strict,
        entity_types: vec![],
        relation_types: vec![],
        ancestor_map: HashMap::new(),
    };
    let prompt = prompts::entity_system_prompt(SourceType::Text, Some(&onto));

    assert!(
        !prompt.contains("<ENTITY_TYPES>"),
        "empty ontology must not produce an <ENTITY_TYPES> section"
    );
    assert!(
        prompt.contains("Person: An individual human being"),
        "default type list must appear for empty ontology"
    );
}

// SC-006: SourceType::Json produces a different system prompt than SourceType::Text.
#[test]
fn source_type_json_differs_from_text_prompt() {
    let text_prompt = prompts::entity_system_prompt(SourceType::Text, None);
    let json_prompt = prompts::entity_system_prompt(SourceType::Json, None);
    let message_prompt = prompts::entity_system_prompt(SourceType::Message, None);

    assert_ne!(
        text_prompt, json_prompt,
        "JSON and text source types must produce distinct system prompts"
    );
    assert_ne!(
        text_prompt, message_prompt,
        "message and text source types must produce distinct system prompts"
    );
    assert_ne!(
        json_prompt, message_prompt,
        "JSON and message source types must produce distinct system prompts"
    );
}

// SC-006 (with ontology): Ontology injection works consistently across source types.
#[test]
fn ontology_injected_into_all_source_type_prompts() {
    let onto = strict_person_ontology();
    for &st in &[SourceType::Text, SourceType::Message, SourceType::Json] {
        let prompt = prompts::entity_system_prompt(st, Some(&onto));
        assert!(
            prompt.contains("<ENTITY_TYPES>"),
            "ontology injection must work for SourceType::{st:?}"
        );
        assert!(
            prompt.contains("Paper"),
            "workspace type 'Paper' must appear for SourceType::{st:?}"
        );
        assert!(
            !prompt.contains("{{ENTITY_TYPES_SECTION}}"),
            "placeholder must not leak into rendered prompt for SourceType::{st:?}"
        );
    }
}

// ── FACT_TYPES injection ──────────────────────────────────────────────────────

#[test]
fn edge_prompt_no_ontology_has_no_fact_types() {
    let prompt = prompts::edge_system_prompt(None);
    assert!(
        !prompt.contains("<FACT_TYPES>"),
        "edge prompt must not contain <FACT_TYPES> section without ontology"
    );
    assert!(
        !prompt.contains("{{FACT_TYPES_SECTION}}"),
        "placeholder must not leak into rendered edge prompt"
    );
}

#[test]
fn edge_prompt_with_relation_types_injects_fact_types() {
    use lcg_core::ontology::RelationTypeDef;
    let onto = Ontology {
        mode: OntologyMode::Open,
        entity_types: vec![],
        relation_types: vec![RelationTypeDef {
            name: "AUTHORED".to_string(),
            description: Some("person authored a paper".to_string()),
            source_type: Some("Person".to_string()),
            target_type: Some("Paper".to_string()),
            aliases: vec![],
            keywords: vec![],
        }],
        ancestor_map: HashMap::new(),
    };
    let prompt = prompts::edge_system_prompt(Some(&onto));
    assert!(
        prompt.contains("<FACT_TYPES>"),
        "edge prompt must contain <FACT_TYPES> section when ontology has relation types"
    );
    assert!(
        prompt.contains("AUTHORED"),
        "edge prompt must list AUTHORED relation type"
    );
    assert!(
        prompt.contains("Person"),
        "edge prompt must include source type signature"
    );
}

// ── SC-003: Edge validation tests ────────────────────────────────────────────

/// Extractor that always returns a self-referential edge.
struct SelfRefExtractor;

impl Extractor for SelfRefExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(async {
            Ok(ExtractionResult {
                entities: vec![ExtractedEntity {
                    name: "Alice".to_string(),
                    entity_type: "Person".to_string(),
                    summary: "Alice is a person".to_string(),
                }],
                edges: vec![ExtractedEdge {
                    source_name: "Alice".to_string(),
                    target_name: "Alice".to_string(), // self-referential
                    fact: "Alice knows Alice".to_string(),
                    relation_type: Some("KNOWS".to_string()),
                    valid_at: None,
                    invalid_at: None,
                }],
            })
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

/// Extractor that returns an edge whose target is not in the entity list.
struct BadEndpointExtractor;

impl Extractor for BadEndpointExtractor {
    fn extract<'a>(
        &'a self,
        _opts: ExtractOptions<'a>,
    ) -> BoxFuture<'a, Result<ExtractionResult, Error>> {
        Box::pin(async {
            Ok(ExtractionResult {
                entities: vec![ExtractedEntity {
                    name: "Alice".to_string(),
                    entity_type: "Person".to_string(),
                    summary: "Alice is a person".to_string(),
                }],
                edges: vec![ExtractedEdge {
                    source_name: "Alice".to_string(),
                    target_name: "Bob".to_string(), // Bob is not in the entity list
                    fact: "Alice knows Bob".to_string(),
                    relation_type: Some("KNOWS".to_string()),
                    valid_at: None,
                    invalid_at: None,
                }],
            })
        })
    }

    fn classify_entities<'a>(
        &'a self,
        entities: &'a [(&'a str, &'a str)],
        _allowed_types: Option<&'a [String]>,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>> {
        let count = entities.len();
        Box::pin(async move { Ok(vec![String::new(); count]) })
    }
}

// SC-003: Self-referential edge (source == target) is dropped post-extraction.
#[tokio::test]
async fn self_referential_edge_is_dropped() {
    let (db, _dir) = make_db();
    let state = make_state(db, Arc::new(SelfRefExtractor), None);

    let result = episode::add_episode(
        state,
        "test-ep",
        "Alice knows herself",
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
        "expected 1 entity (Alice), got {}",
        result.nodes_extracted
    );
    assert_eq!(
        result.edges_extracted, 0,
        "self-referential edge must be dropped; expected 0 edges, got {}",
        result.edges_extracted
    );
}

// SC-003: Edge whose target is not in the episode's entity list is dropped.
#[tokio::test]
async fn edge_with_unresolvable_target_is_dropped() {
    let (db, _dir) = make_db();
    let state = make_state(db, Arc::new(BadEndpointExtractor), None);

    let result = episode::add_episode(
        state,
        "test-ep",
        "Alice knows Bob",
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
        "expected 1 entity (Alice), got {}",
        result.nodes_extracted
    );
    assert_eq!(
        result.edges_extracted, 0,
        "edge with unresolvable endpoint must be dropped; expected 0 edges, got {}",
        result.edges_extracted
    );
}

// ── SC-001/FR-001: relation_type field is populated on every edge ─────────────

fn is_screaming_snake_case(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().unwrap().is_ascii_uppercase()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

// SC-001: After ingestion via MockExtractor, every edge in the DB has a non-null
// relation_type field matching ^[A-Z][A-Z0-9_]*$ (SCREAMING_SNAKE_CASE).
#[tokio::test]
async fn edges_have_screaming_snake_case_relation_type() {
    let (db, _dir) = make_db();
    let state = make_state(db.clone(), Arc::new(MockExtractor), None);

    episode::add_episode(
        state,
        "test-ep-rt",
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

    let conn = db.connect().unwrap();
    let edges = conn.list_relationships(None, 100).unwrap();

    assert!(
        !edges.is_empty(),
        "expected at least one edge after ingestion"
    );
    for edge in &edges {
        let rt = edge.relation_type.as_deref().unwrap_or("");
        assert!(
            is_screaming_snake_case(rt),
            "edge relation_type '{rt}' must match SCREAMING_SNAKE_CASE (^[A-Z][A-Z0-9_]*$)"
        );
    }
}
