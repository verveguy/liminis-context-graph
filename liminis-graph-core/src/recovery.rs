use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::{
    db::Db,
    error::Error,
    replay::{ReplayOptions, WalReplayer},
    schema,
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
};

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CursorReason {
    UuidMatch,
    NoEpisodes,
    UuidNotFound,
}

impl CursorReason {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            CursorReason::UuidMatch => "uuid_match",
            CursorReason::NoEpisodes => "no_episodes",
            CursorReason::UuidNotFound => "uuid_not_found",
        }
    }
}

pub(crate) struct RecoveryReport {
    pub(crate) episodes_before: u64,
    pub(crate) mutations_replayed: u64,
    pub(crate) episodes_after: u64,
    pub(crate) indexes_rebuilt: bool,
    pub(crate) from_seq: u64,
    pub(crate) cursor_reason: CursorReason,
    pub(crate) drop_elapsed_ms: u64,
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
pub(crate) fn derive_episode_cursor(
    conn: &crate::db::Conn<'_>,
    wal_dir: &Path,
) -> Result<(u64, CursorReason), Error> {
    let target_uuid = match conn.get_latest_episode_uuid()? {
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
                    "liminis-graph: WAL recovery: skipping unreadable file {:?}",
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

/// Scans `path` line by line for the first occurrence of `target_uuid` in
/// `params["uuid"]` or `params["ep"]`. Returns the minimum `seq` found, or `None`.
fn scan_file_for_uuid(path: &Path, target_uuid: &str) -> Result<Option<u64>, Error> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut min_seq: Option<u64> = None;

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
                min_seq = Some(min_seq.map_or(wal_line.seq, |m: u64| m.min(wal_line.seq)));
            }
        }
    }

    Ok(min_seq)
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
pub(crate) fn run_full_recovery_sequence(
    db_path: &str,
    wal_dir: &Path,
    embedding_dim: usize,
    sink: Arc<dyn TelemetrySink>,
) -> Result<(Db, RecoveryReport), Error> {
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
        match attempt_checkpoint_drop(db_path, wal_dir, embedding_dim, &sink) {
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
                eprintln!("liminis-graph: startup recovery: {reason}, falling back to full rebuild");
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
    let (from_seq, cursor_reason) = if used_fallback {
        // Fresh DB — no episodes, replay everything
        (0u64, CursorReason::NoEpisodes)
    } else {
        let conn = db.connect()?;
        let (seq, reason) = derive_episode_cursor(&conn, wal_dir)?;
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
    let episodes_before: u64 = {
        let conn = db.connect()?;
        conn.count_nodes("Episodic").unwrap_or(0) as u64
    };

    // ── Step 3: drop FTS, replay WAL mutations at seq >= from_seq ────────────
    let replay_started = Instant::now();
    let stats = {
        let conn = db.connect()?;
        schema::drop_fts_indexes(&conn);
        let stats = WalReplayer::new(wal_dir).replay_opts(
            &conn,
            ReplayOptions {
                from_seq,
                ..Default::default()
            },
        )?;
        stats
    };
    let replay_elapsed_ms = replay_started.elapsed().as_millis() as u64;

    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "replay_complete".to_string(),
        from_seq: Some(from_seq),
        cursor_reason: Some(cursor_reason.as_str().to_string()),
        mutations_replayed: Some(stats.lines_replayed),
        elapsed_ms: Some(replay_elapsed_ms),
        fallback_reason: fallback_reason.clone(),
    });

    // ── Step 4: rebuild FTS + HNSW indexes ───────────────────────────────────
    {
        let conn = db.connect()?;
        conn.build_indices_and_constraints()?;
    }

    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "index_build_complete".to_string(),
        from_seq: Some(from_seq),
        cursor_reason: Some(cursor_reason.as_str().to_string()),
        mutations_replayed: Some(stats.lines_replayed),
        elapsed_ms: None,
        fallback_reason: fallback_reason.clone(),
    });

    let episodes_after: u64 = {
        let conn = db.connect()?;
        conn.count_nodes("Episodic").unwrap_or(0) as u64
    };

    sink.emit(TelemetryEvent::WalAutoRecovery {
        ts_ms: now_ms(),
        phase: "recovery_complete".to_string(),
        from_seq: Some(from_seq),
        cursor_reason: Some(cursor_reason.as_str().to_string()),
        mutations_replayed: Some(stats.lines_replayed),
        elapsed_ms: Some(drop_elapsed_ms + replay_elapsed_ms),
        fallback_reason,
    });

    Ok((
        db,
        RecoveryReport {
            episodes_before,
            mutations_replayed: stats.lines_replayed,
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
        let (seq, reason) = derive_episode_cursor(&conn, &wal_dir).unwrap();
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
        let (seq, reason) = derive_episode_cursor(&conn, &wal_dir).unwrap();
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
        let (seq, reason) = derive_episode_cursor(&conn, &wal_dir).unwrap();
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
        let (seq, reason) = derive_episode_cursor(&conn, &wal_dir).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(reason, CursorReason::UuidMatch);
    }
}
