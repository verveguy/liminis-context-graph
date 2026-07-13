use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{
    db::Db,
    dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter},
    embedder::Embedder,
    env::lcg_env_var,
    extractor::Extractor,
    llm_router::LlmRouter,
    ontology::{load_ontology, Ontology},
    ontology_sidecar,
    rebuild_job::RebuildJob,
    telemetry::TelemetrySink,
    wal::WalWriter,
};

/// Ontology drift state computed at startup and cleared after each successful ingest.
#[derive(Debug, Default, Clone)]
pub struct OntologyDriftState {
    pub drifted: bool,
    pub drift_summary: Option<String>,
}

pub struct AppState {
    /// ArcSwapOption allows `clear_all` and `knowledge_recover` to atomically replace the live Db
    /// under the write lock without holding an inner Mutex. `None` represents degraded state
    /// (DB unavailable). All handlers call `db.load_full()` to get a snapshot — a lock-free read.
    /// See ADR-0003 and ADR-0009.
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
    pub wal_max_events_per_file: usize,
    pub wal_max_bytes_per_file: u64,
    pub embedding_model: String,
    pub wal_writer: Arc<Mutex<Option<WalWriter>>>,
    pub active_writes: Arc<AtomicUsize>,
    pub rebuild_jobs: Arc<Mutex<HashMap<String, RebuildJob>>>,
    /// Tracks whether HNSW vector indices have been built in this session.
    /// Set to `true` after the first successful `build_indices_and_constraints` call
    /// (whether explicit via `knowledge_build_indices` or auto-triggered on first search).
    /// Reset to `false` in `handle_clear_all` so the first post-clear search self-heals.
    pub indices_built: Arc<AtomicBool>,
    /// Workspace root for locating `.liminis/knowledge-corrections.yaml`.
    /// Read from `LIMINIS_WORKSPACE_ROOT` env var. All corrections methods return
    /// an error if this is `None`.
    pub workspace_root: Option<PathBuf>,
    /// Cancelled when graceful shutdown begins. All in-flight async operations select!
    /// against this token at phase boundaries to exit cleanly without waiting for the
    /// full inner shutdown timeout.
    pub cancel_token: CancellationToken,
    /// Counts the number of add_episode calls that were interrupted by cancellation.
    /// Cloned before drop(state) in main.rs to populate the "stopped" telemetry detail.
    pub cancelled_chunks: Arc<AtomicUsize>,
    /// Workspace-scoped entity/relation vocabulary loaded from `.lcg/ontology.yaml`.
    /// `None` when no file is present, empty, or malformed — free-form extraction applies.
    /// Requires a service restart to pick up changes (FR-007; v1.5 will add hot-reload).
    pub ontology: Option<Arc<Ontology>>,
    /// Drift state computed at startup by comparing the current ontology's hash against the
    /// persisted `.lcg/ontology-hash.json` sidecar. Cleared after each successful ingest write.
    pub ontology_drift: Arc<Mutex<OntologyDriftState>>,
}

impl AppState {
    /// Builds `AppState` from environment variables.
    ///
    /// - `LCG_DEDUP_LLM`: if set, uses `LocalDedupAdapter`; otherwise `PassthroughDedupAdapter`.
    /// - `LCG_EXTRACTION_LLM`: parsed by `LlmRouter::from_env`.
    /// - `LCG_WAL_DIR`: WAL directory path (default `.lcg/wal`).
    /// - `LCG_EMBEDDING_MODEL`: embedding model name (default `bge-base-en-v1.5`).
    pub fn from_env(
        sink: Arc<dyn TelemetrySink>,
        db: Option<Arc<Db>>,
        degraded_reason: Option<String>,
        db_path: String,
        embedder: Arc<dyn Embedder>,
        embedding_model: String,
    ) -> Self {
        let extractor: Arc<dyn Extractor> = Arc::new(LlmRouter::from_env(Arc::clone(&sink)));
        // deprecated: remove in Phase B (see #59)
        let dedup: Arc<dyn DedupAdapter> =
            if lcg_env_var("LCG_DEDUP_LLM", "GRAPHITI_DEDUP_LLM").is_ok() {
                Arc::new(LocalDedupAdapter::from_env())
            } else {
                Arc::new(PassthroughDedupAdapter)
            };
        // deprecated: remove in Phase B (see #59)
        // Default to `.lcg/wal` (CWD-relative, matches the convention used by
        // LCG_SOCKET_PATH and LCG_DB_PATH). Application WAL is essential for
        // the `knowledge_rebuild_from_wal` recovery path; without a default,
        // dropping the env var (per liminis#828) silently disabled WAL writes.
        let wal_dir = Some(PathBuf::from(
            lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR")
                .unwrap_or_else(|_| ".lcg/wal".to_string()),
        ));
        let max_events_per_file: usize = std::env::var("LCG_WAL_MAX_EVENTS_PER_FILE")
            .ok()
            .and_then(|v| {
                v.parse::<usize>().map_err(|_| {
                    eprintln!(
                        "liminis-context-graph: LCG_WAL_MAX_EVENTS_PER_FILE={v:?} is not a valid usize; using default 10000"
                    );
                }).ok()
            })
            .unwrap_or(10_000);
        let max_bytes_per_file: u64 = std::env::var("LCG_WAL_MAX_BYTES_PER_FILE")
            .ok()
            .and_then(|v| {
                v.parse::<u64>().map_err(|_| {
                    eprintln!(
                        "liminis-context-graph: LCG_WAL_MAX_BYTES_PER_FILE={v:?} is not a valid u64; using default 5242880"
                    );
                }).ok()
            })
            .unwrap_or(5 * 1024 * 1024);
        let wal_writer = wal_dir
            .as_deref()
            .and_then(|dir| WalWriter::new(dir, max_events_per_file, max_bytes_per_file).ok());
        let workspace_root = std::env::var("LIMINIS_WORKSPACE_ROOT")
            .ok()
            .map(PathBuf::from);
        let ontology = load_ontology(workspace_root.as_deref()).map(Arc::new);
        if ontology.is_none() {
            eprintln!(
                "liminis-context-graph: ontology: none — free-form extraction (restart required to pick up changes)"
            );
        }
        // For pre-#98 workspaces that have no sidecar file, check whether the DB already
        // contains ingested data. If it does, loading a new ontology counts as drift (FR-002).
        let has_prior_data = if workspace_root
            .as_deref()
            .is_none_or(|r| ontology_sidecar::read_sidecar(r).is_some())
        {
            false
        } else {
            db.as_ref()
                .and_then(|d| d.connect().ok())
                .and_then(|c| c.count_nodes("Episodic").ok())
                .unwrap_or(0)
                > 0
        };
        let (drifted, drift_summary) = ontology_sidecar::compute_drift(
            workspace_root.as_deref(),
            ontology.as_deref(),
            has_prior_data,
        );
        if drifted {
            eprintln!(
                "liminis-context-graph: ontology: drift detected — {} — recommend Recreate + re-ingest",
                drift_summary.as_deref().unwrap_or("unknown change")
            );
        }
        let ontology_drift = Arc::new(Mutex::new(OntologyDriftState {
            drifted,
            drift_summary,
        }));
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
            wal_max_events_per_file: max_events_per_file,
            wal_max_bytes_per_file: max_bytes_per_file,
            embedding_model,
            wal_writer: Arc::new(Mutex::new(wal_writer)),
            active_writes: Arc::new(AtomicUsize::new(0)),
            rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
            workspace_root,
            indices_built: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
            cancelled_chunks: Arc::new(AtomicUsize::new(0)),
            ontology,
            ontology_drift,
        }
    }
}
