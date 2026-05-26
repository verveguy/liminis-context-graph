use std::fmt;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use liminis_graph_core::{
    db::Db,
    telemetry::{now_ms, TelemetryEvent, TelemetrySink},
};
use serde_json::json;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum MigrationOutcome {
    /// Migration ran and completed successfully.
    Migrated,
    /// `.lcg/` already exists and `.graphiti/` does not — no work needed.
    AlreadyMigrated,
    /// Neither directory exists — fresh workspace, skip migration.
    NothingToMigrate,
}

#[derive(Debug)]
pub enum MigrationError {
    /// Both `.graphiti/` and `.lcg/` exist without the partial-migration marker.
    /// The binary cannot determine which is canonical. User must resolve manually.
    Schism { guidance: String },
    /// A file rename/create operation failed.
    MoveFile { path: PathBuf, source: io::Error },
    /// The migrated DB could not be opened for validation (FR-005).
    /// Source is the string representation to avoid Send+Sync issues with lbug::Error.
    DbValidation { path: PathBuf, reason: String },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationError::Schism { guidance } => write!(f, "workspace schism: {guidance}"),
            MigrationError::MoveFile { path, source } => {
                write!(f, "failed to move {}: {source}", path.display())
            }
            MigrationError::DbValidation { path, reason } => {
                write!(f, "DB validation failed for {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MigrationError::MoveFile { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Migrate workspace from legacy `.graphiti/` layout to new `.lcg/` layout.
///
/// State machine:
/// - Neither exists → `NothingToMigrate`
/// - Only `.lcg/` exists, correct layout → `AlreadyMigrated`
/// - Only `.lcg/` exists, `.lcg/db` is a file (old simple-rename) → fix layout → `Migrated`
/// - Only `.graphiti/` exists → clean migration
/// - Both exist, `.lcg/db/liminis.db` present → partial-resume migration
/// - Both exist, `.lcg/db/liminis.db` absent → `Schism` error (user must resolve)
///
/// See ADR-0005 for the partial-migration-marker invariant.
pub fn migrate_workspace(
    workspace: &Path,
    sink: &dyn TelemetrySink,
) -> Result<MigrationOutcome, MigrationError> {
    let legacy = workspace.join(".graphiti");
    let new_dir = workspace.join(".lcg");
    // The DB file at this path is the partial-migration marker (ADR-0005).
    let partial_marker = new_dir.join("db").join("liminis.db");

    let legacy_exists = legacy.exists();
    let new_exists = new_dir.exists();

    match (legacy_exists, new_exists) {
        (false, false) => return Ok(MigrationOutcome::NothingToMigrate),
        (false, true) => {
            // `.lcg/` exists and `.graphiti/` is gone. This is the normal "already migrated"
            // case — but the old simple-rename migration (#64) could have left `.lcg/db` as a
            // file instead of a directory. Detect and fix that broken layout in-place.
            let lcg_db = new_dir.join("db");
            if lcg_db.exists() && !lcg_db.is_dir() {
                return fix_broken_lcg_layout(&new_dir, &partial_marker, sink);
            }
            return Ok(MigrationOutcome::AlreadyMigrated);
        }
        (true, true) => {
            if !partial_marker.exists() {
                let guidance = format!(
                    "Both '{}' and '{}' exist without a partial-migration marker \
                     ('{}' absent). The binary cannot determine which directory is canonical. \
                     Manually move one aside (e.g., rename '{}' to '{}.bak') and restart. \
                     WARNING: Do not delete either directory until you have verified your data.",
                    legacy.display(),
                    new_dir.display(),
                    partial_marker.display(),
                    legacy.display(),
                    legacy.display(),
                );
                sink.emit(TelemetryEvent::WorkspaceMigration {
                    ts_ms: now_ms(),
                    phase: "schism".to_string(),
                    detail: Some(json!({ "guidance": guidance })),
                });
                return Err(MigrationError::Schism { guidance });
            }
            // Partial migration: `.lcg/db/liminis.db` exists, complete remaining steps.
            sink.emit(TelemetryEvent::WorkspaceMigration {
                ts_ms: now_ms(),
                phase: "resume_partial".to_string(),
                detail: Some(json!({
                    "from": legacy.display().to_string(),
                    "to": new_dir.display().to_string(),
                })),
            });
        }
        (true, false) => {
            // Clean migration — falls through to migration steps below.
        }
    }

    let file_count = count_entries(&legacy);
    sink.emit(TelemetryEvent::WorkspaceMigration {
        ts_ms: now_ms(),
        phase: "started".to_string(),
        detail: Some(json!({
            "from": legacy.display().to_string(),
            "to": new_dir.display().to_string(),
            "files_to_migrate": file_count,
        })),
    });

    // Step 1: Create .lcg/db/ directory.
    let new_db_dir = new_dir.join("db");
    if !new_db_dir.exists() {
        std::fs::create_dir_all(&new_db_dir).map_err(|e| MigrationError::MoveFile {
            path: new_db_dir.clone(),
            source: e,
        })?;
    }

    // Step 2: Move .graphiti/db (file) → .lcg/db/liminis.db.
    // Skip if destination already exists (partial resume).
    let legacy_db = legacy.join("db");
    if legacy_db.exists() && legacy_db.is_file() && !partial_marker.exists() {
        move_path(&legacy_db, &partial_marker, sink)?;
    }

    // Step 3: Move .graphiti/db.wal → .lcg/db/liminis.db.wal (if present).
    let legacy_db_wal = legacy.join("db.wal");
    let new_db_wal = new_db_dir.join("liminis.db.wal");
    if legacy_db_wal.exists() && !new_db_wal.exists() {
        move_path(&legacy_db_wal, &new_db_wal, sink)?;
    }

    // Step 4: Move .graphiti/wal/ → .lcg/wal/.
    let legacy_wal_dir = legacy.join("wal");
    let new_wal_dir = new_dir.join("wal");
    if legacy_wal_dir.exists() && !new_wal_dir.exists() {
        move_path(&legacy_wal_dir, &new_wal_dir, sink)?;
    }

    // Step 5: Move .graphiti/ontology.yaml → .lcg/ontology.yaml (if present).
    let legacy_ontology = legacy.join("ontology.yaml");
    let new_ontology = new_dir.join("ontology.yaml");
    if legacy_ontology.exists() && !new_ontology.exists() {
        move_path(&legacy_ontology, &new_ontology, sink)?;
    }

    // Step 6: Move .graphiti/ontology-hash.json → .lcg/ontology-hash.json (if present).
    let legacy_hash = legacy.join("ontology-hash.json");
    let new_hash = new_dir.join("ontology-hash.json");
    if legacy_hash.exists() && !new_hash.exists() {
        move_path(&legacy_hash, &new_hash, sink)?;
    }

    // Step 7: Handle unrecognized files — move to .lcg/_unrecognized/<name>.
    // Socket files are skipped (FR-008); they are transient and recreated on start.
    const RECOGNIZED: &[&str] = &[
        "db",
        "db.wal",
        "wal",
        "ontology.yaml",
        "ontology-hash.json",
        "service.sock",
    ];
    if let Ok(entries) = std::fs::read_dir(&legacy) {
        let unrecognized_dir = new_dir.join("_unrecognized");
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            if RECOGNIZED.contains(&name_str.as_ref()) {
                continue;
            }
            #[cfg(unix)]
            {
                // Use entry.file_type() — it reads from the cached d_type in the
                // readdir result on Linux/macOS, avoiding an extra lstat() syscall.
                if entry.file_type().map(|ft| ft.is_socket()).unwrap_or(false) {
                    sink.emit(TelemetryEvent::WorkspaceMigration {
                        ts_ms: now_ms(),
                        phase: "skip_socket".to_string(),
                        detail: Some(json!({ "file": name_str.as_ref() })),
                    });
                    continue;
                }
            }
            if !unrecognized_dir.exists() {
                std::fs::create_dir_all(&unrecognized_dir).map_err(|e| {
                    MigrationError::MoveFile {
                        path: unrecognized_dir.clone(),
                        source: e,
                    }
                })?;
            }
            let dest = unrecognized_dir.join(&file_name);
            sink.emit(TelemetryEvent::WorkspaceMigration {
                ts_ms: now_ms(),
                phase: "unrecognized_file".to_string(),
                detail: Some(json!({
                    "file": name_str.as_ref(),
                    "dest": dest.display().to_string(),
                })),
            });
            move_path(&entry.path(), &dest, sink)?;
        }
    }

    // Step 8: Validate migrated DB can be opened (FR-005).
    // Only runs if a DB file was present; skip for empty-workspace migrations.
    if partial_marker.exists() {
        let db_path_str = partial_marker
            .to_str()
            .ok_or_else(|| MigrationError::DbValidation {
                path: partial_marker.clone(),
                reason: "path is not valid UTF-8".to_string(),
            })?;
        match Db::open(db_path_str) {
            Ok(_db) => {
                sink.emit(TelemetryEvent::WorkspaceMigration {
                    ts_ms: now_ms(),
                    phase: "db_validated".to_string(),
                    detail: Some(json!({ "path": db_path_str })),
                });
            }
            Err(e) => {
                let reason = e.to_string();
                sink.emit(TelemetryEvent::WorkspaceMigration {
                    ts_ms: now_ms(),
                    phase: "db_validation_failed".to_string(),
                    detail: Some(json!({ "path": db_path_str, "error": reason })),
                });
                return Err(MigrationError::DbValidation {
                    path: partial_marker.clone(),
                    reason,
                });
            }
        }
    }

    // Step 9: Remove (or rename to .bak) the now-empty .graphiti/ directory.
    let keep_backup = std::env::var("LCG_MIGRATION_KEEP_BACKUP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if keep_backup {
        let bak = workspace.join(".graphiti.bak");
        // Remove any residual socket from legacy dir before backing up.
        remove_sockets_in_dir(&legacy);
        std::fs::rename(&legacy, &bak).map_err(|e| MigrationError::MoveFile {
            path: legacy.clone(),
            source: e,
        })?;
        sink.emit(TelemetryEvent::WorkspaceMigration {
            ts_ms: now_ms(),
            phase: "backup_created".to_string(),
            detail: Some(json!({ "backup": bak.display().to_string() })),
        });
    } else {
        // Remove residual socket files, then remove the (now empty) directory.
        remove_sockets_in_dir(&legacy);
        std::fs::remove_dir(&legacy).map_err(|e| MigrationError::MoveFile {
            path: legacy.clone(),
            source: e,
        })?;
        sink.emit(TelemetryEvent::WorkspaceMigration {
            ts_ms: now_ms(),
            phase: "legacy_removed".to_string(),
            detail: Some(json!({ "path": legacy.display().to_string() })),
        });
    }

    sink.emit(TelemetryEvent::WorkspaceMigration {
        ts_ms: now_ms(),
        phase: "complete".to_string(),
        detail: Some(json!({
            "from": legacy.display().to_string(),
            "to": new_dir.display().to_string(),
        })),
    });

    Ok(MigrationOutcome::Migrated)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn count_entries(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().count())
        .unwrap_or(0)
}

fn move_path(from: &Path, to: &Path, sink: &dyn TelemetrySink) -> Result<(), MigrationError> {
    std::fs::rename(from, to).map_err(|e| MigrationError::MoveFile {
        path: from.to_path_buf(),
        source: e,
    })?;
    sink.emit(TelemetryEvent::WorkspaceMigration {
        ts_ms: now_ms(),
        phase: "step_moved".to_string(),
        detail: Some(json!({
            "from": from.display().to_string(),
            "to": to.display().to_string(),
        })),
    });
    Ok(())
}

/// Fix `.lcg/` that has the old layout left by the simple-rename migration (#64).
///
/// The old migration did `rename(".graphiti", ".lcg")`, leaving `.lcg/db` as a flat file
/// and `.lcg/db.wal` as a sibling file — neither inside a `db/` subdirectory. The new
/// binary defaults expect `.lcg/db/liminis.db`, so `create_dir_all(".lcg/db")` fails with
/// EEXIST. This function restructures the layout in-place without touching `.graphiti/`.
fn fix_broken_lcg_layout(
    new_dir: &Path,
    partial_marker: &Path,
    sink: &dyn TelemetrySink,
) -> Result<MigrationOutcome, MigrationError> {
    sink.emit(TelemetryEvent::WorkspaceMigration {
        ts_ms: now_ms(),
        phase: "lcg_layout_fix_started".to_string(),
        detail: Some(json!({ "dir": new_dir.display().to_string() })),
    });

    // Use a temp filename in the same directory to keep the rename on the same filesystem.
    // `lcg_db` is `.lcg/db` — a FILE before step A, a DIRECTORY after step B.
    let lcg_db = new_dir.join("db");
    let tmp_db = new_dir.join("_db_migrate_tmp");

    // Step A: Move .lcg/db (file) → .lcg/_db_migrate_tmp.
    std::fs::rename(&lcg_db, &tmp_db).map_err(|e| MigrationError::MoveFile {
        path: lcg_db.clone(),
        source: e,
    })?;

    // Step B: Create .lcg/db/ (directory) at the now-freed path.
    std::fs::create_dir(&lcg_db).map_err(|e| MigrationError::MoveFile {
        path: lcg_db.clone(),
        source: e,
    })?;

    // Step C: Move .lcg/_db_migrate_tmp → .lcg/db/liminis.db.
    std::fs::rename(&tmp_db, partial_marker).map_err(|e| MigrationError::MoveFile {
        path: tmp_db.clone(),
        source: e,
    })?;
    sink.emit(TelemetryEvent::WorkspaceMigration {
        ts_ms: now_ms(),
        phase: "step_moved".to_string(),
        detail: Some(json!({
            "from": lcg_db.display().to_string(),
            "to": partial_marker.display().to_string(),
        })),
    });

    // Step D: Move .lcg/db.wal → .lcg/db/liminis.db.wal (if present).
    let lcg_db_wal = new_dir.join("db.wal");
    let new_db_wal = lcg_db.join("liminis.db.wal");
    if lcg_db_wal.exists() && !new_db_wal.exists() {
        move_path(&lcg_db_wal, &new_db_wal, sink)?;
    }

    // Step E: Validate migrated DB.
    if partial_marker.exists() {
        let db_path_str = partial_marker
            .to_str()
            .ok_or_else(|| MigrationError::DbValidation {
                path: partial_marker.to_path_buf(),
                reason: "path is not valid UTF-8".to_string(),
            })?;
        match Db::open(db_path_str) {
            Ok(_db) => {
                sink.emit(TelemetryEvent::WorkspaceMigration {
                    ts_ms: now_ms(),
                    phase: "db_validated".to_string(),
                    detail: Some(json!({ "path": db_path_str })),
                });
            }
            Err(e) => {
                let reason = e.to_string();
                sink.emit(TelemetryEvent::WorkspaceMigration {
                    ts_ms: now_ms(),
                    phase: "db_validation_failed".to_string(),
                    detail: Some(json!({ "path": db_path_str, "error": reason })),
                });
                return Err(MigrationError::DbValidation {
                    path: partial_marker.to_path_buf(),
                    reason,
                });
            }
        }
    }

    sink.emit(TelemetryEvent::WorkspaceMigration {
        ts_ms: now_ms(),
        phase: "lcg_layout_fix_complete".to_string(),
        detail: Some(json!({ "dir": new_dir.display().to_string() })),
    });

    Ok(MigrationOutcome::Migrated)
}

/// Removes transient files from a directory without failing if they're not there.
///
/// Removes any Unix socket file (transient by nature) and any entry named `service.sock`
/// regardless of file type (handles the edge case where it's a regular file or symlink after
/// an abnormal shutdown). Both are safe to delete — they're recreated on the next bind.
fn remove_sockets_in_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_service_sock = entry.file_name() == "service.sock";
        #[cfg(unix)]
        let is_socket = entry.file_type().map(|ft| ft.is_socket()).unwrap_or(false);
        #[cfg(not(unix))]
        let is_socket = false;
        if is_service_sock || is_socket {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use liminis_graph_core::telemetry::CaptureSink;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serializes `Db::open` across tests in this module. lbug's first-ever
    /// `INSTALL vector` (called inside `Db::open`'s schema init) creates a
    /// per-user extension cache directory at `~/.lbdb/extension/<ver>/<arch>/`.
    /// When multiple tests run in parallel and all hit this for the first
    /// time in the same process, they race on the create — one wins and the
    /// others see EEXIST-ish failures and lbug surfaces them as
    /// "Failed to create directory ... Check if it exists and remove it."
    /// Holding this mutex during `Db::open` in tests forces serialization
    /// of that first-use install. Subsequent opens are fast and harmless.
    static DB_OPEN_LOCK: Mutex<()> = Mutex::new(());

    fn open_db_serialized(path: &str) -> Db {
        let _guard = DB_OPEN_LOCK.lock().expect("DB_OPEN_LOCK poisoned");
        Db::open(path).expect("Db::open failed")
    }

    fn noop() -> CaptureSink {
        CaptureSink::new()
    }

    fn phases(sink: &CaptureSink) -> Vec<String> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                TelemetryEvent::WorkspaceMigration { phase, .. } => Some(phase),
                _ => None,
            })
            .collect()
    }

    // ── SC-004: Idempotency ────────────────────────────────────────────────────

    #[test]
    fn nothing_to_migrate_on_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(matches!(result, Ok(MigrationOutcome::NothingToMigrate)));
        assert!(phases(&sink).is_empty());
    }

    #[test]
    fn already_migrated_when_only_lcg_exists() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".lcg")).unwrap();
        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(matches!(result, Ok(MigrationOutcome::AlreadyMigrated)));
        assert!(phases(&sink).is_empty());
    }

    // ── Broken .lcg/ layout fix (post-simple-rename migration #64) ────────────

    #[test]
    fn fixes_broken_lcg_layout_when_db_is_flat_file() {
        let tmp = TempDir::new().unwrap();
        let new_dir = tmp.path().join(".lcg");
        std::fs::create_dir(&new_dir).unwrap();

        // Simulate the state left by the old simple-rename migration:
        // .lcg/db is a flat file, not a directory.
        let lcg_db_file = new_dir.join("db");
        let db = open_db_serialized(lcg_db_file.to_str().unwrap());
        drop(db);
        assert!(
            lcg_db_file.is_file(),
            "pre-condition: .lcg/db must be a file"
        );

        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(
            matches!(result, Ok(MigrationOutcome::Migrated)),
            "layout fix failed: {result:?}"
        );

        // .lcg/db must now be a directory containing liminis.db
        assert!(
            new_dir.join("db").is_dir(),
            ".lcg/db must be a directory after fix"
        );
        assert!(
            new_dir.join("db").join("liminis.db").is_file(),
            ".lcg/db/liminis.db must exist after fix"
        );

        let p = phases(&sink);
        assert!(
            p.contains(&"lcg_layout_fix_started".to_string()),
            "phases: {p:?}"
        );
        assert!(
            p.contains(&"lcg_layout_fix_complete".to_string()),
            "phases: {p:?}"
        );
        assert!(p.contains(&"db_validated".to_string()), "phases: {p:?}");
    }

    // ── SC-005: Schism detection ───────────────────────────────────────────────

    #[test]
    fn schism_when_both_dirs_exist_without_marker() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".graphiti")).unwrap();
        std::fs::create_dir(tmp.path().join(".lcg")).unwrap();
        // No .lcg/db/liminis.db — schism
        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(matches!(result, Err(MigrationError::Schism { .. })));
        assert!(phases(&sink).contains(&"schism".to_string()));
        // Both directories must still exist (not deleted by the error path)
        assert!(tmp.path().join(".graphiti").exists());
        assert!(tmp.path().join(".lcg").exists());
    }

    // ── FR-002, FR-003: Clean migration with all known files ──────────────────

    #[test]
    fn clean_migration_moves_all_known_files() {
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();

        // Create legacy files (except db which requires lbug)
        let legacy_db_wal = legacy.join("db.wal");
        std::fs::write(&legacy_db_wal, b"wal data").unwrap();

        let legacy_wal_dir = legacy.join("wal");
        std::fs::create_dir(&legacy_wal_dir).unwrap();
        std::fs::write(legacy_wal_dir.join("001.jsonl"), b"{}").unwrap();

        std::fs::write(legacy.join("ontology.yaml"), b"entities:").unwrap();
        std::fs::write(legacy.join("ontology-hash.json"), b"{}").unwrap();

        let sink = noop();
        // Without a db file, validation step is skipped (partial_marker absent).
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(
            matches!(result, Ok(MigrationOutcome::Migrated)),
            "migration failed: {result:?}"
        );

        let new_dir = tmp.path().join(".lcg");
        assert!(new_dir.join("db").is_dir(), ".lcg/db/ must be a directory");
        assert!(
            !new_dir.join("db").join("liminis.db").exists(),
            "no db file in legacy"
        );
        assert!(new_dir.join("db").join("liminis.db.wal").exists());
        assert!(new_dir.join("wal").is_dir());
        assert!(new_dir.join("wal").join("001.jsonl").exists());
        assert!(new_dir.join("ontology.yaml").exists());
        assert!(new_dir.join("ontology-hash.json").exists());

        assert!(
            !legacy.exists(),
            ".graphiti/ must be removed after migration"
        );

        let p = phases(&sink);
        assert!(p.contains(&"started".to_string()));
        assert!(p.contains(&"complete".to_string()));
    }

    // ── FR-002: Unrecognized file handling ────────────────────────────────────

    #[test]
    fn unrecognized_files_move_to_unrecognized_dir() {
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("mystery.txt"), b"unknown").unwrap();

        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(matches!(result, Ok(MigrationOutcome::Migrated)));

        let unrecognized = tmp
            .path()
            .join(".lcg")
            .join("_unrecognized")
            .join("mystery.txt");
        assert!(
            unrecognized.exists(),
            "_unrecognized/mystery.txt must exist"
        );
        assert!(phases(&sink).contains(&"unrecognized_file".to_string()));
    }

    // ── FR-008: Socket file skipping ──────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn socket_file_is_skipped_not_moved() {
        use std::os::unix::net::UnixListener;
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();

        // Create a real Unix socket file in legacy dir
        let sock_path = legacy.join("service.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(matches!(result, Ok(MigrationOutcome::Migrated)));

        // Socket should NOT appear in .lcg/
        assert!(!tmp.path().join(".lcg").join("service.sock").exists());
        // service.sock is in RECOGNIZED list so no unrecognized_file event
        assert!(!phases(&sink).contains(&"skip_socket".to_string()));
    }

    // ── FR-004: Partial resume ─────────────────────────────────────────────────

    #[test]
    fn partial_resume_completes_remaining_steps() {
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();

        // Simulate: db was already moved (partial marker exists), but wal/ wasn't
        let new_dir = tmp.path().join(".lcg");
        std::fs::create_dir_all(new_dir.join("db")).unwrap();
        // Create a real DB at the marker path so validation succeeds
        let marker = new_dir.join("db").join("liminis.db");
        let _db = open_db_serialized(marker.to_str().unwrap());
        drop(_db);

        // Legacy still has wal/ and ontology
        let legacy_wal = legacy.join("wal");
        std::fs::create_dir(&legacy_wal).unwrap();
        std::fs::write(legacy_wal.join("001.jsonl"), b"{}").unwrap();
        std::fs::write(legacy.join("ontology.yaml"), b"entities:").unwrap();

        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(
            matches!(result, Ok(MigrationOutcome::Migrated)),
            "partial resume failed: {result:?}"
        );

        assert!(new_dir.join("wal").is_dir());
        assert!(new_dir.join("ontology.yaml").exists());
        assert!(!legacy.exists());
        assert!(phases(&sink).contains(&"resume_partial".to_string()));
    }

    // ── SC-006: Backup mode ────────────────────────────────────────────────────

    #[test]
    fn backup_mode_renames_legacy_to_bak() {
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("ontology.yaml"), b"entities:").unwrap();

        std::env::set_var("LCG_MIGRATION_KEEP_BACKUP", "1");
        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        std::env::remove_var("LCG_MIGRATION_KEEP_BACKUP");

        assert!(matches!(result, Ok(MigrationOutcome::Migrated)));
        assert!(!tmp.path().join(".graphiti").exists());
        assert!(tmp.path().join(".graphiti.bak").exists());
        assert!(phases(&sink).contains(&"backup_created".to_string()));
    }

    // ── FR-006: Telemetry events ───────────────────────────────────────────────

    #[test]
    fn migration_emits_started_and_complete_events() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".graphiti")).unwrap();

        let sink = noop();
        migrate_workspace(tmp.path(), &sink).unwrap();

        let p = phases(&sink);
        assert!(p.contains(&"started".to_string()), "phases: {p:?}");
        assert!(p.contains(&"complete".to_string()), "phases: {p:?}");
    }

    // ── FR-010 / SC-003: Mid-migration failure leaves source intact ───────────

    #[cfg(unix)]
    #[test]
    fn mid_migration_failure_leaves_source_intact_and_retry_succeeds() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        let new_dir = tmp.path().join(".lcg");

        // Simulate a partially-completed migration: .lcg/db/liminis.db (the partial marker)
        // already exists (step 2 completed), but .graphiti/wal/ and ontology.yaml haven't
        // been moved yet.
        std::fs::create_dir(&legacy).unwrap();
        let legacy_wal = legacy.join("wal");
        std::fs::create_dir(&legacy_wal).unwrap();
        std::fs::write(legacy_wal.join("001.jsonl"), b"{}").unwrap();
        std::fs::write(legacy.join("ontology.yaml"), b"entities:").unwrap();

        std::fs::create_dir_all(new_dir.join("db")).unwrap();
        let marker = new_dir.join("db").join("liminis.db");
        let db = open_db_serialized(marker.to_str().unwrap());
        drop(db);

        // Make .lcg/ non-writable so rename of .graphiti/wal → .lcg/wal fails.
        std::fs::set_permissions(&new_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);

        // Restore permissions before assertions so TempDir can clean up.
        std::fs::set_permissions(&new_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(result, Err(MigrationError::MoveFile { .. })),
            "expected MoveFile error, got: {result:?}"
        );
        // Source must be intact — nothing was deleted by the failed migration.
        assert!(legacy.exists(), ".graphiti/ must still exist after failure");
        assert!(
            legacy_wal.exists(),
            ".graphiti/wal/ must still exist after failure"
        );
        assert!(
            legacy.join("ontology.yaml").exists(),
            ".graphiti/ontology.yaml must still exist after failure"
        );

        // Retry after fixing the permissions — must complete the migration cleanly.
        let sink2 = noop();
        let result2 = migrate_workspace(tmp.path(), &sink2);
        assert!(
            matches!(result2, Ok(MigrationOutcome::Migrated)),
            "retry after permission fix should succeed: {result2:?}"
        );
        assert!(
            !legacy.exists(),
            ".graphiti/ must be gone after successful retry"
        );
        assert!(
            new_dir.join("wal").is_dir(),
            ".lcg/wal/ must exist after retry"
        );
        assert!(
            new_dir.join("ontology.yaml").exists(),
            ".lcg/ontology.yaml must exist after retry"
        );
    }

    // ── DB validation (FR-005) ────────────────────────────────────────────────

    #[test]
    fn clean_migration_with_real_db_validates_and_succeeds() {
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".graphiti");
        std::fs::create_dir(&legacy).unwrap();

        // Create a real lbug DB at the legacy path. We do NOT call init_schema here
        // because lbug's vector extension rewrites internal catalog state on
        // LOAD EXTENSION, and a subsequent Db::open after fs::rename triggers an
        // assertion in the hash-index recovery path. The migration's validation step
        // only calls Db::open (not init_schema), so testing that is sufficient.
        let legacy_db_path = legacy.join("db");
        let db = open_db_serialized(legacy_db_path.to_str().unwrap());
        drop(db);

        let sink = noop();
        let result = migrate_workspace(tmp.path(), &sink);
        assert!(
            matches!(result, Ok(MigrationOutcome::Migrated)),
            "migration with real DB failed: {result:?}"
        );

        let new_db = tmp.path().join(".lcg").join("db").join("liminis.db");
        assert!(new_db.exists(), "migrated DB file must exist");
        // Verify DB is still openable post-migration (without init_schema)
        {
            let _guard = DB_OPEN_LOCK.lock().expect("DB_OPEN_LOCK poisoned");
            assert!(Db::open(new_db.to_str().unwrap()).is_ok());
        }

        let p = phases(&sink);
        assert!(p.contains(&"db_validated".to_string()), "phases: {p:?}");
        assert!(p.contains(&"complete".to_string()), "phases: {p:?}");
    }
}
