//! Integration test: standalone MCP `knowledge_close` / SIGTERM → clean exit → clean DB
//! re-open. Mirrors `clean_shutdown.rs`'s Unix-socket version, applied to `--mcp-stdio`
//! standalone mode (FR-005's "standalone mode shuts down only this process's own DB
//! connection" + the graceful-shutdown parity fix so SIGTERM also checkpoints cleanly).
#![cfg(unix)]

use std::process::Command;
use std::time::Duration;

use lcg_core::db::Db;
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, McpClient};

fn spawn_standalone(dir: &TempDir, embedder_url: &str) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
        .args([
            "--mcp-stdio",
            "--embedder-http",
            embedder_url,
            "--scope=admin",
        ]);
    McpClient::spawn(cmd)
}

#[test]
fn knowledge_close_produces_clean_exit_and_no_wal_corruption() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url);
    client.initialize();

    // Write something to the WAL first, as clean_shutdown.rs does over the socket.
    let build = client.call_tool("knowledge_build_indices", json!({}));
    assert!(
        build["result"]["isError"].as_bool() != Some(true),
        "knowledge_build_indices should succeed: {build:?}"
    );

    let close = client.call_tool("knowledge_close", json!({}));
    assert!(
        close["result"]["isError"].as_bool() != Some(true),
        "knowledge_close should succeed: {close:?}"
    );

    let status = client.wait_for_exit(Duration::from_secs(10));
    assert_eq!(
        status.code(),
        Some(0),
        "expected exit code 0 after knowledge_close"
    );

    let db_result = Db::open(db_path.to_str().unwrap());
    assert!(
        db_result.is_ok(),
        "DB re-open failed after clean shutdown — possible WAL corruption: {:?}",
        db_result.err()
    );
}

#[test]
fn sigterm_produces_clean_exit_and_no_wal_corruption() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut client = spawn_standalone(&dir, &url);
    client.initialize();

    let build = client.call_tool("knowledge_build_indices", json!({}));
    assert!(build["result"]["isError"].as_bool() != Some(true));

    Command::new("kill")
        .args(["-TERM", &client.pid().to_string()])
        .status()
        .expect("kill command failed");

    let status = client.wait_for_exit(Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "expected exit code 0 after SIGTERM");

    let db_result = Db::open(db_path.to_str().unwrap());
    assert!(
        db_result.is_ok(),
        "DB re-open failed after SIGTERM shutdown — possible WAL corruption: {:?}",
        db_result.err()
    );
}
