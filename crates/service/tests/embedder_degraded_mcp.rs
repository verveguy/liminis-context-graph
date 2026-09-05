//! Binary-level coverage for issue #499 (bounded embedder-probe retry + degrade for standalone
//! `--mcp-stdio`).
//!
//! Spawns the real compiled binary — the same `bootstrap_app_state` startup path
//! `embedder_auth.rs` exercises for #497 — pointed at a port nothing is listening on, so every
//! probe attempt gets a transport-classified connection-refused error (`is_transport_error`).
//! Standalone `--mcp-stdio` must retry with bounded backoff and, once the retry window is
//! exhausted, start in degraded mode instead of exiting (FR-002/FR-003); socket-service mode
//! must keep today's immediate-fail-fast behavior unchanged (FR-001).

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, McpClient};

/// Reserves an OS-assigned port and immediately releases it — nothing ever listens there unless
/// a caller separately (re)binds it. **This is a hint only, not a guarantee**: between the
/// release and any later rebind, any other process on the machine can take the same port
/// (TOCTOU). That race is accepted by design here — these callers need "nothing is listening" so
/// every connection attempt gets "connection refused" rather than a slow timeout, and holding a
/// live listener without ever accepting would turn that into "connection accepted then stalled,"
/// changing the behavior under test. Used by the "never reachable" tests and as the pre-reserved
/// port [`spawn_stub_embedder_after_delay`] rebinds later. Contrast with
/// [`bind_ephemeral_listener`], which returns a live, held `TcpListener` with no such race.
fn reserve_ephemeral_port_hint() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Binds an OS-assigned ephemeral port and returns the live, held `TcpListener` together with
/// its port number — race-free by construction, since the port is never released and reacquired
/// by number. The caller must move the returned listener directly into whatever will accept
/// connections on it. Contrast with [`reserve_ephemeral_port_hint`], which releases the port and
/// carries an accepted TOCTOU race.
fn bind_ephemeral_listener() -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Spawns a stub OpenAI-compatible embedder that starts accepting connections only after
/// `delay` — simulating the sidecar/lcg simultaneous-launch race issue #499's retry loop
/// targets. The port is reserved up front (via [`reserve_ephemeral_port_hint`]) so the caller can
/// build a URL before the delay elapses; every connection attempt before the delayed rebind gets
/// "connection refused", exactly like a sidecar process that hasn't started listening yet. If
/// something else takes the reserved port during the delay, the delayed bind fails loudly
/// (rather than silently) so the failure is attributable to a lost port race, not to
/// embedder-reachability logic.
fn spawn_stub_embedder_after_delay(delay: Duration) -> u16 {
    use std::io::{Read, Write};

    let port = reserve_ephemeral_port_hint();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
            panic!(
                "spawn_stub_embedder_after_delay: failed to bind reserved port {port} after \
                 {delay:?} delay — likely lost a port race: {e}"
            )
        });
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            // The request must be read before writing a response — mirrors
            // `common::spawn_stub_embedder`. Skipping this races the client's own write of the
            // request against this thread's write of the response and can reset the connection
            // before the client ever sees a reply, which reqwest surfaces as a generic
            // transport-classified send failure indistinguishable from "still unreachable".
            let mut buf = [0u8; 65536];
            let _ = Read::read(&mut s, &mut buf);
            let embedding: Vec<f64> = (0..8).map(|i| i as f64 / 8.0).collect();
            let embedding_json = serde_json::to_string(&embedding).unwrap();
            let body = format!(
                r#"{{"object":"list","data":[{{"object":"embedding","embedding":{embedding_json},"index":0}}],"model":"stub-model","usage":{{"prompt_tokens":1,"total_tokens":1}}}}"#
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = Write::write_all(&mut s, resp.as_bytes());
        }
    });
    port
}

/// Spawns a stub embedder that accepts every connection but never writes a response — a stalled
/// connection, as opposed to the fast "connection refused" every other stub in this file
/// produces. This is the exact failure shape the per-attempt `tokio::time::timeout` around
/// `resolve_and_probe_embedder` (`main.rs`) exists to bound: neither transport client configures
/// its own request timeout, so without that wrapper a single stalled attempt could hang
/// indefinitely rather than failing within `EMBEDDER_RETRY_CEILING`. Acquires its socket via
/// [`bind_ephemeral_listener`] and moves the live listener directly into the spawned thread —
/// never releases and rebinds a port number, so it cannot race another process for the port.
fn spawn_stub_embedder_that_hangs() -> u16 {
    use std::io::Read;

    let (listener, port) = bind_ephemeral_listener();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            std::thread::spawn(move || {
                // Read the request (if any arrives) but never write a response — the connection
                // stays open and the client hangs waiting for one.
                let mut buf = [0u8; 65536];
                let _ = Read::read(&mut s, &mut buf);
                std::thread::sleep(Duration::from_secs(60));
            });
        }
    });
    port
}

/// Builds a `Command` pointed at `embedder_url`, in a fresh temp workspace, in either standalone
/// `--mcp-stdio` mode or default socket-service mode. Defensively `env_remove`s the key/dim env
/// vars (mirrors `embedder_auth.rs`'s `base_cmd`) so a developer's shell exporting one doesn't
/// leak into the test.
fn base_cmd(dir: &TempDir, embedder_url: &str, mcp_stdio: bool) -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env(
            "LCG_SOCKET_PATH",
            dir.path().join("service.sock").to_str().unwrap(),
        )
        .env_remove("LCG_EMBEDDING_API_KEY")
        .env_remove("GRAPHITI_EMBEDDING_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("LCG_EMBEDDING_DIM")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("LCG_EXTRACTION_URL");
    if mcp_stdio {
        cmd.args(["--mcp-stdio", "--embedder-http", embedder_url]);
    } else {
        cmd.args(["--embedder-http", embedder_url]);
    }
    cmd
}

/// Acceptance Scenario 1 / SC-001: with the embedder never reachable, standalone `--mcp-stdio`
/// starts (rather than exiting) once the retry window is exhausted, and `knowledge_status`
/// reports the new `degraded_reason` with an empty `recovery_available` list — discoverable
/// entirely from within the MCP session, no stderr required.
#[test]
fn mcp_stdio_degrades_when_embedder_never_reachable() {
    let dir = TempDir::new().unwrap();
    let port = reserve_ephemeral_port_hint();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let cmd = base_cmd(&dir, &url, true);
    let mut client = McpClient::spawn(cmd);
    // initialize() blocks until bootstrap_app_state resolves — including the full retry window
    // — so its 15s timeout must clear the 5s retry ceiling with margin.
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "knowledge_status itself should succeed even while degraded: {status:?}"
    );
    let content = &status["result"]["structuredContent"];
    assert_eq!(content["degraded"], json!(true), "{status:?}");
    assert_eq!(
        content["reason"],
        json!("embedder_unreachable_at_startup"),
        "{status:?}"
    );
    assert_eq!(
        content["recovery_available"],
        json!([]),
        "no recovery strategy should be advertised while no embedding dimension was ever \
         established: {status:?}"
    );

    client.shutdown();
}

/// Acceptance Scenario 2 / SC-004: a race where the embedder becomes reachable partway through
/// the retry window resolves to a normal, non-degraded startup — the retry is not merely
/// cosmetic.
#[test]
fn mcp_stdio_recovers_when_embedder_becomes_reachable_mid_retry() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder_after_delay(Duration::from_millis(800));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let cmd = base_cmd(&dir, &url, true);
    let mut client = McpClient::spawn(cmd);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "{status:?}"
    );
    // A healthy (non-degraded) knowledge_status response has no "degraded"/"reason" fields at
    // all — that shape is specific to the db-never-opened degraded branch (see
    // handle_knowledge_status) — so the non-degraded assertion is "connected"/"queryable", not
    // "degraded: false".
    let content = &status["result"]["structuredContent"];
    assert_eq!(
        content["connected"],
        json!(true),
        "a mid-retry-window race should resolve to a normal, non-degraded startup: {status:?}"
    );
    assert_eq!(content["queryable"], json!(true), "{status:?}");
    assert!(
        content.get("degraded").is_none(),
        "a healthy knowledge_status response should have no degraded field at all: {status:?}"
    );

    client.shutdown();
}

/// Acceptance Scenario 3 / FR-001 / SC-002: a hand-started socket-service process with the same
/// misconfiguration keeps today's fail-fast behavior unchanged — same message, same immediate
/// exit, zero retries.
#[test]
fn socket_mode_still_fails_fast_on_unreachable_embedder() {
    let dir = TempDir::new().unwrap();
    let port = reserve_ephemeral_port_hint();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url, false);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let start = std::time::Instant::now();
    let output = cmd.output().expect("spawn liminis-context-graph");
    let elapsed = start.elapsed();

    assert!(
        !output.status.success(),
        "socket-service startup must still fail fast on an unreachable embedder"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "socket mode must not retry — expected an immediate failure, took {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("embedder unreachable at startup"),
        "expected the unchanged FR-011 message, got stderr: {stderr}"
    );
}

/// Regression coverage for the per-attempt `tokio::time::timeout` fix (review finding on
/// #499/PR #514): an embedder that accepts a connection but never responds must not block a
/// single retry attempt past the advertised retry ceiling. Before that fix, this test would hang
/// past `client.initialize()`'s own 15s timeout, since neither transport client configures a
/// request timeout of its own.
#[test]
fn mcp_stdio_degrades_when_embedder_accepts_but_never_responds() {
    let dir = TempDir::new().unwrap();
    let port = spawn_stub_embedder_that_hangs();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let cmd = base_cmd(&dir, &url, true);
    let mut client = McpClient::spawn(cmd);
    let start = std::time::Instant::now();
    client.initialize();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "a stalled (accepted-but-unresponsive) embedder connection must not block startup past \
         the retry ceiling — took {elapsed:?}"
    );

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "knowledge_status itself should succeed even while degraded: {status:?}"
    );
    let content = &status["result"]["structuredContent"];
    assert_eq!(content["degraded"], json!(true), "{status:?}");
    assert_eq!(
        content["reason"],
        json!("embedder_unreachable_at_startup"),
        "{status:?}"
    );

    client.shutdown();
}

/// FR-005: `knowledge_recover` is rejected outright while degraded for this specific reason —
/// the only path out is restarting the process once the embedder is reachable.
#[test]
fn knowledge_recover_rejected_while_embedder_unreachable_degraded() {
    let dir = TempDir::new().unwrap();
    let port = reserve_ephemeral_port_hint();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let cmd = base_cmd(&dir, &url, true);
    let mut client = McpClient::spawn(cmd);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert_eq!(
        status["result"]["structuredContent"]["degraded"],
        json!(true),
        "test setup: expected the process to be degraded before exercising recovery: {status:?}"
    );

    let resp = client.call_tool("knowledge_recover", json!({"strategy": "drop_lbug_wal"}));
    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "knowledge_recover should be rejected while embedder-unreachable degraded: {resp:?}"
    );

    client.shutdown();
}
