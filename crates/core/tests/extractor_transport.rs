// Integration tests for OaiExtractor's UDS transport pooling (FR-007/FR-008).
//
// Mirrors crates/core/tests/embedder_transport.rs's stub-server pattern, adapted
// to the /v1/chat/completions wire shape. Exercises `classify_entities` — the
// simplest OaiExtractor operation (a single send_chat call, no max_tokens retry
// loop) — as the vehicle for driving calls through the pooled UDS transport.

#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use lcg_core::extractor::{Extractor, OaiExtractor};
#[cfg(unix)]
use lcg_core::telemetry::NoopSink;
#[cfg(unix)]
use tokio::task::JoinHandle;

// ── Stub helpers ─────────────────────────────────────────────────────────────

#[cfg(unix)]
fn oai_chat_response_json(entity_type: &str) -> String {
    let content = serde_json::to_string(&serde_json::json!({"types": [entity_type]})).unwrap();
    let escaped_content = serde_json::to_string(&content).unwrap();
    format!(
        r#"{{"choices":[{{"finish_reason":"stop","message":{{"role":"assistant","content":{escaped_content}}}}}],"model":"stub-model","usage":{{"prompt_tokens":1,"completion_tokens":1}}}}"#
    )
}

/// Asserts that the request body is a JSON object with a `"messages"` field
/// (OpenAI chat-completions contract).
#[cfg(unix)]
fn assert_oai_chat_request_body(body: &[u8]) {
    let json: serde_json::Value =
        serde_json::from_slice(body).expect("stub: request body should be valid JSON");
    assert!(
        json.get("messages").is_some_and(|m| m.is_array()),
        "stub: expected OpenAI-compatible 'messages' array in request body, got: {json}"
    );
}

/// Like `write_http_response` in embedder_transport.rs, but omits
/// `Connection: close` so the client can reuse the connection for a
/// subsequent request (HTTP/1.1 keep-alive default).
#[cfg(unix)]
async fn write_http_response_keepalive(
    writer: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    body: &str,
) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    writer.write_all(response.as_bytes()).await.ok();
}

/// Like `read_http_request_body` in embedder_transport.rs, but distinguishes
/// "client closed the connection before sending another request" (`None`)
/// from a request with an empty body (`Some(vec![])`), so a keep-alive server
/// can tell when to stop reading from a connection instead of busy-looping on
/// repeated EOF reads.
#[cfg(unix)]
async fn read_http_request_body_keepalive(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Option<Vec<u8>> {
    use tokio::io::AsyncBufReadExt;

    let mut content_length: usize = 0;
    let mut saw_any_bytes = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 {
            if !saw_any_bytes {
                return None;
            }
            break;
        }
        saw_any_bytes = true;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            if let Some(v) = lower.split(':').nth(1) {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        use tokio::io::AsyncReadExt;
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await.ok();
        Some(body)
    } else {
        Some(Vec::new())
    }
}

/// Spawns a one-shot stub UDS server (`Connection: close`) at `path`. Returns
/// after the listener is bound.
#[cfg(unix)]
async fn spawn_stub_uds_server(path: &std::path::Path) -> JoinHandle<()> {
    use tokio::io::BufReader;
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(path).unwrap();
    let body = oai_chat_response_json("Person");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let response_body = body.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                if let Some(request_body) = read_http_request_body_keepalive(&mut reader).await {
                    assert_oai_chat_request_body(&request_body);
                }
                use tokio::io::AsyncWriteExt;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                write_half.write_all(response.as_bytes()).await.ok();
            });
        }
    })
}

/// A stub UDS server whose accepted connections stay alive across multiple
/// requests. `shutdown()` tears down both the accept loop and every
/// already-accepted connection, simulating the sidecar process dying.
#[cfg(unix)]
struct StubUdsKeepAliveServer {
    accept_task: JoinHandle<()>,
    conn_tasks: std::sync::Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
}

#[cfg(unix)]
impl StubUdsKeepAliveServer {
    fn shutdown(&self) {
        self.accept_task.abort();
        for task in self.conn_tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

/// Spawns a keep-alive stub UDS server at `path`. Each accepted connection is
/// read in a loop and reused for subsequent requests, rather than closed
/// after one response — required to exercise connection pooling, since a
/// `Connection: close` stub (see `spawn_stub_uds_server`) would force a
/// re-dial on every call and hide pooling entirely. Returns the server handle
/// and a shared counter of accepted connections.
#[cfg(unix)]
async fn spawn_stub_uds_keepalive_server(
    path: &std::path::Path,
    per_request_delay: std::time::Duration,
) -> (
    StubUdsKeepAliveServer,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::BufReader;
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(path).unwrap();
    let body = oai_chat_response_json("Person");
    let accept_count = Arc::new(AtomicUsize::new(0));
    let accept_count_task = accept_count.clone();
    let conn_tasks: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let conn_tasks_accept = conn_tasks.clone();

    let accept_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            accept_count_task.fetch_add(1, Ordering::SeqCst);
            let response_body = body.clone();
            let delay = per_request_delay;
            let handle = tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                loop {
                    let Some(request_body) = read_http_request_body_keepalive(&mut reader).await
                    else {
                        break;
                    };
                    assert_oai_chat_request_body(&request_body);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    write_http_response_keepalive(&mut write_half, &response_body).await;
                }
            });
            conn_tasks_accept.lock().unwrap().push(handle);
        }
    });

    (
        StubUdsKeepAliveServer {
            accept_task,
            conn_tasks,
        },
        accept_count,
    )
}

#[cfg(unix)]
fn make_extractor(sock_path: &std::path::Path) -> OaiExtractor {
    OaiExtractor::new_uds(
        sock_path.to_str().unwrap(),
        "test-model",
        Arc::new(NoopSink),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn uds_transport_classify_entities_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("extract_test.sock");
    let _server = spawn_stub_uds_server(&sock_path).await;
    let extractor = make_extractor(&sock_path);

    let entities = [("Alice", "a person mentioned in the text")];
    let result = extractor.classify_entities(&entities, None).await.unwrap();
    assert_eq!(result, vec!["Person".to_string()]);
}

/// SC-001/FR-007: a workload issuing many UDS-transport chat-completion calls
/// opens O(1) (pool-bounded, not O(N)) underlying socket connections. Against
/// a keep-alive stub, 20 sequential classify_entities() calls should reuse
/// the pool's held connections rather than dialing fresh each time —
/// accepted-connection count should stay at or below the pool size (4), not
/// grow to 20.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_connection_count_bounded() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("pool_bound_test.sock");
    let (_server, accept_count) =
        spawn_stub_uds_keepalive_server(&sock_path, std::time::Duration::ZERO).await;
    let extractor = make_extractor(&sock_path);

    let entities = [("Alice", "a person")];
    for _ in 0..20 {
        extractor.classify_entities(&entities, None).await.unwrap();
    }

    let accepted = accept_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        accepted <= 4,
        "expected UDS connection count bounded by pool size, got {accepted} \
         accepted connections for 20 sequential classify_entities() calls"
    );
}

/// SC-002/FR-008: killing and restarting the sidecar mid-run causes at most
/// one failed extraction call internally, with the *caller-visible* call
/// still succeeding transparently once the pool re-dials.
///
/// The pool round-robins across `UDS_POOL_SIZE` slots (one per call), so a
/// single call before and after the restart would land on two different,
/// independently-lazily-dialed slots and never actually touch a connection
/// broken by the restart — exercising nothing but a fresh dial into a virgin
/// slot. To genuinely exercise the redial-on-broken-connection path, every
/// slot must hold a connection to the *first* server before the restart, so
/// that every slot in the following round is provably stale.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_reconnects_after_restart() {
    const UDS_POOL_SIZE: usize = 4;

    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("restart_test.sock");
    let (server1, _accept_count1) =
        spawn_stub_uds_keepalive_server(&sock_path, std::time::Duration::ZERO).await;
    let extractor = make_extractor(&sock_path);
    let entities = [("Alice", "a person")];

    // Populate every pool slot with a held connection to the first server.
    for _ in 0..UDS_POOL_SIZE {
        let result = extractor.classify_entities(&entities, None).await.unwrap();
        assert_eq!(result, vec!["Person".to_string()]);
    }

    // Simulate the sidecar process dying and restarting: tear down the old
    // server (accept loop + already-accepted connections) and remove its
    // socket file so a fresh listener can bind at the same path.
    server1.shutdown();
    std::fs::remove_file(&sock_path).ok();
    let (_server2, _accept_count2) =
        spawn_stub_uds_keepalive_server(&sock_path, std::time::Duration::ZERO).await;

    // Cycle through every slot again — each now holds a connection to the
    // dead first server, so each independently detects the break, re-dials
    // once against the new server, and the call still succeeds transparently
    // with no manual retry needed by the caller.
    for _ in 0..UDS_POOL_SIZE {
        let result = extractor.classify_entities(&entities, None).await.unwrap();
        assert_eq!(result, vec!["Person".to_string()]);
    }
}

/// Edge case (spec): concurrent extraction calls must not be serialized
/// behind a single held connection worse than today. HTTP/1.1 serializes one
/// in-flight request per connection, so a single held connection would fully
/// serialize concurrent calls; the pool must let them proceed in parallel
/// instead. This asserts wall-clock for N concurrent calls is well under
/// N * per-call-delay (full serialization), with a generous margin to avoid
/// CI flakiness.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_concurrent_calls_not_fully_serialized() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("concurrent_test.sock");
    let per_request_delay = std::time::Duration::from_millis(50);
    let (_server, _accept_count) =
        spawn_stub_uds_keepalive_server(&sock_path, per_request_delay).await;
    let extractor = make_extractor(&sock_path);
    let entities = [("Alice", "a person")];

    let concurrent_calls = 8; // 2x pool size, so pool reuse is also exercised
    let start = std::time::Instant::now();
    let results = futures::future::join_all(
        (0..concurrent_calls).map(|_| extractor.classify_entities(&entities, None)),
    )
    .await;
    let elapsed = start.elapsed();

    for result in results {
        assert_eq!(result.unwrap(), vec!["Person".to_string()]);
    }

    let fully_serial = per_request_delay * concurrent_calls;
    assert!(
        elapsed < fully_serial / 2,
        "expected concurrent calls to parallelize across the pool \
         (elapsed {elapsed:?} should be well under fully-serial {fully_serial:?})"
    );
}
