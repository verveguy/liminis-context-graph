//! Integration tests for attached MCP-over-stdio mode (SC-002, SC-004): the MCP process
//! forwards calls over a Unix socket to an already-running service instead of opening the DB
//! itself, so it can coexist with another process (e.g. the Liminis app) without contending
//! for lbug's single-writer lock.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, McpClient};

fn wait_for_socket(socket_path: &Path, timeout: Duration) -> bool {
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
    socket_path: &Path,
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

fn spawn_attached(socket_path: &Path, extra_args: &[&str]) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.args(["--mcp-stdio", "--connect", socket_path.to_str().unwrap()]);
    cmd.args(extra_args);
    McpClient::spawn(cmd)
}

/// Spawns a stub Unix socket "remote service" that accepts one connection, reads (and discards)
/// the request line, then hangs forever without responding — simulating a remote service that
/// crashed or hung mid-call, for the attached-mode read-timeout regression test.
fn spawn_hanging_remote(dir: &TempDir) -> PathBuf {
    let socket_path = dir.path().join("hanging.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind hanging-remote stub socket");
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            // Never respond — the client's read loop should time out rather than block forever.
            std::thread::park();
        }
    });
    socket_path
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

/// Spawns a stub Unix socket "remote service" that reads one request, waits past the client's
/// call timeout before replying to it (a stale reply for an already-abandoned call), then reads
/// a second request and replies to it correctly and promptly — for the stale-response
/// misdelivery regression test.
fn spawn_stale_response_remote(dir: &TempDir, reply_delay: Duration) -> PathBuf {
    let socket_path = dir.path().join("stale.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind stale-response stub socket");
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().expect("clone stub stream"));
            let mut writer = stream;

            let mut line1 = String::new();
            reader.read_line(&mut line1).expect("read first request");
            let req1: serde_json::Value = serde_json::from_str(line1.trim()).unwrap();
            let id1 = req1["id"].clone();

            std::thread::sleep(reply_delay);
            let stale = json!({"jsonrpc": "2.0", "id": id1, "result": {"marker": "STALE-SHOULD-NOT-BE-SEEN"}});
            writeln!(writer, "{stale}").expect("write stale response");

            let mut line2 = String::new();
            reader.read_line(&mut line2).expect("read second request");
            let req2: serde_json::Value = serde_json::from_str(line2.trim()).unwrap();
            let id2 = req2["id"].clone();
            let correct =
                json!({"jsonrpc": "2.0", "id": id2, "result": {"marker": "CALL-2-CORRECT"}});
            writeln!(writer, "{correct}").expect("write correct response");
        }
    });
    socket_path
}

#[test]
fn attached_mode_stale_response_after_timeout_is_not_misdelivered_to_next_call() {
    let dir = TempDir::new().unwrap();
    // The stub's reply to call 1 lands after the 600ms call timeout (so call 1 genuinely times
    // out and releases the mutex) but before call 2's own 600ms read window closes (so call 2's
    // read loop is the one that observes the stale line and must discard it).
    let socket_path = spawn_stale_response_remote(&dir, Duration::from_millis(850));

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_ATTACHED_CALL_TIMEOUT_MS", "600");
    cmd.args(["--mcp-stdio", "--connect", socket_path.to_str().unwrap()]);
    let mut mcp = McpClient::spawn(cmd);
    mcp.initialize();

    let resp1 = mcp.call_tool("knowledge_status", json!({}));
    assert_eq!(
        resp1["result"]["isError"],
        json!(true),
        "expected call 1 to time out before the stub replies: {resp1:?}"
    );

    let resp2 = mcp.call_tool("knowledge_status", json!({}));
    assert_eq!(
        resp2["result"]["isError"].as_bool(),
        Some(false),
        "expected call 2 to succeed with its own response: {resp2:?}"
    );
    assert_eq!(
        resp2["result"]["structuredContent"]["marker"],
        json!("CALL-2-CORRECT"),
        "call 2 must receive its own response, not call 1's stale reply arriving late \
         on the same connection: {resp2:?}"
    );

    mcp.shutdown();
}

#[test]
fn attached_mode_call_times_out_on_hung_remote_instead_of_blocking_forever() {
    let dir = TempDir::new().unwrap();
    let socket_path = spawn_hanging_remote(&dir);

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_ATTACHED_CALL_TIMEOUT_MS", "500");
    cmd.args(["--mcp-stdio", "--connect", socket_path.to_str().unwrap()]);
    let mut mcp = McpClient::spawn(cmd);
    mcp.initialize();

    let start = Instant::now();
    let resp = mcp.call_tool("knowledge_status", json!({}));
    let elapsed = start.elapsed();

    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "expected a clean tool error when the attached call times out: {resp:?}"
    );
    let message = resp["result"]["structuredContent"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("timed out"),
        "expected a timeout-specific error message: {resp:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "expected the call to fail quickly via timeout rather than block, took {elapsed:?}"
    );

    mcp.shutdown();
}
