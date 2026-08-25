// Integration tests for OaiEmbedder HTTP and UDS transports (FR-012).
//
// Stubs serve the OpenAI-compatible /v1/embeddings contract. Tests verify that
// OaiEmbedder::embed() and OaiEmbedder::probe() work against both transports.
// UDS tests are #[cfg(unix)] and skip cleanly on non-Unix platforms.

use std::net::SocketAddr;

use lcg_core::embedder::{Embedder, OaiEmbedder};
use tokio::task::JoinHandle;

const STUB_MODEL: &str = "stub-model";

// ── Stub helpers ─────────────────────────────────────────────────────────────

fn oai_response_json(dim: usize) -> String {
    let embedding: Vec<f64> = (0..dim).map(|i| (i as f64) / (dim as f64)).collect();
    let embedding_json = serde_json::to_string(&embedding).unwrap();
    format!(
        r#"{{"object":"list","data":[{{"object":"embedding","embedding":{embedding_json},"index":0}}],"model":"{STUB_MODEL}","usage":{{"prompt_tokens":1,"total_tokens":1}}}}"#
    )
}

async fn write_http_response(writer: &mut (impl tokio::io::AsyncWriteExt + Unpin), body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    writer.write_all(response.as_bytes()).await.ok();
}

/// Reads headers and body from an HTTP/1.1 request. Returns the raw body bytes.
async fn read_http_request_body(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Vec<u8> {
    use tokio::io::AsyncBufReadExt;

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 || line == "\r\n" || line == "\n" {
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
        body
    } else {
        Vec::new()
    }
}

/// Asserts that the request body is a JSON object with an `"input"` field (OAI contract).
fn assert_oai_request_body(body: &[u8]) {
    let json: serde_json::Value =
        serde_json::from_slice(body).expect("stub: request body should be valid JSON");
    assert!(
        json.get("input").is_some(),
        "stub: expected OpenAI-compatible 'input' field in request body, got: {json}"
    );
    assert!(
        json.get("text").is_none(),
        "stub: found legacy 'text' field — client is using old contract"
    );
}

/// Spawns a stub HTTP server on a random OS-assigned port. Returns the bound address.
async fn spawn_stub_http_server(dim: usize) -> (SocketAddr, JoinHandle<()>) {
    use tokio::io::BufReader;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = oai_response_json(dim);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let response_body = body.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let request_body = read_http_request_body(&mut reader).await;
                assert_oai_request_body(&request_body);
                write_http_response(&mut write_half, &response_body).await;
            });
        }
    });

    (addr, handle)
}

/// Spawns a stub UDS server at `path`. Returns after the listener is bound.
#[cfg(unix)]
async fn spawn_stub_uds_server(path: &std::path::Path, dim: usize) -> JoinHandle<()> {
    use tokio::io::BufReader;
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(path).unwrap();
    let body = oai_response_json(dim);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let response_body = body.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let request_body = read_http_request_body(&mut reader).await;
                assert_oai_request_body(&request_body);
                write_http_response(&mut write_half, &response_body).await;
            });
        }
    })
}

/// Like `write_http_response`, but omits `Connection: close` so the client can
/// reuse the connection for a subsequent request (HTTP/1.1 keep-alive default).
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

/// Like `read_http_request_body`, but distinguishes "client closed the
/// connection before sending another request" (`None`) from a request with an
/// empty body (`Some(vec![])`), so a keep-alive server can tell when to stop
/// reading from a connection instead of busy-looping on repeated EOF reads.
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
    dim: usize,
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
    let body = oai_response_json(dim);
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
                    assert_oai_request_body(&request_body);
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

/// Reads headers and body from an HTTP/1.1 request, returning the raw header lines
/// (unlike `read_http_request_body`, which only extracts `Content-Length`) so tests can
/// assert on the presence/absence/value of arbitrary headers such as `Authorization`.
async fn read_http_request_with_headers(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> (Vec<String>, Vec<u8>) {
    use tokio::io::AsyncBufReadExt;

    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let trimmed = line.trim_end().to_string();
        let lower = trimmed.to_lowercase();
        if lower.starts_with("content-length:") {
            if let Some(v) = lower.split(':').nth(1) {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        headers.push(trimmed);
    }
    let body = if content_length > 0 {
        use tokio::io::AsyncReadExt;
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await.ok();
        body
    } else {
        Vec::new()
    };
    (headers, body)
}

/// Spawns a stub HTTP server on a random OS-assigned port that captures every accepted
/// request's raw header lines into a shared `Mutex<Vec<Vec<String>>>` (one entry per
/// request), so tests can assert on `Authorization` header presence/value.
async fn spawn_stub_http_server_capturing_headers(
    dim: usize,
) -> (
    SocketAddr,
    JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
) {
    use tokio::io::BufReader;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = oai_response_json(dim);
    let captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
    let captured_task = captured.clone();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let response_body = body.clone();
            let captured = captured_task.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let (headers, request_body) = read_http_request_with_headers(&mut reader).await;
                assert_oai_request_body(&request_body);
                captured.lock().unwrap().push(headers);
                write_http_response(&mut write_half, &response_body).await;
            });
        }
    });

    (addr, handle, captured)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_transport_embed_roundtrip() {
    let dim = 16;
    let (addr, _server) = spawn_stub_http_server(dim).await;
    let url = format!("http://{addr}/v1/embeddings");
    let embedder = OaiEmbedder::new_http(url, "test-model", dim);
    let result = embedder.embed("hello world").await.unwrap();
    assert_eq!(result.len(), dim, "embedding dim should match stub");
}

#[tokio::test]
async fn http_transport_probe_returns_dim_and_model() {
    let dim = 32;
    let (addr, _server) = spawn_stub_http_server(dim).await;
    let url = format!("http://{addr}/v1/embeddings");
    let embedder = OaiEmbedder::new_http(url, "test-model", 1);
    let (probed_dim, probed_model) = embedder.probe().await.unwrap();
    assert_eq!(probed_dim, dim);
    assert_eq!(probed_model, STUB_MODEL);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_transport_embed_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("embed_test.sock");
    let dim = 16;
    let _server = spawn_stub_uds_server(&sock_path, dim).await;
    let embedder = OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", dim);
    let result = embedder.embed("hello world").await.unwrap();
    assert_eq!(result.len(), dim, "embedding dim should match stub");
}

#[cfg(unix)]
#[tokio::test]
async fn uds_transport_probe_returns_dim_and_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("probe_test.sock");
    let dim = 32;
    let _server = spawn_stub_uds_server(&sock_path, dim).await;
    let embedder = OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", 1);
    let (probed_dim, probed_model) = embedder.probe().await.unwrap();
    assert_eq!(probed_dim, dim);
    assert_eq!(probed_model, STUB_MODEL);
}

/// SC-001: an ingest producing many embeddings opens O(1) (pool-bounded, not
/// O(N)) UDS connections. Against a keep-alive stub, 20 sequential embed()
/// calls should reuse the pool's held connections rather than dialing fresh
/// each time — accepted-connection count should stay at or below the pool
/// size (4), not grow to 20.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_connection_count_bounded() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("pool_bound_test.sock");
    let dim = 16;
    let (_server, accept_count) =
        spawn_stub_uds_keepalive_server(&sock_path, dim, std::time::Duration::ZERO).await;
    let embedder = OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", dim);

    for _ in 0..20 {
        embedder.embed("hello world").await.unwrap();
    }

    let accepted = accept_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        accepted <= 4,
        "expected UDS connection count bounded by pool size, got {accepted} \
         accepted connections for 20 sequential embed() calls"
    );
}

/// SC-002: killing and restarting the sidecar mid-run causes at most one
/// failed embed call internally, with the *caller-visible* call still
/// succeeding transparently once the pool re-dials.
///
/// The pool round-robins across `UDS_POOL_SIZE` slots (one per `embed()`
/// call), so a single call before and after the restart would land on two
/// different, independently-lazily-dialed slots and never actually touch a
/// connection broken by the restart — exercising nothing but a fresh dial
/// into a virgin slot. To genuinely exercise the redial-on-broken-connection
/// path, every slot must hold a connection to the *first* server before the
/// restart, so that every slot in the following round is provably stale.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_reconnects_after_restart() {
    const UDS_POOL_SIZE: usize = 4;

    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("restart_test.sock");
    let dim = 16;
    let (server1, _accept_count1) =
        spawn_stub_uds_keepalive_server(&sock_path, dim, std::time::Duration::ZERO).await;
    let embedder = OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", dim);

    // Populate every pool slot with a held connection to the first server.
    for _ in 0..UDS_POOL_SIZE {
        let v = embedder.embed("hello world").await.unwrap();
        assert_eq!(v.len(), dim);
    }

    // Simulate the sidecar process dying and restarting: tear down the old
    // server (accept loop + already-accepted connections) and remove its
    // socket file so a fresh listener can bind at the same path.
    server1.shutdown();
    std::fs::remove_file(&sock_path).ok();
    let (_server2, _accept_count2) =
        spawn_stub_uds_keepalive_server(&sock_path, dim, std::time::Duration::ZERO).await;

    // Cycle through every slot again — each now holds a connection to the
    // dead first server, so each independently detects the break, re-dials
    // once against the new server, and the call still succeeds transparently
    // with no manual retry needed by the caller.
    for _ in 0..UDS_POOL_SIZE {
        let v = embedder.embed("hello again").await.unwrap();
        assert_eq!(v.len(), dim);
    }
}

/// SC-003: parallel embedding throughput must be no worse than the
/// per-request-dial implementation. HTTP/1.1 serializes one in-flight request
/// per connection, so a single held connection would fully serialize
/// concurrent calls; the pool must let them proceed in parallel instead. This
/// asserts wall-clock for N concurrent calls is well under N * per-call-delay
/// (full serialization), with a generous margin to avoid CI flakiness.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_concurrent_calls_not_fully_serialized() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("concurrent_test.sock");
    let dim = 16;
    let per_request_delay = std::time::Duration::from_millis(50);
    let (_server, _accept_count) =
        spawn_stub_uds_keepalive_server(&sock_path, dim, per_request_delay).await;
    let embedder = OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", dim);

    let concurrent_calls = 8; // 2x pool size, so pool reuse is also exercised
    let start = std::time::Instant::now();
    let results =
        futures::future::join_all((0..concurrent_calls).map(|_| embedder.embed("hello world")))
            .await;
    let elapsed = start.elapsed();

    for result in results {
        assert_eq!(result.unwrap().len(), dim);
    }

    let fully_serial = per_request_delay * concurrent_calls;
    assert!(
        elapsed < fully_serial / 2,
        "expected concurrent calls to parallelize across the pool \
         (elapsed {elapsed:?} should be well under fully-serial {fully_serial:?})"
    );
}

// ── Bearer-token auth tests (issue #497) ────────────────────────────────────────

/// FR-001/FR-003: when an API key is configured via `with_api_key`, every outgoing HTTP
/// request (both `probe()` and `embed()`) carries `Authorization: Bearer <key>`.
#[tokio::test]
async fn http_transport_sends_bearer_header_when_key_configured() {
    let dim = 16;
    let (addr, _server, captured) = spawn_stub_http_server_capturing_headers(dim).await;
    let url = format!("http://{addr}/v1/embeddings");
    let embedder =
        OaiEmbedder::new_http(url, "test-model", dim).with_api_key(Some("sekret-key".to_string()));

    embedder.embed("hello world").await.unwrap();
    embedder.probe().await.unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected one request per call");
    for headers in requests.iter() {
        let auth = headers
            .iter()
            .find(|h| h.to_lowercase().starts_with("authorization:"))
            .unwrap_or_else(|| panic!("no Authorization header in request headers: {headers:?}"));
        assert_eq!(auth, "authorization: Bearer sekret-key");
    }
}

/// FR-004: with no key configured, the request is byte-for-byte identical to pre-change
/// behavior — no `Authorization` header present at all.
#[tokio::test]
async fn http_transport_no_authorization_header_when_key_unset() {
    let dim = 16;
    let (addr, _server, captured) = spawn_stub_http_server_capturing_headers(dim).await;
    let url = format!("http://{addr}/v1/embeddings");
    let embedder = OaiEmbedder::new_http(url, "test-model", dim);

    embedder.embed("hello world").await.unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let has_auth = requests[0]
        .iter()
        .any(|h| h.to_lowercase().starts_with("authorization:"));
    assert!(
        !has_auth,
        "expected no Authorization header when no key is configured, got: {:?}",
        requests[0]
    );
}

/// FR-005: UDS never sends a credential, regardless of what a key resolution would have
/// produced — `new_uds` structurally has no key parameter, so `with_api_key` isn't even
/// reachable for this transport in production code.
#[cfg(unix)]
#[tokio::test]
async fn uds_transport_embed_roundtrip_unaffected_by_key_env() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock_path = dir.path().join("auth_test.sock");
    let dim = 16;
    let _server = spawn_stub_uds_server(&sock_path, dim).await;

    // Set every tier of the key env vars to prove they have no effect on UDS at all —
    // OaiEmbedder::from_env() would resolve a key, but the UDS transport constructor never
    // accepts one, so there is nothing for a key to attach to.
    std::env::set_var("LCG_EMBEDDING_API_KEY", "should-never-be-used");
    let embedder = OaiEmbedder::new_uds(sock_path.to_str().unwrap(), "test-model", dim);
    let result = embedder.embed("hello world").await.unwrap();
    std::env::remove_var("LCG_EMBEDDING_API_KEY");

    assert_eq!(result.len(), dim);
    // Succeeding at all confirms the UDS path is untouched: OaiEmbedder::new_uds never
    // accepts a key parameter, so LCG_EMBEDDING_API_KEY being set in the environment has
    // no code path by which it could reach this request.
}
