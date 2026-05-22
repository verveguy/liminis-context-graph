use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use tokio::sync::RwLock;

use crate::{
    db::Db,
    dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter},
    embedder::{Embedder, HttpEmbedder},
    extractor::Extractor,
    llm_router::LlmRouter,
    rebuild_job::RebuildJob,
    telemetry::TelemetrySink,
    wal::WalWriter,
};

pub struct AppState {
    /// ArcSwap allows `clear_all` to atomically replace the live Db under the write lock
    /// without holding an inner Mutex. All other handlers call `db.load_full()` to get a
    /// snapshot Arc<Db> — a lock-free read. See ADR-0043.
    pub db: ArcSwap<Db>,
    pub embedder: Arc<dyn Embedder>,
    pub extractor: Arc<dyn Extractor>,
    pub dedup: Arc<dyn DedupAdapter>,
    pub write_lock: Arc<RwLock<()>>,
    pub sink: Arc<dyn TelemetrySink>,
    pub db_path: String,
    pub wal_dir: Option<PathBuf>,
    pub embedding_model: String,
    pub wal_writer: Arc<Mutex<Option<WalWriter>>>,
    pub active_writes: Arc<AtomicUsize>,
    pub rebuild_jobs: Arc<Mutex<HashMap<String, RebuildJob>>>,
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
        let wal_writer = wal_dir
            .as_deref()
            .and_then(|dir| WalWriter::new(dir, 10_000).ok());
        let embedding_model = std::env::var("GRAPHITI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());
        Self {
            db: ArcSwap::from(db),
            embedder,
            extractor,
            dedup,
            write_lock: Arc::new(RwLock::new(())),
            sink,
            db_path,
            wal_dir,
            embedding_model,
            wal_writer: Arc::new(Mutex::new(wal_writer)),
            active_writes: Arc::new(AtomicUsize::new(0)),
            rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
