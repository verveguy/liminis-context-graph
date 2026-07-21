//! Integration tests for attached MCP-over-stdio mode (SC-002, SC-004): the MCP process
//! forwards calls over a Unix socket to an already-running service instead of opening the DB
//! itself, so it can coexist with another process (e.g. the Liminis app) without contending
//! for lbug's single-writer lock.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, McpClient};

fn wait_for_socket(socket_path: &PathBuf, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket_path.exists() && UnixStream::connect(socket_path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Spawns the socket service (not MCP) — the "already-running app instance" this issue's
/// attached mode is designed to coexist with.
fn spawn_socket_service(dir: &TempDir, embedder_url: &str) -> (Child, PathBuf) {
    let db_path = dir.path().join("test.db");
    let socket_path = dir.path().join("service.sock");

    let child = Command::new(binary_path())
        .env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
        .args(["--embedder-http", embedder_url])
        .spawn()
        .expect("failed to spawn socket service");

    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(15)),
        "socket service did not become ready"
    );
    (child, socket_path)
}

fn socket_request(
    socket_path: &PathBuf,
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

fn spawn_attached(socket_path: &PathBuf, extra_args: &[&str]) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--mcp-stdio", "--connect", socket_path.to_str().unwrap()]);
    cmd.args(extra_args);
    McpClient::spawn(cmd)
}

#[test]
fn attached_mode_matches_socket_result_with_no_lock_conflict() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let (mut service, socket_path) = spawn_socket_service(&dir, &url);

    let mut mcp = spawn_attached(&socket_path, &[]);
    mcp.initialize();

    let mcp_resp = mcp.call_tool("knowledge_status", json!({}));
    assert!(
        mcp_resp["result"]["isError"].as_bool() != Some(true),
        "MCP call should not report a lock conflict: {mcp_resp:?}"
    );
    let mcp_structured = &mcp_resp["result"]["structuredContent"];

    let socket_resp = socket_request(&socket_path, "knowledge_status", json!({}));
    assert!(
        socket_resp.get("error").is_none(),
        "socket call should not error: {socket_resp:?}"
    );
    let socket_result = &socket_resp["result"];

    assert_eq!(
        mcp_structured["entity_count"], socket_result["entity_count"],
        "MCP and direct-socket results should be equivalent"
    );
    assert_eq!(mcp_structured["connected"], json!(true));
    assert_eq!(socket_result["connected"], json!(true));

    mcp.shutdown();
    service.kill().ok();
    service.wait().ok();
}

#[test]
fn attached_mode_omits_close_without_allow_remote_close() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let (mut service, socket_path) = spawn_socket_service(&dir, &url);

    let mut mcp = spawn_attached(&socket_path, &["--scope=admin"]);
    mcp.initialize();
    let tools = mcp.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        !names.contains(&"knowledge_close"),
        "knowledge_close must be entirely absent from tools/list in attached mode without --allow-remote-close, got {names:?}"
    );

    // Calling it anyway must still be rejected cleanly (unknown tool), not silently ignored.
    let resp = mcp.call_tool("knowledge_close", json!({}));
    assert!(
        resp.get("error").is_some(),
        "expected a protocol error for a hidden tool: {resp:?}"
    );

    mcp.shutdown();
    service.kill().ok();
    service.wait().ok();
}

#[test]
fn attached_mode_with_allow_remote_close_forwards_shutdown() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let (mut service, socket_path) = spawn_socket_service(&dir, &url);

    let mut mcp = spawn_attached(&socket_path, &["--scope=admin", "--allow-remote-close"]);
    mcp.initialize();
    let tools = mcp.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"knowledge_close"),
        "expected knowledge_close with --allow-remote-close"
    );

    let resp = mcp.call_tool("knowledge_close", json!({}));
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "forwarded close should succeed: {resp:?}"
    );

    // The forwarded close shuts down the *remote* socket service, not this MCP process.
    let status = wait_for_exit(&mut service, Duration::from_secs(10));
    assert!(
        status.is_some(),
        "remote socket service did not exit after forwarded knowledge_close"
    );
    assert_eq!(status.unwrap().code(), Some(0));

    // The MCP process itself is unaffected — per the issue's Assumptions, a further call
    // simply surfaces a normal connection-failure tool error, not a crash.
    mcp.shutdown();
}
