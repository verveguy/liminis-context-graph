use std::sync::Arc;

use tokio::sync::RwLock;

use crate::{
    db::Db,
    dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter},
    embedder::{Embedder, HttpEmbedder},
    extractor::Extractor,
    llm_router::LlmRouter,
    telemetry::TelemetrySink,
};

pub struct AppState {
    pub db: Arc<Db>,
    pub embedder: Arc<dyn Embedder>,
    pub extractor: Arc<dyn Extractor>,
    pub dedup: Arc<dyn DedupAdapter>,
    pub write_lock: Arc<RwLock<()>>,
    pub sink: Arc<dyn TelemetrySink>,
}

impl AppState {
    /// Builds `AppState` from environment variables.
    ///
    /// - `GRAPHITI_DEDUP_LLM`: if set, uses `LocalDedupAdapter`; otherwise `PassthroughDedupAdapter`.
    /// - `GRAPHITI_EXTRACTION_LLM`: parsed by `LlmRouter::from_env`.
    pub fn from_env(sink: Arc<dyn TelemetrySink>, db: Arc<Db>) -> Self {
        let embedder: Arc<dyn Embedder> = Arc::new(HttpEmbedder::from_env());
        let extractor: Arc<dyn Extractor> = Arc::new(LlmRouter::from_env(Arc::clone(&sink)));
        let dedup: Arc<dyn DedupAdapter> = if std::env::var("GRAPHITI_DEDUP_LLM").is_ok() {
            Arc::new(LocalDedupAdapter::from_env())
        } else {
            Arc::new(PassthroughDedupAdapter)
        };
        Self {
            db,
            embedder,
            extractor,
            dedup,
            write_lock: Arc::new(RwLock::new(())),
            sink,
        }
    }
}
