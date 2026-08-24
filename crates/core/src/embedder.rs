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
        pool: UdsPool,
    },
}

// ── UDS connection pool ───────────────────────────────────────────────────────
//
// Each embed call over UDS reuses a held HTTP/1.1 connection from a small,
// lazily-populated, bounded pool instead of dialing a fresh UnixStream, doing
// a full handshake, and spawning a detached driver task per call (see #229).

#[cfg(unix)]
type UdsSender = hyper::client::conn::http1::SendRequest<http_body_util::Full<hyper::body::Bytes>>;

/// Number of held UDS connections. HTTP/1.1 without pipelining serializes one
/// in-flight request per connection, so a small fixed pool (rather than a
/// single connection) keeps concurrent embed calls from bottlenecking behind
/// each other. See ADR documenting this pooling design for the rationale.
#[cfg(unix)]
const UDS_POOL_SIZE: usize = 4;

#[cfg(unix)]
struct UdsPool {
    slots: Vec<tokio::sync::Mutex<Option<UdsSender>>>,
    cursor: std::sync::atomic::AtomicUsize,
}

#[cfg(unix)]
impl UdsPool {
    /// Constructs a pool with all slots empty. No dialing happens here —
    /// connections are established lazily on first use of each slot, so
    /// short-lived embedders (e.g. the startup probe) pay no extra cost.
    fn new() -> Self {
        let slots = (0..UDS_POOL_SIZE)
            .map(|_| tokio::sync::Mutex::new(None))
            .collect();
        Self {
            slots,
            cursor: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// Dials a fresh UnixStream, performs the HTTP/1.1 handshake, and spawns the
/// connection driver task. Used to populate an empty pool slot, or to
/// re-establish a slot whose held connection has broken.
#[cfg(unix)]
async fn dial_uds(path: &str) -> Result<UdsSender, Error> {
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(path)
        .await
        .map_err(|e| Error::Ipc(format!("UDS connect to {path}: {e}")))?;
    let io = TokioIo::new(stream);
    let (sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| Error::Ipc(format!("UDS HTTP/1.1 handshake: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// Distinguishes a `send_request` failure (the held connection is dead — the
/// sidecar restarted, idle-closed the socket, etc. — worth one re-dial) from
/// an error that occurred after a request was successfully sent over a
/// healthy connection (bad status, unreadable body, unparseable JSON — not
/// worth re-dialing, since dialing again would return the same result).
#[cfg(unix)]
enum UdsAttemptError {
    ConnectionBroken(Error),
    Other(Error),
}

/// Sends one embed request over `sender` and reads/parses the response.
/// `body_bytes` is a cheaply-clonable `Bytes` so callers can retry a send
/// against a freshly-dialed connection without re-copying the serialized
/// request.
#[cfg(unix)]
async fn send_and_read_uds(
    sender: &mut UdsSender,
    body_bytes: hyper::body::Bytes,
) -> Result<OaiEmbedResponse, UdsAttemptError> {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("host", "localhost")
        .body(Full::new(body_bytes))
        .map_err(|e| UdsAttemptError::Other(Error::Ipc(format!("build UDS request: {e}"))))?;

    let resp = sender.send_request(req).await.map_err(|e| {
        UdsAttemptError::ConnectionBroken(Error::Ipc(format!("UDS send request: {e}")))
    })?;

    if !resp.status().is_success() {
        let status = resp.status();
        // Drain the body before returning: with HTTP/1.1 keep-alive, leaving
        // it unread would desync framing for the next request reusing this
        // pooled connection.
        let _ = resp.into_body().collect().await;
        return Err(UdsAttemptError::Other(Error::Ipc(format!(
            "UDS embedder returned status {status}"
        ))));
    }

    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| UdsAttemptError::Other(Error::Ipc(format!("UDS read response body: {e}"))))?
        .to_bytes();

    serde_json::from_slice(&bytes)
        .map_err(|e| UdsAttemptError::Other(Error::Ipc(format!("parse UDS embed response: {e}"))))
}

/// Guards a pool slot across one send/read span. If dropped while still
/// armed — meaning the embed future was cancelled mid-flight rather than
/// running to completion — the slot is cleared so the next use re-dials
/// instead of reusing a connection that may be left in an indeterminate
/// state (partially written request, or an unread stale response).
#[cfg(unix)]
struct PoisonGuard<'a> {
    slot: &'a mut Option<UdsSender>,
    armed: bool,
}

#[cfg(unix)]
impl<'a> PoisonGuard<'a> {
    fn new(slot: &'a mut Option<UdsSender>) -> Self {
        Self { slot, armed: true }
    }

    fn sender_mut(&mut self) -> &mut UdsSender {
        self.slot
            .as_mut()
            .expect("PoisonGuard requires a populated slot")
    }

    /// Marks the send/read span as having completed (successfully or with a
    /// definite, fully-read error) rather than having been dropped mid-flight.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PoisonGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            *self.slot = None;
        }
    }
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
            transport: EmbedTransport::Uds {
                path: path.into(),
                pool: UdsPool::new(),
            },
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
            EmbedTransport::Uds { path, .. } => ("uds", path.clone()),
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
            EmbedTransport::Uds { path, pool } => self.do_embed_uds_raw(path, pool, text).await,
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

    /// Sends one embed request over a pooled UDS connection. Picks a slot by
    /// round-robin, lazily dials it if empty, and on a broken-connection
    /// failure (the sidecar restarted, idle-closed the socket, etc.) clears
    /// the slot and retries exactly once against a freshly-dialed connection.
    #[cfg(unix)]
    async fn do_embed_uds_raw(
        &self,
        path: &str,
        pool: &UdsPool,
        text: &str,
    ) -> Result<OaiEmbedResponse, Error> {
        let idx = pool
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % pool.slots.len();
        let mut slot = pool.slots[idx].lock().await;

        let body_bytes = hyper::body::Bytes::from(
            serde_json::to_vec(&OaiEmbedRequest {
                input: text,
                model: &self.model,
            })
            .map_err(|e| Error::Ipc(format!("serialize embed request: {e}")))?,
        );

        if slot.is_none() {
            *slot = Some(dial_uds(path).await?);
        }

        let first = {
            let mut guard = PoisonGuard::new(&mut slot);
            let result = send_and_read_uds(guard.sender_mut(), body_bytes.clone()).await;
            // The span completed (successfully or with a definite, fully-read
            // error) rather than being dropped mid-flight — safe to disarm
            // regardless of outcome; a ConnectionBroken outcome is handled
            // explicitly below by clearing the slot before redialing.
            guard.disarm();
            result
        };

        match first {
            Ok(resp) => Ok(resp),
            Err(UdsAttemptError::Other(e)) => Err(e),
            Err(UdsAttemptError::ConnectionBroken(_)) => {
                *slot = None;
                *slot = Some(dial_uds(path).await?);
                let mut guard = PoisonGuard::new(&mut slot);
                let result = send_and_read_uds(guard.sender_mut(), body_bytes).await;
                guard.disarm();
                drop(guard);
                match result {
                    Ok(resp) => Ok(resp),
                    Err(UdsAttemptError::Other(e)) => Err(e),
                    Err(UdsAttemptError::ConnectionBroken(e)) => {
                        // The redial-and-retry also failed against a fresh
                        // connection — don't leave the known-bad sender in
                        // the slot for the next unrelated call to trip over.
                        *slot = None;
                        Err(e)
                    }
                }
            }
        }
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

// ── HashEmbedder ─────────────────────────────────────────────────────────────

/// Deterministic, offline test embedder that derives a stable pseudo-random unit-ish vector from
/// each input text's hash. Unlike `MockEmbedder`'s constant zero vector, distinct texts get
/// distinct (though semantically meaningless) vectors — important for a test that recomputes an
/// entire corpus's embeddings through this embedder (e.g. issue #440's WAL-replay recompute) and
/// then runs vector/ANN search over the result: a constant embedding collapses every row to the
/// same point, so "nearest neighbor" search degenerates into an artifact of index tie-breaking
/// (e.g. insertion order) rather than harmless, unbiased noise — systematically favoring whichever
/// entities happen to win that tie-break regardless of query, instead of the query-independent
/// randomness a zero-information embedder is meant to approximate. `HashEmbedder` avoids that
/// failure mode while remaining fully deterministic and offline.
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Embedder for HashEmbedder {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        use std::hash::{Hash, Hasher};
        let dim = self.dim;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let mut seed = hasher.finish().max(1); // xorshift64 requires a nonzero seed
        let mut v = Vec::with_capacity(dim);
        for _ in 0..dim {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            v.push(((seed as f64 / u64::MAX as f64) * 2.0 - 1.0) as f32);
        }
        Box::pin(async move { Ok(v) })
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

// ── NameMapEmbedder ───────────────────────────────────────────────────────────

/// Test embedder that maps specific strings to caller-provided vectors.
/// Unknown strings return a zero vector. Useful for controlling cosine
/// similarity precisely in cross-episode dedup tests.
pub struct NameMapEmbedder {
    dim: usize,
    map: std::collections::HashMap<String, Vec<f32>>,
}

impl NameMapEmbedder {
    pub fn new(dim: usize, map: std::collections::HashMap<String, Vec<f32>>) -> Self {
        Self { dim, map }
    }
}

impl Embedder for NameMapEmbedder {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        let v = self
            .map
            .get(text)
            .cloned()
            .unwrap_or_else(|| vec![0.0f32; self.dim]);
        Box::pin(async move { Ok(v) })
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

// ── CountingEmbedder ────────────────────────────────────────────────────────

/// Test embedder that wraps another embedder and counts `embed()` calls. Used to assert a
/// negative — e.g. that an empty-summary entity never triggers an embedder round-trip at all
/// (issue #470's FR-002 empty-summary edge case), which a returned zero-vector alone can't
/// distinguish from "embedded an empty string and got a zero vector back."
pub struct CountingEmbedder {
    inner: std::sync::Arc<dyn Embedder>,
    calls: std::sync::atomic::AtomicUsize,
}

impl CountingEmbedder {
    pub fn new(inner: std::sync::Arc<dyn Embedder>) -> Self {
        Self {
            inner,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.embed(text)
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `main.rs`'s startup probe pattern-matches these exact string prefixes on
    // `Error::Ipc` to distinguish "embedder unreachable" from other failures
    // (see `is_transport_error`'s doc comment). The pooled UDS retry rewrite
    // must preserve this contract verbatim (FR-006).
    #[test]
    fn is_transport_error_recognizes_uds_connect_prefix() {
        let e = Error::Ipc("UDS connect to /tmp/foo.sock: connection refused".to_string());
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_recognizes_uds_handshake_prefix() {
        let e = Error::Ipc("UDS HTTP/1.1 handshake: unexpected eof".to_string());
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_recognizes_uds_send_request_prefix() {
        let e = Error::Ipc("UDS send request: connection closed".to_string());
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_rejects_other_ipc_errors() {
        let e = Error::Ipc("UDS embedder returned status 500".to_string());
        assert!(!is_transport_error(&e));
    }
}
