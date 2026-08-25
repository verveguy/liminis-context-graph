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
        api_key: Option<String>,
    },
    #[cfg(unix)]
    Uds { path: String, pool: UdsPool },
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
                api_key: None,
            },
            model: model.into(),
            dim,
        }
    }

    /// Attaches a Bearer-token API key to be sent as `Authorization: Bearer <key>` on every
    /// HTTP request. A no-op on the UDS transport (FR-005) — UDS is a local socket and never
    /// accepts a key regardless of this call, since `new_uds` structurally has no key parameter.
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        if let EmbedTransport::Http { api_key, .. } = &mut self.transport {
            *api_key = key;
        }
        self
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
    /// - `LCG_EMBEDDING_API_KEY` / `GRAPHITI_EMBEDDING_API_KEY` / `OPENAI_API_KEY` (optional)
    pub fn from_env() -> Self {
        let url = lcg_env_var("LCG_EMBEDDING_URL", "GRAPHITI_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8765/v1/embeddings".to_string());
        let model = lcg_env_var("LCG_EMBEDDING_MODEL", "GRAPHITI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());
        let dim = lcg_env_var("LCG_EMBEDDING_DIM", "GRAPHITI_EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(768usize);
        let api_key = resolve_embedding_api_key(&url);
        Self::new_http(url, model, dim).with_api_key(api_key)
    }

    /// Returns `("uds"|"http", endpoint_string)` for the startup log line. The HTTP endpoint has
    /// any Basic-auth-style userinfo (`user:pass@host`) redacted (FR-007) — never the key itself,
    /// which is never part of the URL for this transport.
    pub fn transport_info(&self) -> (&'static str, String) {
        match &self.transport {
            EmbedTransport::Http { url, .. } => ("http", redact_url_userinfo(url).0),
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
            EmbedTransport::Http {
                client,
                url,
                api_key,
            } => {
                self.do_embed_http_raw(client, url, api_key.as_deref(), text)
                    .await
            }
            #[cfg(unix)]
            EmbedTransport::Uds { path, pool } => self.do_embed_uds_raw(path, pool, text).await,
        }
    }

    async fn do_embed_http_raw(
        &self,
        client: &Client,
        url: &str,
        api_key: Option<&str>,
        text: &str,
    ) -> Result<OaiEmbedResponse, Error> {
        let body = OaiEmbedRequest {
            input: text,
            model: &self.model,
        };
        let mut req = client.post(url).json(&body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let resp: OaiEmbedResponse = req.send().await?.error_for_status()?.json().await?;
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

/// Returns `true` if the error is an HTTP 401/403 authentication failure.
///
/// Used in `main.rs` to distinguish "credential rejected" (always fatal at startup, FR-008,
/// never bypassable by `LCG_EMBEDDING_DIM`) from other non-transport probe failures (unexpected
/// response shape, which *can* be bypassed by `LCG_EMBEDDING_DIM`).
pub fn is_auth_error(e: &Error) -> bool {
    match e {
        Error::Http(re) => matches!(
            re.status(),
            Some(reqwest::StatusCode::UNAUTHORIZED) | Some(reqwest::StatusCode::FORBIDDEN)
        ),
        _ => false,
    }
}

/// Resolves the embedder API key via a three-tier lookup, in order:
/// `LCG_EMBEDDING_API_KEY` → `GRAPHITI_EMBEDDING_API_KEY` (deprecated alias, warns on use) →
/// `OPENAI_API_KEY` (convenience fallback, no *deprecation* warning — it isn't a legacy spelling
/// of an LCG-specific variable — but does log a one-line notice, since this tier sends whatever
/// key is exported for OpenAI tooling generally to *whatever* `--embedder-http`/
/// `LCG_EMBEDDING_URL` endpoint is configured, not only to OpenAI's own API; a silent send of a
/// possibly-unrelated credential to an operator-chosen endpoint is worth surfacing even though
/// it isn't wrong per FR-002/Acceptance Scenario 2). An empty string at any tier is treated as
/// absent for that tier (FR-002).
///
/// `target_url` is used only for this notice's wording (naming the endpoint the key will be
/// sent to); it does not gate whether the tier applies — that would contradict FR-002, which
/// requires the fallback to work against any configured HTTP endpoint, not only OpenAI's.
///
/// Deliberately bespoke rather than composed from `lcg_env_var`: that helper is 2-tier, treats
/// `""` as a valid value, and has no way to customize its warning text for a third tier.
pub fn resolve_embedding_api_key(target_url: &str) -> Option<String> {
    let lcg = std::env::var("LCG_EMBEDDING_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(key) = lcg {
        return Some(key);
    }

    let graphiti = std::env::var("GRAPHITI_EMBEDDING_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(key) = graphiti {
        eprintln!(
            "[liminis-context-graph] DEPRECATED: env var GRAPHITI_EMBEDDING_API_KEY is \
             deprecated; rename to LCG_EMBEDDING_API_KEY. Support will be removed in Phase B \
             (see issue #59)."
        );
        return Some(key);
    }

    let openai = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if openai.is_some() {
        let (redacted_url, _) = redact_url_userinfo(target_url);
        eprintln!(
            "[liminis-context-graph] Using OPENAI_API_KEY as the embedder credential; it will \
             be sent as a Bearer token to {redacted_url}, which may not be OpenAI's own API. \
             Set LCG_EMBEDDING_API_KEY instead if this key should not go to that endpoint."
        );
    }
    openai
}

/// Redacts Basic-auth-style userinfo (`user:pass@host`) from a URL before it is echoed in a log
/// line or error message (FR-007). Returns `(redacted_url, Some(original_userinfo_substring))`
/// when userinfo was present and removed, or `(url unchanged, None)` otherwise — the second
/// element lets a caller additionally scrub the same substring out of other text (e.g. a wrapped
/// `reqwest::Error`'s `Display` output) that isn't itself parsed as a URL.
///
/// Scope is deliberately narrow: only the standard `user:pass@host` userinfo component. Ad hoc
/// "API key in query string" conventions are out of scope (see spec Assumptions).
pub fn redact_url_userinfo(url: &str) -> (String, Option<String>) {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return (url.to_string(), None);
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return (url.to_string(), None);
    }

    // Capture the original "user:pass@" (or "user@") substring so callers can scrub it out of
    // unrelated text too, e.g. an error message that independently embeds the raw URL.
    let userinfo = match parsed.password() {
        Some(pass) => format!("{}:{}@", parsed.username(), pass),
        None => format!("{}@", parsed.username()),
    };

    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    (parsed.to_string(), Some(userinfo))
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

// ── UnconfiguredEmbedder ────────────────────────────────────────────────────

/// `AppState.degraded_reason` value set when standalone `--mcp-stdio` mode exhausts its bounded
/// retry against an unreachable embedder at startup (issue #499, FR-003). Reuses the existing
/// "DB never opened" degraded branch `knowledge_status` already exposes for other startup
/// failures (e.g. `lbug_wal_corrupt`) — this is a new reason string on that same shape, not a
/// new response field.
pub const EMBEDDER_UNREACHABLE_DEGRADED_REASON: &str = "embedder_unreachable_at_startup";

/// Stands in for "the embedder was unreachable at startup and the process degraded rather than
/// exiting" (#499). Mirrors `UnconfiguredExtractor` (#331): every `embed()` call fails
/// immediately rather than the process refusing to start. `AppState.embedder` is `Arc<dyn
/// Embedder>`, not optional, so this placeholder satisfies that field without changing its type
/// or touching any `.embed()` call site — none of them are reachable anyway while the DB is
/// never opened, since `handle`'s degraded-mode guard rejects every method except a small exempt
/// list (`crates/core/src/handlers.rs`).
///
/// `dim()` uses the trait's default (768) and is never actually read in this state: the only
/// exempt-list handler that calls `state.embedder.dim()` (`knowledge_recover`) is itself
/// rejected outright for this specific degraded reason before it gets there (see
/// `handlers.rs`'s `handle_knowledge_recover`).
pub struct UnconfiguredEmbedder;

impl Embedder for UnconfiguredEmbedder {
    fn embed<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>> {
        Box::pin(async {
            Err(Error::Ipc(
                "embedder unreachable at startup; the process is running in degraded mode \
                 (see knowledge_status) — restart once the embedder is reachable"
                    .to_string(),
            ))
        })
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

    #[test]
    fn is_auth_error_rejects_ipc_errors() {
        // is_auth_error only recognizes Error::Http with a 401/403 status; it must never
        // misclassify a UDS-side Error::Ipc as an auth failure.
        let e = Error::Ipc("UDS embedder returned status 401".to_string());
        assert!(!is_auth_error(&e));
    }

    #[test]
    fn redact_url_userinfo_leaves_plain_url_unchanged() {
        let (redacted, scrub) = redact_url_userinfo("https://api.openai.com/v1/embeddings");
        assert_eq!(redacted, "https://api.openai.com/v1/embeddings");
        assert_eq!(scrub, None);
    }

    #[test]
    fn redact_url_userinfo_strips_user_and_pass() {
        let (redacted, scrub) =
            redact_url_userinfo("https://alice:s3cret@example.com/v1/embeddings");
        assert!(
            !redacted.contains("s3cret"),
            "redacted URL leaked password: {redacted}"
        );
        assert!(
            !redacted.contains("alice"),
            "redacted URL leaked username: {redacted}"
        );
        assert_eq!(scrub, Some("alice:s3cret@".to_string()));
    }

    #[test]
    fn redact_url_userinfo_passes_through_unparseable_input() {
        let (redacted, scrub) = redact_url_userinfo("not a url");
        assert_eq!(redacted, "not a url");
        assert_eq!(scrub, None);
    }

    // ENV_LOCK-equivalent: these three tests each use a distinct env var namespace and run under
    // the crate's default single-threaded-per-module test isolation is NOT guaranteed by cargo,
    // so each test saves/restores the exact three vars it touches to avoid cross-test races.
    fn with_key_env<F: FnOnce()>(
        lcg: Option<&str>,
        graphiti: Option<&str>,
        openai: Option<&str>,
        f: F,
    ) {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        for (name, val) in [
            ("LCG_EMBEDDING_API_KEY", lcg),
            ("GRAPHITI_EMBEDDING_API_KEY", graphiti),
            ("OPENAI_API_KEY", openai),
        ] {
            match val {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        f();
        for name in [
            "LCG_EMBEDDING_API_KEY",
            "GRAPHITI_EMBEDDING_API_KEY",
            "OPENAI_API_KEY",
        ] {
            std::env::remove_var(name);
        }
    }

    const TEST_URL: &str = "http://127.0.0.1:9999/v1/embeddings";

    #[test]
    fn resolve_embedding_api_key_none_set_returns_none() {
        with_key_env(None, None, None, || {
            assert_eq!(resolve_embedding_api_key(TEST_URL), None);
        });
    }

    #[test]
    fn resolve_embedding_api_key_lcg_wins_over_all() {
        with_key_env(
            Some("lcg-key"),
            Some("graphiti-key"),
            Some("openai-key"),
            || {
                assert_eq!(
                    resolve_embedding_api_key(TEST_URL),
                    Some("lcg-key".to_string())
                );
            },
        );
    }

    #[test]
    fn resolve_embedding_api_key_falls_back_to_graphiti() {
        with_key_env(None, Some("graphiti-key"), Some("openai-key"), || {
            assert_eq!(
                resolve_embedding_api_key(TEST_URL),
                Some("graphiti-key".to_string())
            );
        });
    }

    #[test]
    fn resolve_embedding_api_key_falls_back_to_openai() {
        with_key_env(None, None, Some("openai-key"), || {
            assert_eq!(
                resolve_embedding_api_key(TEST_URL),
                Some("openai-key".to_string())
            );
        });
    }

    #[test]
    fn resolve_embedding_api_key_falls_back_to_openai_against_non_openai_host() {
        // FR-002/Acceptance Scenario 2 requires the OPENAI_API_KEY fallback tier to work against
        // *any* configured HTTP embedder endpoint, not only OpenAI's own API — target_url only
        // affects the informational notice's wording, never whether the tier applies.
        with_key_env(None, None, Some("openai-key"), || {
            assert_eq!(
                resolve_embedding_api_key("http://127.0.0.1:8080/v1/embeddings"),
                Some("openai-key".to_string())
            );
        });
    }

    #[test]
    fn resolve_embedding_api_key_empty_string_treated_as_absent_at_every_tier() {
        with_key_env(Some(""), Some(""), Some("openai-key"), || {
            assert_eq!(
                resolve_embedding_api_key(TEST_URL),
                Some("openai-key".to_string())
            );
        });
        with_key_env(Some(""), Some(""), Some(""), || {
            assert_eq!(resolve_embedding_api_key(TEST_URL), None);
        });
    }

    #[tokio::test]
    async fn unconfigured_embedder_embed_always_errors() {
        let e = UnconfiguredEmbedder.embed("hello").await;
        assert!(e.is_err());
    }

    #[test]
    fn unconfigured_embedder_dim_uses_trait_default() {
        assert_eq!(UnconfiguredEmbedder.dim(), 768);
    }
}
