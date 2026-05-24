use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use tokio::sync::RwLock;

use crate::{
    db::Db,
    dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter},
    embedder::{Embedder, HttpEmbedder},
    env::lcg_env_var,
    extractor::Extractor,
    llm_router::LlmRouter,
    rebuild_job::RebuildJob,
    telemetry::TelemetrySink,
    wal::WalWriter,
};

pub struct AppState {
    /// ArcSwapOption allows `clear_all` and `knowledge_recover` to atomically replace the live Db
    /// under the write lock without holding an inner Mutex. `None` represents degraded state
    /// (DB unavailable). All handlers call `db.load_full()` to get a snapshot — a lock-free read.
    /// See ADR-0043 and ADR-0046.
    pub db: ArcSwapOption<Db>,
    /// Set at startup when DB open fails recoverably; cleared after successful recovery.
    pub degraded_reason: Arc<Mutex<Option<String>>>,
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
    /// Workspace root for locating `.liminis/knowledge-corrections.yaml`.
    /// Read from `LIMINIS_WORKSPACE_ROOT` env var. All corrections methods return
    /// an error if this is `None`.
    pub workspace_root: Option<PathBuf>,
}

impl AppState {
    /// Builds `AppState` from environment variables.
    ///
    /// - `LCG_DEDUP_LLM`: if set, uses `LocalDedupAdapter`; otherwise `PassthroughDedupAdapter`.
    /// - `LCG_EXTRACTION_LLM`: parsed by `LlmRouter::from_env`.
    /// - `LCG_WAL_DIR`: optional WAL directory path.
    /// - `LCG_EMBEDDING_MODEL`: embedding model name (default `bge-base-en-v1.5`).
    pub fn from_env(
        sink: Arc<dyn TelemetrySink>,
        db: Option<Arc<Db>>,
        degraded_reason: Option<String>,
        db_path: String,
    ) -> Self {
        let embedder: Arc<dyn Embedder> = Arc::new(HttpEmbedder::from_env());
        let extractor: Arc<dyn Extractor> = Arc::new(LlmRouter::from_env(Arc::clone(&sink)));
        // deprecated: remove in Phase B (see #59)
        let dedup: Arc<dyn DedupAdapter> =
            if lcg_env_var("LCG_DEDUP_LLM", "GRAPHITI_DEDUP_LLM").is_ok() {
                Arc::new(LocalDedupAdapter::from_env())
            } else {
                Arc::new(PassthroughDedupAdapter)
            };
        // deprecated: remove in Phase B (see #59)
        let wal_dir =
            lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR").ok().map(PathBuf::from);
        let wal_writer = wal_dir
            .as_deref()
            .and_then(|dir| WalWriter::new(dir, 10_000).ok());
        // deprecated: remove in Phase B (see #59)
        let embedding_model = lcg_env_var("LCG_EMBEDDING_MODEL", "GRAPHITI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "bge-base-en-v1.5".to_string());
        let workspace_root = std::env::var("LIMINIS_WORKSPACE_ROOT")
            .ok()
            .map(PathBuf::from);
        Self {
            db: ArcSwapOption::from(db),
            degraded_reason: Arc::new(Mutex::new(degraded_reason)),
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
            workspace_root,
        }
    }
}
