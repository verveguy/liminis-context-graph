// Integration tests for the LLM cassette record/replay decorators (#232).
//
// Stubs a minimal Anthropic /v1/messages endpoint (tool_use response shape) so
// AnthropicExtractor's real HTTP path can be exercised without live credentials or
// network access. Covers all four user stories from specs/232-record-replay-llm-cassette:
//   (a) record -> replay determinism for a bare extractor (User Story 1 / SC-001)
//   (b) a replay miss fails loudly and identifiably (User Story 2 / SC-002)
//   (c) LlmRouter primary/fallback recording, then flat replay (User Story 4 / SC-004)
//   (d) no credential material ever reaches the cassette file (User Story 3 / SC-003)

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use lcg_core::cassette::{CassetteWriter, RecordingExtractor, ReplayingExtractor};
use lcg_core::error::Error;
use lcg_core::extraction_failures::{ExtractionFailureSink, ExtractionFailureWriter};
use lcg_core::extractor::{AnthropicExtractor, ExtractOptions, Extractor};
use lcg_core::llm_router::LlmRouter;
use lcg_core::telemetry::{NoopSink, TelemetrySink};
use lcg_core::types::SourceType;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

// ── Stub Anthropic /v1/messages server ──────────────────────────────────────

fn entity_tool_response() -> String {
    serde_json::json!({
        "stop_reason": "tool_use",
        "content": [{
            "type": "tool_use",
            "id": "toolu_01",
            "name": "extract_entities",
            "input": {
                "entities": [
                    {"name": "Alice", "entity_type": "Person", "summary": "A person"},
                    {"name": "Acme Corp", "entity_type": "Organization", "summary": "A company"}
                ]
            }
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

fn edge_tool_response() -> String {
    serde_json::json!({
        "stop_reason": "tool_use",
        "content": [{
            "type": "tool_use",
            "id": "toolu_02",
            "name": "extract_edges",
            "input": {
                "edges": [{
                    "source_name": "Alice",
                    "target_name": "Acme Corp",
                    "fact": "Alice works at Acme Corp",
                    "relation_type": "WORKS_AT"
                }]
            }
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

async fn write_http_response(writer: &mut (impl tokio::io::AsyncWriteExt + Unpin), body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    writer.write_all(response.as_bytes()).await.ok();
}

/// Reads headers (returned as raw, un-lowercased lines, for credential assertions) and the
/// body from one HTTP/1.1 request.
async fn read_http_request(
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
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            if let Some(v) = lower.split(':').nth(1) {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        headers.push(line.trim_end().to_string());
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

/// Spawns a stub Anthropic `/v1/messages` server on a random OS-assigned port. Dispatches on
/// the request body's `tools[0].name` to return either the entity or edge tool_use response,
/// serving `AnthropicExtractor::do_extract`'s two sequential calls. Every request's headers
/// are appended to `captured_headers` so tests can assert on what was actually sent over the
/// wire (e.g. that a fake API key really was carried in the `x-api-key` header).
async fn spawn_stub_anthropic_server() -> (SocketAddr, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    use tokio::io::BufReader;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let entity_body = entity_tool_response();
    let edge_body = edge_tool_response();
    let captured_headers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_headers_task = Arc::clone(&captured_headers);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let entity_body = entity_body.clone();
            let edge_body = edge_body.clone();
            let captured_headers = Arc::clone(&captured_headers_task);
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let (headers, body) = read_http_request(&mut reader).await;
                captured_headers.lock().unwrap().extend(headers);
                let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let tool_name = json["tools"][0]["name"].as_str().unwrap_or("");
                let response = match tool_name {
                    "extract_entities" => &entity_body,
                    "extract_edges" => &edge_body,
                    other => panic!("stub: unexpected tool name {other:?}"),
                };
                write_http_response(&mut write_half, response).await;
            });
        }
    });

    (addr, handle, captured_headers)
}

/// Spawns a stub `/v1/messages` server that always responds with the same fixed
/// `status_line`/`body`, regardless of request content (#306: used to force each of the three
/// `ExtractionFailure` classes deterministically).
async fn spawn_stub_server_with_fixed_response(
    status_line: &'static str,
    body: &'static str,
) -> (SocketAddr, JoinHandle<()>) {
    use tokio::io::BufReader;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let _ = read_http_request(&mut reader).await;
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = write_half.write_all(response.as_bytes()).await;
            });
        }
    });

    (addr, handle)
}

/// Spawns a stub `/v1/messages` server that dispatches on `tools[0].name` like
/// `spawn_stub_anthropic_server`, but always truncates the edge call (`stop_reason:
/// max_tokens`, regardless of the doubled `max_tokens` sent) while the entity call succeeds
/// normally — #307 FR-004's edge-path truncation scenario, which `spawn_stub_anthropic_server`
/// cannot exercise since it always succeeds.
async fn spawn_stub_server_entities_succeed_edges_exhaust() -> (SocketAddr, JoinHandle<()>) {
    use tokio::io::BufReader;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let entity_body = entity_tool_response();
    let truncated_edge_body = serde_json::json!({
        "stop_reason": "max_tokens",
        "content": [],
        "usage": {"input_tokens": 10, "output_tokens": 8192}
    })
    .to_string();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let entity_body = entity_body.clone();
            let truncated_edge_body = truncated_edge_body.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let (_headers, body) = read_http_request(&mut reader).await;
                let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let tool_name = json["tools"][0]["name"].as_str().unwrap_or("");
                let response = match tool_name {
                    "extract_entities" => &entity_body,
                    "extract_edges" => &truncated_edge_body,
                    other => panic!("stub: unexpected tool name {other:?}"),
                };
                write_http_response(&mut write_half, response).await;
            });
        }
    });

    (addr, handle)
}

/// Binds then immediately drops a TCP listener, returning a `/v1/messages` URL that is
/// guaranteed to refuse connections — used to simulate an unreachable primary extractor
/// without relying on a slow, unpredictable external timeout.
async fn unreachable_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/v1/messages")
}

fn sink() -> Arc<dyn TelemetrySink> {
    Arc::new(NoopSink)
}

fn opts(episode_body: &str) -> ExtractOptions<'_> {
    ExtractOptions {
        episode_body,
        group_id: "g1",
        source_type: SourceType::Text,
        custom_instructions: None,
        reference_time: "2026-01-01T00:00:00Z",
        ontology: None,
        chunk_key: None,
    }
}

fn opts_with_chunk_key<'a>(episode_body: &'a str, chunk_key: &'a str) -> ExtractOptions<'a> {
    ExtractOptions {
        chunk_key: Some(chunk_key),
        ..opts(episode_body)
    }
}

// ── (a) User Story 1 / SC-001: record -> replay determinism ────────────────

#[tokio::test]
async fn record_then_replay_bare_anthropic_extractor() {
    let (addr, _server, _headers) = spawn_stub_anthropic_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        sink(),
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", Arc::clone(&writer));

    let recorded = recorder
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap()
        .result;
    assert_eq!(recorded.entities.len(), 2);
    assert_eq!(recorded.edges.len(), 1);

    // Replay against the same cassette. No stub server is queried — ReplayingExtractor never
    // dials out, so SC-001's "zero outbound LLM requests" holds by construction.
    let replayer = ReplayingExtractor::load(&cassette_path).unwrap();
    let replayed = replayer
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap()
        .result;

    assert_eq!(replayed.entities.len(), recorded.entities.len());
    for (r, o) in replayed.entities.iter().zip(recorded.entities.iter()) {
        assert_eq!(r.name, o.name);
        assert_eq!(r.entity_type, o.entity_type);
        assert_eq!(r.summary, o.summary);
    }
    assert_eq!(replayed.edges.len(), recorded.edges.len());
    for (r, o) in replayed.edges.iter().zip(recorded.edges.iter()) {
        assert_eq!(r.source_name, o.source_name);
        assert_eq!(r.target_name, o.target_name);
        assert_eq!(r.fact, o.fact);
        assert_eq!(r.relation_type, o.relation_type);
    }
}

// ── (b) User Story 2 / SC-002: loud, identifiable failure on cassette miss ──

#[tokio::test]
async fn replay_cassette_miss_is_loud_and_identifiable() {
    let (addr, _server, _headers) = spawn_stub_anthropic_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        sink(),
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", Arc::clone(&writer));
    recorder
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap();

    // A prompt/parsing change or a genuinely different episode alters the semantic request
    // content — replay must fail loudly rather than silently diverge or fall through live.
    let replayer = ReplayingExtractor::load(&cassette_path).unwrap();
    let err = replayer
        .extract(opts("This text was never recorded."))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::CassetteMiss(_)),
        "expected Error::CassetteMiss, got {err:?}"
    );
}

// ── (c) User Story 4 / SC-004: LlmRouter primary/fallback recording, flat replay ─

#[tokio::test]
async fn llm_router_records_per_leaf_and_replays_flat() {
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    // Router A: primary reachable — records under "model-a".
    let (addr_a, _server_a, _headers_a) = spawn_stub_anthropic_server().await;
    let primary_a: Arc<dyn Extractor> = Arc::new(AnthropicExtractor::with_url(
        "model-a".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr_a}/v1/messages"),
        sink(),
    ));
    let primary_a: Arc<dyn Extractor> = Arc::new(RecordingExtractor::new(
        primary_a,
        "anthropic",
        "model-a",
        Arc::clone(&writer),
    ));
    let router_a = LlmRouter::new(
        primary_a,
        "model-a".to_string(),
        None,
        String::new(),
        sink(),
    );
    let result_a = router_a
        .extract(opts("Router A episode text."))
        .await
        .unwrap()
        .result;

    // Router B: primary unreachable, fallback reachable — only the fallback call, under
    // "model-b-fallback", should be recorded; the failed primary attempt must not appear.
    let unreachable = unreachable_url().await;
    let primary_b: Arc<dyn Extractor> = Arc::new(AnthropicExtractor::with_url(
        "model-b-primary".to_string(),
        "sk-test-key".to_string(),
        unreachable,
        sink(),
    ));
    let primary_b: Arc<dyn Extractor> = Arc::new(RecordingExtractor::new(
        primary_b,
        "anthropic",
        "model-b-primary",
        Arc::clone(&writer),
    ));

    let (addr_b_fb, _server_b_fb, _headers_b_fb) = spawn_stub_anthropic_server().await;
    let fallback_b: Arc<dyn Extractor> = Arc::new(AnthropicExtractor::with_url(
        "model-b-fallback".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr_b_fb}/v1/messages"),
        sink(),
    ));
    let fallback_b: Arc<dyn Extractor> = Arc::new(RecordingExtractor::new(
        fallback_b,
        "anthropic",
        "model-b-fallback",
        Arc::clone(&writer),
    ));

    let router_b = LlmRouter::new(
        primary_b,
        "model-b-primary".to_string(),
        Some(fallback_b),
        "model-b-fallback".to_string(),
        sink(),
    );
    let result_b = router_b
        .extract(opts("Router B episode text."))
        .await
        .unwrap()
        .result;

    // Exactly two records: RecordingExtractor only appends after `inner.extract()` succeeds,
    // so the failed primary_b attempt never produced a cassette entry.
    let content = std::fs::read_to_string(&cassette_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly 2 cassette records (router A's primary, router B's fallback), got: {content}"
    );
    let models: Vec<String> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["model"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(models.contains(&"model-a".to_string()));
    assert!(models.contains(&"model-b-fallback".to_string()));
    assert!(
        !models.contains(&"model-b-primary".to_string()),
        "the failed primary attempt must not produce a cassette record: {models:?}"
    );

    // A single flat ReplayingExtractor — no router, no primary/fallback distinction —
    // correctly serves both originally-recorded calls (User Story 4 acceptance scenario 2).
    let replayer = ReplayingExtractor::load(&cassette_path).unwrap();
    let replayed_a = replayer
        .extract(opts("Router A episode text."))
        .await
        .unwrap()
        .result;
    let replayed_b = replayer
        .extract(opts("Router B episode text."))
        .await
        .unwrap()
        .result;

    assert_eq!(replayed_a.entities.len(), result_a.entities.len());
    assert_eq!(replayed_b.entities.len(), result_b.entities.len());
    assert_eq!(replayed_a.edges[0].fact, result_a.edges[0].fact);
    assert_eq!(replayed_b.edges[0].fact, result_b.edges[0].fact);
}

// ── (d) User Story 3 / SC-003: no credential material reaches the cassette ──

#[tokio::test]
async fn recorded_cassette_contains_no_credential_material() {
    const FAKE_API_KEY: &str = "sk-ant-super-secret-do-not-leak-4f8c9a1b";
    let (addr, _server, captured_headers) = spawn_stub_anthropic_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        FAKE_API_KEY.to_string(),
        format!("http://{addr}/v1/messages"),
        sink(),
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", Arc::clone(&writer));
    recorder
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap();

    // Sanity: prove the credential really was sent over the wire, so the "never leaks"
    // assertion below is not vacuously true because the key never appeared anywhere.
    {
        let headers = captured_headers.lock().unwrap();
        assert!(
            headers
                .iter()
                .any(|h| h.to_lowercase().starts_with("x-api-key") && h.contains(FAKE_API_KEY)),
            "stub should have received the x-api-key header carrying the fake key: {headers:?}"
        );
    }

    let raw = std::fs::read(&cassette_path).unwrap();
    let raw_str = String::from_utf8_lossy(&raw);
    assert!(
        !raw_str.contains(FAKE_API_KEY),
        "cassette must never contain the API key value"
    );
    assert!(
        !raw_str.to_lowercase().contains("x-api-key"),
        "cassette must never contain the auth header name"
    );
    assert!(
        !raw_str.to_lowercase().contains("authorization"),
        "cassette must never contain an Authorization header"
    );
}

// ── (e) #306 FR-001/FR-002/FR-007: failure-record sidecar ──────────────────

/// Builds the sidecar-writing sink and its base path, mirroring how `main.rs` wires
/// `ExtractionFailureSink` alongside a `RecordingExtractor`/`CassetteWriter` pair in
/// production.
fn failure_sink_and_path(
    cassette_path: &std::path::Path,
) -> (Arc<dyn TelemetrySink>, std::path::PathBuf) {
    let writer =
        ExtractionFailureWriter::open(cassette_path, lcg_core::DEFAULT_MAX_BYTES_PER_FILE).unwrap();
    let sidecar_path = writer.base_path().to_path_buf();
    (Arc::new(ExtractionFailureSink::new(writer)), sidecar_path)
}

fn read_sidecar_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn http_error_produces_one_complete_sidecar_record() {
    let (addr, _server) = spawn_stub_server_with_fixed_response(
        "HTTP/1.1 503 Service Unavailable",
        "upstream overloaded",
    )
    .await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let (failure_sink, sidecar_path) = failure_sink_and_path(&cassette_path);
    let cassette_writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        failure_sink,
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", cassette_writer);

    let err = recorder
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "chunk-http",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Ipc(_)), "got {err:?}");

    let records = read_sidecar_records(&sidecar_path);
    assert_eq!(
        records.len(),
        1,
        "exactly one failure record must be written"
    );
    let r = &records[0];
    assert_eq!(r["call_type"], "entities");
    assert_eq!(r["chunk_key"], "chunk-http");
    assert_eq!(r["classification"], "http_error");
    assert_eq!(
        r["raw_body"], "upstream overloaded",
        "the complete raw body must be stored, not dropped"
    );
    assert!(r["finish_reason"].is_null());
    assert!(r["completion_tokens"].is_null());
    // "Alice works at Acme Corp." (26 bytes) lands on the token-budget floor (#307 FR-002),
    // not the old uniform 8192 default.
    assert_eq!(r["max_tokens"], lcg_core::token_budget::MAX_TOKENS_FLOOR);

    // FR-007: the failure goes to the sidecar, not the cassette — RecordingExtractor's
    // success-only invariant is unaffected.
    assert_eq!(
        std::fs::read_to_string(&cassette_path).unwrap(),
        "",
        "a failed extract() call must never produce a cassette record"
    );
}

#[tokio::test]
async fn malformed_response_produces_one_complete_sidecar_record() {
    // A 200 OK response with a well-formed JSON envelope but no extract_entities tool_use
    // block — parse_entity_response returns ParseError, distinct from an HTTP-level failure.
    let malformed_body = serde_json::json!({
        "stop_reason": "end_turn",
        "content": [],
        "usage": {"input_tokens": 10, "output_tokens": 7}
    })
    .to_string();
    let (addr, _server) = spawn_stub_server_with_fixed_response(
        "HTTP/1.1 200 OK",
        Box::leak(malformed_body.into_boxed_str()),
    )
    .await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let (failure_sink, sidecar_path) = failure_sink_and_path(&cassette_path);
    let cassette_writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        failure_sink,
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", cassette_writer);

    let err = recorder
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "chunk-malformed",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Ipc(_) | Error::Json(_)), "got {err:?}");

    let records = read_sidecar_records(&sidecar_path);
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["call_type"], "entities");
    assert_eq!(r["chunk_key"], "chunk-malformed");
    assert_eq!(r["classification"], "malformed");
    assert_eq!(r["finish_reason"], "end_turn");
    assert_eq!(r["completion_tokens"], 7);
    // The complete raw response (the full JSON envelope) must be recoverable from the
    // stored body, not a prefix of it — SC-004's tail-defect rationale for FR-002.
    let stored: serde_json::Value = serde_json::from_str(r["raw_body"].as_str().unwrap()).unwrap();
    assert_eq!(stored["stop_reason"], "end_turn");
}

#[tokio::test]
async fn single_item_all_malformed_batch_salvages_to_empty_success_no_sidecar_record() {
    // A 200 OK response with a well-formed JSON tool_use block, but the one entity in it is
    // missing the genuinely required `name` field. Before #342 this was valid JSON that failed
    // schema validation and hard-failed the whole call (`schema_invalid`) — distinct from
    // `malformed_response_produces_one_complete_sidecar_record`'s missing tool_use block, which
    // has no JSON to validate against at all (#314 FR-003/US2 AS2). Since #342, a malformed item
    // is salvaged (dropped and counted) rather than failing the batch: with nothing else in this
    // one-item batch, the call succeeds with an empty result and `entities_dropped_malformed: 1`
    // — FR-004's "all malformed" case, not an error, so no sidecar record and no schema_invalid
    // classification (a salvaged response is not a failure).
    let salvaged_entity_body = serde_json::json!({
        "stop_reason": "tool_use",
        "content": [{
            "type": "tool_use",
            "id": "toolu_01",
            "name": "extract_entities",
            "input": {
                "entities": [{"entity_type": "Mission", "summary": "no name here"}]
            }
        }],
        "usage": {"input_tokens": 10, "output_tokens": 7}
    })
    .to_string();
    let (addr, _server) = spawn_stub_server_with_fixed_response(
        "HTTP/1.1 200 OK",
        Box::leak(salvaged_entity_body.into_boxed_str()),
    )
    .await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let (failure_sink, sidecar_path) = failure_sink_and_path(&cassette_path);
    let cassette_writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        failure_sink,
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", cassette_writer);

    let outcome = recorder
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "chunk-schema-invalid",
        ))
        .await
        .unwrap();
    assert!(outcome.result.entities.is_empty());
    assert_eq!(outcome.entities_dropped_malformed, 1);

    // No sidecar record — a salvaged response is a success, not a failure (#306's sidecar only
    // ever captures the three hard-failure classes: http_error, truncation, malformed/schema_invalid).
    assert!(
        !sidecar_path.exists() || read_sidecar_records(&sidecar_path).is_empty(),
        "a salvaged (successful) response must not produce a sidecar failure record"
    );

    // Unlike a failed call, RecordingExtractor::extract records on success — a salvaged response
    // now produces exactly one cassette record, where before #342 it produced none.
    let cassette_records = std::fs::read_to_string(&cassette_path).unwrap();
    assert_eq!(
        cassette_records.lines().count(),
        1,
        "a successful (salvaged) extract() call must produce one cassette record"
    );
}

#[tokio::test]
async fn a_2xx_response_with_invalid_json_is_classified_malformed_not_http_error() {
    // A response was received and the HTTP layer itself succeeded — the failure is in the
    // body's syntax, not the transport. Must not be classified "http_error" (which would also
    // produce a misleading "HTTP 200" error message) — review finding from Copilot/CodeRabbit
    // on PR #308.
    let (addr, _server) =
        spawn_stub_server_with_fixed_response("HTTP/1.1 200 OK", "not valid json {").await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let (failure_sink, sidecar_path) = failure_sink_and_path(&cassette_path);
    let cassette_writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        failure_sink,
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", cassette_writer);

    let err = recorder
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "chunk-invalid-json",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Ipc(_)), "got {err:?}");
    assert!(
        !format!("{err}").contains("HTTP 200"),
        "error must not claim a successful status failed: {err}"
    );

    let records = read_sidecar_records(&sidecar_path);
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["call_type"], "entities");
    assert_eq!(r["chunk_key"], "chunk-invalid-json");
    assert_eq!(r["classification"], "malformed");
    assert_eq!(
        r["raw_body"], "not valid json {",
        "the complete raw body must be stored, not dropped"
    );
}

#[tokio::test]
async fn budget_exhaustion_after_retry_produces_one_complete_sidecar_record() {
    // Always responds with stop_reason: max_tokens, regardless of the (doubled) max_tokens
    // sent — forces exhaustion after exactly one retry, matching do_extract_entities' own
    // give-up-after-one-retry policy.
    let truncated_body = serde_json::json!({
        "stop_reason": "max_tokens",
        "content": [],
        "usage": {"input_tokens": 10, "output_tokens": 8192}
    })
    .to_string();
    let (addr, _server) = spawn_stub_server_with_fixed_response(
        "HTTP/1.1 200 OK",
        Box::leak(truncated_body.into_boxed_str()),
    )
    .await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let (failure_sink, sidecar_path) = failure_sink_and_path(&cassette_path);
    let cassette_writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());

    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        failure_sink,
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", cassette_writer);

    let err = recorder
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "chunk-truncated",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Ipc(_)), "got {err:?}");

    let records = read_sidecar_records(&sidecar_path);
    assert_eq!(
        records.len(),
        1,
        "exactly one record — the retry itself must not double-emit"
    );
    let r = &records[0];
    assert_eq!(r["call_type"], "entities");
    assert_eq!(r["chunk_key"], "chunk-truncated");
    assert_eq!(r["classification"], "truncation");
    assert_eq!(r["finish_reason"], "max_tokens");
    assert_eq!(r["completion_tokens"], 8192);
    // The doubled max_tokens (2x the floor, since "Alice works at Acme Corp." starts at the
    // #307 FR-002 floor), not the initial value — the max_tokens "in force" at the moment of
    // the failing call (FR-001).
    assert_eq!(
        r["max_tokens"],
        lcg_core::token_budget::MAX_TOKENS_FLOOR * 2
    );
}

#[tokio::test]
async fn replay_mode_never_creates_a_failure_sidecar() {
    // Edge Case: a cassette in replay mode, where no live failure can occur — the sidecar
    // must simply not be created. Record one successful call first (so a cassette exists to
    // replay from), then replay it purely through ReplayingExtractor — which never
    // constructs a RecordingExtractor, a CassetteWriter, or an ExtractionFailureWriter.
    let (addr, _server, _headers) = spawn_stub_anthropic_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());
    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        sink(),
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", Arc::clone(&writer));
    recorder
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap();

    let replayer = ReplayingExtractor::load(&cassette_path).unwrap();
    replayer
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap();

    let sidecar_path = dir.path().join("cassette.jsonl.failures.jsonl");
    assert!(
        !sidecar_path.exists(),
        "replay mode must never create a failures sidecar, since no ExtractionFailureWriter \
         is ever constructed for it"
    );
}

#[tokio::test]
async fn chunk_key_does_not_affect_the_cassette_request_hash() {
    // #306 Plan Key Decision: chunk_key is observational metadata, excluded from
    // cassette.rs's request_key hash — two calls differing only in chunk_key must still be
    // treated as the exact same request for replay-matching purposes.
    let (addr, _server, _headers) = spawn_stub_anthropic_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let writer = Arc::new(CassetteWriter::open(&cassette_path).unwrap());
    let inner = Arc::new(AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        sink(),
    ));
    let recorder = RecordingExtractor::new(inner, "anthropic", "test-model", Arc::clone(&writer));
    recorder
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "recorded-under-this-key",
        ))
        .await
        .unwrap();

    // Replaying with a *different* chunk_key over the same episode_body must still hit —
    // proving chunk_key plays no role in the match.
    let replayer = ReplayingExtractor::load(&cassette_path).unwrap();
    let replayed = replayer
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "a-completely-different-key",
        ))
        .await
        .unwrap()
        .result;
    assert_eq!(replayed.entities.len(), 2);
}

// ── (f) #307 FR-004/FR-007: edge-path budget exhaustion returns Err ────────

#[tokio::test]
async fn edge_budget_exhaustion_after_retry_returns_err_with_entities_extracted_count() {
    // User Story 1's Independent Test: entity extraction succeeds (2 entities), then edge
    // extraction exhausts its token budget after the doubling retry. Before #307 FR-004 this
    // returned `Ok(vec![])` — a success indistinguishable from a model that genuinely found no
    // edges. It must now return `Err`, and the already-extracted entity count must survive in
    // the ExtractionFailureRecord sidecar via FR-007's `entities_extracted` field.
    let (addr, _server) = spawn_stub_server_entities_succeed_edges_exhaust().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cassette_path = dir.path().join("cassette.jsonl");
    let (failure_sink, sidecar_path) = failure_sink_and_path(&cassette_path);

    let extractor = AnthropicExtractor::with_url(
        "test-model".to_string(),
        "sk-test-key".to_string(),
        format!("http://{addr}/v1/messages"),
        failure_sink,
    );

    let err = extractor
        .extract(opts_with_chunk_key(
            "Alice works at Acme Corp.",
            "chunk-edge-truncated",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Ipc(_)), "got {err:?}");

    let records = read_sidecar_records(&sidecar_path);
    assert_eq!(
        records.len(),
        1,
        "exactly one failure record — for the edge call; the entity call succeeded and must \
         not itself produce a sidecar record"
    );
    let r = &records[0];
    assert_eq!(r["call_type"], "edges");
    assert_eq!(r["chunk_key"], "chunk-edge-truncated");
    assert_eq!(r["classification"], "truncation");
    assert_eq!(
        r["entities_extracted"], 2,
        "the 2 entities extracted before the edge call failed must be recoverable for \
         forensics even though Err discards them from extract()'s return value"
    );
}
