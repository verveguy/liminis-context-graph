//! Integration tests for MCP progress-token bridging (FR-007, SC-005): a streaming tool
//! (`knowledge_rebuild_from_wal`) called with a progress token must surface at least one
//! `notifications/progress` line before the terminal `tools/call` result, in both standalone
//! and attached mode.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, McpClient};

fn wait_for_socket(socket_path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if socket_path.exists() && UnixStream::connect(socket_path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn socket_request(
    socket_path: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket_path).expect("connect to socket service");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    writeln!(stream, "{req}").expect("write request");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    serde_json::from_str(line.trim()).expect("parse response")
}

#[test]
fn standalone_rebuild_surfaces_progress_before_terminal_result() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = McpClient::spawn({
        let mut cmd = Command::new(binary_path());
        cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
            .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
            .args(["--mcp-stdio", "--embedder-http", &url]);
        cmd
    });
    client.initialize();

    // Write an episode so the WAL has at least one file/mutation to replay.
    let add = client.call_tool(
        "knowledge_add_episode",
        json!({"name": "ep1", "episode_body": "Alice met Bob at the cafe."}),
    );
    assert!(
        add["result"]["isError"].as_bool() != Some(true),
        "seed episode failed: {add:?}"
    );

    let resp = client.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({}),
        "progress-tok-1",
        Duration::from_secs(30),
    );
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "rebuild should succeed: {resp:?}"
    );
    assert!(
        client
            .stashed_notifications
            .iter()
            .any(|n| n["method"] == "notifications/progress"),
        "expected at least one progress notification before the terminal result, got: {:?}",
        client.stashed_notifications
    );

    client.shutdown();
}

#[test]
fn attached_rebuild_surfaces_bridged_progress() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let db_path = dir.path().join("test.db");
    let socket_path = dir.path().join("service.sock");
    let mut service: Child = Command::new(binary_path())
        .env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
        .args(["--embedder-http", &url])
        .spawn()
        .expect("failed to spawn socket service");
    assert!(wait_for_socket(&socket_path, Duration::from_secs(15)));

    // Seed WAL content directly over the socket.
    let add = socket_request(
        &socket_path,
        "knowledge_add_episode",
        json!({"name": "ep1", "episode_body": "Alice met Bob at the cafe."}),
    );
    assert!(add.get("error").is_none(), "seed episode failed: {add:?}");

    let mut mcp = McpClient::spawn({
        let mut cmd = Command::new(binary_path());
        cmd.args([
            "--mcp-stdio",
            "--connect",
            socket_path.to_str().unwrap(),
            "--scope=admin",
        ]);
        cmd
    });
    mcp.initialize();

    let resp = mcp.call_tool_with_progress(
        "knowledge_rebuild_from_wal",
        json!({}),
        "progress-tok-2",
        Duration::from_secs(30),
    );
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "attached rebuild should succeed: {resp:?}"
    );
    assert!(
        mcp.stashed_notifications
            .iter()
            .any(|n| n["method"] == "notifications/progress"),
        "expected at least one bridged progress notification, got: {:?}",
        mcp.stashed_notifications
    );

    mcp.shutdown();
    service.kill().ok();
    service.wait().ok();
}
