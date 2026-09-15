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

#[path = "common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

use common::{binary_path, spawn_stub_embedder, ChildGuard};

/// One client connection, as a reader/writer pair over the platform's stream type.
struct Connection {
    reader: BufReader<Box<dyn Read>>,
    writer: Box<dyn Write>,
}

impl Connection {
    #[cfg(unix)]
    fn open(socket_path: &Path) -> std::io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self {
            reader: BufReader::new(Box::new(stream.try_clone()?)),
            writer: Box::new(stream),
        })
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
        Ok(Self {
            reader: BufReader::new(Box::new(pipe.try_clone()?)),
            writer: Box::new(pipe),
        })
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{request}").expect("write request");
        self.writer.flush().expect("flush request");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response {line:?}: {e}"))
    }
}

/// Polls `health_check` on fresh connections until it reports healthy — the documented readiness
/// signal (docs/ipc-mcp-reference.md#readiness), since the endpoint exists before the DB opens.
fn wait_until_healthy(socket_path: &Path, timeout: Duration) -> Connection {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("never connected");
    while Instant::now() < deadline {
        match Connection::open(socket_path) {
            Ok(mut conn) => {
                let response = conn.call(1, "health_check", json!({}));
                if response.pointer("/result/healthy") == Some(&Value::Bool(true)) {
                    return conn;
                }
                last = response.to_string();
            }
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("service not healthy within {timeout:?}; last: {last}");
}

#[test]
fn socket_service_answers_over_the_platform_transport_and_closes_cleanly() {
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
    let mut child = ChildGuard::spawn(cmd);

    let mut conn = wait_until_healthy(&socket_path, Duration::from_secs(60));

    // Checked only once healthy: the file is written at bind, which the spawn above races.
    #[cfg(windows)]
    {
        let recorded = std::fs::read_to_string(socket_path.with_extension("endpoint"))
            .expect("service.endpoint written at bind");
        assert!(
            recorded.trim().starts_with(r"\\.\pipe\lcg-"),
            "unexpected endpoint {recorded:?}"
        );
    }

    let status = conn.call(2, "knowledge_status", json!({}));
    assert_eq!(
        status.pointer("/result/connected"),
        Some(&Value::Bool(true)),
        "knowledge_status: {status}"
    );

    let closed = conn.call(3, "knowledge_close", json!({}));
    assert!(closed.get("error").is_none(), "knowledge_close: {closed}");
    drop(conn);

    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
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
