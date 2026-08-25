//! Integration tests for #208: HNSW/FTS indices are built eagerly at startup, before the
//! socket accepts any request, so `knowledge_status` never lies about `indices_built` and
//! ingest never discovers a missing `entity_name_embedding_idx` mid-chunk.
//!
//! Spawns the real compiled binary and drives it over the Unix-socket JSON-RPC protocol,
//! mirroring the pattern in `clean_shutdown.rs`/`migration_binary.rs`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use lcg_core::db::Db;
use tempfile::TempDir;

mod common;
use common::{binary_path, spawn_stub_embedder, ChildGuard};

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

/// Sends a single JSON-RPC request over a fresh connection to `socket_path` and returns the
/// parsed response. Used to make `knowledge_status` the very first request the service ever
/// sees, so a `true` result can only come from the eager startup build (FR-001/FR-002), not
/// from some other test-harness request having triggered the lazy auto-heal path first.
fn send_request(
    socket_path: &PathBuf,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket_path).expect("failed to connect to service socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    writeln!(stream, "{request}").expect("failed to write request");

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .expect("failed to read response");
    serde_json::from_str(&response)
        .unwrap_or_else(|e| panic!("bad JSON response {response:?}: {e}"))
}

/// Writes garbage bytes to `{db_path}.wal` to simulate a torn WAL tail — the same corruption
/// shape `auto_recovery.rs`'s `corrupt_lbug_wal` helper produces, triggering the recoverable
/// classification in `bootstrap_app_state` (ADR-0009) and the startup self-recovery sequence.
fn corrupt_lbug_wal(db_path: &std::path::Path) {
    let wal_path = format!("{}.wal", db_path.to_str().unwrap());
    std::fs::write(&wal_path, b"CORRUPT_WAL_TAIL_NOT_VALID_LBUG_WAL").unwrap();
}

/// SC-003 scenario 1: fresh, never-opened DB → first request of the session is
/// `knowledge_status` → `indices_built: true` (FR-001, FR-008).
#[test]
fn fresh_startup_reports_indices_built_true() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let socket_path = dir.path().join("service.sock");
    let wal_dir = dir.path().join("wal");

    let embedder_port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{embedder_port}/v1/embeddings");

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .args(["--embedder-http", &embedder_url])
        // No ANTHROPIC_API_KEY/sidecar/LCG_EXTRACTION_URL in the CI test environment: an
        // explicit --extractor-http avoids the FR-011 fatal-startup error. Never dialed in
        // these tests (no extraction tool is called).
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"]);
    // ChildGuard ensures the spawned process is killed and reaped even if this test panics
    // before reaching the explicit cleanup below (#500) — e.g. `send_request`'s `.expect()`s.
    let child = ChildGuard::spawn(cmd);

    if !wait_for_socket(&socket_path, Duration::from_secs(15)) {
        panic!("service did not become ready within 15s");
    }

    let response = send_request(&socket_path, "knowledge_status", serde_json::json!({}));
    drop(child);

    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("expected a successful result, got: {response}"));
    assert_eq!(
        result.get("indices_built"),
        Some(&serde_json::json!(true)),
        "indices_built should be true immediately after fresh startup, got: {response}"
    );
}

/// SC-003 scenario 3: startup self-recovery (torn lbug WAL) completes → `indices_built: true`
/// (FR-002, FR-008). `run_full_recovery_sequence` already builds indices (predates #208); this
/// proves the fix that `bootstrap_app_state` now reflects that build in `AppState.indices_built`
/// instead of hardcoding `false`.
#[test]
fn post_recovery_startup_reports_indices_built_true() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let socket_path = dir.path().join("service.sock");
    let wal_dir = dir.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();

    // Pre-create a DB and schema (dim=768 to match the stub embedder's response shape below),
    // then corrupt the lbug WAL to force the startup recoverable-error → self-recovery path.
    {
        let db = Db::open(db_path.to_str().unwrap()).unwrap();
        let conn = db.connect().unwrap();
        conn.init_schema(768).unwrap();
    }
    corrupt_lbug_wal(&db_path);

    let embedder_port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{embedder_port}/v1/embeddings");

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .args(["--embedder-http", &embedder_url])
        // No ANTHROPIC_API_KEY/sidecar/LCG_EXTRACTION_URL in the CI test environment: an
        // explicit --extractor-http avoids the FR-011 fatal-startup error. Never dialed in
        // these tests (no extraction tool is called).
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"]);
    // ChildGuard ensures the spawned process is killed and reaped even if this test panics
    // before reaching the explicit cleanup below (#500) — e.g. `send_request`'s `.expect()`s.
    let child = ChildGuard::spawn(cmd);

    if !wait_for_socket(&socket_path, Duration::from_secs(15)) {
        panic!("service did not become ready within 15s");
    }

    let response = send_request(&socket_path, "knowledge_status", serde_json::json!({}));
    drop(child);

    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("expected a successful result, got: {response}"));
    assert_eq!(
        result.get("indices_built"),
        Some(&serde_json::json!(true)),
        "indices_built should be true after post-recovery startup, got: {response}"
    );
}
