//! Integration tests for standalone MCP-over-stdio mode (SC-001, SC-003).
//!
//! Spawns the compiled binary with `--mcp-stdio` and drives it over stdin/stdout with
//! newline-delimited JSON-RPC, mirroring how `clean_shutdown.rs`/`migration_binary.rs` spawn
//! the binary and drive it over the Unix-socket protocol.
#![cfg(unix)]

use std::process::Command;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, McpClient};

fn spawn_standalone(dir: &TempDir, embedder_url: &str, extra_args: &[&str]) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env(
            "LCG_SOCKET_PATH",
            dir.path().join("unused.sock").to_str().unwrap(),
        )
        .args(["--mcp-stdio", "--embedder-http", embedder_url])
        .args(extra_args);
    McpClient::spawn(cmd)
}

#[test]
fn standalone_lists_and_calls_read_and_write_tools() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &[]);
    client.initialize();

    let tools = client.list_tools();
    assert_eq!(
        tools.len(),
        33,
        "default --scope=all should advertise all 33 tools"
    );
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"knowledge_status"));
    assert!(names.contains(&"knowledge_add_episode"));
    assert!(names.contains(&"knowledge_query_cypher"));

    // Read tool.
    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "knowledge_status should not error: {status:?}"
    );
    assert_eq!(
        status["result"]["structuredContent"]["context_graph_initialized"],
        json!(true)
    );

    // Write tool. `knowledge_clear_all` is used here rather than `knowledge_add_episode`
    // because episode ingestion calls the real Anthropic extraction API with no test-side
    // stub hook (unlike the embedder, which supports `--embedder-http` pointing at a stub) —
    // it would make this test depend on network access and a valid ANTHROPIC_API_KEY.
    // `knowledge_clear_all` is a genuine write-scope mutation with no such dependency.
    let clear = client.call_tool("knowledge_clear_all", json!({"confirm": true}));
    assert!(
        clear["result"]["isError"].as_bool() != Some(true),
        "knowledge_clear_all should not error: {clear:?}"
    );
    assert_eq!(clear["result"]["structuredContent"]["success"], json!(true));

    client.shutdown();
}

#[test]
fn call_tool_with_missing_required_argument_is_a_clean_tool_error() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &[]);
    client.initialize();

    // knowledge_get_entity_neighbors requires entity_uuid; omit it.
    let resp = client.call_tool("knowledge_get_entity_neighbors", json!({}));
    // Handled by the core dispatch's own validation (FR-008): a clean tool-level error,
    // not a crash or an opaque transport failure.
    assert!(
        resp.get("result").is_some(),
        "expected a result, not a protocol error: {resp:?}"
    );
    assert_eq!(resp["result"]["isError"], json!(true));

    client.shutdown();
}

#[test]
fn call_tool_with_missing_required_argument_never_reaches_the_handler() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &[]);
    client.initialize();

    // `knowledge_delete_episode`'s handler in handlers.rs (out of scope for this issue) does
    // not itself validate `episode_uuid` — it falls back to an empty string and the underlying
    // delete matches nothing, returning a misleading "deleted" success. The MCP layer's own
    // required-argument check must catch this before it ever reaches the backend.
    let resp = client.call_tool("knowledge_delete_episode", json!({}));
    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "expected a clean tool error for a missing required argument: {resp:?}"
    );
    let message = resp["result"]["structuredContent"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("episode_uuid"),
        "expected the error to name the missing field: {resp:?}"
    );

    // `knowledge_query_cypher` similarly falls back to an empty query string rather than
    // erroring in the handler.
    let resp = client.call_tool("knowledge_query_cypher", json!({}));
    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "expected a clean tool error for a missing required argument: {resp:?}"
    );

    client.shutdown();
}

#[test]
fn scope_read_advertises_only_read_tools_and_rejects_write() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &["--scope=read"]);
    client.initialize();

    let tools = client.list_tools();
    assert_eq!(
        tools.len(),
        14,
        "read scope should advertise exactly 14 tools"
    );
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"knowledge_status"));
    assert!(!names.contains(&"knowledge_add_episode"));
    assert!(!names.contains(&"knowledge_query_cypher"));
    assert!(!names.contains(&"knowledge_build_indices"));

    // Calling an out-of-scope tool must be rejected cleanly, not silently ignored.
    let resp = client.call_tool(
        "knowledge_add_episode",
        json!({"name": "x", "episode_body": "y"}),
    );
    assert!(
        resp.get("error").is_some(),
        "expected a protocol-level error for an unlisted tool: {resp:?}"
    );

    let resp = client.call_tool(
        "knowledge_query_cypher",
        json!({"query": "MATCH (n) RETURN n"}),
    );
    assert!(
        resp.get("error").is_some(),
        "expected a protocol-level error for an unlisted tool: {resp:?}"
    );

    client.shutdown();
}

#[test]
fn scope_admin_advertises_wal_lifecycle_tools() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &["--scope=admin"]);
    client.initialize();

    let tools = client.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 7);
    for expected in [
        "knowledge_dump_wal",
        "knowledge_prepare_checkpoint",
        "knowledge_rebuild_from_wal",
        "knowledge_recover",
        "knowledge_recover_full",
        "knowledge_close",
        "knowledge_build_indices",
    ] {
        assert!(
            names.contains(&expected),
            "expected {expected} under admin scope, got {names:?}"
        );
    }

    client.shutdown();
}

#[test]
fn scope_cypher_advertises_only_query_cypher() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &["--scope=cypher"]);
    client.initialize();

    let tools = client.list_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], json!("knowledge_query_cypher"));

    client.shutdown();
}

#[test]
fn scope_union_advertises_both_sets() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url, &["--scope=read,admin"]);
    client.initialize();

    let tools = client.list_tools();
    assert_eq!(tools.len(), 21, "read(14) + admin(7) = 21");

    client.shutdown();
}

#[test]
fn standalone_mode_always_advertises_close_regardless_of_allow_remote_close() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    // --allow-remote-close has no effect in standalone mode (edge case in the spec).
    let mut client = spawn_standalone(&dir, &url, &["--scope=admin", "--allow-remote-close"]);
    client.initialize();
    let tools = client.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"knowledge_close"));
    client.shutdown();
}

#[test]
fn unrecognized_scope_fails_fast_at_startup() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .args(["--mcp-stdio", "--embedder-http", &url, "--scope=bogus"]);
    let output = cmd.output().expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected a non-zero exit for an unrecognized scope"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bogus"),
        "stderr should mention the bad scope value: {stderr}"
    );
}

#[test]
fn connect_to_nonexistent_socket_fails_fast_not_hang() {
    let dir = TempDir::new().unwrap();
    let missing_socket = dir.path().join("does-not-exist.sock");

    let mut cmd = Command::new(binary_path());
    cmd.args(["--mcp-stdio", "--connect", missing_socket.to_str().unwrap()]);
    let start = std::time::Instant::now();
    let output = cmd.output().expect("failed to run binary");
    let elapsed = start.elapsed();

    assert!(
        !output.status.success(),
        "expected a non-zero exit for a missing socket"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "expected a fast failure, took {elapsed:?}"
    );
}
