//! Regression coverage for #331 (FR-007): a missing extraction provider must not prevent
//! startup, in either socket or `--mcp-stdio` mode, at any scope — only extraction-dependent
//! calls should fail, and only at call time.
//!
//! Every test here deliberately omits `ANTHROPIC_API_KEY`, `--extractor-uds`/`--extractor-http`,
//! and `LCG_EXTRACTION_URL` (`.env_remove`s the first and third defensively, in case a
//! developer's shell happens to export them — CI itself never sets them, per the same
//! environmental assumption `mcp_stdio.rs`/`clean_shutdown.rs`/etc. already rely on). This is the
//! "orac" read-only-consumer scenario from #330: an embedder is configured, nothing else is.

#![cfg(unix)]

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, McpClient};

/// Spawns `--mcp-stdio` with an embedder configured and no extraction provider at all.
fn spawn_unconfigured(dir: &TempDir, embedder_url: &str, extra_args: &[&str]) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env(
            "LCG_SOCKET_PATH",
            dir.path().join("unused.sock").to_str().unwrap(),
        )
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("LCG_EXTRACTION_URL")
        .args(["--mcp-stdio", "--embedder-http", embedder_url])
        .args(extra_args);
    McpClient::spawn(cmd)
}

/// Writes a single minimal WAL line (`CREATE (:Episodic {uuid: $uuid})`) — the same
/// hand-crafted-fixture pattern `crates/core/src/recovery.rs`'s test helpers use — so
/// `knowledge_rebuild_from_wal` has something to replay without depending on the large
/// real-corpus fixture or any extraction/embedding call having ever produced it.
fn write_minimal_wal_file(wal_dir: &std::path::Path) {
    std::fs::create_dir_all(wal_dir).expect("create wal dir");
    let line = "{\"seq\":0,\"ts\":\"2026-01-01T00:00:00Z\",\"db\":\"test\",\
                \"cypher\":\"CREATE (:Episodic {uuid: $uuid, name: 'n', group_id: 'g', \
                created_at: timestamp('2026-01-01'), source: 'text', source_description: '', \
                content: 'c', valid_at: timestamp('2026-01-01')})\",\
                \"params\":{\"uuid\":\"wal-fixture-episode\"}}\n";
    let mut f = std::fs::File::create(wal_dir.join("0.jsonl")).expect("create wal file");
    f.write_all(line.as_bytes()).expect("write wal fixture");
}

/// SC-001 / Acceptance Scenarios 1-2: starts with no provider, serves reads, and completes
/// `knowledge_rebuild_from_wal` — pure Cypher replay, no extraction anywhere in the path.
#[test]
fn starts_with_no_provider_and_completes_wal_rebuild() {
    let dir = TempDir::new().unwrap();
    write_minimal_wal_file(&dir.path().join("wal"));
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_unconfigured(&dir, &url, &[]);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "knowledge_status should succeed with no extraction provider configured: {status:?}"
    );

    let rebuild = client.call_tool("knowledge_rebuild_from_wal", json!({}));
    assert!(
        rebuild["result"]["isError"].as_bool() != Some(true),
        "knowledge_rebuild_from_wal should succeed with no extraction provider configured \
         (it never calls the extractor): {rebuild:?}"
    );
    assert_eq!(
        rebuild["result"]["structuredContent"]["success"],
        json!(true),
        "{rebuild:?}"
    );

    let status = client.call_tool("knowledge_status", json!({}));
    assert_eq!(
        status["result"]["structuredContent"]["entity_count"],
        json!(0),
        "the minimal WAL fixture creates one Episodic node and zero entities: {status:?}"
    );

    client.shutdown();
}

/// SC-002 / Acceptance Scenario 3: `--mcp-stdio --scope read` starts and advertises its read
/// tools, with no extraction provider configured at all.
#[test]
fn scope_read_starts_with_no_provider() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_unconfigured(&dir, &url, &["--scope=read"]);
    client.initialize();

    let tools = client.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"knowledge_status"),
        "read scope should still advertise knowledge_status: {names:?}"
    );
    assert!(
        !names.contains(&"knowledge_process_chunk"),
        "read scope should not advertise any extraction-touching tool: {names:?}"
    );

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "knowledge_status should succeed: {status:?}"
    );

    client.shutdown();
}

/// SC-003 / Acceptance Scenarios (User Story 2): `knowledge_process_chunk` fails with a clear,
/// actionable error when no provider is configured — the process stays up and reads keep working
/// afterward, rather than a panic, a silent no-op, or the process having refused to start at all.
#[test]
fn extraction_call_fails_cleanly_and_process_keeps_serving_reads() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    // Default scope (all) so both the write-scoped extraction tool and read tools are callable
    // from one client.
    let mut client = spawn_unconfigured(&dir, &url, &[]);
    client.initialize();

    let resp = client.call_tool(
        "knowledge_process_chunk",
        json!({
            "chunk_text": "some text",
            "chunk_id": "chunk-1",
            "source_file": "test.md",
        }),
    );
    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "knowledge_process_chunk should fail cleanly with no extraction provider: {resp:?}"
    );
    let message = resp["result"]["structuredContent"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("ANTHROPIC_API_KEY") && message.contains("extractor"),
        "error should name what to configure (the FR-002 guidance): {message:?}"
    );

    // The process is still alive and serving reads — not a panic, not an exit.
    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "knowledge_status should still succeed after a failed extraction call: {status:?}"
    );

    client.shutdown();
}
