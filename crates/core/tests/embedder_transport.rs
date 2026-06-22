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
