use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::json;

use crate::{error::Error, types::EmbeddingResult};

// ── Embedder trait ────────────────────────────────────────────────────────────

pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>>;

    /// Embedding dimension. Used when pre-populating DB rows in tests/benches.
    fn dim(&self) -> usize {
        768
    }
}

// ── HttpEmbedder ──────────────────────────────────────────────────────────────

/// Out-of-process embedding adapter (Principle V).
///
/// Calls an HTTP embedding service — no ML runtime in this crate.
pub struct HttpEmbedder {
    url: String,
    model: String,
    pub dim: usize,
    client: Client,
}

impl HttpEmbedder {
    /// Constructs from environment variables with sensible defaults.
    ///
    /// - `GRAPHITI_EMBEDDING_URL` (default `http://127.0.0.1:8765`)
    /// - `GRAPHITI_EMBEDDING_MODEL` (default `bge-base-en-v1.5`)
    /// - `GRAPHITI_EMBEDDING_DIM` (default `768`)
    pub fn from_env() -> Self {
        let url = std::env::var("GRAPHITI_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8765".to_string());
        let model = std::env::var("GRAPHITI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());
        let dim = std::env::var("GRAPHITI_EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(768usize);
        Self {
            url,
            model,
            dim,
            client: Client::new(),
        }
    }

    async fn do_embed(&self, text: &str) -> Result<Vec<f32>, Error> {
        let body = json!({ "text": text, "model": &self.model });
        let resp: EmbeddingResult = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.embedding)
    }
}

impl Embedder for HttpEmbedder {
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
