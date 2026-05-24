use futures::future::BoxFuture;
use reqwest::Client;
use serde_json::json;

use crate::{
    env::lcg_env_var,
    error::Error,
    types::{EntityRow, ExtractedEntity},
};

// ── DedupAdapter trait ────────────────────────────────────────────────────────

pub trait DedupAdapter: Send + Sync {
    fn is_duplicate<'a>(
        &'a self,
        candidate: &'a EntityRow,
        incoming: &'a ExtractedEntity,
    ) -> BoxFuture<'a, Result<bool, Error>>;
}

// ── PassthroughDedupAdapter ───────────────────────────────────────────────────

/// Always returns `Ok(true)` — preserves cosine-only dedup behavior when LCG_DEDUP_LLM unset.
pub struct PassthroughDedupAdapter;

impl DedupAdapter for PassthroughDedupAdapter {
    fn is_duplicate<'a>(
        &'a self,
        _candidate: &'a EntityRow,
        _incoming: &'a ExtractedEntity,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}

// ── LocalDedupAdapter ─────────────────────────────────────────────────────────

/// Calls an out-of-process local model via local HTTP for dedup verification (Principle V).
///
/// Configured via `LCG_DEDUP_ADAPTER_URL` (default: `http://127.0.0.1:8767`).
pub struct LocalDedupAdapter {
    url: String,
    client: Client,
}

impl LocalDedupAdapter {
    pub fn from_env() -> Self {
        // deprecated: remove in Phase B (see #59)
        let url = lcg_env_var("LCG_DEDUP_ADAPTER_URL", "GRAPHITI_DEDUP_ADAPTER_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8767".to_string());
        Self {
            url,
            client: Client::new(),
        }
    }

    async fn call(&self, candidate: &EntityRow, incoming: &ExtractedEntity) -> Result<bool, Error> {
        let body = json!({
            "candidate": {
                "uuid": candidate.uuid,
                "name": candidate.name,
                "summary": candidate.summary,
            },
            "incoming": {
                "name": incoming.name,
                "entity_type": incoming.entity_type,
                "summary": incoming.summary,
            }
        });

        let resp: serde_json::Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp["is_duplicate"].as_bool().unwrap_or(false))
    }
}

impl DedupAdapter for LocalDedupAdapter {
    fn is_duplicate<'a>(
        &'a self,
        candidate: &'a EntityRow,
        incoming: &'a ExtractedEntity,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(self.call(candidate, incoming))
    }
}
