//! Cross-platform end-to-end test for the JSON-RPC socket service over its platform transport
//! (issue #581): a Unix domain socket on Unix, a named pipe on Windows.
//!
//! Spawns the real binary against a stub embedder and drives it the way an out-of-process
//! client (the Liminis app, orac's Python clients) does: find the endpoint, connect, poll
//! `health_check` until healthy, query `knowledge_status`, then `knowledge_close` and expect a
//! clean exit. On Windows the endpoint is discovered through `service.endpoint`, the file the
//! service writes for clients that cannot compute the pipe name themselves — so this also pins
//! that contract.
//!
//! Unlike the rest of this directory it is deliberately not `#![cfg(unix)]`: it is the Windows
//! CI job's coverage of the transport.
//!
//! Issue #587 extends this beyond transport plumbing: `vector_index_create_and_query_returns_real_rows`
//! and `fts_index_create_and_query_returns_real_rows` prove the vector/FTS extension DLLs not only
//! load but actually build and query an index over real data — closing the gap that let the lbug
//! 0.20.0 `win_amd64` extensions crash on `CREATE_VECTOR_INDEX`/`CREATE_FTS_INDEX` while `LOAD
//! EXTENSION` (and therefore the original e2e) kept passing.

#[path = "common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

use common::{binary_path, spawn_stub_embedder, ChildGuard};

/// One client connection, as a reader/writer pair over the platform's stream type. Reads run on
/// a dedicated background thread that pushes lines into an `mpsc::channel` (the same pattern
/// `McpClient` in `common/mod.rs` uses for stdio), so every `call()` has a hard timeout on both
/// platforms rather than relying on the OS's broken-pipe behavior for a hard process abort — the
/// only failure mode the #561 crash investigation actually observed, not a guarantee against a
/// wedge/hang (FR-006).
struct Connection {
    writer: Box<dyn Write>,
    lines: Receiver<std::io::Result<String>>,
}

impl Connection {
    #[cfg(unix)]
    fn open(socket_path: &Path) -> std::io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(socket_path)?;
        let read_half = stream.try_clone()?;
        Ok(Self::from_parts(Box::new(read_half), Box::new(stream)))
    }

    /// A named pipe opens like a file; the name comes from the discovery file, exactly as a
    /// Python or Node client would find it.
    #[cfg(windows)]
    fn open(socket_path: &Path) -> std::io::Result<Self> {
        let endpoint = std::fs::read_to_string(socket_path.with_extension("endpoint"))?;
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint.trim())?;
        let read_half = pipe.try_clone()?;
        Ok(Self::from_parts(Box::new(read_half), Box::new(pipe)))
    }

    fn from_parts(read_half: Box<dyn Read + Send>, writer: Box<dyn Write>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(read_half);
            loop {
                let mut line = String::new();
                let result = match reader.read_line(&mut line) {
                    Ok(0) => Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed by peer",
                    )),
                    Ok(_) => Ok(line),
                    Err(e) => Err(e),
                };
                let is_err = result.is_err();
                if tx.send(result).is_err() || is_err {
                    break;
                }
            }
        });
        Self { writer, lines: rx }
    }

    /// Sends a request and waits up to 30s (matching the read timeout the Unix branch used to
    /// set directly on the socket) for a matching line. Returns `Err` — rather than panicking —
    /// on a write failure, a closed connection, a timeout, or unparseable JSON, so callers can
    /// report which stage failed and inspect the child process's state (see `call_checked`).
    fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value, String> {
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{request}").map_err(|e| format!("write request: {e}"))?;
        self.writer
            .flush()
            .map_err(|e| format!("flush request: {e}"))?;
        match self.lines.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(line)) => serde_json::from_str(&line)
                .map_err(|e| format!("bad response {line:?} to {method}: {e}")),
            Ok(Err(e)) => Err(format!("connection closed while awaiting {method}: {e}")),
            Err(e) => Err(format!("timed out waiting for {method} response: {e}")),
        }
    }
}

/// Polls `health_check` until it reports healthy — the documented readiness signal
/// (docs/ipc-mcp-reference.md#readiness), since the endpoint exists before the DB opens.
///
/// Reuses a single connection across "connected but not yet healthy" polls instead of opening a
/// fresh one every iteration: each open spawns `Connection`'s background reader thread, and since
/// `Connection` has no way to signal that thread to stop, discarding a connection every 200ms
/// while polling would leak a thread blocked forever in `read_line` (the peer never sees the
/// socket/pipe fully closed while the reader's cloned handle is still open) for every retry —
/// hundreds of them over a slow startup. A connection is only replaced when `call` itself reports
/// the connection as broken; simply reporting "not yet healthy" keeps reusing it.
fn wait_until_healthy(socket_path: &Path, timeout: Duration) -> Connection {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("never connected");
    let mut conn: Option<Connection> = None;
    while Instant::now() < deadline {
        if conn.is_none() {
            match Connection::open(socket_path) {
                Ok(c) => conn = Some(c),
                Err(e) => last = e.to_string(),
            }
        }
        if let Some(active) = conn.as_mut() {
            match active.call(1, "health_check", json!({})) {
                Ok(response) => {
                    if response.pointer("/result/healthy") == Some(&Value::Bool(true)) {
                        return conn.take().unwrap();
                    }
                    last = response.to_string();
                }
                Err(e) => {
                    last = e;
                    conn = None;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("service not healthy within {timeout:?}; last: {last}");
}

/// Calls `conn`, and on failure inspects `child`'s process state to report whether it crashed,
/// exited cleanly, or is still running (a genuine hang) — so a native access violation inside the
/// extension DLL during index creation/querying (the #561 0.20.0 crash shape) surfaces as a clear,
/// attributable panic instead of a generic timeout (FR-006, FR-007).
fn call_checked(
    conn: &mut Connection,
    child: &mut Child,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    match conn.call(id, method, params) {
        Ok(v) => v,
        Err(e) => {
            let process_state = match child.try_wait() {
                Ok(Some(status)) => format!("service process exited with {status}"),
                Ok(None) => "service process is still running (hang, not a crash)".to_string(),
                Err(err) => format!("could not determine service process state: {err}"),
            };
            panic!("call to {method} failed: {e}; {process_state}");
        }
    }
}

/// Everything kept alive for the duration of a spawned service: the temp dir (must outlive the
/// child, which writes into it) and the guarded child process. `socket_path` is retained for the
/// Windows-only endpoint-file check.
struct SpawnedService {
    // Declaration order is drop order: `child` must be killed (and its handles into `_dir`
    // released) before `_dir` is recursively deleted, or the delete can hit files the still-running
    // process has open — a sharing violation on Windows, the platform this PR is hardening.
    child: ChildGuard,
    _dir: TempDir,
    #[cfg_attr(not(windows), allow(dead_code))]
    socket_path: PathBuf,
}

/// Spawns the real service binary against a stub embedder, waits for it to report healthy, and
/// returns the running service alongside a connection to it. Factored out of the original single
/// test so the new vector/FTS index tests (issue #587) can each spawn their own subprocess —
/// keeping a crash in one attributable and independent of the others (FR-007).
fn spawn_service() -> (SpawnedService, Connection) {
    let dir = TempDir::new().unwrap();
    let socket_path = dir.path().join("service.sock");
    let embedder_url = format!("http://127.0.0.1:{}/v1/embeddings", spawn_stub_embedder());

    let mut cmd = Command::new(binary_path());
    cmd.env("LCG_DB_PATH", dir.path().join("test.db"))
        .env("LCG_SOCKET_PATH", &socket_path)
        .env("LCG_SHUTDOWN_TIMEOUT_MS", "2000")
        .args(["--embedder-http", &embedder_url])
        // Never called: the test performs no extraction, but startup requires an extractor.
        .args(["--extractor-http", "http://127.0.0.1:1/v1/chat/completions"]);
    let child = ChildGuard::spawn(cmd);

    let conn = wait_until_healthy(&socket_path, Duration::from_secs(60));

    (
        SpawnedService {
            _dir: dir,
            child,
            socket_path,
        },
        conn,
    )
}

/// An 8-dimensional one-hot vector (matching `ldb_spike_ipc.rs`'s `DIM=8` convention for these
/// index spikes — the extension code path is dimension-agnostic, so this keeps literals short).
fn unit_vec(hot_idx: usize) -> Vec<f32> {
    const DIM: usize = 8;
    let mut v = vec![0.0f32; DIM];
    if hot_idx < DIM {
        v[hot_idx] = 1.0;
    }
    v
}

fn format_float_array(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:.6}")).collect();
    format!("[{}]", parts.join(","))
}

/// Issues `query` via `knowledge_query_cypher` and asserts the JSON-RPC response carries no
/// `error` field — the "call completed without error" half of FR-003/FR-005's assertion (rows
/// actually coming back is checked separately by each test).
fn run_cypher(conn: &mut Connection, child: &mut Child, id: u64, query: &str) -> Value {
    let response = call_checked(
        conn,
        child,
        id,
        "knowledge_query_cypher",
        json!({"query": query}),
    );
    assert!(
        response.get("error").is_none(),
        "knowledge_query_cypher failed for {query:?}: {response}"
    );
    response
}

fn rows_of(response: &Value) -> &Vec<Value> {
    response
        .pointer("/result/rows")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("no rows in response: {response}"))
}

#[test]
fn socket_service_answers_over_the_platform_transport_and_closes_cleanly() {
    let (mut service, mut conn) = spawn_service();

    // Checked only once healthy: the file is written at bind, which the spawn above races.
    #[cfg(windows)]
    {
        let recorded = std::fs::read_to_string(service.socket_path.with_extension("endpoint"))
            .expect("service.endpoint written at bind");
        assert!(
            recorded.trim().starts_with(r"\\.\pipe\lcg-"),
            "unexpected endpoint {recorded:?}"
        );
    }

    let status = call_checked(
        &mut conn,
        &mut service.child,
        2,
        "knowledge_status",
        json!({}),
    );
    assert_eq!(
        status.pointer("/result/connected"),
        Some(&Value::Bool(true)),
        "knowledge_status: {status}"
    );

    let closed = call_checked(
        &mut conn,
        &mut service.child,
        3,
        "knowledge_close",
        json!({}),
    );
    assert!(closed.get("error").is_none(), "knowledge_close: {closed}");
    drop(conn);

    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(status) = service.child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "service did not exit after knowledge_close"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(exit.success(), "service exited with {exit}");
}

/// FR-001/FR-002/FR-003: insert entities carrying literal `FLOAT[]` embeddings, build a vector
/// index over them (after insert, matching `ldb_spike_ipc.rs::test_hnsw_vector_query`'s
/// insert-then-index shape and the #561 crash trigger), then query it and assert a real
/// nearest-neighbor row comes back — not merely that the call completed without error.
///
/// Uses a dedicated throwaway table (`E2eIndexProbe`) rather than `Entity`, whose vector indexes
/// are already built eagerly at startup (issue #208) over an empty table: reusing those names
/// would either collide ("already exists") or skip real index construction over populated data
/// entirely, missing the code path the #561 crash actually lives in.
#[test]
fn vector_index_create_and_query_returns_real_rows() {
    let (mut service, mut conn) = spawn_service();
    let child = &mut service.child;

    run_cypher(
        &mut conn,
        child,
        2,
        "CREATE NODE TABLE E2eIndexProbe(uuid STRING PRIMARY KEY, name STRING, summary STRING, embedding FLOAT[8])",
    );

    for i in 0..3usize {
        let embedding = format_float_array(&unit_vec(i));
        let insert = format!(
            "CREATE (:E2eIndexProbe {{uuid: 'v{i}', name: 'Probe {i}', summary: 'summary {i}', embedding: {embedding}}})"
        );
        run_cypher(&mut conn, child, 3 + i as u64, &insert);
    }

    run_cypher(
        &mut conn,
        child,
        10,
        "CALL CREATE_VECTOR_INDEX('E2eIndexProbe', 'e2e_vector_idx', 'embedding', metric := 'cosine')",
    );

    let query_vec = format_float_array(&unit_vec(0));
    let query = format!(
        "CALL QUERY_VECTOR_INDEX('E2eIndexProbe', 'e2e_vector_idx', {query_vec}, 3) \
         RETURN node.uuid, distance"
    );
    let queried = run_cypher(&mut conn, child, 11, &query);

    let rows = rows_of(&queried);
    assert!(!rows.is_empty(), "vector query returned no rows: {queried}");
    let first = rows[0]
        .as_array()
        .unwrap_or_else(|| panic!("row is not an array: {queried}"));
    assert_eq!(
        first.first().and_then(Value::as_str),
        Some("v0"),
        "nearest neighbor should be v0 (same direction as the query vector): {queried}"
    );
}

/// FR-004/FR-005: create a full-text index over inserted entities and query it, asserting a real
/// row matching the expected inserted content comes back. Mirrors
/// `ldb_spike_ipc.rs::test_fts_index_creation_and_query`'s shape, over the same dedicated
/// throwaway table `vector_index_create_and_query_returns_real_rows` uses (its own subprocess,
/// per FR-007 — a crash here cannot affect, or be masked by, the vector-path test).
#[test]
fn fts_index_create_and_query_returns_real_rows() {
    let (mut service, mut conn) = spawn_service();
    let child = &mut service.child;

    run_cypher(
        &mut conn,
        child,
        2,
        "CREATE NODE TABLE E2eIndexProbe(uuid STRING PRIMARY KEY, name STRING, summary STRING)",
    );

    let fixtures = [
        ("f0", "Widget Zero", "a hydraulic pump manufactured in Ohio"),
        ("f1", "Widget One", "a cardboard box factory"),
        ("f2", "Widget Two", "an electric motor assembly line"),
    ];
    for (i, (uuid, name, summary)) in fixtures.iter().enumerate() {
        let insert = format!(
            "CREATE (:E2eIndexProbe {{uuid: '{uuid}', name: '{name}', summary: '{summary}'}})"
        );
        run_cypher(&mut conn, child, 3 + i as u64, &insert);
    }

    run_cypher(
        &mut conn,
        child,
        10,
        "CALL CREATE_FTS_INDEX('E2eIndexProbe', 'e2e_fts_idx', ['name', 'summary'])",
    );

    let queried = run_cypher(
        &mut conn,
        child,
        11,
        "CALL QUERY_FTS_INDEX('E2eIndexProbe', 'e2e_fts_idx', 'hydraulic') \
         WITH node, score RETURN node.uuid, score",
    );

    let rows = rows_of(&queried);
    assert!(!rows.is_empty(), "FTS query returned no rows: {queried}");
    assert!(
        rows.iter()
            .any(|r| r.get(0).and_then(Value::as_str) == Some("f0")),
        "FTS query for 'hydraulic' should match f0, whose summary contains it: {queried}"
    );
}
