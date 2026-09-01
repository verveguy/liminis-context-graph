use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::{
    db::Db,
    embedding_cache::EmbedderContext,
    error::Error,
    replay::{ReplayOptions, WalReplayer},
    schema,
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
};

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum CursorReason {
    UuidMatch,
    NoEpisodes,
    UuidNotFound,
}

impl CursorReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CursorReason::UuidMatch => "uuid_match",
            CursorReason::NoEpisodes => "no_episodes",
            CursorReason::UuidNotFound => "uuid_not_found",
        }
    }
}

pub struct RecoveryReport {
    pub episodes_before: u64,
    pub mutations_replayed: u64,
    pub episodes_after: u64,
    pub indexes_rebuilt: bool,
    pub from_seq: u64,
    pub cursor_reason: CursorReason,
    pub drop_elapsed_ms: u64,
}

// ── Episode-cursor derivation ─────────────────────────────────────────────────

/// Derives the WAL resume point from the last episode in `conn`.
///
/// Returns `(from_seq, reason)` where `from_seq` is the inclusive WAL sequence
/// number to start replay at. Conservative: never skips mutations.
///
/// - If no episodes exist → `(0, NoEpisodes)`
/// - If the last episode uuid is not found in any WAL file → `(0, UuidNotFound)`
/// - Otherwise → `(min_seq_across_all_matches, UuidMatch)`
///
/// Scans ALL files to find the global minimum seq (episode uuid may appear in
/// multiple files, e.g. as `params["ep"]` on MENTIONS edges).
///
/// `wal_dir` is `group_id`'s own resolved WAL directory (not the WAL root) — the caller
/// resolves it via `wal_group::group_wal_dir` before calling. Scoped by `group_id` (issue #378
/// FR-010) so a backfill for one group can never pick up another group's most recent episode.
pub fn derive_episode_cursor(
    conn: &crate::db::Conn<'_>,
    group_id: &str,
    wal_dir: &Path,
) -> Result<(u64, CursorReason), Error> {
    let target_uuid = match conn.get_latest_episode_uuid(group_id)? {
        Some(u) => u,
        None => return Ok((0, CursorReason::NoEpisodes)),
    };

    let mut min_seq: Option<u64> = None;

    let wal_files = collect_wal_files(wal_dir);
    for wal_file in &wal_files {
        match scan_file_for_uuid(wal_file, &target_uuid) {
            Ok(Some(seq)) => {
                min_seq = Some(min_seq.map_or(seq, |m: u64| m.min(seq)));
            }
            Ok(None) => {}
            Err(_) => {
                eprintln!(
                    "liminis-context-graph: WAL recovery: skipping unreadable file {:?}",
                    wal_file
                );
            }
        }
    }

    match min_seq {
        Some(seq) => Ok((seq, CursorReason::UuidMatch)),
        None => Ok((0, CursorReason::UuidNotFound)),
    }
}

fn collect_wal_files(wal_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = wal_dir
        .read_dir()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

/// Scans `path` line by line for the first match of `target_uuid` in
/// `params["uuid"]` or `params["ep"]`. Returns that line's `seq`, or `None`.
///
/// WAL files are append-only with a globally monotonic seq counter, so within a
/// single file seq is strictly increasing. The first match is therefore the minimum
/// seq for the file — no need to scan further.
fn scan_file_for_uuid(path: &Path, target_uuid: &str) -> Result<Option<u64>, Error> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        // Cheap text-search before parsing JSON
        if !line.contains(target_uuid) {
            continue;
        }
        // Parse only lines that contain the uuid as a substring
        if let Ok(wal_line) = serde_json::from_str::<crate::wal::WalLine>(&line) {
            let params = &wal_line.params;
            let matches = params.get("uuid").and_then(|v| v.as_str()) == Some(target_uuid)
                || params.get("ep").and_then(|v| v.as_str()) == Some(target_uuid);
            if matches {
                return Ok(Some(wal_line.seq));
            }
        }
    }

    Ok(None)
}

// ── Applied-WAL-seq backfill (issue #353) ──────────────────────────────────────

/// Backfills the persisted applied-WAL-seq position for a pre-existing DB that has content but
/// no recorded position (FR-007/FR-008) — the state every deployment that pre-dates this
/// feature hits exactly once, on first open after upgrade. No-op if a position is already
/// recorded (the common case on every subsequent boot).
///
/// - Fresh/empty DB (no `Episodic` nodes) → writes `0` directly, skipping the WAL scan
///   entirely. This is what distinguishes "genuinely fresh" from "populated but unknown" —
///   the spec's core ambiguity (see the issue's "gap #351 does not cover" discussion).
/// - Populated DB whose last episode's uuid is found in the WAL (`CursorReason::UuidMatch`) →
///   writes that line's `seq`. Reuses ADR-0026's episode-cursor mechanism, which that ADR
///   documents as explicitly retroactive (works on databases that predate any cursor
///   mechanism) — exactly the upgrade case. The derived value is conservative (an episode
///   boundary, so `<=` the true applied position), the same safe direction FR-003 requires.
/// - Populated DB whose last episode's uuid is not found in the WAL
///   (`CursorReason::UuidNotFound`) → leaves the row absent (`null`), per FR-008/SC-007; the
///   documented action for that state is a full rebuild, the same fallback ADR-0026 already
///   defines for its own recovery path. `CursorReason::NoEpisodes` is unreachable here since
///   the episode-count guard above already ran.
///
/// `wal_dir` is `group_id`'s own resolved WAL directory (not the WAL root). Scoped by
/// `group_id` throughout (issue #378 FR-010): both the emptiness check and the episode-cursor
/// derivation only ever look at `group_id`'s own content, so a backfill for group A can never
/// be derived from — or degrade because of — group B's episodes.
///
/// Every position this backfills carries whatever generation is currently on disk for `wal_dir`
/// (issue #387) — `None` for a pre-#387 stream. This is FR-009's "adopt on first encounter"
/// behavior: it falls out of the ordinary write path with no separate branch, since a backfill
/// only ever runs when no position (and so no generation) has been recorded yet.
pub fn backfill_applied_seq_if_absent(
    conn: &crate::db::Conn<'_>,
    group_id: &str,
    wal_dir: &Path,
) -> Result<(), Error> {
    if conn.get_wal_position(group_id)?.applied_seq.is_some() {
        return Ok(());
    }
    let generation = crate::wal_generation::read_generation(wal_dir);
    let gids = [group_id];
    if conn.count_episodics_by_group_ids(&gids)? == 0 {
        // Episodic count alone doesn't prove the group is empty: `remove_episode` and
        // `remove_episodes_by_source`/`_by_chunk_id` only `DETACH DELETE` the `Episodic`
        // node, never the `Entity`/edge data it created (db.rs), so a group that had all its
        // episodes deleted can still hold real, unrecorded content with zero episodes left
        // to anchor a position derivation on. Only collapse to the "genuinely fresh" `0`
        // case when there is truly nothing else either — otherwise leave the row absent
        // (`null`, the documented "unknown, full rebuild" signal) rather than falsely
        // reporting "known, nothing applied" for a populated-but-episode-less group.
        if group_has_no_content(conn, group_id)? {
            return conn.set_wal_position(group_id, 0, generation.as_deref(), None);
        }
        return Ok(());
    }
    let (seq, reason) = derive_episode_cursor(conn, group_id, wal_dir)?;
    if reason == CursorReason::UuidMatch {
        conn.set_wal_position(group_id, seq, generation.as_deref(), None)?;
    }
    // UuidNotFound (and the unreachable NoEpisodes): leave the row absent — null is the
    // correct, documented "backfill failed, full rebuild required" report (FR-008).
    Ok(())
}

/// True if `group_id` holds zero `Episodic` nodes, zero `Entity` nodes, and zero relationship
/// edges — the same three-count emptiness check `backfill_applied_seq_if_absent` uses to tell
/// "genuinely fresh" from "populated but episode-less" (issue #353), scoped per group by issue
/// #378. Reused by the WAL checkpoint feature (issue #365, FR-005) to disambiguate
/// `applied_seq == 0`, which is otherwise ambiguous between "nothing applied" and "WAL line 0
/// applied". `&&`-short-circuits so the common non-empty case costs a single query.
pub(crate) fn group_has_no_content(
    conn: &crate::db::Conn<'_>,
    group_id: &str,
) -> Result<bool, Error> {
    let gids = [group_id];
    Ok(conn.count_episodics_by_group_ids(&gids)? == 0
        && conn.count_entities_by_group_ids(&gids)? == 0
        && conn.count_relates_to_by_group_ids(&gids)? == 0)
}

// ── Full recovery sequence ────────────────────────────────────────────────────

/// Executes the 4-step WAL-corruption self-recovery sequence synchronously.
///
/// Intended to be called from `tokio::task::spawn_blocking`. Returns the recovered
/// `Db` and a `RecoveryReport` describing what was done.
///
/// Steps:
/// 1. Rename torn WAL aside, reopen `liminis.db` at its last checkpoint.
///    On failure → fall back to full `rebuild_from_workspace_wal`.
/// 2. Derive episode-cursor (`from_seq`) from the last episode in the DB.
/// 3. Drop FTS indexes, replay WAL mutations at `seq >= from_seq`, rebuild indexes.
/// 4. Return recovered `Db` with report.
///
/// `wal_root` is the WAL **root** (not one group's own directory). The non-fallback
/// checkpoint-drop path (step 1 succeeds) only ever needs `group_id`'s own subdirectory —
/// today always the default group, matching the startup backfill's scope — since that path
/// never deletes the DB, only catches up a possible tail. The fallback path (`full_rebuild`,
/// triggered when checkpoint-drop itself fails) **deletes the entire embedded DB file**, which
/// holds every group's data, not just `group_id`'s — so replaying only `group_id`'s directory
/// back in would silently drop every other group's data from the live graph (issue #378: FR-009
/// requires this recovery path stay additive to the multi-group case, not a de facto
/// single-group-only tool the moment a second group exists). The fallback branch therefore
/// replays **every** group directory found under `wal_root` via `wal_group::list_group_wal_dirs`
/// and persists each group's own `applied_seq`. No group's on-disk WAL files are ever deleted by
/// this function, so a group's data remains recoverable via an explicit
/// `knowledge_rebuild_from_wal` even if this function's own bookkeeping missed it — but silently
/// vanishing from the live, queryable graph on an autonomous self-heal is a correctness bug this
/// function must not have.
/// `embedder_ctx` (issue #526: mandatory, not `Option`) enables recompute-on-replay (FR-001) for
/// both the checkpoint-drop and full-rebuild paths below: every replayed row's recognized
/// embedding vector param is recomputed from its co-located source text, and never bound from a
/// value stored in the WAL (FR-002). The resulting model identity is persisted alongside each
/// group's `applied_seq` (FR-007, issue #440). This function is intended to run inside
/// `tokio::task::spawn_blocking`, so the sync bridge uses `Handle::current().block_on` rather
/// than building its own runtime.
pub fn run_full_recovery_sequence(
    db_path: &str,
    group_id: &str,
    wal_root: &Path,
    embedding_dim: usize,
    sink: Arc<dyn TelemetrySink>,
    embedder_ctx: EmbedderContext,
) -> Result<(Db, RecoveryReport), Error> {
    let wal_dir = crate::wal_group::group_wal_dir(wal_root, group_id)?;
    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "corruption_detected".to_string(),
        from_seq: None,
        cursor_reason: None,
        mutations_replayed: None,
        elapsed_ms: None,
        fallback_reason: None,
    });

    // ── Step 1: checkpoint-drop (rename WAL aside, reopen DB) ────────────────
    let drop_started = Instant::now();
    let (db, used_fallback, fallback_reason) =
        match attempt_checkpoint_drop(db_path, &wal_dir, embedding_dim, &sink) {
            Ok(db) => (db, false, None),
            Err(e) => {
                let reason = format!("drop_lbug_wal failed: {e}");
                sink.emit(TelemetryEvent::WalAutoRecovery {
                    ts_ms: now_ms(),
                    phase: "fallback_triggered".to_string(),
                    from_seq: None,
                    cursor_reason: None,
                    mutations_replayed: None,
                    elapsed_ms: None,
                    fallback_reason: Some(reason.clone()),
                });
                eprintln!(
                "liminis-context-graph: startup recovery: {reason}, falling back to full rebuild"
            );
                let fallback_db = full_rebuild(db_path, embedding_dim)?;
                (fallback_db, true, Some(reason))
            }
        };
    let drop_elapsed_ms = drop_started.elapsed().as_millis() as u64;

    if !used_fallback {
        sink.emit(TelemetryEvent::WalAutoRecovery {
            ts_ms: now_ms(),
            phase: "checkpoint_drop_complete".to_string(),
            from_seq: None,
            cursor_reason: None,
            mutations_replayed: None,
            elapsed_ms: Some(drop_elapsed_ms),
            fallback_reason: None,
        });
    }

    // ── Step 2: episode-cursor derivation ────────────────────────────────────
    // Only meaningful for the non-fallback path: the fallback wipes the DB entirely and
    // replays every group from scratch (step 3 below), so there is no single group's cursor
    // to derive here.
    let (from_seq, cursor_reason) = if used_fallback {
        // Fresh DB — no episodes, replay everything
        (0u64, CursorReason::NoEpisodes)
    } else {
        let conn = db.connect()?;
        let (seq, reason) = derive_episode_cursor(&conn, group_id, &wal_dir)?;
        drop(conn);
        sink.emit(TelemetryEvent::WalAutoRecovery {
            ts_ms: now_ms(),
            phase: "cursor_derived".to_string(),
            from_seq: Some(seq),
            cursor_reason: Some(reason.as_str().to_string()),
            mutations_replayed: None,
            elapsed_ms: None,
            fallback_reason: fallback_reason.clone(),
        });
        (seq, reason)
    };

    // Count episodes before replay (for the report)
    let episodes_before = {
        let conn = db.connect()?;
        conn.count_nodes("Episodic").unwrap_or(0)
    };

    // ── Step 3: drop FTS, replay WAL mutations (position(s) persisted only after Step 4) ────
    // Positions are collected here but deliberately NOT written yet — see Step 4's comment for
    // why persisting before the index rebuild succeeds would defeat the `null` = "unknown, needs
    // full rebuild" safety invariant the rest of this module relies on.
    // Positions gain a third element, the embedding identity to persist alongside
    // seq/generation (issue #440, FR-007).
    let embedding_identity = embedder_ctx.identity();
    let replay_started = Instant::now();
    let (mutations_replayed, positions_to_persist) = if used_fallback {
        // The DB was just wiped in its entirety — restore every group's data, not only
        // `group_id`'s.
        let conn = db.connect()?;
        schema::drop_fts_indexes(&conn);
        let mut total = 0u64;
        let mut positions = Vec::new();
        for (gid, dir) in crate::wal_group::list_group_wal_dirs(wal_root)? {
            let recompute_embed_fn = embedder_ctx.recompute_fn_via_handle();
            let group_stats = WalReplayer::new(&dir).replay_opts(
                &conn,
                ReplayOptions {
                    from_seq: 0,
                    ..ReplayOptions::new(recompute_embed_fn, embedding_dim)
                },
            )?;
            total += group_stats.lines_replayed;
            if let Some(seq) = group_stats.last_committed_seq {
                let generation = crate::wal_generation::read_generation(&dir);
                positions.push((
                    gid,
                    seq,
                    generation,
                    group_stats.embeddings_recompute_had_no_failures(),
                ));
            }
        }
        (total, positions)
    } else {
        let recompute_embed_fn = embedder_ctx.recompute_fn_via_handle();
        let conn = db.connect()?;
        schema::drop_fts_indexes(&conn);
        let stats = WalReplayer::new(&wal_dir).replay_opts(
            &conn,
            ReplayOptions {
                from_seq,
                ..ReplayOptions::new(recompute_embed_fn, embedding_dim)
            },
        )?;
        let positions = stats
            .last_committed_seq
            .map(|seq| {
                let generation = crate::wal_generation::read_generation(&wal_dir);
                vec![(
                    group_id.to_string(),
                    seq,
                    generation,
                    stats.embeddings_recompute_had_no_failures(),
                )]
            })
            .unwrap_or_default();
        (stats.lines_replayed, positions)
    };
    let replay_elapsed_ms = replay_started.elapsed().as_millis() as u64;

    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "replay_complete".to_string(),
        from_seq: Some(from_seq),
        cursor_reason: Some(cursor_reason.as_str().to_string()),
        mutations_replayed: Some(mutations_replayed),
        elapsed_ms: Some(replay_elapsed_ms),
        fallback_reason: fallback_reason.clone(),
    });

    // ── Step 4: rebuild FTS + HNSW indexes, and backfill Entity.lookup_key ────
    {
        let conn = db.connect()?;
        // A pre-#470 WAL recording's Entity CREATE never mentions summary_embedding, so
        // replaying it verbatim in Step 3 leaves that column NULL — must zero-fill before
        // build_indices_and_constraints below ever builds entity_summary_embedding_idx over the
        // column (issue #470).
        if let Err(e) = schema::zero_fill_null_entity_summary_embeddings(&conn, embedding_dim) {
            eprintln!("liminis-context-graph: run_full_recovery_sequence: zero-fill Entity.summary_embedding failed (non-fatal): {e}");
        }
        // WAL replay above bypassed insert_entity/update_entity_created_at — every replayed
        // row's lookup_key is NULL — so backfill it before build_indices_and_constraints below
        // ever builds entity_lookup_key_idx over the column (issue #221 FR-006). Persists the
        // outcome to SchemaState too, not just the in-process flag.
        schema::backfill_entity_lookup_keys_and_record_status(&conn);
        conn.build_indices_and_constraints()?;
        // Persist the applied-WAL-seq position(s) (issue #353) — a deliberate extension beyond
        // FR-004's literal text (which names knowledge_rebuild_from_wal), since this autonomous
        // WAL-corruption self-heal produces an equally-precise ReplayStats via the same replay
        // path. Deliberately placed *after* the index rebuild above succeeds (both calls are
        // fatal via `?`, so this line is unreached on failure): persisting a "known" position
        // for data whose indexes were never confirmed rebuilt would silently defeat the `null` =
        // "unknown, needs full rebuild" safety invariant `backfill_applied_seq_if_absent`/
        // `knowledge_status` rely on elsewhere — a caller reading a non-null `applied_seq` after
        // a failed rebuild would wrongly skip re-deriving it. Each write is independently
        // non-fatal: a missed write doesn't undo the recovery that already succeeded.
        for (gid, seq, generation, fully_recomputed) in &positions_to_persist {
            // issue #440 FR-006/FR-008: only claim the running embedder's identity for a group
            // whose replay had no failed recompute attempt — see
            // `ReplayStats::embeddings_recompute_had_no_failures`'s doc comment.
            let group_embedding_identity =
                fully_recomputed.then(|| (embedding_identity.0.as_str(), embedding_identity.1));
            if let Err(e) =
                conn.set_wal_position(gid, *seq, generation.as_deref(), group_embedding_identity)
            {
                eprintln!(
                    "liminis-context-graph: startup recovery: failed to persist applied_seq={seq} for group {gid:?} (non-fatal): {e}"
                );
            }
        }
    }

    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "index_build_complete".to_string(),
        from_seq: Some(from_seq),
        cursor_reason: Some(cursor_reason.as_str().to_string()),
        mutations_replayed: Some(mutations_replayed),
        elapsed_ms: None,
        fallback_reason: fallback_reason.clone(),
    });

    let episodes_after = {
        let conn = db.connect()?;
        conn.count_nodes("Episodic").unwrap_or(0)
    };

    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "recovery_complete".to_string(),
        from_seq: Some(from_seq),
        cursor_reason: Some(cursor_reason.as_str().to_string()),
        mutations_replayed: Some(mutations_replayed),
        elapsed_ms: Some(drop_elapsed_ms + replay_elapsed_ms),
        fallback_reason,
    });

    Ok((
        db,
        RecoveryReport {
            episodes_before,
            mutations_replayed,
            episodes_after,
            indexes_rebuilt: true,
            from_seq,
            cursor_reason,
            drop_elapsed_ms,
        },
    ))
}

/// Step 1 happy path: rename torn WAL aside, reopen DB at last checkpoint, init schema.
fn attempt_checkpoint_drop(
    db_path: &str,
    _wal_dir: &Path,
    embedding_dim: usize,
    _sink: &Arc<dyn TelemetrySink>,
) -> Result<Db, Error> {
    let wal_path = format!("{}.wal", db_path);
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let corrupt_path = format!("{}.wal.corrupt-{}", db_path, ts);

    if std::path::Path::new(&wal_path).exists() {
        std::fs::rename(&wal_path, &corrupt_path)?;
    }

    let db = Db::open(db_path)?;
    {
        let conn = db.connect()?;
        conn.init_schema(embedding_dim)?;
    }
    Ok(db)
}

/// Fallback: delete all DB files and replay the full WAL from scratch.
fn full_rebuild(db_path: &str, embedding_dim: usize) -> Result<Db, Error> {
    let path = std::path::Path::new(db_path);
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    for ext in &[".wal", ".lock"] {
        let _ = std::fs::remove_file(format!("{}{}", db_path, ext));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let db = Db::open(db_path)?;
    {
        let conn = db.connect()?;
        conn.init_schema(embedding_dim)?;
    }
    Ok(db)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use crate::db::Db;

    fn make_db_with_schema(dir: &tempfile::TempDir) -> Db {
        let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
        let db = Db::open(&db_path).unwrap();
        {
            let conn = db.connect().unwrap();
            conn.init_schema(4).unwrap();
        }
        db
    }

    fn write_wal_line(wal_dir: &Path, filename: &str, seq: u64, uuid: &str) {
        let content = format!(
            "{{\"seq\":{seq},\"ts\":\"2026-01-01T00:00:00Z\",\"db\":\"test\",\
             \"cypher\":\"CREATE (:Episodic {{uuid: $uuid}})\",\
             \"params\":{{\"uuid\":\"{uuid}\"}}}}\n"
        );
        let path = wal_dir.join(filename);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn write_wal_mentions_line(wal_dir: &Path, filename: &str, seq: u64, ep_uuid: &str) {
        let content = format!(
            "{{\"seq\":{seq},\"ts\":\"2026-01-01T00:00:00Z\",\"db\":\"test\",\
             \"cypher\":\"MERGE (:MENTIONS {{ep: $ep}})\",\
             \"params\":{{\"ep\":\"{ep_uuid}\"}}}}\n"
        );
        let path = wal_dir.join(filename);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // FR-012 (d): empty DB → from_seq = 0, CursorReason::NoEpisodes
    #[test]
    fn cursor_no_episodes() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        let conn = db.connect().unwrap();
        let (seq, reason) = derive_episode_cursor(&conn, "g", &wal_dir).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(reason, CursorReason::NoEpisodes);
    }

    // FR-012 (e): episode uuid not found in WAL → from_seq = 0, CursorReason::UuidNotFound
    #[test]
    fn cursor_uuid_not_found_in_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        // Insert an episode directly into DB (bypassing WAL, so uuid won't appear in WAL)
        {
            let conn = db.connect().unwrap();
            conn.raw_query(
                "CREATE (:Episodic {uuid: 'ep-not-in-wal', name: 'Test', group_id: 'g', \
                 created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')})",
            )
            .unwrap();
        }
        // Write a WAL file with a different uuid
        write_wal_line(&wal_dir, "0001.jsonl", 5, "ep-different-uuid");

        let conn = db.connect().unwrap();
        let (seq, reason) = derive_episode_cursor(&conn, "g", &wal_dir).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(reason, CursorReason::UuidNotFound);
    }

    // FR-012 (c): DB with one episode and matching WAL file → returns correct seq
    #[test]
    fn cursor_uuid_match_returns_seq() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let ep_uuid = "ep-abc-123";
        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            conn.raw_query(&format!(
                "CREATE (:Episodic {{uuid: '{ep_uuid}', name: 'Test', group_id: 'g', \
                 created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')}})"
            ))
            .unwrap();
        }
        // CREATE at seq 10, MENTIONS at seq 15
        write_wal_line(&wal_dir, "0001.jsonl", 10, ep_uuid);
        write_wal_mentions_line(&wal_dir, "0001.jsonl", 15, ep_uuid);

        let conn = db.connect().unwrap();
        let (seq, reason) = derive_episode_cursor(&conn, "g", &wal_dir).unwrap();
        // Must take the minimum seq across all matches
        assert_eq!(seq, 10);
        assert_eq!(reason, CursorReason::UuidMatch);
    }

    // Minimum seq across multiple files
    #[test]
    fn cursor_takes_minimum_seq_across_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let ep_uuid = "ep-multi-file";
        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            conn.raw_query(&format!(
                "CREATE (:Episodic {{uuid: '{ep_uuid}', name: 'Test', group_id: 'g', \
                 created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')}})"
            ))
            .unwrap();
        }
        write_wal_mentions_line(&wal_dir, "0001.jsonl", 20, ep_uuid);
        write_wal_line(&wal_dir, "0002.jsonl", 7, ep_uuid);

        let conn = db.connect().unwrap();
        let (seq, reason) = derive_episode_cursor(&conn, "g", &wal_dir).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(reason, CursorReason::UuidMatch);
    }

    // ── backfill_applied_seq_if_absent (issue #353) ─────────────────────────────

    /// Fresh DB (no episodes) → backfill writes `0` directly, without touching the WAL dir at
    /// all (a nonexistent wal_dir must not cause an error, since the episode-count guard short
    /// -circuits before any WAL scan).
    #[test]
    fn backfill_fresh_db_writes_zero_without_scanning_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal-does-not-exist");
        let db = make_db_with_schema(&dir);
        let conn = db.connect().unwrap();

        backfill_applied_seq_if_absent(&conn, "g", &wal_dir).unwrap();

        assert_eq!(conn.get_wal_position("g").unwrap().applied_seq, Some(0));
    }

    /// A DB with zero `Episodic` nodes but a surviving `Entity` (e.g. every episode was
    /// deleted via `remove_episode`, which only `DETACH DELETE`s the `Episodic` node, never
    /// the entities it created — db.rs) must NOT be treated as "genuinely fresh." Backfilling
    /// to `0` here would falsely claim "known, nothing applied" for a graph that actually has
    /// unrecorded content; `null` (backfill declined) is the correct, safe report.
    #[test]
    fn backfill_entity_survives_episode_deletion_is_not_treated_as_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal-does-not-exist");
        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            conn.raw_query("CREATE (:Entity {uuid: 'orphaned-entity', group_id: 'g'})")
                .unwrap();
        }
        let conn = db.connect().unwrap();

        backfill_applied_seq_if_absent(&conn, "g", &wal_dir).unwrap();

        assert_eq!(
            conn.get_wal_position("g").unwrap().applied_seq,
            None,
            "an Entity-only DB (no episodes) must not be backfilled to 0 — its position is \
             genuinely unknown, not known-empty"
        );
    }

    /// SC-005: a populated DB with no persisted applied-seq record backfills to the episode
    /// cursor's seq, derived via ADR-0026's retroactive mechanism — not `null`.
    #[test]
    fn backfill_populated_db_uuid_match_writes_cursor_seq() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let ep_uuid = "ep-backfill-match";
        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            conn.raw_query(&format!(
                "CREATE (:Episodic {{uuid: '{ep_uuid}', name: 'Test', group_id: 'g', \
                 created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')}})"
            ))
            .unwrap();
        }
        write_wal_line(&wal_dir, "0001.jsonl", 41, ep_uuid);

        let conn = db.connect().unwrap();
        backfill_applied_seq_if_absent(&conn, "g", &wal_dir).unwrap();

        assert_eq!(conn.get_wal_position("g").unwrap().applied_seq, Some(41));
    }

    /// SC-007: a populated DB whose last episode's uuid is absent from the WAL leaves
    /// `applied_seq` as `null` (row absent) — the one case where `null` remains correct,
    /// documented action being a full rebuild.
    #[test]
    fn backfill_populated_db_uuid_not_found_leaves_null() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            conn.raw_query(
                "CREATE (:Episodic {uuid: 'ep-backfill-not-found', name: 'Test', \
                 group_id: 'g', created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')})",
            )
            .unwrap();
        }
        write_wal_line(&wal_dir, "0001.jsonl", 5, "ep-completely-different");

        let conn = db.connect().unwrap();
        backfill_applied_seq_if_absent(&conn, "g", &wal_dir).unwrap();

        assert_eq!(conn.get_wal_position("g").unwrap().applied_seq, None);
    }

    /// A DB that already has a persisted position must not be overwritten by backfill —
    /// idempotent, and doesn't relitigate a value that may have advanced since.
    #[test]
    fn backfill_is_a_noop_when_row_already_present() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        let conn = db.connect().unwrap();
        conn.set_wal_position("g", 99, None, None).unwrap();

        backfill_applied_seq_if_absent(&conn, "g", &wal_dir).unwrap();

        assert_eq!(conn.get_wal_position("g").unwrap().applied_seq, Some(99));
    }

    /// FR-010 (issue #378): in a multi-group database, backfilling group A must never derive
    /// its position from group B's episode, even when B's episode is the more recently created
    /// one. Group A here is "populated but episode-less" (an Entity of its own, but no episode)
    /// — the same ambiguous state `backfill_entity_survives_episode_deletion_is_not_treated_as_fresh`
    /// covers in the single-group case — so its backfill must degrade to `null`, not silently
    /// borrow B's cursor (which an unscoped `count_episodics_by_group_ids`/`derive_episode_cursor`
    /// would do, since B's episode is the only one in the database).
    #[test]
    fn backfill_does_not_borrow_a_different_groups_episode() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            // Group A has an Entity of its own but no episode — populated, not genuinely fresh.
            conn.raw_query(
                "CREATE (:Entity {uuid: 'entity-group-a', name: 'A', group_id: 'group-a', \
                 labels: ['Entity'], created_at: timestamp('2026-01-01'), \
                 name_embedding: [1.0, 0.0, 0.0, 0.0], summary: '', attributes: '{}'})",
            )
            .unwrap();
            // Only group B has an episode.
            conn.raw_query(
                "CREATE (:Episodic {uuid: 'ep-group-b', name: 'Test', group_id: 'group-b', \
                 created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')})",
            )
            .unwrap();
        }
        write_wal_line(&wal_dir, "0001.jsonl", 7, "ep-group-b");

        let conn = db.connect().unwrap();
        backfill_applied_seq_if_absent(&conn, "group-a", &wal_dir).unwrap();
        backfill_applied_seq_if_absent(&conn, "group-b", &wal_dir).unwrap();

        assert_eq!(
            conn.get_wal_position("group-a").unwrap().applied_seq,
            None,
            "group-a has no episodes of its own and must not borrow group-b's cursor"
        );
        assert_eq!(
            conn.get_wal_position("group-b").unwrap().applied_seq,
            Some(7),
            "group-b's own backfill must still derive its own cursor correctly"
        );
    }

    /// FR-010: `derive_episode_cursor` itself, called directly for group A, must not pick up
    /// group B's more-recently-created episode uuid.
    #[test]
    fn derive_episode_cursor_is_scoped_to_its_own_group() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        {
            let conn = db.connect().unwrap();
            conn.raw_query(
                "CREATE (:Episodic {uuid: 'ep-a', name: 'A', group_id: 'group-a', \
                 created_at: timestamp('2026-01-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-01-01')})",
            )
            .unwrap();
            // Created later than group-a's episode, so an unscoped lookup would prefer this one.
            conn.raw_query(
                "CREATE (:Episodic {uuid: 'ep-b', name: 'B', group_id: 'group-b', \
                 created_at: timestamp('2026-06-01'), source: 'text', \
                 source_description: '', content: 'test', valid_at: timestamp('2026-06-01')})",
            )
            .unwrap();
        }
        write_wal_line(&wal_dir, "0001.jsonl", 3, "ep-a");
        write_wal_line(&wal_dir, "0001.jsonl", 9, "ep-b");

        let conn = db.connect().unwrap();
        let (seq, reason) = derive_episode_cursor(&conn, "group-a", &wal_dir).unwrap();
        assert_eq!(
            seq, 3,
            "must derive group-a's own episode's seq, not group-b's later one"
        );
        assert_eq!(reason, CursorReason::UuidMatch);
    }
}
