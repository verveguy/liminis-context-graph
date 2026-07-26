//! Shared test helpers for the MCP-over-stdio integration tests (issue #195). Spawns the
//! compiled binary as a subprocess and drives it over stdin/stdout with newline-delimited
//! JSON-RPC 2.0 — the same "spawn the real binary" pattern `clean_shutdown.rs` and
//! `migration_binary.rs` use for the Unix-socket protocol, applied to the MCP transport.

#![allow(dead_code)]

pub mod real_corpus;

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

// ── Stub extraction endpoint (issue #235) ─────────────────────────────────────
//
// A deterministic, content-keyed OpenAI-compatible `/v1/chat/completions` stub, so the
// write/mutation-path suite can exercise `knowledge_process_chunk`/`knowledge_add_episode`/
// `knowledge_reprocess_entity_types`/`knowledge_reprocess_relation_types` with no live LLM.
// `OaiExtractor::send_chat` (crates/core/src/extractor.rs) posts a plain JSON body
// (`{model, max_tokens, response_format:{type:"json_object"}, messages:[{role:"system",...},
// {role:"user",...}]}`) and expects a standard OpenAI chat-completion response whose
// `choices[0].message.content` is itself a JSON string. There are four distinct request shapes
// sharing this one endpoint (entity extraction, edge extraction, entity classification, relation
// classification) — distinguished here by fixed substrings that `extractor.rs`'s own prompt
// construction always includes verbatim in the system prompt (see the marker constants below).

/// One canned extraction result: the entities and edges a marker in the ingested text should
/// produce. `edges` reference `entities`/pre-existing-fixture names by exact string; the stub
/// does not validate cross-references, it just echoes what the test configured.
#[derive(Clone, Default)]
pub struct ExtractionFixture {
    pub entities: Vec<(String, String, String)>, // (name, entity_type, summary)
    pub edges: Vec<(String, String, String, Option<String>)>, // (source_name, target_name, fact, relation_type)
}

/// Shared, mutable configuration for [`spawn_stub_extractor`]'s server thread. The test updates
/// this (via the returned `Arc<Mutex<_>>`) before each `tools/call` that will trigger
/// extraction/classification, so one long-lived stub server can serve every ingest/reclassify
/// call in a suite without restarting.
#[derive(Default)]
pub struct StubExtractorConfig {
    /// Marker substring (expected to appear in the raw episode/chunk text) → the extraction
    /// result to return for both the entity pass and the edge pass. Checked as a substring
    /// against the request's user-message text; first configured marker found wins.
    pub extraction: std::collections::HashMap<String, ExtractionFixture>,
    /// Entity classification verdicts, keyed by exact entity name. Missing key ⇒ empty string
    /// (abstain), matching production's own "LLM returned no assignment" contract.
    pub entity_verdicts: std::collections::HashMap<String, String>,
    /// Relation classification verdicts, keyed by exact edge `fact` text. Missing key ⇒ empty
    /// string (abstain ⇒ caller writes the literal `UNCLASSIFIED` sentinel).
    pub relation_verdicts: std::collections::HashMap<String, String>,
}

/// Spawns the stub extraction server on a random OS-assigned port and returns
/// `(port, shared_config)`. The caller mutates `shared_config` (via `.lock().unwrap()`) to
/// script each subsequent extraction/classification call before issuing the `tools/call` that
/// triggers it.
pub fn spawn_stub_extractor() -> (u16, std::sync::Arc<std::sync::Mutex<StubExtractorConfig>>) {
    use std::io::{BufRead, BufReader, Read as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(Mutex::new(StubExtractorConfig::default()));
    let config_thread = Arc::clone(&config);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stub tcp stream"));

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 || line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some(rest) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::to_string)
                {
                    content_length = rest.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

            let content = build_stub_response_content(&req, &config_thread);
            let resp_body = json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": content},
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            });
            let resp_body_str = serde_json::to_string(&resp_body).unwrap();
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body_str.len(),
                resp_body_str
            );
            let _ = Write::write_all(&mut stream, http_response.as_bytes());
        }
    });

    (port, config)
}

/// Fixed substrings `extractor.rs`'s `OaiExtractor` always includes verbatim in its system
/// prompt for each of the four request shapes this stub must distinguish (see
/// `ENTITY_JSON_INSTRUCTION`/`EDGE_JSON_INSTRUCTION`/`do_classify_entities`/
/// `do_classify_relations` in crates/core/src/extractor.rs).
const ENTITY_EXTRACTION_MARKER: &str = "\"entities\": [{\"name\"";
const EDGE_EXTRACTION_MARKER: &str = "\"edges\": [{\"source_name\"";
const ENTITY_CLASSIFY_MARKER: &str = "entity classifier";
const RELATION_CLASSIFY_MARKER: &str = "relation classifier";

fn stub_message(req: &Value, role: &str) -> String {
    req["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["role"].as_str() == Some(role))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

fn build_stub_response_content(
    req: &Value,
    config: &std::sync::Mutex<StubExtractorConfig>,
) -> String {
    let system_text = stub_message(req, "system");
    let user_text = stub_message(req, "user");
    let config = config.lock().unwrap_or_else(|e| e.into_inner());

    if system_text.contains(ENTITY_CLASSIFY_MARKER) {
        let items = extract_json_array_after_prefix(&user_text);
        let types: Vec<Value> = items
            .iter()
            .map(|item| {
                let name = item["name"].as_str().unwrap_or("");
                json!(config
                    .entity_verdicts
                    .get(name)
                    .cloned()
                    .unwrap_or_default())
            })
            .collect();
        return json!({"types": types}).to_string();
    }

    if system_text.contains(RELATION_CLASSIFY_MARKER) {
        let items = extract_json_array_after_prefix(&user_text);
        let types: Vec<Value> = items
            .iter()
            .map(|item| {
                let fact = item["fact"].as_str().unwrap_or("");
                json!(config
                    .relation_verdicts
                    .get(fact)
                    .cloned()
                    .unwrap_or_default())
            })
            .collect();
        return json!({"types": types}).to_string();
    }

    let fixture = config
        .extraction
        .iter()
        .find(|(marker, _)| user_text.contains(marker.as_str()))
        .map(|(_, fixture)| fixture.clone())
        .unwrap_or_default();

    if system_text.contains(ENTITY_EXTRACTION_MARKER) {
        let entities: Vec<Value> = fixture
            .entities
            .iter()
            .map(|(name, entity_type, summary)| {
                json!({"name": name, "entity_type": entity_type, "summary": summary})
            })
            .collect();
        return json!({"entities": entities}).to_string();
    }

    if system_text.contains(EDGE_EXTRACTION_MARKER) {
        let edges: Vec<Value> = fixture
            .edges
            .iter()
            .map(|(source_name, target_name, fact, relation_type)| {
                json!({
                    "source_name": source_name,
                    "target_name": target_name,
                    "fact": fact,
                    "relation_type": relation_type,
                    "valid_at": Value::Null,
                    "invalid_at": Value::Null,
                })
            })
            .collect();
        return json!({"edges": edges}).to_string();
    }

    // Unrecognized request shape: respond with an empty object rather than panicking on the
    // background thread (a panic there is invisible — the caller just sees a parse failure).
    json!({}).to_string()
}

/// Parses the JSON array embedded after `"...:\n\n"` in a classification user prompt (see
/// `do_classify_entities`/`do_classify_relations`'s `format!("Classify the ... for:\n\n{json}")`
/// construction in extractor.rs).
fn extract_json_array_after_prefix(user_text: &str) -> Vec<Value> {
    let json_start = user_text.find('[').unwrap_or(user_text.len());
    serde_json::from_str(&user_text[json_start..]).unwrap_or_default()
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
