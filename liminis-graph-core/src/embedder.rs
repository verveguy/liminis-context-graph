use crate::{error::Error, types::EmbeddingResult};
use reqwest::Client;
use serde_json::json;

/// Out-of-process embedding adapter (AD-5).
///
/// Calls an HTTP embedding service — no ML runtime in this crate (Principle V).
pub struct Embedder {
    url: String,
    model: String,
    pub dim: usize,
    client: Client,
}

impl Embedder {
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

    /// Embeds `text` and returns the embedding vector.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, Error> {
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
