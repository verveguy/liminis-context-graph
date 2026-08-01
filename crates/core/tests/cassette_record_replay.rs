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
use lcg_core::extractor::{AnthropicExtractor, ExtractOptions, Extractor};
use lcg_core::llm_router::LlmRouter;
use lcg_core::telemetry::{NoopSink, TelemetrySink};
use lcg_core::types::SourceType;
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
        .unwrap();
    assert_eq!(recorded.entities.len(), 2);
    assert_eq!(recorded.edges.len(), 1);

    // Replay against the same cassette. No stub server is queried — ReplayingExtractor never
    // dials out, so SC-001's "zero outbound LLM requests" holds by construction.
    let replayer = ReplayingExtractor::load(&cassette_path).unwrap();
    let replayed = replayer
        .extract(opts("Alice works at Acme Corp."))
        .await
        .unwrap();

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
        .unwrap();

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
        .unwrap();

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
        .unwrap();
    let replayed_b = replayer
        .extract(opts("Router B episode text."))
        .await
        .unwrap();

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
