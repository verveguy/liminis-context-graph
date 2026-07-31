//! MCP write/mutation-path e2e suite over the real-corpus WAL fixture (issue #235).
//!
//! Sibling of `mcp_real_corpus_e2e.rs` (#234, the read-path suite): both seed a workspace from
//! the same golden real-corpus WAL fixture and drive the real compiled binary over
//! MCP-over-stdio against it. This suite instead exercises the mutation surface — deletes,
//! merges/corrections, entity/relation retyping, ingest into the pre-populated graph, and
//! `knowledge_clear_all` — the operations most likely to corrupt a real graph, per the issues
//! that motivated it (#202 dropped/misresolved edges during ingest, #203 index loss under
//! sustained write, #205 relation-type pollution from fact-prefix pseudo-types).
//!
//! **Isolation (FR-002)**: the fixture is seeded once (`seed_real_corpus_workspace`, ~60-140s),
//! then each user-story block below gets its own independent copy via `SeededWorkspace::fork()`
//! — a cheap recursive filesystem copy of the already-rebuilt `db_path`/`wal_dir`, not a
//! from-scratch reseed — so one block's mutations can never leak into another's assertions.
//!
//! **Stub extraction (FR-003)**: `knowledge_process_chunk`/`knowledge_add_episode`/
//! `knowledge_reprocess_entity_types`/`knowledge_reprocess_relation_types` call the configured
//! extraction LLM. This suite points `--extractor-http` at `spawn_stub_extractor()`'s loopback,
//! content-keyed stub instead — no live LLM, no `ANTHROPIC_API_KEY`. Combined with
//! `spawn_stub_embedder()` (used for WAL replay/query-time embedding), this suite makes zero
//! outbound live-LLM or real-embedder network calls end to end (FR-017), verifiable
//! environmentally: this test process never sets `ANTHROPIC_API_KEY`, and every `--embedder-http`
//! / `--extractor-http` URL below points only at this suite's own loopback stubs.
//!
//! **Workspace root / ontology (new relative to #234)**: several mutation tools
//! (`knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`,
//! `knowledge_apply_corrections`, `knowledge_validate_corrections`) require
//! `LIMINIS_WORKSPACE_ROOT` to be set, and the relation-retyping tools additionally require a
//! workspace ontology declaring at least one relation type (ADR-0037). `SeededWorkspace::fork`'s
//! sibling `spawn_mutator` sets `LIMINIS_WORKSPACE_ROOT`; this suite authors `.lcg/ontology.yaml`
//! / `.liminis/knowledge-corrections.yaml` into each fork's workspace-root directory as needed
//! before spawning (ontology is loaded once at process startup, corrections files are read fresh
//! per call).
#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

mod common;
use common::real_corpus::seed_real_corpus_workspace;
use common::{spawn_stub_embedder, spawn_stub_extractor, ExtractionFixture, StubExtractorConfig};

const MUTATOR_SCOPE: &str = "--scope=read,write,admin";

/// Extracts `structuredContent` from a `tools/call` response, asserting it wasn't a tool-level
/// error first — the common shape every assertion below needs. Takes `resp` by value (rather
/// than by reference, as `mcp_real_corpus_e2e.rs`'s sibling helper does) so it can be called
/// directly on a `client.call_tool(...)` temporary without an intermediate `let` binding at
/// every call site.
fn structured(resp: Value, context: &str) -> Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "{context} errored: {resp:?}"
    );
    resp["result"]["structuredContent"].clone()
}

fn find_by_name<'a>(nodes: &'a [Value], name: &str) -> &'a Value {
    nodes
        .iter()
        .find(|n| n["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("entity {name:?} not found among {} nodes", nodes.len()))
}

/// Swaps the case of every letter (uppercase↔lowercase, other characters untouched) — the same
/// bytes case-insensitively, but a different string byte-for-byte whenever `s` contains at least
/// one letter. Used to prove entity-name resolution (`get_entity_by_name_ci`) is genuinely
/// case-insensitive rather than an accidental exact match.
fn flip_case(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_uppercase() {
                c.to_lowercase().next().unwrap_or(c)
            } else if c.is_lowercase() {
                c.to_uppercase().next().unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn set_extraction_fixture(
    cfg: &Arc<Mutex<StubExtractorConfig>>,
    marker: &str,
    fixture: ExtractionFixture,
) {
    cfg.lock()
        .unwrap()
        .extraction
        .insert(marker.to_string(), fixture);
}

fn active_edges(neighbors: &Value) -> Vec<&Value> {
    neighbors["edges"]
        .as_array()
        .unwrap_or_else(|| panic!("expected edges array: {neighbors}"))
        .iter()
        .filter(|e| e["invalid_at"].is_null())
        .collect()
}

fn write_ontology_yaml(workspace_root: &Path, entity_type: &str, relation_type: &str) {
    let dir = workspace_root.join(".lcg");
    std::fs::create_dir_all(&dir).expect("create .lcg dir");
    let yaml = format!(
        "mode: open\nentity_types:\n  - name: {entity_type:?}\nrelation_types:\n  - name: {relation_type:?}\n"
    );
    std::fs::write(dir.join("ontology.yaml"), yaml).expect("write ontology.yaml");
}

fn write_corrections_yaml(
    workspace_root: &Path,
    canonical_name: &str,
    alias_name: &str,
    retract_edge_uuid: &str,
) {
    let dir = workspace_root.join(".liminis");
    std::fs::create_dir_all(&dir).expect("create .liminis dir");
    let yaml = format!(
        "corrections:\n\
         \x20\x20- id: us2-same-as\n\
         \x20\x20\x20\x20type: same_as\n\
         \x20\x20\x20\x20canonical: {canonical_name:?}\n\
         \x20\x20\x20\x20aliases:\n\
         \x20\x20\x20\x20\x20\x20- {alias_name:?}\n\
         \x20\x20- id: us2-retract\n\
         \x20\x20\x20\x20type: retract\n\
         \x20\x20\x20\x20edge_uuid: {retract_edge_uuid:?}\n"
    );
    std::fs::write(dir.join("knowledge-corrections.yaml"), yaml).expect("write corrections yaml");
}

// #[ignore]: seeds the fixture once (~60-140s), then forks a cheap, independent filesystem copy
// per user-story block below so mutations never leak across assertions (FR-002/FR-013).
// Consolidated into one #[test] so the expensive seed step runs exactly once, mirroring
// mcp_real_corpus_e2e.rs's (#234) and real_corpus_e2e.rs's (#217) own rationale. Run explicitly
// via `cargo test -p lcg-service --release --test mcp_real_corpus_mutation_e2e -- --ignored`;
// wired into CI on push-to-main / workflow_dispatch (.github/workflows/real-corpus-e2e.yml,
// FR-018).
#[test]
#[ignore]
fn mcp_write_path_over_real_corpus_fixture() {
    let embed_port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{embed_port}/v1/embeddings");
    let (extractor_port, extractor_cfg) = spawn_stub_extractor();
    let extractor_url = format!("http://127.0.0.1:{extractor_port}/v1/chat/completions");

    let base = seed_real_corpus_workspace(&embedder_url);
    let group_id = base.group_id().to_string();
    let expected = base.expected.clone();
    let base_entity_count = expected["entity_count"].as_i64().unwrap();
    let base_relationship_count = expected["relationship_count"].as_i64().unwrap();
    let base_episode_count = expected["episode_count"].as_i64().unwrap();
    assert!(
        base_entity_count > 1000,
        "SC-004 requires the pre-populated graph to exceed the hybrid-dedup threshold (1000); \
         fixture reports {base_entity_count}"
    );

    // ── Stats-gathering pass over the (unmutated) base workspace ──────────────────────────
    // Gathers everything every user-story block below needs to pick real targets and compute
    // independently-derived expected counts, before any fork exists.
    let (
        sample_entities,
        sample_edge_uuid,
        chosen_entity_type,
        chosen_relation_type,
        entity_untyped_expected,
        entity_off_ontology_expected,
        entity_all_expected,
        relation_untyped_expected,
        relation_off_ontology_expected,
        relation_all_expected,
        sample_untyped_entity,
        noise_edge_sample,
    ) = {
        let mut reader = base.spawn_reader(&embedder_url, &["--scope=read"]);
        reader.initialize();

        let entities_resp = reader.call_tool(
            "knowledge_list_entities",
            json!({"group_ids": [group_id.clone()], "num_results": base_entity_count + 100}),
        );
        let entities_result = structured(entities_resp, "list_entities (stats)");
        let entity_nodes = entities_result["nodes"].as_array().unwrap().clone();

        let edges_resp = reader.call_tool(
            "knowledge_list_relationships",
            json!({"group_ids": [group_id.clone()], "num_results": base_relationship_count + 100}),
        );
        let edges_result = structured(edges_resp, "list_relationships (stats)");
        let edge_facts = edges_result["facts"].as_array().unwrap().clone();

        let mut entity_label_counts: HashMap<String, i64> = HashMap::new();
        let mut untyped_entities: Vec<&Value> = Vec::new();
        for n in &entity_nodes {
            let labels: Vec<String> = n["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let specific: Vec<&String> = labels.iter().filter(|l| l.as_str() != "Entity").collect();
            if specific.is_empty() {
                untyped_entities.push(n);
            } else {
                for l in specific {
                    *entity_label_counts.entry(l.clone()).or_insert(0) += 1;
                }
            }
        }
        let chosen_entity_type = entity_label_counts
            .iter()
            .max_by_key(|(_, c)| **c)
            .map(|(k, _)| k.clone())
            .expect("fixture must contain at least one typed entity label");

        let entity_untyped_expected = untyped_entities.len() as i64;
        let mut entity_off_ontology_typed = 0i64;
        for n in &entity_nodes {
            let labels: Vec<String> = n["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let specific: Vec<&String> = labels.iter().filter(|l| l.as_str() != "Entity").collect();
            if !specific.is_empty() && specific.iter().any(|l| **l != chosen_entity_type) {
                entity_off_ontology_typed += 1;
            }
        }
        let entity_off_ontology_expected = entity_untyped_expected + entity_off_ontology_typed;
        let entity_all_expected = entity_nodes.len() as i64;

        let mut relation_type_counts: HashMap<String, i64> = HashMap::new();
        for f in &edge_facts {
            if let Some(rt) = f["relation_type"].as_str() {
                if !rt.trim().is_empty() {
                    *relation_type_counts.entry(rt.to_string()).or_insert(0) += 1;
                }
            }
        }
        let chosen_relation_type = relation_type_counts
            .iter()
            .max_by_key(|(_, c)| **c)
            .map(|(k, _)| k.clone())
            .expect("fixture must contain at least one typed relation");

        let mut relation_untyped_expected = 0i64;
        let mut relation_off_ontology_expected = 0i64;
        for f in &edge_facts {
            let rt = f["relation_type"].as_str().unwrap_or("").to_string();
            let is_untyped = rt.trim().is_empty();
            if is_untyped {
                relation_untyped_expected += 1;
            }
            if is_untyped || rt != chosen_relation_type {
                relation_off_ontology_expected += 1;
            }
        }
        let relation_all_expected = edge_facts.len() as i64;

        let sample_entities: Vec<(String, String)> = entity_nodes
            .iter()
            .take(6)
            .map(|n| {
                (
                    n["name"].as_str().unwrap().to_string(),
                    n["uuid"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let sample_edge_uuid = edge_facts[0]["uuid"].as_str().unwrap().to_string();
        let sample_untyped_entity = untyped_entities.first().map(|n| {
            (
                n["name"].as_str().unwrap().to_string(),
                n["uuid"].as_str().unwrap().to_string(),
            )
        });

        // A real arrow-named "noise" edge with a genuine (non-empty) predicate, if the fixture
        // has one — used by US3's canonicalize_relations noise-edge-preserves-predicate check
        // (README's relation-typing section / ADR-0033).
        let noise_re = regex_lite_is_noise_edge as fn(&str) -> bool; // see helper below (mirrors canonicalize.rs)
        let noise_edge_sample = edge_facts
            .iter()
            .find(|f| {
                let name = f["name"].as_str().unwrap_or("");
                let rt = f["relation_type"].as_str().unwrap_or("");
                noise_re(name) && !rt.trim().is_empty()
            })
            .map(|f| {
                (
                    f["uuid"].as_str().unwrap().to_string(),
                    f["relation_type"].as_str().unwrap().to_string(),
                )
            });

        reader.shutdown();

        (
            sample_entities,
            sample_edge_uuid,
            chosen_entity_type,
            chosen_relation_type,
            entity_untyped_expected,
            entity_off_ontology_expected,
            entity_all_expected,
            relation_untyped_expected,
            relation_off_ontology_expected,
            relation_all_expected,
            sample_untyped_entity,
            noise_edge_sample,
        )
    };

    // ═══════════════════════════════════════════════════════════════════════════════════
    // User Story 4 — ingesting into the pre-populated graph resolves against pre-existing
    // entities (FR-013; the #202 regression guard).
    // ═══════════════════════════════════════════════════════════════════════════════════
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &[MUTATOR_SCOPE],
        );
        client.initialize();

        let (existing_name, existing_uuid) = &sample_entities[0];
        let collision_name = flip_case(existing_name);
        let new_entity_name = "US4 New Suite Entity";
        let fact = format!("{new_entity_name} relates to {collision_name} in US4 test content.");
        let marker = "US4_INGEST_MARKER";

        set_extraction_fixture(
            &extractor_cfg,
            marker,
            ExtractionFixture {
                entities: vec![
                    (
                        new_entity_name.to_string(),
                        "Concept".to_string(),
                        "a brand-new suite-ingested entity".to_string(),
                    ),
                    (
                        collision_name.clone(),
                        "Person".to_string(),
                        "a case-varied alias of a pre-existing fixture entity".to_string(),
                    ),
                ],
                edges: vec![(
                    new_entity_name.to_string(),
                    collision_name.clone(),
                    fact.clone(),
                    Some("RELATES_TO".to_string()),
                )],
            },
        );

        let status_before = structured(
            client.call_tool("knowledge_status", json!({})),
            "US4 status before",
        );
        assert_eq!(
            status_before["entity_count"].as_i64().unwrap(),
            base_entity_count
        );

        let ingest_resp = client.call_tool(
            "knowledge_process_chunk",
            json!({
                "chunk_text": format!("{marker}: {new_entity_name} and {collision_name} appear together."),
                "chunk_id": "us4-chunk-1",
                "source_file": "us4-source.txt",
                "group_id": group_id,
            }),
        );
        let ingest_result = structured(ingest_resp, "US4 knowledge_process_chunk");
        assert_eq!(ingest_result["success"], json!(true));
        assert_eq!(ingest_result["nodes_extracted"], json!(2));

        let status_after = structured(
            client.call_tool("knowledge_status", json!({})),
            "US4 status after ingest",
        );
        assert_eq!(
            status_after["entity_count"].as_i64().unwrap(),
            base_entity_count + 1,
            "exactly one new entity should be created; the case-varied collision name must \
             resolve to the pre-existing entity, not duplicate it"
        );

        let neighbors_resp = client.call_tool(
            "knowledge_get_entity_neighbors",
            json!({"entity_uuid": existing_uuid, "group_ids": [group_id.clone()], "num_results": 500}),
        );
        let neighbors = structured(neighbors_resp, "US4 neighbors of pre-existing entity");
        let matching = active_edges(&neighbors)
            .into_iter()
            .find(|e| e["fact"] == json!(fact))
            .unwrap_or_else(|| {
                panic!("expected new edge with fact {fact:?} among neighbors: {neighbors}")
            });
        assert!(
            matching["source_node_uuid"] == json!(*existing_uuid)
                || matching["target_node_uuid"] == json!(*existing_uuid),
            "new edge must reference the pre-existing entity's real UUID: {matching}"
        );

        // FR-015: WAL rebuild round trip.
        let rebuild = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us4-rebuild",
            Duration::from_secs(300),
        );
        let rebuild_result = structured(rebuild, "US4 rebuild");
        assert_eq!(rebuild_result["success"], json!(true));
        let status_rebuilt = structured(
            client.call_tool("knowledge_status", json!({})),
            "US4 status after rebuild",
        );
        assert_eq!(status_rebuilt["entity_count"], status_after["entity_count"]);
        assert_eq!(
            status_rebuilt["relationship_count"],
            status_after["relationship_count"]
        );
        assert_eq!(
            status_rebuilt["episode_count"],
            status_after["episode_count"]
        );

        client.shutdown();
    }

    // ═══════════════════════════════════════════════════════════════════════════════════
    // User Story 1 — deletions remove exactly what they should, nothing else (FR-004/FR-005).
    // ═══════════════════════════════════════════════════════════════════════════════════
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &[MUTATOR_SCOPE],
        );
        client.initialize();

        // ── Scenario 1: knowledge_delete_by_source ────────────────────────────────────
        let marker_a = "US1_SOURCE_A_MARKER";
        let entity_a = "US1 Source-Deletable Entity";
        set_extraction_fixture(
            &extractor_cfg,
            marker_a,
            ExtractionFixture {
                entities: vec![(
                    entity_a.to_string(),
                    "Concept".to_string(),
                    "solo entity for delete_by_source".to_string(),
                )],
                edges: vec![],
            },
        );
        let status_before_a = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status before A",
        );
        let ingest_a = structured(
            client.call_tool(
                "knowledge_process_chunk",
                json!({
                    "chunk_text": format!("{marker_a}: content mentioning {entity_a}."),
                    "chunk_id": "us1-chunk-a",
                    "source_file": "us1-source-a.txt",
                    "group_id": group_id,
                }),
            ),
            "US1 ingest A",
        );
        assert_eq!(ingest_a["success"], json!(true));
        let by_source_a = structured(
            client.call_tool(
                "knowledge_get_entities_by_source",
                json!({"source": "us1-source-a.txt:us1-chunk-a", "group_ids": [group_id.clone()], "num_results": 10}),
            ),
            "US1 get_entities_by_source A",
        );
        let entity_a_uuid = by_source_a["nodes"][0]["uuid"]
            .as_str()
            .unwrap()
            .to_string();
        let status_after_ingest_a = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status after ingest A",
        );
        assert_eq!(
            status_after_ingest_a["entity_count"].as_i64().unwrap(),
            status_before_a["entity_count"].as_i64().unwrap() + 1
        );

        let del_by_source = structured(
            client.call_tool(
                "knowledge_delete_by_source",
                json!({"source_file": "us1-source-a.txt", "group_ids": [group_id.clone()]}),
            ),
            "US1 delete_by_source",
        );
        assert_eq!(del_by_source["deleted_count"], json!(1));

        let status_after_del_source = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status after delete_by_source",
        );
        assert_eq!(
            status_after_del_source["episode_count"].as_i64().unwrap(),
            status_after_ingest_a["episode_count"].as_i64().unwrap() - 1
        );
        assert_eq!(
            status_after_del_source["entity_count"], status_after_ingest_a["entity_count"],
            "orphaned entity must remain in the graph, not be cascade-deleted"
        );
        assert_eq!(
            status_after_del_source["relationship_count"],
            status_after_ingest_a["relationship_count"],
            "unrelated relationship count must be unchanged"
        );

        let list_after_del_source = structured(
            client.call_tool(
                "knowledge_list_entities",
                json!({"group_ids": [group_id.clone()], "num_results": base_entity_count + 200}),
            ),
            "US1 list_entities after delete_by_source",
        );
        let uuids_after_del_source: Vec<&str> = list_after_del_source["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["uuid"].as_str().unwrap())
            .collect();
        assert!(
            uuids_after_del_source.contains(&entity_a_uuid.as_str()),
            "entity extracted solely from the deleted source must remain (orphaned, not deleted)"
        );

        // ── Scenario 2: knowledge_delete_episode ──────────────────────────────────────
        let marker_b = "US1_EPISODE_B_MARKER";
        let entity_b = "US1 Episode-Deletable Entity";
        set_extraction_fixture(
            &extractor_cfg,
            marker_b,
            ExtractionFixture {
                entities: vec![(
                    entity_b.to_string(),
                    "Concept".to_string(),
                    "solo entity for delete_episode".to_string(),
                )],
                edges: vec![],
            },
        );
        let ingest_b = structured(
            client.call_tool(
                "knowledge_process_chunk",
                json!({
                    "chunk_text": format!("{marker_b}: content mentioning {entity_b}."),
                    "chunk_id": "us1-chunk-b",
                    "source_file": "us1-source-b.txt",
                    "group_id": group_id,
                }),
            ),
            "US1 ingest B",
        );
        let episode_uuid_b = ingest_b["episode_uuid"].as_str().unwrap().to_string();
        let by_source_b = structured(
            client.call_tool(
                "knowledge_get_entities_by_source",
                json!({"source": "us1-source-b.txt:us1-chunk-b", "group_ids": [group_id.clone()], "num_results": 10}),
            ),
            "US1 get_entities_by_source B",
        );
        let entity_b_uuid = by_source_b["nodes"][0]["uuid"]
            .as_str()
            .unwrap()
            .to_string();
        let status_before_del_ep = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status before delete_episode",
        );

        let del_episode = structured(
            client.call_tool(
                "knowledge_delete_episode",
                json!({"episode_uuid": episode_uuid_b}),
            ),
            "US1 delete_episode",
        );
        assert_eq!(del_episode["status"], json!("deleted"));

        let status_after_del_ep = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status after delete_episode",
        );
        assert_eq!(
            status_after_del_ep["episode_count"].as_i64().unwrap(),
            status_before_del_ep["episode_count"].as_i64().unwrap() - 1
        );
        assert_eq!(
            status_after_del_ep["entity_count"],
            status_before_del_ep["entity_count"]
        );

        let list_after_del_ep = structured(
            client.call_tool(
                "knowledge_list_entities",
                json!({"group_ids": [group_id.clone()], "num_results": base_entity_count + 200}),
            ),
            "US1 list_entities after delete_episode",
        );
        let uuids_after_del_ep: Vec<&str> = list_after_del_ep["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["uuid"].as_str().unwrap())
            .collect();
        assert!(
            uuids_after_del_ep.contains(&entity_b_uuid.as_str()),
            "entity extracted solely from the deleted episode must remain (orphaned, not deleted)"
        );

        // ── Scenario 3: knowledge_delete_chunk_episode ────────────────────────────────
        let marker_c = "US1_CHUNK_C_MARKER";
        let entity_c = "US1 Chunk-Deletable Entity";
        set_extraction_fixture(
            &extractor_cfg,
            marker_c,
            ExtractionFixture {
                entities: vec![(
                    entity_c.to_string(),
                    "Concept".to_string(),
                    "solo entity for delete_chunk_episode".to_string(),
                )],
                edges: vec![],
            },
        );
        structured(
            client.call_tool(
                "knowledge_process_chunk",
                json!({
                    "chunk_text": format!("{marker_c}: content mentioning {entity_c}."),
                    "chunk_id": "us1-chunk-c",
                    "source_file": "us1-source-c.txt",
                    "group_id": group_id,
                }),
            ),
            "US1 ingest C",
        );
        let status_before_del_chunk = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status before delete_chunk_episode",
        );
        let del_chunk = structured(
            client.call_tool(
                "knowledge_delete_chunk_episode",
                json!({"chunk_id": "us1-chunk-c"}),
            ),
            "US1 delete_chunk_episode",
        );
        assert_eq!(del_chunk["deleted_count"], json!(1));
        let status_after_del_chunk = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status after delete_chunk_episode",
        );
        assert_eq!(
            status_after_del_chunk["episode_count"].as_i64().unwrap(),
            status_before_del_chunk["episode_count"].as_i64().unwrap() - 1
        );
        assert_eq!(
            status_after_del_chunk["entity_count"],
            status_before_del_chunk["entity_count"]
        );

        // FR-015: WAL rebuild round trip for the whole US1 block.
        let pre_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status pre-rebuild",
        );
        let rebuild = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us1-rebuild",
            Duration::from_secs(300),
        );
        let rebuild_result = structured(rebuild, "US1 rebuild");
        assert_eq!(rebuild_result["success"], json!(true));
        let post_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US1 status post-rebuild",
        );
        assert_eq!(post_rebuild["entity_count"], pre_rebuild["entity_count"]);
        assert_eq!(
            post_rebuild["relationship_count"],
            pre_rebuild["relationship_count"]
        );
        assert_eq!(post_rebuild["episode_count"], pre_rebuild["episode_count"]);

        client.shutdown();
    }

    // ═══════════════════════════════════════════════════════════════════════════════════
    // User Story 2 — merges and corrections are safe, previewable, and reversible-by-preview
    // (FR-006/FR-007).
    // ═══════════════════════════════════════════════════════════════════════════════════
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &[MUTATOR_SCOPE],
        );
        client.initialize();

        // ── Scenario 1: knowledge_merge_entities ──────────────────────────────────────
        let (anchor_name, _anchor_uuid) = &sample_entities[1];
        let canonical_name = "US2 Merge Canonical";
        let alias_name = "US2 Merge Alias";
        let fact_canonical = format!("{canonical_name} co-authored work with {anchor_name}.");
        let fact_alias = format!("{alias_name} co-authored work with {anchor_name}.");
        let marker_merge = "US2_MERGE_MARKER";
        set_extraction_fixture(
            &extractor_cfg,
            marker_merge,
            ExtractionFixture {
                entities: vec![
                    (
                        canonical_name.to_string(),
                        "Person".to_string(),
                        "merge canonical".to_string(),
                    ),
                    (
                        alias_name.to_string(),
                        "Person".to_string(),
                        "merge alias".to_string(),
                    ),
                ],
                edges: vec![
                    (
                        canonical_name.to_string(),
                        anchor_name.clone(),
                        fact_canonical.clone(),
                        Some("COAUTHORED".to_string()),
                    ),
                    (
                        alias_name.to_string(),
                        anchor_name.clone(),
                        fact_alias.clone(),
                        Some("COAUTHORED".to_string()),
                    ),
                ],
            },
        );
        structured(
            client.call_tool(
                "knowledge_process_chunk",
                json!({
                    "chunk_text": format!("{marker_merge}: {canonical_name} and {alias_name}, both linked to {anchor_name}."),
                    "chunk_id": "us2-chunk-merge",
                    "source_file": "us2-source-merge.txt",
                    "group_id": group_id,
                }),
            ),
            "US2 ingest merge pair",
        );

        let list_resp = structured(
            client.call_tool(
                "knowledge_list_entities",
                json!({"group_ids": [group_id.clone()], "num_results": base_entity_count + 200}),
            ),
            "US2 list_entities (find merge pair)",
        );
        let nodes = list_resp["nodes"].as_array().unwrap().clone();
        let canonical_uuid = find_by_name(&nodes, canonical_name)["uuid"]
            .as_str()
            .unwrap()
            .to_string();
        let alias_uuid = find_by_name(&nodes, alias_name)["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let dry_merge = structured(
            client.call_tool(
                "knowledge_merge_entities",
                json!({
                    "canonical_uuid": canonical_uuid,
                    "alias_uuids": [alias_uuid.clone()],
                    "group_id": group_id,
                    "dry_run": true,
                }),
            ),
            "US2 merge dry_run",
        );
        assert_eq!(dry_merge["success"], json!(true));

        let real_merge = structured(
            client.call_tool(
                "knowledge_merge_entities",
                json!({
                    "canonical_uuid": canonical_uuid,
                    "alias_uuids": [alias_uuid.clone()],
                    "group_id": group_id,
                }),
            ),
            "US2 merge real",
        );
        assert_eq!(real_merge["success"], json!(true));

        assert_eq!(dry_merge["edges_rewritten"], real_merge["edges_rewritten"]);
        assert_eq!(
            dry_merge["edges_deduplicated"],
            real_merge["edges_deduplicated"]
        );
        assert_eq!(dry_merge["merged_count"], real_merge["merged_count"]);
        assert_eq!(
            dry_merge["plan"]["total_edges_rewritten"],
            real_merge["edges_rewritten"]
        );
        assert_eq!(
            dry_merge["plan"]["total_edges_collapsed"],
            real_merge["edges_deduplicated"]
        );
        assert_eq!(real_merge["merged_count"], json!(1));
        assert_eq!(
            real_merge["edges_rewritten"],
            json!(1),
            "alias's distinctly-named edge must be rewritten, not dropped"
        );

        let alias_neighbors = structured(
            client.call_tool(
                "knowledge_get_entity_neighbors",
                json!({"entity_uuid": alias_uuid, "group_ids": [group_id.clone()], "num_results": 200}),
            ),
            "US2 alias neighbors after merge",
        );
        assert!(
            active_edges(&alias_neighbors).is_empty(),
            "no active edge may still reference the merged-away alias UUID: {alias_neighbors}"
        );

        let canonical_neighbors = structured(
            client.call_tool(
                "knowledge_get_entity_neighbors",
                json!({"entity_uuid": canonical_uuid, "group_ids": [group_id.clone()], "num_results": 200}),
            ),
            "US2 canonical neighbors after merge",
        );
        let rewritten = active_edges(&canonical_neighbors)
            .into_iter()
            .find(|e| e["fact"] == json!(fact_alias))
            .unwrap_or_else(|| panic!("expected alias's original fact rewritten onto canonical: {canonical_neighbors}"));
        assert_eq!(rewritten["source_node_uuid"], json!(canonical_uuid));

        // ── Scenario 2: knowledge_validate_corrections / knowledge_apply_corrections ────
        let (corr_anchor_name, _) = &sample_entities[2];
        let corr_canonical = "US2 Corrections Canonical";
        let corr_alias = "US2 Corrections Alias";
        let corr_fact = format!("{corr_alias} is linked to {corr_anchor_name}.");
        let marker_corr = "US2_CORRECTIONS_MARKER";
        set_extraction_fixture(
            &extractor_cfg,
            marker_corr,
            ExtractionFixture {
                entities: vec![
                    (
                        corr_canonical.to_string(),
                        "Person".to_string(),
                        "corrections canonical".to_string(),
                    ),
                    (
                        corr_alias.to_string(),
                        "Person".to_string(),
                        "corrections alias".to_string(),
                    ),
                ],
                edges: vec![(
                    corr_alias.to_string(),
                    corr_anchor_name.clone(),
                    corr_fact.clone(),
                    Some("LINKED_TO".to_string()),
                )],
            },
        );
        structured(
            client.call_tool(
                "knowledge_process_chunk",
                json!({
                    "chunk_text": format!("{marker_corr}: {corr_canonical} and {corr_alias}, linked to {corr_anchor_name}."),
                    "chunk_id": "us2-chunk-corr",
                    "source_file": "us2-source-corr.txt",
                    "group_id": group_id,
                }),
            ),
            "US2 ingest corrections pair",
        );

        let retract_edge_uuid = sample_edge_uuid.clone();
        write_corrections_yaml(
            workspace_root.path(),
            corr_canonical,
            corr_alias,
            &retract_edge_uuid,
        );

        let validate_before = structured(
            client.call_tool("knowledge_validate_corrections", json!({})),
            "US2 validate_corrections",
        );
        assert_eq!(validate_before["valid"], json!(true), "{validate_before}");
        assert_eq!(validate_before["total_corrections"], json!(2));
        assert_eq!(validate_before["unapplied_corrections"], json!(2));

        let apply_dry = structured(
            client.call_tool("knowledge_apply_corrections", json!({"dry_run": true})),
            "US2 apply_corrections dry_run",
        );
        assert_eq!(apply_dry["success"], json!(true), "{apply_dry}");
        assert_eq!(apply_dry["applied"], json!(0));
        assert_eq!(apply_dry["skipped"], json!(0));
        let dry_ids: Vec<Value> = apply_dry["details"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].clone())
            .collect();
        assert_eq!(dry_ids.len(), 2);

        let apply_real = structured(
            client.call_tool("knowledge_apply_corrections", json!({})),
            "US2 apply_corrections real",
        );
        assert_eq!(apply_real["success"], json!(true), "{apply_real}");
        assert_eq!(apply_real["applied"], json!(2));
        assert_eq!(apply_real["skipped"], json!(0));
        let real_ids: Vec<Value> = apply_real["details"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].clone())
            .collect();
        assert_eq!(
            dry_ids, real_ids,
            "dry-run preview and real apply must agree on which corrections changed"
        );

        let validate_after = structured(
            client.call_tool("knowledge_validate_corrections", json!({})),
            "US2 validate_corrections after apply",
        );
        assert_eq!(validate_after["unapplied_corrections"], json!(0));

        let list_after_corr = structured(
            client.call_tool(
                "knowledge_list_entities",
                json!({"group_ids": [group_id.clone()], "num_results": base_entity_count + 200}),
            ),
            "US2 list_entities after corrections",
        );
        let corr_alias_uuid =
            find_by_name(list_after_corr["nodes"].as_array().unwrap(), corr_alias)["uuid"]
                .as_str()
                .unwrap()
                .to_string();
        let corr_alias_neighbors = structured(
            client.call_tool(
                "knowledge_get_entity_neighbors",
                json!({"entity_uuid": corr_alias_uuid, "group_ids": [group_id.clone()], "num_results": 200}),
            ),
            "US2 corrections alias neighbors after apply",
        );
        assert!(
            active_edges(&corr_alias_neighbors).is_empty(),
            "corrections same_as alias must have no active edges after apply: {corr_alias_neighbors}"
        );

        let retracted = structured(
            client.call_tool(
                "knowledge_get_edges_by_uuids",
                json!({"uuids": [retract_edge_uuid]}),
            ),
            "US2 retracted edge check",
        );
        assert!(
            retracted["edges"][0]["invalid_at"].is_string(),
            "retracted edge must have invalid_at set after apply: {retracted}"
        );

        // FR-015: WAL rebuild round trip for the whole US2 block.
        let pre_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US2 status pre-rebuild",
        );
        let rebuild = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us2-rebuild",
            Duration::from_secs(300),
        );
        let rebuild_result = structured(rebuild, "US2 rebuild");
        assert_eq!(rebuild_result["success"], json!(true));
        let post_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US2 status post-rebuild",
        );
        assert_eq!(post_rebuild["entity_count"], pre_rebuild["entity_count"]);
        assert_eq!(
            post_rebuild["relationship_count"],
            pre_rebuild["relationship_count"]
        );
        assert_eq!(post_rebuild["episode_count"], pre_rebuild["episode_count"]);

        client.shutdown();
    }

    // ═══════════════════════════════════════════════════════════════════════════════════
    // User Story 3 — retyping is correctly scoped, idempotent, and abstains honestly
    // (FR-008/FR-009/FR-010/FR-011/FR-012). Two forks: reprocess (entity + relation) and
    // canonicalize — kept separate so canonicalize's noise-edge check sees the fixture's
    // original real predicates, not values already overwritten by the reprocess block.
    // ═══════════════════════════════════════════════════════════════════════════════════
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        write_ontology_yaml(
            workspace_root.path(),
            &chosen_entity_type,
            &chosen_relation_type,
        );
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &[MUTATOR_SCOPE],
        );
        client.initialize();

        // ── Scenarios 1-2: entity scope selection + dry-run/applied parity (all abstain,
        // by construction, since no entity_verdicts are configured — this isolates "which rows
        // are selected" from "what they get reclassified to", per FR-008's own framing). ──
        for (scope, expected_count) in [
            ("untyped", entity_untyped_expected),
            ("off_ontology", entity_off_ontology_expected),
            ("all", entity_all_expected),
        ] {
            let dry = client.call_tool_with_progress(
                "knowledge_reprocess_entity_types",
                json!({"group_id": group_id, "scope": scope, "dry_run": true}),
                &format!("us3-entity-dry-{scope}"),
                Duration::from_secs(300),
            );
            let dry_result =
                structured(dry, &format!("US3 entity reprocess dry_run scope={scope}"));
            assert_eq!(
                dry_result["would_reclassify_count"],
                json!(0),
                "all-abstain stub yields zero reclassifications"
            );

            let real = client.call_tool_with_progress(
                "knowledge_reprocess_entity_types",
                json!({"group_id": group_id, "scope": scope}),
                &format!("us3-entity-real-{scope}"),
                Duration::from_secs(300),
            );
            let real_result = structured(real, &format!("US3 entity reprocess real scope={scope}"));
            assert_eq!(real_result["success"], json!(true));
            assert_eq!(
                real_result["reclassified_count"],
                dry_result["would_reclassify_count"]
            );
            let selected = real_result["reclassified_count"].as_i64().unwrap()
                + real_result["unchanged_count"].as_i64().unwrap();
            assert_eq!(
                selected, expected_count,
                "scope={scope} must select exactly {expected_count} candidate entities, got {selected}"
            );
        }

        // FR-011 (entity side): an untyped entity's labels are left unchanged on abstention.
        if let Some((_, uuid)) = &sample_untyped_entity {
            let neighbors = structured(
                client.call_tool(
                    "knowledge_get_entity_neighbors",
                    json!({"entity_uuid": uuid, "group_ids": [group_id.clone()]}),
                ),
                "US3 untyped entity neighbors (abstention check)",
            );
            let _ = neighbors; // presence check only: the entity is still resolvable, not force-typed.
            let list_resp = structured(
                client.call_tool(
                    "knowledge_list_entities",
                    json!({"group_ids": [group_id.clone()], "num_results": base_entity_count + 200}),
                ),
                "US3 list_entities (abstention check)",
            );
            let entity = list_resp["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["uuid"].as_str() == Some(uuid.as_str()))
                .expect("sampled untyped entity must still exist");
            let labels: Vec<&str> = entity["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            assert_eq!(
                labels,
                vec!["Entity"],
                "abstained entity must remain untyped, never force-assigned"
            );
        }

        // ── Scenarios 1-2-3 (relation side): scope selection, dry-run parity, and
        // idempotency. Order matters here: `untyped` runs first (captures the original
        // untyped-count correctly before any writes), then `off_ontology`/`all` — abstention
        // writes UNCLASSIFIED, but UNCLASSIFIED is itself off-ontology, so the off_ontology/all
        // candidate sets are unaffected by the untyped pass's writes. ──
        for (scope, expected_count) in [
            ("untyped", relation_untyped_expected),
            ("off_ontology", relation_off_ontology_expected),
            ("all", relation_all_expected),
        ] {
            let dry = client.call_tool_with_progress(
                "knowledge_reprocess_relation_types",
                json!({"group_id": group_id, "scope": scope, "dry_run": true}),
                &format!("us3-relation-dry-{scope}"),
                Duration::from_secs(300),
            );
            let dry_result = structured(
                dry,
                &format!("US3 relation reprocess dry_run scope={scope}"),
            );

            let real = client.call_tool_with_progress(
                "knowledge_reprocess_relation_types",
                json!({"group_id": group_id, "scope": scope}),
                &format!("us3-relation-real-{scope}"),
                Duration::from_secs(300),
            );
            let real_result =
                structured(real, &format!("US3 relation reprocess real scope={scope}"));
            assert_eq!(real_result["success"], json!(true), "{real_result}");
            assert_eq!(
                real_result["reclassified_count"], dry_result["would_reclassify_count"],
                "scope={scope}: dry-run plan must match applied result"
            );
            let selected = real_result["reclassified_count"].as_i64().unwrap()
                + real_result["unchanged_count"].as_i64().unwrap();
            assert_eq!(
                selected, expected_count,
                "scope={scope} must select exactly {expected_count} candidate edges, got {selected}"
            );

            if scope == "all" {
                // FR-010: idempotency — re-running the same completed scope with the same
                // (all-abstain) stub verdicts is a no-op.
                let rerun = client.call_tool_with_progress(
                    "knowledge_reprocess_relation_types",
                    json!({"group_id": group_id, "scope": scope}),
                    "us3-relation-idempotency",
                    Duration::from_secs(300),
                );
                let rerun_result = structured(rerun, "US3 relation reprocess idempotency re-run");
                assert_eq!(
                    rerun_result["reclassified_count"],
                    json!(0),
                    "re-run must write nothing: {rerun_result}"
                );
            }
        }

        // FR-011 (relation side): the sampled real edge must now be honestly UNCLASSIFIED
        // (abstain), never force-assigned the ontology's one declared type.
        let sample_after = structured(
            client.call_tool(
                "knowledge_get_edges_by_uuids",
                json!({"uuids": [sample_edge_uuid.clone()]}),
            ),
            "US3 sampled edge after relation reprocess (abstention check)",
        );
        assert_eq!(
            sample_after["edges"][0]["relation_type"],
            json!("UNCLASSIFIED"),
            "abstained edge must be written as the literal UNCLASSIFIED sentinel: {sample_after}"
        );

        // FR-015: WAL rebuild round trip for the reprocess block.
        let pre_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US3a status pre-rebuild",
        );
        let rebuild = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us3a-rebuild",
            Duration::from_secs(300),
        );
        let rebuild_result = structured(rebuild, "US3a rebuild");
        assert_eq!(rebuild_result["success"], json!(true));
        let post_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US3a status post-rebuild",
        );
        assert_eq!(post_rebuild["entity_count"], pre_rebuild["entity_count"]);
        assert_eq!(
            post_rebuild["relationship_count"],
            pre_rebuild["relationship_count"]
        );
        assert_eq!(post_rebuild["episode_count"], pre_rebuild["episode_count"]);

        client.shutdown();
    }

    // ── Scenario 5 (separate fork): knowledge_canonicalize_relations, over the fixture's
    // original, un-reprocessed predicates. ──
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        write_ontology_yaml(
            workspace_root.path(),
            &chosen_entity_type,
            &chosen_relation_type,
        );
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &[MUTATOR_SCOPE],
        );
        client.initialize();

        let first = client.call_tool_with_progress(
            "knowledge_canonicalize_relations",
            json!({}),
            "us3b-canonicalize-1",
            Duration::from_secs(300),
        );
        let first_result = structured(first, "US3b canonicalize (first run)");

        let second = client.call_tool_with_progress(
            "knowledge_canonicalize_relations",
            json!({}),
            "us3b-canonicalize-2",
            Duration::from_secs(300),
        );
        let second_result = structured(second, "US3b canonicalize (second run)");

        assert_eq!(first_result["total_edges"], second_result["total_edges"]);
        assert_eq!(
            first_result["mapped_count"], second_result["mapped_count"],
            "stable input must produce a stable second run"
        );
        assert_eq!(first_result["noise_count"], second_result["noise_count"]);
        assert_eq!(
            first_result["residual_count"],
            second_result["residual_count"]
        );

        if let Some((noise_uuid, original_rt)) = &noise_edge_sample {
            let after = structured(
                client.call_tool(
                    "knowledge_get_edges_by_uuids",
                    json!({"uuids": [noise_uuid.clone()]}),
                ),
                "US3b noise edge after canonicalize",
            );
            assert_eq!(
                after["edges"][0]["relation_type"],
                json!(original_rt),
                "an arrow-named noise edge with an existing non-empty predicate must keep it \
                 unchanged, per ADR-0033/README: {after}"
            );
        }

        // FR-015: WAL rebuild round trip.
        let pre_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US3b status pre-rebuild",
        );
        let rebuild = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({"force_clear": true}),
            "us3b-rebuild",
            Duration::from_secs(300),
        );
        let rebuild_result = structured(rebuild, "US3b rebuild");
        assert_eq!(rebuild_result["success"], json!(true));
        let post_rebuild = structured(
            client.call_tool("knowledge_status", json!({})),
            "US3b status post-rebuild",
        );
        assert_eq!(post_rebuild["entity_count"], pre_rebuild["entity_count"]);
        assert_eq!(
            post_rebuild["relationship_count"],
            pre_rebuild["relationship_count"]
        );
        assert_eq!(post_rebuild["episode_count"], pre_rebuild["episode_count"]);

        client.shutdown();
    }

    // ═══════════════════════════════════════════════════════════════════════════════════
    // User Story 5 — full graph reset requires confirmation and leaves a rebuildable graph
    // (FR-014). Runs against a fork about to be discarded — the one genuinely destructive
    // mutation in this suite.
    // ═══════════════════════════════════════════════════════════════════════════════════
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &[MUTATOR_SCOPE],
        );
        client.initialize();

        let rejected = client.call_tool("knowledge_clear_all", json!({}));
        assert_eq!(
            rejected["result"]["isError"],
            json!(true),
            "clear_all without confirm:true must be rejected: {rejected:?}"
        );
        let status_unchanged = structured(
            client.call_tool("knowledge_status", json!({})),
            "US5 status after rejection",
        );
        assert_eq!(
            status_unchanged["entity_count"].as_i64().unwrap(),
            base_entity_count
        );
        assert_eq!(
            status_unchanged["relationship_count"].as_i64().unwrap(),
            base_relationship_count
        );
        assert_eq!(
            status_unchanged["episode_count"].as_i64().unwrap(),
            base_episode_count
        );

        let cleared = structured(
            client.call_tool(
                "knowledge_clear_all",
                json!({"confirm": true, "preserve_wal": true}),
            ),
            "US5 clear_all confirmed",
        );
        assert_eq!(cleared["success"], json!(true));

        let status_cleared = structured(
            client.call_tool("knowledge_status", json!({})),
            "US5 status after clear",
        );
        assert_eq!(status_cleared["entity_count"], json!(0));
        assert_eq!(status_cleared["relationship_count"], json!(0));
        assert_eq!(status_cleared["episode_count"], json!(0));
        assert_eq!(status_cleared["context_graph_initialized"], json!(true));
        assert_eq!(status_cleared["connected"], json!(true));

        let rebuild = client.call_tool_with_progress(
            "knowledge_rebuild_from_wal",
            json!({}),
            "us5-rebuild",
            Duration::from_secs(300),
        );
        let rebuild_result = structured(rebuild, "US5 rebuild from preserved WAL");
        assert_eq!(rebuild_result["success"], json!(true));

        let status_rebuilt = structured(
            client.call_tool("knowledge_status", json!({})),
            "US5 status after rebuild",
        );
        assert_eq!(
            status_rebuilt["entity_count"].as_i64().unwrap(),
            base_entity_count
        );
        assert_eq!(
            status_rebuilt["relationship_count"].as_i64().unwrap(),
            base_relationship_count
        );
        assert_eq!(
            status_rebuilt["episode_count"].as_i64().unwrap(),
            base_episode_count
        );

        client.shutdown();
    }

    // ═══════════════════════════════════════════════════════════════════════════════════
    // User Story 6 — mutations are invisible and rejected under read scope (FR-016).
    // ═══════════════════════════════════════════════════════════════════════════════════
    {
        let fork = base.fork();
        let workspace_root = TempDir::new().unwrap();
        let mut client = fork.spawn_mutator(
            &embedder_url,
            &extractor_url,
            workspace_root.path(),
            &["--scope=read"],
        );
        client.initialize();

        let mutation_tools = [
            "knowledge_delete_by_source",
            "knowledge_delete_episode",
            "knowledge_delete_chunk_episode",
            "knowledge_merge_entities",
            "knowledge_apply_corrections",
            "knowledge_reprocess_entity_types",
            "knowledge_reprocess_relation_types",
            "knowledge_canonicalize_relations",
            "knowledge_process_chunk",
            "knowledge_add_episode",
            "knowledge_clear_all",
        ];

        let tools = client.list_tools();
        let tool_names: std::collections::HashSet<&str> =
            tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for name in mutation_tools {
            assert!(
                !tool_names.contains(name),
                "{name} must be absent from tools/list under --scope=read"
            );
        }
        assert!(
            tool_names.contains("knowledge_validate_corrections"),
            "knowledge_validate_corrections is Read-scope and must remain visible"
        );

        for name in mutation_tools {
            let resp = client.call_tool(name, json!({}));
            assert!(
                resp.get("error").is_some(),
                "expected a protocol-level rejection calling unlisted tool {name} under --scope=read: {resp:?}"
            );
        }

        let status = structured(
            client.call_tool("knowledge_status", json!({})),
            "US6 status after rejections",
        );
        assert_eq!(status["entity_count"].as_i64().unwrap(), base_entity_count);
        assert_eq!(
            status["relationship_count"].as_i64().unwrap(),
            base_relationship_count
        );
        assert_eq!(
            status["episode_count"].as_i64().unwrap(),
            base_episode_count
        );

        client.shutdown();
    }

    // FR-017: verified environmentally, not by an in-process counter (this suite spawns the
    // real compiled binary as an opaque subprocess) — this test process never sets
    // ANTHROPIC_API_KEY, and every `--embedder-http`/`--extractor-http` URL above points only
    // at this suite's own loopback stubs (spawn_stub_embedder / spawn_stub_extractor).
}

/// Mirrors `canonicalize::is_noise_edge` (crates/core/src/canonicalize.rs) so the stats-
/// gathering pass can find a real arrow-named noise edge without depending on that function
/// being `pub` outside the core crate.
fn regex_lite_is_noise_edge(s: &str) -> bool {
    let s = s.trim();
    let mut chars = s.chars().peekable();
    fn take_run(chars: &mut std::iter::Peekable<std::str::Chars>) -> bool {
        let Some(&first) = chars.peek() else {
            return false;
        };
        if !first.is_ascii_uppercase() {
            return false;
        }
        while let Some(&c) = chars.peek() {
            if c.is_ascii_uppercase() || c.is_ascii_digit() || c == ' ' {
                chars.next();
            } else {
                break;
            }
        }
        true
    }
    if !take_run(&mut chars) {
        return false;
    }
    while matches!(chars.peek(), Some(' ')) {
        chars.next();
    }
    let matched_arrow = if chars.clone().take(2).collect::<String>() == "->" {
        chars.next();
        chars.next();
        true
    } else if chars.peek() == Some(&'→') {
        chars.next();
        true
    } else {
        false
    };
    if !matched_arrow {
        return false;
    }
    while matches!(chars.peek(), Some(' ')) {
        chars.next();
    }
    if !take_run(&mut chars) {
        return false;
    }
    chars.next().is_none()
}
