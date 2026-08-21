//! Process-level SC-001 verification for per-group ontology support (issue #446).
//!
//! `mcp_multistream_e2e.rs`'s harness is deliberately assert-only (`knowledge_assert_entity`/
//! `knowledge_assert_relationship`, FR-002 there) — and per this issue's own FR-011, direct-assert
//! is unaffected by ontology resolution, so that harness cannot exercise what SC-001 asks for
//! ("distinct entity/relation vocabularies... in extraction... results"). This suite instead
//! drives real `knowledge_add_episode` extraction over MCP-over-stdio, using the same
//! content-keyed stub-extractor harness `mcp_real_corpus_mutation_e2e.rs` established (#235) —
//! no live LLM, no `ANTHROPIC_API_KEY`.
//!
//! Two co-resident groups in one workspace, one process, one `LIMINIS_WORKSPACE_ROOT`:
//! - `catalog` has its own per-group ontology file (`mode: strict`, `Person` only).
//! - `content` has neither a per-group file nor a workspace-wide `.lcg/ontology.yaml`.
//!
//! The stub extractor returns the identical raw extraction (Alice/Person, Acme Corp/Organization,
//! `WORKS_AT`) for both groups' `knowledge_add_episode` calls — any difference in what's
//! persisted can only come from per-group ontology resolution, not from different extraction
//! input, which is what makes this a genuine zero-cross-group-leakage check rather than a
//! coincidence of two different inputs.
#![cfg(unix)]

use std::process::Command;

use serde_json::json;

mod common;
use common::{binary_path, spawn_stub_embedder, spawn_stub_extractor, ExtractionFixture};

fn structured(resp: serde_json::Value, context: &str) -> serde_json::Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "{context} errored: {resp:?}"
    );
    resp["result"]["structuredContent"].clone()
}

#[test]
fn per_group_ontology_zero_cross_group_leakage_over_mcp() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ontology_path = lcg_core::ontology::group_ontology_path(workspace.path(), "catalog")
        .expect("encode group_id");
    std::fs::create_dir_all(ontology_path.parent().unwrap()).unwrap();
    std::fs::write(
        &ontology_path,
        "mode: strict\nentity_types:\n  - name: Person\n",
    )
    .unwrap();
    // Deliberately no `.lcg/ontology.yaml` and no per-group file for "content" — this workspace
    // has no workspace-wide ontology at all, so if catalog's strict vocabulary ever leaked into
    // content, it could only be via a resolution bug, not a fallback that happens to agree.

    let embedder_port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{embedder_port}/v1/embeddings");
    let (extractor_port, extractor_config) = spawn_stub_extractor();
    let extractor_url = format!("http://127.0.0.1:{extractor_port}/v1/chat/completions");

    {
        let mut cfg = extractor_config.lock().unwrap();
        cfg.extraction.insert(
            "TRIGGER_EXTRACTION".to_string(),
            ExtractionFixture {
                entities: vec![
                    (
                        "Alice".to_string(),
                        "Person".to_string(),
                        "A person named Alice".to_string(),
                    ),
                    (
                        "Acme Corp".to_string(),
                        "Organization".to_string(),
                        "A company called Acme Corp".to_string(),
                    ),
                ],
                edges: vec![(
                    "Alice".to_string(),
                    "Acme Corp".to_string(),
                    "Alice works at Acme Corp".to_string(),
                    Some("WORKS_AT".to_string()),
                )],
            },
        );
    }

    let tmp = tempfile::tempdir().expect("db/wal tempdir");
    let db_path = tmp.path().join("test.db");
    let wal_dir = tmp.path().join("wal");

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .env("LIMINIS_WORKSPACE_ROOT", workspace.path().to_str().unwrap())
        .args(["--mcp-stdio", "--embedder-http", &embedder_url])
        .args(["--extractor-http", &extractor_url]);
    let mut client = common::McpClient::spawn(cmd);
    client.initialize();

    for group in ["catalog", "content"] {
        let resp = client.call_tool(
            "knowledge_add_episode",
            json!({
                "name": format!("{group}-episode"),
                "episode_body": "TRIGGER_EXTRACTION: Alice works at Acme Corp",
                "source": "text",
                "source_description": "test",
                "reference_time": "2026-01-01T00:00:00Z",
                "group_id": group,
            }),
        );
        structured(resp, &format!("add_episode {group}"));
    }

    let catalog_entities = structured(
        client.call_tool(
            "knowledge_list_entities",
            json!({"group_ids": ["catalog"], "num_results": 10}),
        ),
        "list_entities catalog",
    );
    let catalog_acme = catalog_entities["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == json!("Acme Corp"))
        .expect("Acme Corp must be persisted in group catalog");
    let catalog_labels: Vec<&str> = catalog_acme["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(
        catalog_labels.contains(&"Unclassified"),
        "group catalog's own strict {{Person}} ontology must reclassify Acme Corp: {catalog_labels:?}"
    );
    assert!(
        !catalog_labels.contains(&"Organization"),
        "the raw out-of-vocabulary type must not leak into catalog's labels: {catalog_labels:?}"
    );

    let content_entities = structured(
        client.call_tool(
            "knowledge_list_entities",
            json!({"group_ids": ["content"], "num_results": 10}),
        ),
        "list_entities content",
    );
    let content_acme = content_entities["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == json!("Acme Corp"))
        .expect("Acme Corp must be persisted in group content");
    let content_labels: Vec<&str> = content_acme["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(
        content_labels.contains(&"Organization"),
        "group content has no ontology at all (no per-group file, no workspace-wide file) and \
         must extract free-form, unaffected by group catalog's strict vocabulary: {content_labels:?}"
    );
    assert!(
        !content_labels.contains(&"Unclassified"),
        "group catalog's strict mode must not leak into group content: {content_labels:?}"
    );

    client.shutdown();
}
