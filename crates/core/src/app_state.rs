use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{
    db::Db,
    dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter},
    embedder::Embedder,
    env::lcg_env_var,
    error::Error,
    extractor::Extractor,
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
    /// WAL **root** directory (issue #378): contains one subdirectory per `group_id`, each an
    /// independent WAL stream. `LCG_WAL_DIR`'s pre-378 meaning (a single shared directory)
    /// migrates automatically into `<wal_root>/liminis/` — see `wal_group::migrate_wal_root_if_needed`.
    pub wal_root: Option<PathBuf>,
    pub wal_max_events_per_file: usize,
    pub wal_max_bytes_per_file: u64,
    pub embedding_model: String,
    /// Per-group `WalWriter` map (issue #378 FR-003), keyed by `group_id`. A group's writer and
    /// directory are created lazily on that group's first write — see [`AppState::with_wal_writer`].
    /// That method holds this `Mutex` for its whole callback, including the WAL disk write, not
    /// just the lookup-or-create step — but that never adds contention beyond what already
    /// exists: every caller reaches `with_wal_writer` while already holding `write_lock`
    /// exclusively (the single embedded DB is a single-writer store regardless of group count,
    /// see `write_lock`'s own doc comment), so at most one write is ever in flight across the
    /// whole instance anyway. This map exists to give each group its own `global_seq` and
    /// directory, not to enable concurrent cross-group flushes.
    pub wal_writers: Arc<Mutex<HashMap<String, WalWriter>>>,
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
    /// Per-group ontology resolution cache (issue #446), keyed by `group_id`. Populated lazily
    /// by [`AppState::resolve_ontology`] on first use per group, mirroring `wal_writers`'
    /// lazy-populate pattern rather than an eager startup scan of `.lcg/ontology/*.yaml` — a
    /// group this process never touches never pays the file read. The cached value is already
    /// the *fully resolved* ontology for that group (a per-group file if one exists and is
    /// valid, otherwise the workspace-wide `ontology` above), so callers never need to
    /// re-apply the fallback themselves. Like `ontology`, requires a restart to pick up
    /// changes — hot-reload is out of scope (see the issue's Assumptions).
    pub group_ontologies: Arc<Mutex<HashMap<String, Option<Arc<Ontology>>>>>,
}

impl AppState {
    /// Builds `AppState` from environment variables.
    ///
    /// - `LCG_DEDUP_LLM`: if set, uses `LocalDedupAdapter`; otherwise `PassthroughDedupAdapter`.
    /// - `extractor`: already-resolved by the caller (mirrors `embedder`) — provider/transport
    ///   selection (Anthropic vs. local OpenAI-compatible) happens once in `main.rs`, not here.
    /// - `LCG_WAL_DIR`: WAL directory path (default `.lcg/wal`).
    /// - `LCG_EMBEDDING_MODEL`: embedding model name (default `bge-base-en-v1.5`).
    pub fn from_env(
        sink: Arc<dyn TelemetrySink>,
        db: Option<Arc<Db>>,
        degraded_reason: Option<String>,
        db_path: String,
        embedder: Arc<dyn Embedder>,
        embedding_model: String,
        extractor: Arc<dyn Extractor>,
    ) -> Self {
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
        //
        // `LCG_WAL_DIR`'s meaning changed with issue #378: it now names a WAL **root**
        // containing one subdirectory per `group_id`, not a single shared stream. A pre-378
        // workspace's flat directory is migrated in place, once, before any per-group writer is
        // constructed (FR-001, FR-009).
        let wal_root = Some(PathBuf::from(
            lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR")
                .unwrap_or_else(|_| ".lcg/wal".to_string()),
        ));
        // #437: this call is idempotent, but it is not always a no-op — `from_env` has other
        // callers (e.g. tests, or any future direct construction path) besides the service
        // binary's own startup, and for one of those a flat `.lcg/wal` root with nothing yet
        // relocated makes this the operative migration call. What it can *never* do is perform
        // the `.graphiti/` → `.lcg/` workspace move itself (`from_env` has no access to
        // `.graphiti/`-era paths), so on the service binary's startup path specifically, this
        // call is only reached after `main.rs`'s `migration::migrate_workspace` and
        // `bootstrap_app_state`'s own `migrate_wal_root_if_needed` call have already run — see
        // those call sites for why that order matters. It must never become the *sole* call on
        // that path.
        if let Some(root) = wal_root.as_deref() {
            if let Err(e) = crate::wal_group::migrate_wal_root_if_needed(root) {
                eprintln!(
                    "liminis-context-graph: wal root migration failed for {root:?} (non-fatal to \
                     startup, but every per-group path below resolves under <wal_root>/<group_id> \
                     regardless — any pre-378 loose top-level WAL content at {root:?} stays on \
                     disk untouched but becomes invisible to this process until migration \
                     succeeds; it is not read as a fallback): {e}"
                );
            }
        }
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
        // Per-group writers are created lazily on first write (FR-003) — nothing is created
        // eagerly here, unlike the pre-378 single-writer startup path.
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
            wal_root,
            wal_max_events_per_file: max_events_per_file,
            wal_max_bytes_per_file: max_bytes_per_file,
            embedding_model,
            wal_writers: Arc::new(Mutex::new(HashMap::new())),
            active_writes: Arc::new(AtomicUsize::new(0)),
            rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
            workspace_root,
            indices_built: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
            cancelled_chunks: Arc::new(AtomicUsize::new(0)),
            ontology,
            ontology_drift,
            group_ontologies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolves the operative ontology for `group_id` (FR-001, FR-002, FR-005): a per-group
    /// ontology file at `.lcg/ontology/<encoded_group_id>.yaml` if one exists and is valid,
    /// otherwise the workspace-wide `ontology` field, otherwise `None`.
    ///
    /// Lazily loads and caches the result per `group_id` on first call (mirrors
    /// `with_wal_writer`'s lazy-populate pattern) — later calls for the same group are a
    /// `HashMap` lookup, not a disk read. A malformed or unreadable per-group file falls back to
    /// the workspace ontology rather than to `None` or a hard error: `load_group_ontology`
    /// already logs the failure loudly, so silently using the one ontology already known to be
    /// valid is the least-surprising degradation (see the issue's Plan stage for the tradeoff).
    ///
    /// Returns `None` (rather than caching) if `workspace_root` isn't configured — matches
    /// `load_ontology`'s own "no root, no ontology" behavior, and avoids caching an answer that
    /// would be wrong once a root is configured.
    pub fn resolve_ontology(&self, group_id: &str) -> Option<Arc<Ontology>> {
        let root = self.workspace_root.as_deref()?;

        if let Ok(guard) = self.group_ontologies.lock() {
            if let Some(cached) = guard.get(group_id) {
                return cached.clone();
            }
        }

        let resolved = crate::ontology::load_group_ontology(root, group_id)
            .map(Arc::new)
            .or_else(|| self.ontology.clone());

        match self.group_ontologies.lock() {
            Ok(mut guard) => {
                guard.insert(group_id.to_string(), resolved.clone());
            }
            Err(e) => {
                eprintln!(
                    "liminis-context-graph: group_ontologies: lock poisoned for group {group_id:?}: {e}"
                );
            }
        }
        resolved
    }

    /// Locks `wal_writers`, lazily creating `group_id`'s writer (and its WAL directory) on
    /// first use if it doesn't already exist (issue #378 FR-003), and hands the caller mutable
    /// access to it via `f`.
    ///
    /// Returns `None` when no `wal_root` is configured, the lock is poisoned, or the writer or
    /// its directory could not be created — WAL is a non-fatal recovery artifact (see
    /// `wal_exec.rs`'s module doc), so every caller here already treats `None` the same as
    /// "nothing to flush," not a hard error.
    pub fn with_wal_writer<T>(
        &self,
        group_id: &str,
        f: impl FnOnce(&mut WalWriter) -> T,
    ) -> Option<T> {
        let root = self.wal_root.as_deref()?;
        let mut guard = match self.wal_writers.lock() {
            Ok(g) => g,
            Err(e) => {
                eprintln!(
                    "liminis-context-graph: wal_writers: lock poisoned for group {group_id:?}: {e}"
                );
                return None;
            }
        };
        if !guard.contains_key(group_id) {
            let dir_name = match crate::wal_group::encode_group_dir_name(group_id) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: wal_writers: cannot resolve directory for group {group_id:?}: {e}"
                    );
                    return None;
                }
            };
            // Fail loudly rather than silently interleaving two groups into one physical
            // directory on a case-insensitive filesystem (issue #378: encode_group_dir_name is
            // bijective at the string level only, see check_no_case_insensitive_collision).
            if let Err(e) = crate::wal_group::check_no_case_insensitive_collision(root, &dir_name) {
                eprintln!(
                    "liminis-context-graph: wal_writers: refusing to create directory for group {group_id:?}: {e}"
                );
                return None;
            }
            let dir = root.join(&dir_name);
            match WalWriter::new(
                &dir,
                self.wal_max_events_per_file,
                self.wal_max_bytes_per_file,
            ) {
                Ok(w) => {
                    guard.insert(group_id.to_string(), w);
                }
                Err(e) => {
                    eprintln!(
                        "liminis-context-graph: wal_writers: failed to create writer for group {group_id:?} at {dir:?}: {e}"
                    );
                    return None;
                }
            }
        }
        guard.get_mut(group_id).map(f)
    }
}

/// Extract `Arc<Db>` from the `ArcSwapOption`, returning `Error::DbUnavailable` if `None`.
///
/// The degraded-mode guard in `handlers::handle()` prevents most handlers from reaching this
/// point when the DB is unavailable, so this acts as a safety net for internal calls.
pub fn load_db(state: &AppState) -> Result<Arc<Db>, Error> {
    state.db.load_full().ok_or_else(|| {
        let reason = state
            .degraded_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Error::DbUnavailable(reason)
    })
}

/// Acquires the write lock and calls `build_indices_and_constraints`, then sets the
/// `indices_built` flag so subsequent callers skip the auto-heal path (FR-003).
/// Called at most once per session per DB lifecycle event. Shared by the search handlers'
/// and the ingest dedup path's auto-heal logic.
pub async fn build_indices_once(state: &Arc<AppState>) -> Result<(), Error> {
    let _guard = state.write_lock.write().await;
    // Double-check inside the lock: another task may have completed the build while we waited.
    if state.indices_built.load(Ordering::Acquire) {
        return Ok(());
    }
    // Load DB after acquiring the lock so we build on the current instance, not a stale
    // snapshot that predates a concurrent clear_all swap.
    let db = load_db(state)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()
    })
    .await;
    match result {
        Ok(Ok(())) => {
            // Set flag while still holding the write lock to eliminate the window between
            // guard release and flag update that would allow redundant builds.
            state.indices_built.store(true, Ordering::Release);
            Ok(())
        }
        Ok(Err(e)) => Err(Error::Ipc(format!(
            "Auto-build of knowledge graph indices failed: {e}"
        ))),
        Err(e) => Err(Error::Ipc(format!(
            "Auto-build of knowledge graph indices failed: {e}"
        ))),
    }
}
