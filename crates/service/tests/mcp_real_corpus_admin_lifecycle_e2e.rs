//! MCP admin/lifecycle e2e suite over the real-corpus WAL fixture (issue #236) —
//! process-lifecycle half: degraded-mode startup/recovery (ADR-0009), `knowledge_close`
//! checkpoint semantics (ADR-0017) and attached-mode `--allow-remote-close` gating, and
//! admin-scope tool-visibility gating. The data-plane half (dump/rebuild round-trips, progress
//! notifications, checkpoint coherence, index rebuild) lives in
//! `mcp_real_corpus_admin_data_e2e.rs`.
//!
//! **User Story 3 is this suite's highest-consequence coverage**: it is the direct regression
//! guard for #207 (a real recovery attempt that couldn't be completed because the relevant
//! tools weren't discoverable, and a `relation_type` reset that turned out to be unrecoverable).
//! Regression here would mean the recovery path this suite exists to validate silently doesn't
//! work at real scale.
//!
//! **Corruption mechanism (Plan's Key Decision)**: a garbage-byte write to `<db_path>.wal` (the
//! existing small-scale `auto_recovery.rs` technique) does not work here — lbug's
//! checkpoint-drop step trivially renames it aside and reopens, so `main.rs`'s autonomous
//! startup self-recovery (ADR-0026) would silently succeed and degraded mode would never be
//! observed. Instead, this suite chmod's the *directory containing* a forked `db_path` to
//! read-only (`0o555`) before spawning — this defeats both the checkpoint-drop rename and the
//! full-rebuild fallback's `remove_dir_all`, so the process genuinely falls through to degraded
//! mode, while the *application* WAL (`wal_dir`, FR-007's "intact" requirement) is left
//! completely untouched. See `corrupt_via_readonly_dir`'s doc comment for the full mechanics.
//!
//! **Isolation (FR-014)**: every corruption/removal/shutdown scenario below runs against its own
//! `SeededWorkspace::fork()` — never the shared `base` workspace, and never a real developer's
//! `.lcg/` (the corruption helper only ever chmod's a subdirectory freshly created inside a
//! `tempfile::TempDir`, so it is structurally incapable of targeting anything else).
//!
//! **Runtime budget**: because the induced corruption never touches on-disk *bytes* (only
//! directory permission bits, restored before recovery is invoked), `knowledge_recover`'s
//! `drop_lbug_wal` strategy and `knowledge_recover_full`'s own checkpoint-drop-first branch both
//! resolve via the cheap checkpoint-reopen path — no full WAL replay is required anywhere in
//! this file, so it runs in well under a minute of real work, not the "5+ full rebuild cycles"
//! a naive corruption mechanism would have required.
#![cfg(unix)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod common;
use common::real_corpus::{seed_real_corpus_workspace, SeededWorkspace};
use common::{binary_path, spawn_stub_embedder, McpClient};

/// The seven admin tool names FR-013 requires be both absent from `tools/list` and rejected
/// when called anyway, under a non-admin scope.
const ADMIN_TOOL_NAMES: &[&str] = &[
    "knowledge_dump_wal",
    "knowledge_prepare_checkpoint",
    "knowledge_rebuild_from_wal",
    "knowledge_recover",
    "knowledge_recover_full",
    "knowledge_close",
    "knowledge_build_indices",
];

fn structured<'a>(resp: &'a Value, context: &str) -> &'a Value {
    assert!(
        resp["result"]["isError"].as_bool() != Some(true),
        "{context} errored: {resp:?}"
    );
    &resp["result"]["structuredContent"]
}

fn spawn_at(db_path: &Path, wal_dir: &Path, embedder_url: &str, extra_args: &[&str]) -> McpClient {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .args(["--mcp-stdio", "--embedder-http", embedder_url])
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
        .args(extra_args);
    McpClient::spawn(cmd)
}

/// Restores a chmod'd-read-only directory to writable on drop — a safety net so a test panic
/// between `corrupt_via_readonly_dir` and its explicit `restore_writable` call doesn't leave a
/// permission-denied directory behind for the backing `TempDir` to fail to clean up on drop.
struct PermissionGuard {
    path: PathBuf,
}

impl Drop for PermissionGuard {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o755));
    }
}

/// Relocates `fork`'s `db_path` (and any `.wal`/`.lock` siblings) into a dedicated subdirectory
/// of `fork`'s own temp dir, chmod's that subdirectory read-only (`0o555`), and returns the new
/// db_path plus a guard that restores write permissions when it drops. `fork.wal_dir` (the
/// application WAL) is left completely untouched. See this file's module doc comment for why a
/// permission-based mechanism is used instead of a garbage-byte write.
fn corrupt_via_readonly_dir(fork: &SeededWorkspace) -> (PathBuf, PermissionGuard) {
    let db_subdir = fork.dir.path().join("dbdir");
    std::fs::create_dir_all(&db_subdir).expect("create db subdir");
    let new_db_path = db_subdir.join(fork.db_path.file_name().unwrap());
    std::fs::rename(&fork.db_path, &new_db_path).expect("relocate db_path into subdir");
    for ext in &[".wal", ".lock"] {
        let old_sibling = PathBuf::from(format!("{}{}", fork.db_path.display(), ext));
        if old_sibling.exists() {
            let new_sibling = PathBuf::from(format!("{}{}", new_db_path.display(), ext));
            std::fs::rename(&old_sibling, &new_sibling).expect("relocate db sibling into subdir");
        }
    }
    std::fs::set_permissions(&db_subdir, std::fs::Permissions::from_mode(0o555))
        .expect("chmod db subdir read-only");
    (new_db_path, PermissionGuard { path: db_subdir })
}

/// Restores write access to the directory `corrupt_via_readonly_dir` chmod'd read-only —
/// simulates the underlying condition clearing, a prerequisite before any recovery strategy
/// (which itself needs to rename/recreate files in that directory) can succeed.
fn restore_writable(db_subdir: &Path) {
    std::fs::set_permissions(db_subdir, std::fs::Permissions::from_mode(0o755))
        .expect("chmod db subdir back to writable");
}

/// The lbug on-disk shadow WAL file backing `db_path` — present with nonzero size once any write
/// has occurred against an open `Db`, and removed entirely once that `Db` is cleanly dropped
/// (checkpointed). Empirically confirmed against a small synthetic DB before being relied on
/// here as this suite's FR-011 on-disk checkpoint signal (no `Db::drop` observable was
/// documented anywhere in the codebase prior to this issue).
fn shadow_wal_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.wal", db_path.display()))
}

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

fn wait_for_process_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Kills and reaps the wrapped socket-service child on drop — a safety net so a test panic
/// while a remote service is up in User Story 7's attached-mode scenarios doesn't leave an
/// orphaned process holding the socket file open.
struct ChildGuard(Child);

impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns the socket (non-MCP) service against an existing `SeededWorkspace`'s already-rebuilt
/// `db_path`/`wal_dir` — the "already-running app instance" attached-mode MCP coexists with.
/// Mirrors `mcp_attached.rs`'s `spawn_socket_service`, but pointed at real-corpus-seeded files
/// instead of a fresh empty database (User Story 7's attached-mode scenarios are new ground for
/// this suite family — `mcp_attached.rs`'s own precedent always uses small/empty graphs).
fn spawn_socket_service_for(
    db_path: &Path,
    wal_dir: &Path,
    socket_path: &Path,
    embedder_url: &str,
) -> ChildGuard {
    let child = Command::new(binary_path())
        .env("LCG_DB_PATH", db_path.to_str().unwrap())
        .env("LCG_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("LCG_WAL_DIR", wal_dir.to_str().unwrap())
        .env("LCG_SHUTDOWN_TIMEOUT_MS", "5000")
        .args(["--embedder-http", embedder_url])
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"])
        .spawn()
        .expect("failed to spawn socket service");
    assert!(
        wait_for_socket(socket_path, Duration::from_secs(15)),
        "socket service backing the real-corpus fixture did not become ready"
    );
    ChildGuard(child)
}

fn socket_request(socket_path: &Path, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(socket_path).expect("connect to socket service");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
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

// #[ignore]: seeds the fixture once (~60-140s), then runs every admin lifecycle story against
// independently forked copies. No full WAL-replay cycle is needed anywhere below (see module doc
// comment), so total added runtime beyond the one-time seed is well under a minute. Run
// explicitly via `cargo test -p lcg-service --release --test mcp_real_corpus_admin_lifecycle_e2e
// -- --ignored`; wired into CI on push-to-main / workflow_dispatch.
#[test]
#[ignore]
fn mcp_admin_lifecycle_operations_over_real_corpus_fixture() {
    let port = spawn_stub_embedder();
    let embedder_url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let base = seed_real_corpus_workspace(&embedder_url);
    let expected = base.expected.clone();
    let group_id = base.group_id().to_string();

    // ── User Story 8 (FR-013): admin tools invisible/rejected without admin scope ──────────
    // Forks even though nothing here mutates graph *content* — `client.shutdown()` below is a
    // SIGKILL, and merely opening the DB for a read (even a single knowledge_status call) makes
    // lbug create its on-disk shadow `<db_path>.wal` file; a killed process never checkpoints
    // that away. Left on `base` itself, every later `base.fork()` in this file would silently
    // inherit that stray `.wal`, which defeats User Story 3's read-only-directory corruption
    // technique — reopening a *pre-existing* `.wal` file doesn't require creating a new one, so
    // it does not hit the parent directory's write permission at all (empirically confirmed
    // while debugging why degraded mode was never observed). Forking keeps that side effect
    // confined to this block's own disposable copy.
    {
        let fork = base.fork();
        let mut client = fork.spawn_reader(&embedder_url, &["--scope=read"]);
        client.initialize();

        let tools = client.list_tools();
        let tool_names: HashSet<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for name in ADMIN_TOOL_NAMES {
            assert!(
                !tool_names.contains(name),
                "{name} must be absent from tools/list under --scope=read"
            );
        }

        for name in ADMIN_TOOL_NAMES {
            let resp = client.call_tool(name, json!({}));
            assert!(
                resp.get("error").is_some(),
                "expected a protocol-level error calling unlisted admin tool {name} under \
                 --scope=read: {resp:?}"
            );
        }

        let status_resp = client.call_tool("knowledge_status", json!({}));
        let status = structured(&status_resp, "knowledge_status (post scope-rejection)");
        assert_eq!(
            status["entity_count"], expected["entity_count"],
            "graph must be verifiably unchanged after the rejected admin-tool calls: {status}"
        );

        client.shutdown();
    }

    // ── User Story 3 (FR-007, FR-008): degraded-mode corruption → knowledge_recover ────────
    {
        let fork = base.fork();
        let db_subdir = fork.dir.path().join("dbdir");
        let (corrupted_db_path, _guard) = corrupt_via_readonly_dir(&fork);

        let mut client = spawn_at(
            &corrupted_db_path,
            &fork.wal_dir,
            &embedder_url,
            &["--scope=admin,read"],
        );
        // Scenario 1: the process must start successfully (no crash/crash-loop) and serve
        // exactly the ADR-0009 degraded-mode allow-list.
        client.initialize();

        let status_resp = client.call_tool("knowledge_status", json!({}));
        assert!(
            status_resp["result"]["isError"].as_bool() != Some(true),
            "knowledge_status (allow-listed) must succeed while degraded: {status_resp:?}"
        );

        let rejected_resp = client.call_tool("knowledge_build_indices", json!({}));
        assert_eq!(
            rejected_resp["result"]["isError"],
            json!(true),
            "a visible-but-non-allow-listed admin tool must be rejected while degraded, \
             confirming the ADR-0009 dispatch-level guard (not merely tool-visibility gating): \
             {rejected_resp:?}"
        );
        let rejected_message = rejected_resp["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            rejected_message.contains("DB unavailable"),
            "expected a DB-unavailable rejection message: {rejected_resp:?}"
        );

        // Scenario 2/3: restore the underlying condition, then recover via drop_lbug_wal.
        restore_writable(&db_subdir);

        let recover_resp =
            client.call_tool("knowledge_recover", json!({"strategy": "drop_lbug_wal"}));
        let recover_result = structured(&recover_resp, "knowledge_recover (drop_lbug_wal)");
        assert_eq!(recover_result["success"], json!(true), "{recover_result}");
        assert_eq!(
            recover_result["restart_required"],
            json!(false),
            "{recover_result}"
        );

        // The golden search below exercises the recovered indices directly; it is not what
        // makes indices_built true — handle_knowledge_recover now sets the flag itself in its
        // shared success arm for every strategy, including drop_lbug_wal (issue #297).
        let entity_queries = expected["golden_entity_queries"].as_array().unwrap();
        let q = &entity_queries[0];
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let find_resp = client.call_tool(
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
        );
        let find_result = structured(&find_resp, "knowledge_find_entities (post-recovery)");
        let actual_names: HashSet<String> = find_result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_string())
            .collect();
        let expected_names: Vec<String> = q["expected_entity_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            expected_names.iter().any(|n| actual_names.contains(n)),
            "post-recovery golden query {query:?} expected overlap with {expected_names:?}, got \
             {actual_names:?}"
        );

        let status2_resp = client.call_tool("knowledge_status", json!({}));
        let status2 = structured(&status2_resp, "knowledge_status (post-recovery)");
        assert_eq!(status2["indices_built"], json!(true), "{status2}");
        assert_eq!(
            status2["entity_count"], expected["entity_count"],
            "recovery must restore the fixture's exact counts, not just clear the degraded \
             flag: {status2}"
        );
        assert_eq!(
            status2["relationship_count"], expected["relationship_count"],
            "{status2}"
        );
        assert_eq!(
            status2["episode_count"], expected["episode_count"],
            "{status2}"
        );

        client.shutdown();
    }

    // ── User Story 3 Scenario 4 (FR-009): knowledge_recover_full, then idempotent no-op ────
    {
        let fork = base.fork();
        let db_subdir = fork.dir.path().join("dbdir");
        let (corrupted_db_path, _guard) = corrupt_via_readonly_dir(&fork);

        let mut client = spawn_at(
            &corrupted_db_path,
            &fork.wal_dir,
            &embedder_url,
            &["--scope=admin,read"],
        );
        client.initialize();

        let status_resp = client.call_tool("knowledge_status", json!({}));
        assert!(
            status_resp["result"]["isError"].as_bool() != Some(true),
            "knowledge_status must succeed while degraded: {status_resp:?}"
        );

        restore_writable(&db_subdir);

        let recover_full_resp = client.call_tool("knowledge_recover_full", json!({}));
        let recover_full_result =
            structured(&recover_full_resp, "knowledge_recover_full (first call)");
        assert_eq!(
            recover_full_result["success"],
            json!(true),
            "{recover_full_result}"
        );
        assert_eq!(
            recover_full_result["recovery_needed"],
            json!(true),
            "{recover_full_result}"
        );
        assert_eq!(
            recover_full_result["episodes_after"], expected["episode_count"],
            "{recover_full_result}"
        );

        // The golden search below exercises the rebuilt indices directly; it is not what makes
        // indices_built true — handle_knowledge_recover_full now stores report.indexes_rebuilt
        // into AppState.indices_built itself on success (issue #297).
        let entity_queries = expected["golden_entity_queries"].as_array().unwrap();
        let q = &entity_queries[0];
        let query = q["query"].as_str().unwrap();
        let top_n = q["expected_top_n"].as_u64().unwrap();
        let find_resp = client.call_tool(
            "knowledge_find_entities",
            json!({"query": query, "group_ids": [group_id.clone()], "num_results": top_n}),
        );
        let find_result = structured(&find_resp, "knowledge_find_entities (post-recover_full)");
        let actual_names: HashSet<String> = find_result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_string())
            .collect();
        let expected_names: Vec<String> = q["expected_entity_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            expected_names.iter().any(|n| actual_names.contains(n)),
            "post-recover_full golden query {query:?} expected overlap with {expected_names:?}, \
             got {actual_names:?}"
        );

        let status2_resp = client.call_tool("knowledge_status", json!({}));
        let status2 = structured(&status2_resp, "knowledge_status (post-recover_full)");
        assert_eq!(status2["indices_built"], json!(true), "{status2}");
        assert_eq!(
            status2["entity_count"], expected["entity_count"],
            "{status2}"
        );
        assert_eq!(
            status2["relationship_count"], expected["relationship_count"],
            "{status2}"
        );

        // Idempotency: calling knowledge_recover_full again on the now-healthy graph is a
        // confirmed no-op.
        let recover_full_again_resp = client.call_tool("knowledge_recover_full", json!({}));
        let recover_full_again_result = structured(
            &recover_full_again_resp,
            "knowledge_recover_full (second call)",
        );
        assert_eq!(
            recover_full_again_result["recovery_needed"],
            json!(false),
            "second call must be a confirmed no-op: {recover_full_again_result}"
        );
        assert_eq!(
            recover_full_again_result["episodes_after"], expected["episode_count"],
            "{recover_full_again_result}"
        );

        client.shutdown();
    }

    // ── User Story 7 Scenario 1 (FR-011): knowledge_close checkpoints the WAL ───────────────
    // Two independent forks, each dirtied with one identical write; one shut down cleanly via
    // knowledge_close, the other killed as a negative control — the lbug shadow `<db_path>.wal`
    // file is present-with-nonzero-size immediately after a write and removed entirely once its
    // owning Db is cleanly dropped (empirically confirmed before relying on it here).
    {
        let dim = base.embedding_dim();
        let embedding_literal = format!("[{}]", vec!["0.0"; dim].join(","));
        let write_cypher = format!(
            "CREATE (:Entity {{uuid: 'us7-marker', name: 'US7 Marker', group_id: '{group_id}', \
             labels: ['Entity'], created_at: timestamp('2026-01-01 00:00:00'), \
             name_embedding: {embedding_literal}, summary: 's', attributes: '{{}}'}})"
        );

        let clean_fork = base.fork();
        let mut clean_client = clean_fork.spawn_reader(&embedder_url, &["--scope=admin,cypher"]);
        clean_client.initialize();
        let write_resp =
            clean_client.call_tool("knowledge_query_cypher", json!({"query": write_cypher}));
        assert!(
            write_resp["result"]["isError"].as_bool() != Some(true),
            "marker write must succeed: {write_resp:?}"
        );
        let clean_wal = shadow_wal_path(&clean_fork.db_path);
        assert!(
            clean_wal.exists(),
            "expected the shadow WAL to exist immediately after a write"
        );

        let close_resp = clean_client.call_tool("knowledge_close", json!({}));
        assert!(
            close_resp["result"]["isError"].as_bool() != Some(true),
            "knowledge_close should succeed: {close_resp:?}"
        );
        let close_status = clean_client.wait_for_exit(Duration::from_secs(10));
        assert_eq!(
            close_status.code(),
            Some(0),
            "expected a clean exit after knowledge_close"
        );
        assert!(
            !clean_wal.exists(),
            "expected the shadow WAL to be gone after a clean knowledge_close checkpoint \
             (ADR-0017), but it still exists"
        );

        let killed_fork = base.fork();
        let mut killed_client = killed_fork.spawn_reader(&embedder_url, &["--scope=admin,cypher"]);
        killed_client.initialize();
        let write_resp2 =
            killed_client.call_tool("knowledge_query_cypher", json!({"query": write_cypher}));
        assert!(
            write_resp2["result"]["isError"].as_bool() != Some(true),
            "marker write must succeed: {write_resp2:?}"
        );
        let killed_wal = shadow_wal_path(&killed_fork.db_path);
        assert!(
            killed_wal.exists(),
            "expected the shadow WAL to exist immediately after a write"
        );
        killed_client.shutdown(); // SIGKILL — the negative control, never checkpoints.
        assert!(
            killed_wal.exists(),
            "expected the shadow WAL to survive a killed (non-checkpointed) process — if this \
             now fails, the differential signal this test relies on for FR-011 no longer holds"
        );
    }

    // ── User Story 7 Scenarios 2-3 (FR-012): attached-mode --allow-remote-close gating ──────
    {
        let fork_without = base.fork();
        let socket_dir_without = fork_without.dir.path().join("socket-without");
        std::fs::create_dir_all(&socket_dir_without).unwrap();
        let socket_path_without = socket_dir_without.join("service.sock");
        let mut service_without = spawn_socket_service_for(
            &fork_without.db_path,
            &fork_without.wal_dir,
            &socket_path_without,
            &embedder_url,
        );

        let mut mcp_without = spawn_attached(&socket_path_without, &["--scope=admin"]);
        mcp_without.initialize();
        let names_without: Vec<String> = mcp_without
            .list_tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !names_without.contains(&"knowledge_close".to_string()),
            "knowledge_close must be absent from tools/list in attached mode without \
             --allow-remote-close, got {names_without:?}"
        );
        let resp_without = mcp_without.call_tool("knowledge_close", json!({}));
        assert!(
            resp_without.get("error").is_some(),
            "expected a protocol error calling the hidden knowledge_close tool: {resp_without:?}"
        );
        // The remote service backing the real-corpus fixture must still be reachable — the
        // (rejected) call must not have forwarded a shutdown.
        let still_up = socket_request(&socket_path_without, "knowledge_status", json!({}));
        assert!(
            still_up.get("error").is_none(),
            "remote service must still be running after the non-forwarded close: {still_up:?}"
        );

        mcp_without.shutdown();
        service_without.kill().ok();
        service_without.wait().ok();

        let fork_with = base.fork();
        let socket_dir_with = fork_with.dir.path().join("socket-with");
        std::fs::create_dir_all(&socket_dir_with).unwrap();
        let socket_path_with = socket_dir_with.join("service.sock");
        let mut service_with = spawn_socket_service_for(
            &fork_with.db_path,
            &fork_with.wal_dir,
            &socket_path_with,
            &embedder_url,
        );

        let mut mcp_with = spawn_attached(
            &socket_path_with,
            &["--scope=admin", "--allow-remote-close"],
        );
        mcp_with.initialize();
        let names_with: Vec<String> = mcp_with
            .list_tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            names_with.contains(&"knowledge_close".to_string()),
            "expected knowledge_close to be visible with --allow-remote-close"
        );
        let resp_with = mcp_with.call_tool("knowledge_close", json!({}));
        assert!(
            resp_with["result"]["isError"].as_bool() != Some(true),
            "forwarded close should succeed: {resp_with:?}"
        );
        let status = wait_for_process_exit(&mut service_with, Duration::from_secs(10));
        assert!(
            status.is_some(),
            "remote service backing the real-corpus fixture did not exit after a forwarded \
             knowledge_close"
        );
        assert_eq!(status.unwrap().code(), Some(0));

        mcp_with.shutdown();
    }

    // FR-015: verified environmentally — no ANTHROPIC_API_KEY is set and every --embedder-http
    // URL above points only at this suite's own loopback stub.
}
