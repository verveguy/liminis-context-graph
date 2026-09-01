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
    embedding_cache::EmbeddingCache,
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

/// Per-group generalization of [`OntologyDriftState`] (issue #451). A group's entry is absent
/// from `AppState::group_ontologies` until that group's ontology is first resolved in this
/// process (FR-007) — callers distinguish "not yet computed" from "not drifted" by checking for
/// the group's presence in the cache, not by any field on this struct.
#[derive(Debug, Default, Clone)]
pub struct GroupDriftStatus {
    pub drifted: bool,
    pub drift_summary: Option<String>,
}

/// Cached per-group resolution result (issue #446/#451): the fully-resolved ontology for a group
/// alongside that group's drift status, computed together on the same first-use trigger under the
/// same lock — see [`AppState::resolve_ontology`].
#[derive(Debug, Clone)]
pub struct GroupOntologyEntry {
    pub ontology: Option<Arc<Ontology>>,
    pub drift: GroupDriftStatus,
}

/// `AppState::group_ontologies`'s value type (issue #495). A group's first `resolve_ontology`
/// call brackets its read-compute-insert sequence with a `Resolving` marker before it reads the
/// sidecar, so that a concurrent remediation's `clear_group_drift` — landing in the window
/// between that read and the eventual insert — can tell "a first resolution is racing me right
/// now" (upsert, so the stale insert that follows can't clobber this clear) apart from "nothing
/// has resolved this group yet" (stay absent, preserving FR-007: a remediation alone must never
/// populate the cache as a side effect). Only `Resolved` is a real answer — `group_drift_status`,
/// `all_group_drift_statuses`, and the `resolve_ontology`/`peek_or_load_ontology` cache-hit fast
/// paths all treat `Resolving` the same as absent.
#[derive(Debug, Clone)]
pub enum GroupOntologyCacheState {
    Resolving,
    Resolved(GroupOntologyEntry),
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
    /// group this process never touches never pays the file read. The cached value's `ontology`
    /// is already the *fully resolved* ontology for that group (a per-group file if one exists
    /// and is valid, otherwise the workspace-wide `ontology` above), so callers never need to
    /// re-apply the fallback themselves. Like `ontology`, requires a restart to pick up
    /// changes — hot-reload is out of scope (see the issue's Assumptions).
    ///
    /// Since issue #451, each entry's `drift` field also carries that group's drift status,
    /// computed on the same first-resolution trigger (FR-007) — folded into this existing cache
    /// rather than a new sibling field so that every one of `AppState`'s 54 literal-construction
    /// call sites (tests, a bench, `wal_exec.rs`'s helper) needs no edit: they all write
    /// `group_ontologies: Arc::new(Mutex::new(HashMap::new()))` with the value type inferred
    /// from this field's declaration alone. See ADR-0451.
    pub group_ontologies: Arc<Mutex<HashMap<String, GroupOntologyCacheState>>>,
    /// Content-addressed embedding cache (issue #440, FR-003) shared across every WAL-replaying
    /// call site that recomputes embeddings during this process's lifetime — constructed once in
    /// `main.rs` right after the embedder is probed, so it stays warm across the startup-recovery
    /// → serving transition, and threaded through here so `handle_rebuild_from_wal` reuses the
    /// same instance rather than starting cold on every rebuild.
    pub embedding_cache: Arc<EmbeddingCache>,
}

impl AppState {
    /// Builds `AppState` from environment variables.
    ///
    /// - `LCG_DEDUP_LLM`: if set, uses `LocalDedupAdapter`; otherwise `PassthroughDedupAdapter`.
    /// - `extractor`: already-resolved by the caller (mirrors `embedder`) — provider/transport
    ///   selection (Anthropic vs. local OpenAI-compatible) happens once in `main.rs`, not here.
    /// - `LCG_WAL_DIR`: WAL directory path (default `.lcg/wal`).
    /// - `LCG_EMBEDDING_MODEL`: embedding model name (default `bge-base-en-v1.5`).
    #[allow(clippy::too_many_arguments)]
    pub fn from_env(
        sink: Arc<dyn TelemetrySink>,
        db: Option<Arc<Db>>,
        degraded_reason: Option<String>,
        db_path: String,
        embedder: Arc<dyn Embedder>,
        embedding_model: String,
        extractor: Arc<dyn Extractor>,
        embedding_cache: Arc<EmbeddingCache>,
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
            embedding_cache,
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
    /// When `workspace_root` isn't configured, per-group file lookup is skipped (there's no root
    /// to look under) and this always falls through to the workspace-wide `ontology` field
    /// unchanged — this keeps direct-construction callers (tests, or any future caller that sets
    /// `ontology` without a `workspace_root`) working exactly as `state.ontology` did before this
    /// method existed.
    pub fn resolve_ontology(&self, group_id: &str) -> Option<Arc<Ontology>> {
        if let Ok(guard) = self.group_ontologies.lock() {
            if let Some(GroupOntologyCacheState::Resolved(cached)) = guard.get(group_id) {
                return cached.ontology.clone();
            }
        }

        // Mark this group as being resolved *before* the sidecar read below, so a concurrent
        // remediation's `clear_group_drift` landing in the window between this read and this
        // call's own insert (issue #495) can tell it's racing an in-flight resolution and upsert
        // accordingly, rather than stay absent (FR-007). A no-op if another racing
        // first-resolution of this same group already marked it (Edge Case: two racing
        // first-resolutions — both fall through and compute redundantly, exactly as the pre-#495
        // unconditional-insert behavior did for this ordering), or if it's already `Resolved`.
        self.mark_group_resolving(group_id);

        let (resolved, entry) = self.compute_group_ontology_entry(group_id);
        let drift = entry.drift.clone();
        // Only warn if this thread's (possibly stale) computation actually won the race and
        // became the cached state (issue #495): if a concurrent remediation's `clear_group_drift`
        // got there first, `entry` is discarded below and the group is not actually drifted, so
        // printing here would reproduce the same false-positive stderr symptom the cache-state
        // fix already closes.
        if self.insert_group_ontology_entry_if_not_resolved(group_id, entry) && drift.drifted {
            eprintln!(
                "liminis-context-graph: ontology: drift detected for group {group_id:?} — {} — recommend Recreate + re-ingest",
                drift.drift_summary.as_deref().unwrap_or("unknown change")
            );
        }
        resolved
    }

    /// Marks `group_id` as having a first resolution in flight (issue #495), unless it's already
    /// `Resolved` or already `Resolving` (a no-op either way — see [`Self::resolve_ontology`]'s
    /// Edge Case notes). Lets a concurrent `clear_group_drift` distinguish "a first resolution is
    /// racing me right now" from "nothing has resolved this group yet" (FR-007).
    fn mark_group_resolving(&self, group_id: &str) {
        if let Ok(mut guard) = self.group_ontologies.lock() {
            guard
                .entry(group_id.to_string())
                .or_insert(GroupOntologyCacheState::Resolving);
        }
    }

    /// Computes `group_id`'s resolved ontology and drift status by reading its sidecar, without
    /// touching `group_ontologies` — the read-and-compute half of [`Self::resolve_ontology`]'s
    /// first-resolution path, split out so the insert half can be insert-if-not-resolved (issue
    /// #495: this split makes the two halves independently, deterministically testable).
    fn compute_group_ontology_entry(
        &self,
        group_id: &str,
    ) -> (Option<Arc<Ontology>>, GroupOntologyEntry) {
        let resolved = self.load_resolved_ontology(group_id);

        // Per-group drift (issue #451, FR-001/FR-007): computed on this same first-resolution
        // trigger, against whatever this group actually resolved through (own file, workspace
        // fallback, or neither) — never the raw per-group file, consistent with #446's own
        // malformed-file-falls-back-silently behavior (ADR-0446 Decision 2).
        //
        // has_prior_data (FR-010) mirrors `from_env`'s workspace-level gate exactly, scoped to
        // this group: only queried when a workspace_root is configured AND this group has no
        // prior drift sidecar of its own — a group with an existing record never pays the DB
        // round trip, and a direct-construction caller with no workspace_root never touches the
        // DB at all (`is_none_or` short-circuits to `false` on `None`).
        let has_prior_data = if self
            .workspace_root
            .as_deref()
            .is_none_or(|root| ontology_sidecar::read_group_sidecar(root, group_id).is_some())
        {
            false
        } else {
            let db_opt = self.db.load_full();
            db_opt
                .as_ref()
                .and_then(|d| d.connect().ok())
                .and_then(|c| c.count_episodics_by_group_ids(&[group_id]).ok())
                .unwrap_or(0)
                > 0
        };
        let (drifted, drift_summary) = ontology_sidecar::compute_group_drift(
            self.workspace_root.as_deref(),
            group_id,
            resolved.as_deref(),
            has_prior_data,
        );
        // Emitting the stderr warning is deferred to the caller (issue #495): whether this
        // computation is actually drifted-and-cached depends on whether it wins the
        // insert-if-not-resolved race in `resolve_ontology`, which isn't known yet here.

        let entry = GroupOntologyEntry {
            ontology: resolved.clone(),
            drift: GroupDriftStatus {
                drifted,
                drift_summary,
            },
        };
        (resolved, entry)
    }

    /// Inserts `entry` for `group_id` unless it's already `Resolved` (issue #495), returning
    /// whether `entry` actually became the cached state. Closes the stale-drift-insert race: a
    /// concurrent remediation (WAL rebuild or successful `add_episode`) racing this group's first
    /// resolution now always wins if it reaches the cache first — via `clear_group_drift`'s
    /// upsert of the `Resolving` marker `resolve_ontology` leaves before calling this — since
    /// this insert finds the slot already `Resolved` and backs off instead of clobbering it. If
    /// the slot is still `Resolving` (this call's own marker, or another racing first-resolution's
    /// — see the Edge Case in `resolve_ontology`) or absent, `entry` is inserted. No lock is ever
    /// held across a DB round trip or sidecar read. The return value lets the caller
    /// (`resolve_ontology`) avoid warning about drift that lost the race and was never cached.
    fn insert_group_ontology_entry_if_not_resolved(
        &self,
        group_id: &str,
        entry: GroupOntologyEntry,
    ) -> bool {
        match self.group_ontologies.lock() {
            Ok(mut guard) => {
                if matches!(
                    guard.get(group_id),
                    Some(GroupOntologyCacheState::Resolved(_))
                ) {
                    false
                } else {
                    guard.insert(
                        group_id.to_string(),
                        GroupOntologyCacheState::Resolved(entry),
                    );
                    true
                }
            }
            Err(e) => {
                eprintln!(
                    "liminis-context-graph: group_ontologies: lock poisoned for group {group_id:?}: {e}"
                );
                false
            }
        }
    }

    /// Loads the ontology `group_id` resolves to (its own per-group file if present and valid,
    /// otherwise the workspace-wide fallback), without caching it or computing drift. The shared
    /// resolution step behind [`Self::resolve_ontology`] (which layers caching + drift on top)
    /// and [`Self::peek_or_load_ontology`] (which needs only the value, not those side effects).
    fn load_resolved_ontology(&self, group_id: &str) -> Option<Arc<Ontology>> {
        self.workspace_root
            .as_deref()
            .and_then(|root| crate::ontology::load_group_ontology(root, group_id))
            .map(Arc::new)
            .or_else(|| self.ontology.clone())
    }

    /// Returns `group_id`'s already-resolved ontology if this process has cached one, otherwise
    /// loads (without caching, and without computing or warning about drift) the value it would
    /// resolve to.
    ///
    /// Used by drift-clear sites (issue #451, FR-009 — the two `handle_rebuild_from_wal` paths)
    /// that need the currently-resolved ontology only to record it into that group's sidecar as
    /// part of clearing drift. Calling [`Self::resolve_ontology`] there instead would, for a
    /// group this process hasn't touched yet (the realistic case for an admin-triggered WAL
    /// rebuild used as degraded-mode recovery — see this repo's "WAL-corruption recovery" runbook
    /// in CLAUDE.md), compute drift against the *pre-remediation* sidecar and emit a "drift
    /// detected ... recommend Recreate + re-ingest" warning in the middle of the very operation
    /// that performs that remediation, immediately followed by this call's own clear — a
    /// misleading false alarm, not a real signal, since nothing about the group's state was ever
    /// observed by a caller in between.
    pub fn peek_or_load_ontology(&self, group_id: &str) -> Option<Arc<Ontology>> {
        if let Ok(guard) = self.group_ontologies.lock() {
            if let Some(GroupOntologyCacheState::Resolved(cached)) = guard.get(group_id) {
                return cached.ontology.clone();
            }
        }
        self.load_resolved_ontology(group_id)
    }

    /// Returns `group_id`'s cached drift status, or `None` if that group's ontology has not yet
    /// been resolved in this process (User Story 4, Scenario 2) — distinct from `Some(status)`
    /// with `drifted: false`. A group with only a `Resolving` marker (a first resolution still in
    /// flight, issue #495) is treated the same as absent — that marker isn't a real answer.
    pub fn group_drift_status(&self, group_id: &str) -> Option<GroupDriftStatus> {
        match self.group_ontologies.lock().ok()?.get(group_id)? {
            GroupOntologyCacheState::Resolved(e) => Some(e.drift.clone()),
            GroupOntologyCacheState::Resolving => None,
        }
    }

    /// Returns every group's cached drift status for `knowledge_status` (FR-002a) — purely from
    /// the in-memory cache, never a disk scan, so a group this process has never resolved is
    /// simply absent rather than falsely reported as "not drifted". Groups with only a
    /// `Resolving` marker (issue #495) are excluded for the same reason.
    pub fn all_group_drift_statuses(&self) -> Vec<(String, GroupDriftStatus)> {
        self.group_ontologies
            .lock()
            .map(|guard| {
                guard
                    .iter()
                    .filter_map(|(gid, state)| match state {
                        GroupOntologyCacheState::Resolved(entry) => {
                            Some((gid.clone(), entry.drift.clone()))
                        }
                        GroupOntologyCacheState::Resolving => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clears `group_id`'s cached drift status (FR-009), scoped to that one group only — called
    /// after a successful remediation (a WAL rebuild or a fresh `add_episode` ingest) for that
    /// specific group, never for every cached group (see ADR-0451).
    ///
    /// Three cases (issue #495):
    /// - **Already `Resolved`**: mutated in place, as before #495 — the common case.
    /// - **`Resolving`**: a first resolution of this same group is racing this remediation right
    ///   now, in the window between `resolve_ontology`'s sidecar read and its own insert. Upsert a
    ///   fresh, non-drifted entry seeded with `ontology` so that later insert finds the slot
    ///   already `Resolved` and backs off instead of clobbering this clear with a stale
    ///   `drifted: true` computed from the pre-remediation sidecar. `ontology` must be the
    ///   caller's already-resolved value for this group (all three call sites have one in hand); a
    ///   wrong value here would be cached for the life of the process, since a cache hit
    ///   short-circuits `resolve_ontology` forever after.
    /// - **Absent**: no resolution is in flight for this group at all — this remediation is
    ///   running alone. Stays a no-op, same as pre-#495: a remediation must never populate the
    ///   drift cache as a side effect for a group nothing has genuinely resolved yet (FR-007;
    ///   regression-tested by `per_group_ontology.rs`'s
    ///   `wal_rebuild_of_never_resolved_group_does_not_populate_drift_cache`).
    pub fn clear_group_drift(&self, group_id: &str, ontology: Option<Arc<Ontology>>) {
        if let Ok(mut guard) = self.group_ontologies.lock() {
            match guard.get_mut(group_id) {
                Some(GroupOntologyCacheState::Resolved(e)) => {
                    e.drift = GroupDriftStatus::default();
                }
                Some(slot @ GroupOntologyCacheState::Resolving) => {
                    *slot = GroupOntologyCacheState::Resolved(GroupOntologyEntry {
                        ontology,
                        drift: GroupDriftStatus::default(),
                    });
                }
                None => {}
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::MockEmbedder;
    use crate::extractor::MockExtractor;
    use crate::ontology::{EntityTypeDef, OntologyMode};
    use crate::telemetry::NoopSink;
    use std::path::Path;
    use tempfile::TempDir;

    const EMB_DIM: usize = 4;

    fn ontology_with(mode: OntologyMode, names: &[&str]) -> Ontology {
        Ontology {
            mode,
            entity_types: names
                .iter()
                .map(|n| EntityTypeDef {
                    name: n.to_string(),
                    description: None,
                    parent: None,
                })
                .collect(),
            relation_types: vec![],
            ancestor_map: HashMap::new(),
        }
    }

    // Mirrors `tests/per_group_ontology.rs`'s `make_db`: the caller owns the `TempDir` for the
    // test's scope so it's cleaned up on drop, instead of leaking its backing directory.
    fn make_db(dir: &TempDir) -> Arc<Db> {
        let db = Arc::new(Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap());
        {
            let conn = db.connect().unwrap();
            conn.init_schema(EMB_DIM).unwrap();
        }
        db
    }

    fn make_state(db: Arc<Db>, workspace_root: &Path, ontology: Option<Ontology>) -> AppState {
        let sink: Arc<dyn TelemetrySink> = Arc::new(NoopSink);
        AppState {
            db: ArcSwapOption::from(Some(db)),
            degraded_reason: Arc::new(Mutex::new(None)),
            embedder: Arc::new(MockEmbedder::new(EMB_DIM)),
            extractor: Arc::new(MockExtractor),
            dedup: Arc::new(PassthroughDedupAdapter),
            write_lock: Arc::new(RwLock::new(())),
            sink,
            db_path: "test.db".to_string(),
            wal_root: None,
            wal_max_events_per_file: 10_000,
            wal_max_bytes_per_file: 5 * 1024 * 1024,
            embedding_model: "bge-base-en-v1.5".to_string(),
            wal_writers: Arc::new(Mutex::new(HashMap::new())),
            active_writes: Arc::new(AtomicUsize::new(0)),
            rebuild_jobs: Arc::new(Mutex::new(HashMap::new())),
            workspace_root: Some(workspace_root.to_path_buf()),
            indices_built: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
            cancelled_chunks: Arc::new(AtomicUsize::new(0)),
            ontology: ontology.map(Arc::new),
            ontology_drift: Arc::new(Mutex::new(OntologyDriftState::default())),
            group_ontologies: Arc::new(Mutex::new(HashMap::new())),
            embedding_cache: Arc::new(EmbeddingCache::new()),
        }
    }

    fn write_group_ontology_file(workspace_root: &Path, group_id: &str, content: &str) {
        let path = crate::ontology::group_ontology_path(workspace_root, group_id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    // Issue #495, User Story 1: a concurrent remediation's drift clear must survive a
    // subsequent, stale first-resolution insert for the same never-before-resolved group.
    #[test]
    fn concurrent_remediation_clear_survives_a_stale_first_resolution_insert() {
        let workspace_dir = TempDir::new().unwrap();
        let root = workspace_dir.path();

        // Group's per-group file on disk declares {Person, Organization}, but its recorded
        // sidecar only reflects {Person} — the pre-remediation state `resolve_ontology`'s stale
        // read observes.
        write_group_ontology_file(
            root,
            "group-a",
            "mode: open\nentity_types:\n  - name: Person\n  - name: Organization\n",
        );
        ontology_sidecar::write_group_sidecar(
            root,
            "group-a",
            Some(&ontology_with(OntologyMode::Open, &["Person"])),
        )
        .unwrap();

        let db_dir = TempDir::new().unwrap();
        let state = make_state(make_db(&db_dir), root, None);

        // Simulates `resolve_ontology`'s own mark-resolving step, which runs before its sidecar
        // read so a racing remediation's clear can tell a first resolution is in flight.
        state.mark_group_resolving("group-a");

        // Simulates `resolve_ontology`'s read-compute half: observes drift against the
        // as-yet-unremediated sidecar.
        let (resolved, stale_entry) = state.compute_group_ontology_entry("group-a");
        assert!(
            stale_entry.drift.drifted,
            "the stale read must observe drift, since the sidecar hasn't caught up to the \
             per-group file yet"
        );

        // Simulates a concurrent remediation (WAL rebuild / add_episode) completing in the
        // window between that read and `resolve_ontology`'s insert: it writes a matching
        // sidecar and clears drift via `clear_group_drift`, racing ahead of the stale insert.
        ontology_sidecar::write_group_sidecar(
            root,
            "group-a",
            Some(&ontology_with(
                OntologyMode::Open,
                &["Person", "Organization"],
            )),
        )
        .unwrap();
        state.clear_group_drift("group-a", resolved.clone());
        assert!(
            !state
                .group_drift_status("group-a")
                .expect("clear_group_drift must upsert over the Resolving marker")
                .drifted,
            "the remediation's clear must record not-drifted"
        );

        // The stale insert now arrives — it must not clobber the remediation's clear.
        state.insert_group_ontology_entry_if_not_resolved("group-a", stale_entry);
        assert!(
            !state
                .group_drift_status("group-a")
                .expect("entry must still exist")
                .drifted,
            "a stale first-resolution insert must not overwrite a concurrent remediation's clear"
        );
    }

    // Guards the risk flagged in Research: `clear_group_drift`'s upsert path (triggered when a
    // first resolution is racing it) must record the ontology it was given, not `None` — a wrong
    // seed here is cached for the process's entire life, since a cache hit short-circuits
    // `resolve_ontology` forever after.
    #[test]
    fn clear_group_drift_upsert_during_in_flight_resolution_records_the_given_ontology() {
        let workspace_dir = TempDir::new().unwrap();
        let root = workspace_dir.path();
        let db_dir = TempDir::new().unwrap();
        let state = make_state(make_db(&db_dir), root, None);

        let ontology = Arc::new(ontology_with(OntologyMode::Open, &["Person"]));
        assert!(state.group_drift_status("group-a").is_none());

        // Without this, `clear_group_drift` must stay a no-op (see the next test) — the upsert
        // path only triggers when a first resolution is genuinely in flight.
        state.mark_group_resolving("group-a");
        state.clear_group_drift("group-a", Some(ontology.clone()));

        let resolved = state.resolve_ontology("group-a");
        assert!(
            resolved.is_some_and(|o| Arc::ptr_eq(&o, &ontology)),
            "resolve_ontology must return the ontology clear_group_drift's upsert recorded, \
             not None or a freshly re-resolved value"
        );
    }

    // FR-007 regression guard (issue #495): a remediation's clear must never populate the drift
    // cache for a group nothing has genuinely resolved yet — only an in-flight resolution's
    // `Resolving` marker may make `clear_group_drift` upsert. Without this, a bare
    // upsert-on-absence (the design this test replaced) reintroduces the exact bug
    // `per_group_ontology.rs`'s `wal_rebuild_of_never_resolved_group_does_not_populate_drift_cache`
    // guards against: a WAL rebuild of a never-resolved group would populate its drift cache as a
    // side effect, even with no `resolve_ontology` call anywhere in the picture.
    #[test]
    fn clear_group_drift_is_a_noop_when_no_resolution_is_in_flight() {
        let workspace_dir = TempDir::new().unwrap();
        let root = workspace_dir.path();
        let db_dir = TempDir::new().unwrap();
        let state = make_state(make_db(&db_dir), root, None);

        let ontology = Arc::new(ontology_with(OntologyMode::Open, &["Person"]));
        state.clear_group_drift("group-a", Some(ontology));

        assert!(
            state.group_drift_status("group-a").is_none(),
            "a remediation's clear must not populate the drift cache for a group nothing has \
             genuinely resolved yet (FR-007)"
        );
    }

    // Edge case: an already-cached group is unaffected by the insert-if-not-resolved change —
    // `resolve_ontology` never reaches the compute/insert path again once cached.
    #[test]
    fn already_resolved_group_is_unaffected_by_insert_if_not_resolved() {
        let workspace_dir = TempDir::new().unwrap();
        let root = workspace_dir.path();
        let db_dir = TempDir::new().unwrap();
        let state = make_state(make_db(&db_dir), root, None);

        let first = state.resolve_ontology("group-a");
        let second = state.resolve_ontology("group-a");
        assert_eq!(
            first.is_some(),
            second.is_some(),
            "repeated resolution of an already-cached group must be stable"
        );
        assert_eq!(state.group_ontologies.lock().unwrap().len(), 1);
    }
}
