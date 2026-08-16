//! Integration tests for MCP progress-token bridging (FR-007, SC-005): a streaming tool
//! (`knowledge_rebuild_from_wal`) called with a progress token must surface at least one
//! `notifications/progress` line before the terminal `tools/call` result, in both standalone
//! and attached mode.
#![cfg(unix)]

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

/// Seeds the WAL directory with a single `.jsonl` file directly on disk, rather than via a
/// real write call. `WalReplayer::replay_opts` fires its per-file progress event as soon as it
/// starts reading a `.jsonl` file — before parsing any line inside it — so an empty file is
/// enough to exercise progress bridging (FR-007) without depending on a live extraction LLM
/// call (`knowledge_add_episode`/`knowledge_process_chunk` both call the real Anthropic API,
/// with no test-side stub hook, unlike the embedder's `--embedder-http`).
///
/// Also mints a generation directly (issue #414): `main.rs` backfills the default group's
/// `applied_seq` at every process startup, before any client request — so by the time this
/// test's single `knowledge_rebuild_from_wal` call runs, a position is already recorded. A
/// real (unminted) fixture would trip FR-002's unknown-generation refusal on a concern this
/// test isn't exercising (progress-notification bridging, not WAL generation identity).
fn seed_wal_file(wal_dir: &std::path::Path) {
    std::fs::create_dir_all(wal_dir).expect("create wal dir");
    std::fs::write(wal_dir.join("0000000000000-seed.jsonl"), "").expect("write seed wal file");
    lcg_core::wal_generation::ensure_generation(wal_dir)
        .expect("mint a generation for the fixture");
}

#[test]
fn standalone_rebuild_surfaces_progress_before_terminal_result() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");
    let wal_dir = dir.path().join("wal");
    seed_wal_file(&wal_dir);

    let mut client = McpClient::spawn({
        let mut cmd = Command::new(binary_path());
        cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
            .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
            .args(["--mcp-stdio", "--embedder-http", &url])
            // No ANTHROPIC_API_KEY/sidecar/LCG_EXTRACTION_URL in the CI test environment: an
            // explicit --extractor-http avoids the FR-011 fatal-startup error. Never dialed in
            // this test (no extraction tool is called).
            .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"]);
        cmd
    });
    client.initialize();

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
    let wal_dir = dir.path().join("wal");
    seed_wal_file(&wal_dir);

    let db_path = dir.path().join("test.db");
    let socket_path = dir.path().join("service.sock");
    let mut service: Child = Command::new(binary_path())
        .env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
        .args(["--embedder-http", &url])
        // No ANTHROPIC_API_KEY/sidecar/LCG_EXTRACTION_URL in the CI test environment: an
        // explicit --extractor-http avoids the FR-011 fatal-startup error. Never dialed in
        // this test (no extraction tool is called).
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
        .spawn()
        .expect("failed to spawn socket service");
    assert!(wait_for_socket(&socket_path, Duration::from_secs(15)));

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
