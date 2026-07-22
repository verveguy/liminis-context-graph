//! Shared test helpers for the MCP-over-stdio integration tests (issue #195). Spawns the
//! compiled binary as a subprocess and drives it over stdin/stdout with newline-delimited
//! JSON-RPC 2.0 — the same "spawn the real binary" pattern `clean_shutdown.rs` and
//! `migration_binary.rs` use for the Unix-socket protocol, applied to the MCP transport.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

/// Spawns a minimal stub HTTP embedder on a random OS-assigned port, so tests don't depend on
/// a real embedder sidecar being available. Mirrors `clean_shutdown.rs`'s helper.
pub fn spawn_stub_embedder() -> u16 {
    use std::io::Read;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    std::thread::spawn(move || {
        let embedding = format!("[{}]", vec!["0.0"; 768].join(","));
        let body = format!(
            r#"{{"object":"list","data":[{{"object":"embedding","embedding":{embedding},"index":0}}],"model":"stub-model","usage":{{"prompt_tokens":1,"total_tokens":1}}}}"#
        );
        let http_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = Read::read(&mut s, &mut buf);
            let _ = Write::write_all(&mut s, http_response.as_bytes());
        }
    });

    port
}

pub fn binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_liminis-context-graph")
}

/// Drives one MCP-over-stdio subprocess. Reads run on a dedicated background thread so every
/// wait in the test has an explicit timeout instead of risking an indefinite blocking read.
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Lines observed that were not the response to the most recently awaited id — mostly
    /// `notifications/progress`. Kept so progress tests can inspect them.
    pub stashed_notifications: Vec<Value>,
}

impl McpClient {
    /// Spawns `cmd` with piped stdin/stdout (stderr inherited so failures show in test output).
    pub fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn liminis-context-graph");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");

        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Self {
            child,
            stdin,
            lines: rx,
            next_id: 1,
            stashed_notifications: Vec::new(),
        }
    }

    fn write_line(&mut self, value: &Value) {
        let line = serde_json::to_string(value).expect("serialize request");
        writeln!(self.stdin, "{line}").expect("write to child stdin");
        self.stdin.flush().expect("flush child stdin");
    }

    fn recv_line(&mut self, timeout: Duration) -> Value {
        let raw = self
            .lines
            .recv_timeout(timeout)
            .unwrap_or_else(|e| panic!("timed out waiting for a line from child stdout: {e}"));
        let trimmed = raw.trim();
        serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("bad JSON line {trimmed:?}: {e}"))
    }

    /// Sends a JSON-RPC request and waits (up to `timeout`) for the response matching `id`,
    /// stashing any other lines observed in the meantime (e.g. progress notifications).
    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));

        loop {
            let value = self.recv_line(timeout);
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return value;
            }
            self.stashed_notifications.push(value);
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Performs the mandatory MCP initialize handshake. Must be called before any other request.
    pub fn initialize(&mut self) -> Value {
        let resp = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "lcg-test-client", "version": "0.0.1"}
            }),
            Duration::from_secs(15),
        );
        self.notify("notifications/initialized", json!({}));
        resp
    }

    pub fn list_tools(&mut self) -> Vec<Value> {
        let resp = self.request("tools/list", json!({}), Duration::from_secs(10));
        resp["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            Duration::from_secs(20),
        )
    }

    /// Like `call_tool`, but attaches an MCP progress token (per SEP-1319 `_meta.progressToken`)
    /// so streaming tools bridge progress notifications (FR-007).
    pub fn call_tool_with_progress(
        &mut self,
        name: &str,
        arguments: Value,
        progress_token: &str,
        timeout: Duration,
    ) -> Value {
        self.request(
            "tools/call",
            json!({
                "_meta": {"progressToken": progress_token},
                "name": name,
                "arguments": arguments
            }),
            timeout,
        )
    }

    /// Waits until at least one stashed `notifications/progress` line has arrived, or panics
    /// after `timeout`.
    pub fn wait_for_progress_notification(&mut self, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(pos) = self.stashed_notifications.iter().position(|v| {
                v.get("method").and_then(|m| m.as_str()) == Some("notifications/progress")
            }) {
                return self.stashed_notifications.remove(pos);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for a notifications/progress line");
            }
            let value = self.recv_line(remaining.min(Duration::from_secs(5)));
            self.stashed_notifications.push(value);
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for the child process to exit on its own (e.g. after a `knowledge_close` call),
    /// returning its exit status.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status;
            }
            if std::time::Instant::now() >= deadline {
                self.child.kill().ok();
                panic!("child did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
