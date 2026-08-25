//! Binary-level coverage for issue #497 (Bearer-token auth on the embedder's HTTP transport).
//!
//! Spawns the real compiled binary in standalone `--mcp-stdio` mode (no `--connect`), which
//! runs the exact `bootstrap_app_state` startup path — including the embedder probe — used by
//! both the socket service and standalone MCP mode. A stub HTTP embedder requires an exact
//! `Authorization: Bearer <key>` header to succeed, so these tests exercise the whole
//! resolve-key -> attach-header -> probe -> serve chain end to end, and confirm the configured
//! key value never appears in the child's stderr (FR-006/FR-009).

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

mod common;
use common::{binary_path, McpClient};

/// Spawns a stub HTTP embedder that requires `Authorization: Bearer <required_key>` (when
/// `Some`) to return a 200 OpenAI-compatible embedding response; any other/missing credential
/// gets a 401. When `required_key` is `None`, every request succeeds regardless of headers.
/// Captures every accepted request's raw header lines into the returned `Mutex<Vec<Vec<String>>>`.
fn spawn_stub_auth_embedder(
    required_key: Option<&'static str>,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
    let captured_thread = captured.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 65536];
            let n = Read::read(&mut s, &mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let headers: Vec<String> = request
                .split("\r\n\r\n")
                .next()
                .unwrap_or("")
                .lines()
                .map(str::to_string)
                .collect();
            let authorized = match required_key {
                None => true,
                Some(key) => headers
                    .iter()
                    .any(|h| h.eq_ignore_ascii_case(&format!("authorization: Bearer {key}"))),
            };
            captured_thread.lock().unwrap().push(headers);

            if !authorized {
                let body =
                    r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#;
                let resp = format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = Write::write_all(&mut s, resp.as_bytes());
                continue;
            }

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

    (port, captured)
}

/// Builds a `Command` for standalone `--mcp-stdio` (no `--connect`), pointed at `embedder_url`,
/// in a fresh temp workspace. Defensively `env_remove`s all three key tiers plus
/// `LCG_EMBEDDING_DIM` so a developer's shell exporting one doesn't leak into the test; callers
/// then set exactly the env vars their scenario needs.
fn base_cmd(dir: &TempDir, embedder_url: &str) -> Command {
    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db").to_str().unwrap())
        .env("LCG_WAL_DIR", dir.path().join("wal").to_str().unwrap())
        .env(
            "LCG_SOCKET_PATH",
            dir.path().join("unused.sock").to_str().unwrap(),
        )
        .env_remove("LCG_EMBEDDING_API_KEY")
        .env_remove("GRAPHITI_EMBEDDING_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("LCG_EMBEDDING_DIM")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("LCG_EXTRACTION_URL")
        .args(["--mcp-stdio", "--embedder-http", embedder_url]);
    cmd
}

/// Acceptance Scenario 1 (User Story 1): `LCG_EMBEDDING_API_KEY` set to a valid key, endpoint
/// requires that exact key — startup succeeds and the service serves reads.
#[test]
fn startup_succeeds_with_lcg_embedding_api_key() {
    let dir = TempDir::new().unwrap();
    let (port, _captured) = spawn_stub_auth_embedder(Some("test-key-abc123"));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url);
    cmd.env("LCG_EMBEDDING_API_KEY", "test-key-abc123");
    let mut client = McpClient::spawn(cmd);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "startup/probe should succeed with a matching LCG_EMBEDDING_API_KEY: {status:?}"
    );

    client.shutdown();
}

/// Acceptance Scenario 2: `LCG_EMBEDDING_API_KEY` unset, `OPENAI_API_KEY` set — the fallback
/// tier resolves and is used.
#[test]
fn startup_succeeds_via_openai_api_key_fallback() {
    let dir = TempDir::new().unwrap();
    let (port, _captured) = spawn_stub_auth_embedder(Some("openai-fallback-key"));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url);
    cmd.env("OPENAI_API_KEY", "openai-fallback-key");
    let mut client = McpClient::spawn(cmd);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "startup/probe should succeed via the OPENAI_API_KEY fallback tier: {status:?}"
    );

    client.shutdown();
}

/// Review finding (issue #497 PR #506): unlike `LCG_EMBEDDING_API_KEY`/`GRAPHITI_EMBEDDING_API_KEY`,
/// the `OPENAI_API_KEY` fallback tier sends whatever key is exported for OpenAI tooling
/// generally to *whatever* endpoint is configured — not necessarily OpenAI's own API. That's
/// intentional per FR-002/Acceptance Scenario 2 (this test's sibling above proves it still
/// works against a non-OpenAI stub), but it must not be silent: this asserts the informational
/// notice appears, naming the endpoint, while the key value itself still never leaks.
#[test]
fn openai_api_key_fallback_logs_informational_notice_without_leaking_key() {
    let dir = TempDir::new().unwrap();
    const KEY: &str = "openai-notice-key-12345";
    let (port, _captured) = spawn_stub_auth_embedder(Some(KEY));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url);
    cmd.env("OPENAI_API_KEY", KEY);
    let mut client = McpClient::spawn_capturing_stderr(cmd);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "startup should have succeeded: {status:?}"
    );

    let rc = unsafe { libc::kill(client.pid() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "libc::kill failed: {}",
        std::io::Error::last_os_error()
    );
    client.wait_for_exit(Duration::from_secs(10));
    let stderr = client.collect_stderr().join("");

    assert!(
        stderr.contains("Using OPENAI_API_KEY as the embedder credential"),
        "expected an informational (non-deprecation) notice when the OPENAI_API_KEY fallback \
         tier is used, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("DEPRECATED"),
        "OPENAI_API_KEY is a convenience fallback, not a deprecated alias (FR-003) — it must \
         not trigger the GRAPHITI_*-style deprecation warning: {stderr}"
    );
    assert!(
        !stderr.contains(KEY),
        "the informational notice must name the endpoint, never the key value itself: {stderr}"
    );
}

/// FR-008 / Acceptance Scenario 1 (User Story 4): the endpoint requires a key, none is
/// configured, so the probe gets a 401 — startup must fail with a distinguishable, actionable
/// message rather than a generic/raw error dump.
#[test]
fn startup_fails_with_actionable_message_on_401() {
    let dir = TempDir::new().unwrap();
    let (port, _captured) = spawn_stub_auth_embedder(Some("required-key"));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn liminis-context-graph");

    assert!(
        !output.status.success(),
        "startup must fail when the embedder rejects the (missing) credential"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("authentication") && stderr.contains("LCG_EMBEDDING_API_KEY"),
        "expected an actionable authentication-failure message naming LCG_EMBEDDING_API_KEY, \
         got stderr: {stderr}"
    );
}

/// FR-008 / Acceptance Scenario 2: `LCG_EMBEDDING_DIM` cannot mask an authentication failure —
/// unlike a generic non-transport probe failure, a 401/403 stays fatal even with a dim override
/// set, since a dimension override cannot meaningfully paper over a rejected credential.
#[test]
fn startup_fails_on_401_even_with_dim_override_set() {
    let dir = TempDir::new().unwrap();
    let (port, _captured) = spawn_stub_auth_embedder(Some("required-key"));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url);
    cmd.env("LCG_EMBEDDING_DIM", "768")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn liminis-context-graph");

    assert!(
        !output.status.success(),
        "LCG_EMBEDDING_DIM must not override an authentication failure (FR-008)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("authentication"),
        "expected the auth-failure message even with LCG_EMBEDDING_DIM set, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("using LCG_EMBEDDING_DIM"),
        "the dim-override log line must not fire for an auth failure: {stderr}"
    );
}

/// FR-006/FR-009: with a real (fake-but-distinguishable) key configured and proven — via the
/// stub's own header capture — to have actually been sent on the wire, the key value must never
/// appear anywhere in the child process's stderr, including the startup log line.
#[test]
fn stderr_never_contains_configured_key_value() {
    let dir = TempDir::new().unwrap();
    const SECRET: &str = "sekret-value-should-never-leak-99887766";
    let (port, captured) = spawn_stub_auth_embedder(Some(SECRET));
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");

    let mut cmd = base_cmd(&dir, &url);
    cmd.env("LCG_EMBEDDING_API_KEY", SECRET);
    let mut client = McpClient::spawn_capturing_stderr(cmd);
    client.initialize();

    let status = client.call_tool("knowledge_status", json!({}));
    assert!(
        status["result"]["isError"].as_bool() != Some(true),
        "startup should have succeeded: {status:?}"
    );

    // Terminate via SIGTERM (rather than `shutdown()`, which consumes `client` and would make
    // the subsequent `collect_stderr()` call unreachable) so the child's stderr pipe reaches
    // EOF and `collect_stderr` can return, mirroring `mcp_clean_shutdown.rs`'s pattern.
    let rc = unsafe { libc::kill(client.pid() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "libc::kill failed: {}",
        std::io::Error::last_os_error()
    );
    client.wait_for_exit(Duration::from_secs(10));
    let stderr_lines = client.collect_stderr();
    let stderr = stderr_lines.join("");

    // Prove the assertion below isn't vacuous: the key really was sent over the wire.
    let sent_correct_auth = captured.lock().unwrap().iter().any(|headers| {
        headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&format!("authorization: Bearer {SECRET}")))
    });
    assert!(
        sent_correct_auth,
        "test setup error: the stub server never observed the expected Authorization header"
    );

    assert!(
        !stderr.contains(SECRET),
        "child stderr must never contain the configured key value, got: {stderr}"
    );
    assert!(
        stderr.contains("embedder: transport=http"),
        "expected the startup log line to appear: {stderr}"
    );
}
