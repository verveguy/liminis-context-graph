use std::path::PathBuf;
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
    pub db_path: String,
    pub wal_dir: Option<PathBuf>,
    pub embedding_model: String,
}

impl AppState {
    /// Builds `AppState` from environment variables.
    ///
    /// - `GRAPHITI_DEDUP_LLM`: if set, uses `LocalDedupAdapter`; otherwise `PassthroughDedupAdapter`.
    /// - `GRAPHITI_EXTRACTION_LLM`: parsed by `LlmRouter::from_env`.
    /// - `GRAPHITI_WAL_DIR`: optional WAL directory path.
    /// - `GRAPHITI_EMBEDDING_MODEL`: embedding model name (default `bge-base-en-v1.5`).
    pub fn from_env(sink: Arc<dyn TelemetrySink>, db: Arc<Db>, db_path: String) -> Self {
        let embedder: Arc<dyn Embedder> = Arc::new(HttpEmbedder::from_env());
        let extractor: Arc<dyn Extractor> = Arc::new(LlmRouter::from_env(Arc::clone(&sink)));
        let dedup: Arc<dyn DedupAdapter> = if std::env::var("GRAPHITI_DEDUP_LLM").is_ok() {
            Arc::new(LocalDedupAdapter::from_env())
        } else {
            Arc::new(PassthroughDedupAdapter)
        };
        let wal_dir = std::env::var("GRAPHITI_WAL_DIR").ok().map(PathBuf::from);
        let embedding_model = std::env::var("GRAPHITI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());
        Self {
            db,
            embedder,
            extractor,
            dedup,
            write_lock: Arc::new(RwLock::new(())),
            sink,
            db_path,
            wal_dir,
            embedding_model,
        }
    }
}
