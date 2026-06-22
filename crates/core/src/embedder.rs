use futures::future::BoxFuture;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{env::lcg_env_var, error::Error};

// ── OpenAI-compatible wire types ──────────────────────────────────────────────

#[derive(Serialize)]
struct OaiEmbedRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct OaiEmbedResponse {
    data: Vec<OaiEmbedding>,
    model: String,
}

#[derive(Deserialize)]
struct OaiEmbedding {
    // Deserialize as f64 (the Swift sidecar returns [Double]) then convert to f32 explicitly.
    embedding: Vec<f64>,
}

// ── Transport ─────────────────────────────────────────────────────────────────

enum EmbedTransport {
    Http {
        client: Client,
        url: String,
    },
    #[cfg(unix)]
    Uds {
        path: String,
    },
}

// ── Embedder trait ────────────────────────────────────────────────────────────

pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>>;

    /// Embedding dimension. Used when pre-populating DB rows in tests/benches.
    fn dim(&self) -> usize {
        768
    }
}

// ── OaiEmbedder ───────────────────────────────────────────────────────────────

/// Out-of-process embedding adapter (Principle V).
///
/// Calls an OpenAI-compatible `POST /v1/embeddings` endpoint — no ML runtime in this crate.
/// Supports two transports: HTTP (reqwest) and Unix domain socket (hyper 1.x).
pub struct OaiEmbedder {
    transport: EmbedTransport,
    model: String,
    pub dim: usize,
}

impl OaiEmbedder {
    /// Constructs an HTTP-transport embedder pointing at the given URL.
    pub fn new_http(url: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            transport: EmbedTransport::Http {
                client: Client::new(),
                url: url.into(),
            },
            model: model.into(),
            dim,
        }
    }

    /// Constructs a UDS-transport embedder pointing at the given socket path.
    #[cfg(unix)]
    pub fn new_uds(path: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            transport: EmbedTransport::Uds { path: path.into() },
            model: model.into(),
            dim,
        }
    }

    /// Constructs from environment variables — HTTP transport, same env vars as before.
    ///
    /// - `LCG_EMBEDDING_URL` (default `http://127.0.0.1:8765/v1/embeddings`)
    /// - `LCG_EMBEDDING_MODEL` (default `bge-base-en-v1.5`)
    /// - `LCG_EMBEDDING_DIM` (default `768`)
    pub fn from_env() -> Self {
        let url = lcg_env_var("LCG_EMBEDDING_URL", "GRAPHITI_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8765/v1/embeddings".to_string());
        let model = lcg_env_var("LCG_EMBEDDING_MODEL", "GRAPHITI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());
        let dim = lcg_env_var("LCG_EMBEDDING_DIM", "GRAPHITI_EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(768usize);
        Self::new_http(url, model, dim)
    }

    /// Returns `("uds"|"http", endpoint_string)` for the startup log line.
    pub fn transport_info(&self) -> (&'static str, String) {
        match &self.transport {
            EmbedTransport::Http { url, .. } => ("http", url.clone()),
            #[cfg(unix)]
            EmbedTransport::Uds { path } => ("uds", path.clone()),
        }
    }

    /// Sends a probe request and returns `(dim, model_name)` from the response.
    ///
    /// Used at startup to auto-detect embedding dimension and confirm the embedder is reachable.
    pub async fn probe(&self) -> Result<(usize, String), Error> {
        let resp = self.do_embed_raw("probe").await?;
        let model = resp.model.clone();
        let vec = extract_embedding(resp)?;
        Ok((vec.len(), model))
    }

    async fn do_embed(&self, text: &str) -> Result<Vec<f32>, Error> {
        let resp = self.do_embed_raw(text).await?;
        let vec = extract_embedding(resp)?;
        if vec.len() != self.dim {
            return Err(Error::Ipc(format!(
                "embedding response shape mismatch: expected {} dimensions, got {}",
                self.dim,
                vec.len()
            )));
        }
        Ok(vec)
    }

    async fn do_embed_raw(&self, text: &str) -> Result<OaiEmbedResponse, Error> {
        match &self.transport {
            EmbedTransport::Http { client, url } => self.do_embed_http_raw(client, url, text).await,
            #[cfg(unix)]
            EmbedTransport::Uds { path } => self.do_embed_uds_raw(path, text).await,
        }
    }

    async fn do_embed_http_raw(
        &self,
        client: &Client,
        url: &str,
        text: &str,
    ) -> Result<OaiEmbedResponse, Error> {
        let body = OaiEmbedRequest {
            input: text,
            model: &self.model,
        };
        let resp: OaiEmbedResponse = client
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    #[cfg(unix)]
    async fn do_embed_uds_raw(&self, path: &str, text: &str) -> Result<OaiEmbedResponse, Error> {
        use http_body_util::{BodyExt, Full};
        use hyper::body::Bytes;
        use hyper::Request;
        use hyper_util::rt::TokioIo;
        use tokio::net::UnixStream;

        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| Error::Ipc(format!("UDS connect to {path}: {e}")))?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| Error::Ipc(format!("UDS HTTP/1.1 handshake: {e}")))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let body_bytes = serde_json::to_vec(&OaiEmbedRequest {
            input: text,
            model: &self.model,
        })
        .map_err(|e| Error::Ipc(format!("serialize embed request: {e}")))?;

        let req = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .header("host", "localhost")
            .body(Full::new(Bytes::from(body_bytes)))
            .map_err(|e| Error::Ipc(format!("build UDS request: {e}")))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| Error::Ipc(format!("UDS send request: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::Ipc(format!(
                "UDS embedder returned status {}",
                resp.status()
            )));
        }

        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::Ipc(format!("UDS read response body: {e}")))?
            .to_bytes();

        serde_json::from_slice(&bytes)
            .map_err(|e| Error::Ipc(format!("parse UDS embed response: {e}")))
    }
}

/// Returns `true` if the error is a transport/connectivity failure (not reachable).
///
/// Used in `main.rs` to distinguish "embedder unreachable" (always fatal at startup,
/// per FR-011) from "embedder reachable but bad response" (can be bypassed by
/// `LCG_EMBEDDING_DIM` per FR-008).
pub fn is_transport_error(e: &Error) -> bool {
    match e {
        Error::Http(re) => re.is_connect() || re.is_timeout(),
        Error::Ipc(msg) => {
            msg.starts_with("UDS connect")
                || msg.starts_with("UDS HTTP/1.1 handshake")
                || msg.starts_with("UDS send request")
        }
        _ => false,
    }
}

fn extract_embedding(resp: OaiEmbedResponse) -> Result<Vec<f32>, Error> {
    let embedding = resp
        .data
        .into_iter()
        .next()
        .ok_or_else(|| Error::Ipc("embedding response: empty data array".to_string()))?
        .embedding;
    if embedding.is_empty() {
        return Err(Error::Ipc(
            "embedding response shape mismatch: zero-length vector".to_string(),
        ));
    }
    // Convert f64 → f32 explicitly; sidecar returns [Double], precision loss is acceptable
    // for unit-normalized BGE embeddings (all values in [-1, 1]).
    Ok(embedding.into_iter().map(|v| v as f32).collect())
}

impl Embedder for OaiEmbedder {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        Box::pin(self.do_embed(text))
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

// ── MockEmbedder ──────────────────────────────────────────────────────────────

/// Zero-latency embedder for tests and benches. Returns a fixed zero vector.
pub struct MockEmbedder {
    pub dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Embedder for MockEmbedder {
    fn embed<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        let v = vec![0.0f32; self.dim];
        Box::pin(async move { Ok(v) })
    }

    fn dim(&self) -> usize {
        self.dim
    }
}
