//! MCP read-path e2e suite over the real-corpus WAL fixture (issue #234).
//!
//! Extends `real_corpus_e2e.rs` (#217, `crates/core/tests/`, direct in-process
//! `handlers::dispatch`) and `mcp_stdio.rs` (#195, MCP-over-stdio against an empty graph) by
//! joining the two: this suite seeds a workspace from the same golden real-corpus WAL fixture
//! and drives the real compiled binary over MCP-over-stdio against it, so the layer external
//! MCP clients actually touch (`tools/list` schema generation, `tools/call` argument
//! marshalling, scope gating, error-to-`CallToolResult` mapping) is exercised against real
//! content instead of an empty graph or direct in-process dispatch.
//!
//! Golden query/traversal/relation-type expectations are read from the fixture's
//! `expected_results.json`, not hardcoded — see `real_corpus.rs`'s `SeededWorkspace` doc
//! comments for how the workspace is seeded (one rebuild, reused across every assertion below,
//! per FR-013).
//!
//! Query-time embedding uses `--embedder-http` pointed at `spawn_stub_embedder()`'s loopback
//! stub (a fixed zero vector), not a live embedder — this is what makes FR-014's zero-network
//! requirement possible while still testing against the *real* embeddings baked into the graph
//! at capture time. As in `real_corpus_e2e.rs`, this makes the vector half of RRF-fused hybrid
//! search deterministic-but-uninformative, so golden entity/relationship query assertions are
//! top-N set-membership/overlap, not exact top-1/ordering. Graph traversal
//! (`knowledge_get_entity_neighbors`) and raw enumeration (`knowledge_list_entities` /
//! `knowledge_list_relationships` / `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`
//! / `knowledge_get_edges_by_uuids`) don't depend on embeddings at all, so those assertions are
//! exact.
//!
//! FR-014 (zero outbound LLM/embedder network calls): unlike `real_corpus_e2e.rs`, this suite
//! spawns the real compiled binary as an opaque subprocess, so there is no in-process call
//! counter to assert against (see `real_corpus.rs`'s module doc comment). The guarantee is
//! instead environmental and verifiable independently: this suite runs with no
//! `ANTHROPIC_API_KEY` and no network reachability beyond the suite's own loopback stub
//! embedder — consistent with `mcp_stdio.rs`'s existing precedent (#195) for the same
//! guarantee.
#![cfg(unix)]

use std::collections::{BTreeSet, HashSet};

use serde_json::{json, Value};

mod common;
use common::real_corpus::seed_real_corpus_workspace;
use common::spawn_stub_embedder;

/// Extracts `structuredContent` from a `tools/call` response, asserting it wasn't a tool-level
/// error first — the common shape every assertion below needs.
fn structured<'a>(resp: &'a Value, context: &str) -> &'a Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "{context} errored: {resp:?}"
    );
    &resp["result"]["structuredContent"]
}

fn as_uuid_set(v: &Value, key: &str) -> BTreeSet<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("expected {key} to be an array: {v}"))
        .iter()
        .map(|n| {
            n["uuid"]
                .as_str()
                .unwrap_or_else(|| panic!("expected {key} element to have a string `uuid`: {n}"))
                .to_string()
        })
        .collect()
}

fn as_name_set(v: &Value, key: &str) -> HashSet<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("expected {key} to be an array: {v}"))
        .iter()
        .map(|n| {
            n["name"]
                .as_str()
                .unwrap_or_else(|| panic!("expected {key} element to have a string `name`: {n}"))
                .to_string()
        })
        .collect()
}

/// Asserts `arr`'s `uuid` fields form a set of exactly `expected_count` distinct values — the
/// enumeration-correctness technique from `real_corpus_e2e.rs` (#217): no duplicates, no gaps,
/// applied here through `tools/call` instead of direct dispatch (FR-008).
fn assert_exact_uuid_enumeration(v: &Value, key: &str, expected_count: i64, context: &str) {
    let arr = v[key]
        .as_array()
        .unwrap_or_else(|| panic!("{context}: expected {key} to be an array: {v}"));
    let uuids = as_uuid_set(v, key);
    assert_eq!(
        arr.len() as i64,
        expected_count,
        "{context}: expected exactly {expected_count} {key}, got {}",
        arr.len()
    );
    assert_eq!(
        uuids.len(),
        arr.len(),
        "{context}: found duplicate UUIDs among {key}"
    );
}

// #[ignore]: seeds the fixture (a full WAL replay + HNSW/FTS index build over 1,506 entities,
// ~60-140s per real_corpus_wal/README.md) once, then spawns a second `--scope=read` process to
// run every read-path/scope-gating assertion — consolidated into one #[test] so the expensive
// seed step runs exactly once (FR-013), mirroring real_corpus_e2e.rs's own consolidation
// rationale (#217). Run explicitly via `cargo test -p lcg-service --release
// --test mcp_real_corpus_e2e -- --ignored`; wired into CI on push-to-main /
// workflow_dispatch (.github/workflows/real-corpus-e2e.yml, FR-015).
#[test]
#[ignore]
fn mcp_read_path_over_real_corpus_fixture() {
    let port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let workspace = seed_real_corpus_workspace(&embedder_url);
    let expected = workspace.expected.clone();
    let group_id = workspace.group_id().to_string();

    let mut client = workspace.spawn_reader(&embedder_url, &["--scope=read"]);
    client.initialize();

    // ── Acceptance Scenario 1 (FR-004): knowledge_status matches fixture counts ──────────
    let status_resp = client.call_tool("knowledge_status", json!({}));
    let status = structured(&status_resp, "knowledge_status");
    assert_eq!(
        status["entity_count"], expected["entity_count"],
        "entity_count mismatch: {status}"
    );
    assert_eq!(
        status["relationship_count"], expected["relationship_count"],
        "relationship_count mismatch: {status}"
    );
    assert_eq!(
        status["episode_count"], expected["episode_count"],
        "episode_count mismatch: {status}"
    );
    assert_eq!(status["indices_built"], json!(true), "{status}");

    // ── Acceptance Scenario 2 (FR-005): golden knowledge_find_entities queries ───────────
    let entity_queries = expected["golden_entity_queries"].as_array().unwrap();
    assert!(
        !entity_queries.is_empty(),
        "fixture must record golden entity queries"
    );
    for q in entity_queries {
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let resp = client.call_tool(
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
        );
        let result = structured(&resp, "knowledge_find_entities");
        let actual_names = as_name_set(result, "nodes");
        let expected_names: Vec<String> = q["expected_entity_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect();

        // Query-time embedding is the loopback stub (a fixed zero vector), so the fused top-N
        // won't exactly reproduce capture-time's real-embedder ranking — set-membership, not
        // exact top-1/ordering (same tolerance as real_corpus_e2e.rs, #217).
        let overlap = expected_names
            .iter()
            .filter(|n| actual_names.contains(*n))
            .count();
        assert!(
            overlap > 0,
            "query {query:?}: expected at least one of {expected_names:?} in actual top-{top_n} \
             {actual_names:?}"
        );
    }

    // ── Acceptance Scenario 3 (FR-005/FR-006): golden knowledge_find_relationships + \
    //    knowledge_search_passages ────────────────────────────────────────────────────────
    let relationship_queries = expected["golden_relationship_queries"].as_array().unwrap();
    assert!(
        !relationship_queries.is_empty(),
        "fixture must record golden relationship queries"
    );
    for q in relationship_queries {
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let resp = client.call_tool(
            "knowledge_find_relationships",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
        );
        let result = structured(&resp, "knowledge_find_relationships");
        let facts = result["edges"].as_array().unwrap();
        assert!(
            !facts.is_empty(),
            "query {query:?} returned no relationships: {result}"
        );

        let actual_relation_types: HashSet<String> = facts
            .iter()
            .filter_map(|f| f["relation_type"].as_str().map(str::to_string))
            .collect();
        let expected_relation_types: HashSet<String> = q["expected_relation_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();

        let overlap = expected_relation_types
            .intersection(&actual_relation_types)
            .count();
        assert!(
            overlap > 0,
            "query {query:?}: expected relation_type overlap between {expected_relation_types:?} \
             and actual {actual_relation_types:?}"
        );
    }

    // knowledge_search_passages: expected_results.json records no golden passages for this
    // tool (#217 never exercised it) — self-referential check instead. Unlike hybrid
    // entity/relationship search, search_passages (search.rs) is pure-vector (no BM25/FTS
    // component), so with a stub zero-vector query embedding there is no real semantic signal
    // to assert content/entity-name overlap against — doing so would make this assertion
    // flaky by construction, not just tolerant. Instead: assert a non-empty, min_score-
    // respecting result (FR-006's "real-content assertion") whose passage UUIDs are a subset
    // of the fixture's real episode UUIDs, proving the passages are genuine ingested content,
    // not stub/empty rows.
    let episodes_resp = client.call_tool(
        "knowledge_get_episodes",
        json!({"group_id": group_id, "last_n": expected["episode_count"].as_i64().unwrap() + 50}),
    );
    let episodes_result = structured(&episodes_resp, "knowledge_get_episodes");
    let all_episode_uuids = as_uuid_set(episodes_result, "episodes");
    assert_exact_uuid_enumeration(
        episodes_result,
        "episodes",
        expected["episode_count"].as_i64().unwrap(),
        "knowledge_get_episodes",
    );

    let passages_query = entity_queries[0]["query"].as_str().unwrap();
    let passages_resp = client.call_tool(
        "knowledge_search_passages",
        json!({"query": passages_query, "num_results": 20, "min_score": 0.0}),
    );
    let passages_result = structured(&passages_resp, "knowledge_search_passages");
    let passages = passages_result["passages"].as_array().unwrap();
    assert!(
        !passages.is_empty(),
        "knowledge_search_passages returned no passages for {passages_query:?}: {passages_result}"
    );
    for passage in passages {
        let uuid = passage["uuid"]
            .as_str()
            .expect("passage must have a string uuid");
        assert!(
            all_episode_uuids.contains(uuid),
            "passage uuid {uuid} is not among the fixture's real episode UUIDs"
        );
        assert!(
            !passage["content"].as_str().unwrap_or_default().is_empty(),
            "passage {uuid} has empty content"
        );
        let score = passage["score"]
            .as_f64()
            .expect("passage must have a score");
        assert!(
            score >= 0.0,
            "passage {uuid} score {score} below the requested min_score 0.0"
        );
    }

    // ── Acceptance Scenario 4 (FR-007): knowledge_get_entity_neighbors exact traversal ───
    let traversal = &expected["traversal"];
    assert!(!traversal.is_null(), "fixture must record a traversal path");

    let hop1_resp = client.call_tool(
        "knowledge_get_entity_neighbors",
        json!({
            "entity_uuid": traversal["start_entity_uuid"],
            "group_ids": [group_id.clone()],
            "num_results": 25,
        }),
    );
    let hop1 = structured(&hop1_resp, "knowledge_get_entity_neighbors (hop1)");
    let expected_hop1: BTreeSet<String> = traversal["expected_hop1_node_uuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        as_uuid_set(hop1, "nodes"),
        expected_hop1,
        "hop1 node set mismatch from {}",
        traversal["start_entity_name"]
    );

    let hop2_resp = client.call_tool(
        "knowledge_get_entity_neighbors",
        json!({
            "entity_uuid": traversal["hop1_entity_uuid"],
            "group_ids": [group_id.clone()],
            "num_results": 25,
        }),
    );
    let hop2 = structured(&hop2_resp, "knowledge_get_entity_neighbors (hop2)");
    let expected_hop2: BTreeSet<String> = traversal["expected_hop2_node_uuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        as_uuid_set(hop2, "nodes"),
        expected_hop2,
        "hop2 node set mismatch from {}",
        traversal["hop1_entity_name"]
    );

    // ── Acceptance Scenario 5 (FR-008): full-enumeration list_entities/list_relationships ─
    let entity_total = expected["entity_count"].as_i64().unwrap();
    let list_entities_resp = client.call_tool(
        "knowledge_list_entities",
        json!({"group_ids": [group_id.clone()], "num_results": entity_total + 100}),
    );
    let list_entities_result = structured(&list_entities_resp, "knowledge_list_entities");
    assert_exact_uuid_enumeration(
        list_entities_result,
        "nodes",
        entity_total,
        "knowledge_list_entities",
    );
    let all_entity_uuids = as_uuid_set(list_entities_result, "nodes");

    let relationship_total = expected["relationship_count"].as_i64().unwrap();
    let list_relationships_resp = client.call_tool(
        "knowledge_list_relationships",
        json!({"group_ids": [group_id.clone()], "num_results": relationship_total + 100}),
    );
    let list_relationships_result =
        structured(&list_relationships_resp, "knowledge_list_relationships");
    assert_exact_uuid_enumeration(
        list_relationships_result,
        "edges",
        relationship_total,
        "knowledge_list_relationships",
    );

    // ── Acceptance Scenario 6 (FR-009): get_entities_by_source, get_nodes_by_group, \
    //    get_edges_by_group, get_edges_by_uuids ──────────────────────────────────────────
    let first_episode = &episodes_result["episodes"].as_array().unwrap()[0];
    let source_description = first_episode["source_description"]
        .as_str()
        .expect("episode must have a source_description")
        .to_string();
    let by_source_resp = client.call_tool(
        "knowledge_get_entities_by_source",
        json!({
            "source": source_description,
            "group_ids": [group_id.clone()],
            "num_results": 1000,
        }),
    );
    let by_source_result = structured(&by_source_resp, "knowledge_get_entities_by_source");
    let by_source_nodes = by_source_result["nodes"].as_array().unwrap();
    assert!(
        !by_source_nodes.is_empty(),
        "knowledge_get_entities_by_source returned no entities for real source \
         {source_description:?}: {by_source_result}"
    );
    for uuid in as_uuid_set(by_source_result, "nodes") {
        assert!(
            all_entity_uuids.contains(&uuid),
            "knowledge_get_entities_by_source returned uuid {uuid} not present in the full \
             entity set"
        );
    }

    let nodes_by_group_resp = client.call_tool(
        "knowledge_get_nodes_by_group",
        json!({"group_ids": [group_id.clone()]}),
    );
    let nodes_by_group_result = structured(&nodes_by_group_resp, "knowledge_get_nodes_by_group");
    assert_exact_uuid_enumeration(
        nodes_by_group_result,
        "nodes",
        entity_total,
        "knowledge_get_nodes_by_group",
    );

    let edges_by_group_resp = client.call_tool(
        "knowledge_get_edges_by_group",
        json!({"group_ids": [group_id.clone()]}),
    );
    let edges_by_group_result = structured(&edges_by_group_resp, "knowledge_get_edges_by_group");
    assert_exact_uuid_enumeration(
        edges_by_group_result,
        "edges",
        relationship_total,
        "knowledge_get_edges_by_group",
    );

    let samples = expected["relation_type_samples"].as_array().unwrap();
    assert!(
        !samples.is_empty(),
        "fixture must record relation_type_samples"
    );
    let sample_uuids: Vec<Value> = samples.iter().map(|s| s["uuid"].clone()).collect();
    let edges_by_uuids_resp = client.call_tool(
        "knowledge_get_edges_by_uuids",
        json!({"uuids": sample_uuids}),
    );
    let edges_by_uuids_result = structured(&edges_by_uuids_resp, "knowledge_get_edges_by_uuids");
    let by_uuid: std::collections::HashMap<String, Value> = edges_by_uuids_result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["uuid"].as_str().unwrap().to_string(), e.clone()))
        .collect();
    assert_eq!(
        by_uuid.len(),
        samples.len(),
        "knowledge_get_edges_by_uuids returned {} edges for {} requested sample uuids",
        by_uuid.len(),
        samples.len()
    );
    for sample in samples {
        let uuid = sample["uuid"].as_str().unwrap();
        let actual = by_uuid
            .get(uuid)
            .unwrap_or_else(|| panic!("relationship {uuid} from relation_type_samples not found via knowledge_get_edges_by_uuids"));
        assert_eq!(
            actual["relation_type"], sample["relation_type"],
            "relation_type mismatch for {uuid} ({:?}): {actual}",
            sample["fact"]
        );
        assert_eq!(
            actual["fact"], sample["fact"],
            "fact text mismatch for {uuid}: {actual}"
        );
    }

    // ── Acceptance Scenario 7 (FR-011): --scope=read hides/rejects write/admin/cypher tools ─
    let tools = client.list_tools();
    let tool_names: HashSet<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains("knowledge_status"));
    assert!(!tool_names.contains("knowledge_add_episode"));
    assert!(!tool_names.contains("knowledge_query_cypher"));
    assert!(!tool_names.contains("knowledge_rebuild_from_wal"));

    let rejected = client.call_tool(
        "knowledge_add_episode",
        json!({"name": "x", "episode_body": "y"}),
    );
    assert!(
        rejected.get("error").is_some(),
        "expected a protocol-level error calling an unlisted write tool under --scope=read: \
         {rejected:?}"
    );

    // Read tools continue to work against the real data after the rejected call above.
    let status_again = client.call_tool("knowledge_status", json!({}));
    let status_again = structured(&status_again, "knowledge_status (after scope rejection)");
    assert_eq!(status_again["entity_count"], expected["entity_count"]);

    // ── Acceptance Scenario 8 (FR-012): missing required argument is a clean tool error ──
    let missing_arg_resp = client.call_tool("knowledge_get_entity_neighbors", json!({}));
    assert!(
        missing_arg_resp.get("result").is_some(),
        "expected a result, not a protocol error: {missing_arg_resp:?}"
    );
    assert_eq!(
        missing_arg_resp["result"]["isError"],
        json!(true),
        "expected a clean tool error for a missing required entity_uuid: {missing_arg_resp:?}"
    );

    client.shutdown();

    // Acceptance Scenario 9 (FR-014): verified environmentally, not by an in-process counter
    // (see this file's module doc comment) — this test process never sets ANTHROPIC_API_KEY
    // and `--embedder-http` above points only at this test's own loopback stub embedder, so no
    // outbound LLM or real-embedder network call is reachable anywhere in this suite, in either
    // the seeding subprocess (real_corpus.rs) or the read-path subprocess spawned here.
}
