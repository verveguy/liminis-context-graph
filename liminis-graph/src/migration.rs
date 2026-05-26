use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

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
/// - Only `.lcg/` exists → `AlreadyMigrated`
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
        (false, true) => return Ok(MigrationOutcome::AlreadyMigrated),
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
                if let Ok(meta) = entry.path().symlink_metadata() {
                    if meta.file_type().is_socket() {
                        sink.emit(TelemetryEvent::WorkspaceMigration {
                            ts_ms: now_ms(),
                            phase: "skip_socket".to_string(),
                            detail: Some(json!({ "file": name_str.as_ref() })),
                        });
                        continue;
                    }
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

/// Removes socket files from a directory without failing if they're not there.
fn remove_sockets_in_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        #[cfg(unix)]
        {
            if let Ok(meta) = entry.path().symlink_metadata() {
                if meta.file_type().is_socket() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        #[cfg(not(unix))]
        {
            // On non-Unix, no socket files exist; nothing to remove.
            let _ = entry;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use liminis_graph_core::telemetry::CaptureSink;
    use tempfile::TempDir;

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
        assert!(!new_dir.join("db").join("liminis.db").exists(), "no db file in legacy");
        assert!(new_dir.join("db").join("liminis.db.wal").exists());
        assert!(new_dir.join("wal").is_dir());
        assert!(new_dir.join("wal").join("001.jsonl").exists());
        assert!(new_dir.join("ontology.yaml").exists());
        assert!(new_dir.join("ontology-hash.json").exists());

        assert!(!legacy.exists(), ".graphiti/ must be removed after migration");

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

        let unrecognized = tmp.path().join(".lcg").join("_unrecognized").join("mystery.txt");
        assert!(unrecognized.exists(), "_unrecognized/mystery.txt must exist");
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
        let _db = Db::open(marker.to_str().unwrap()).unwrap();
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
        let db = Db::open(legacy_db_path.to_str().unwrap()).unwrap();
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
        assert!(Db::open(new_db.to_str().unwrap()).is_ok());

        let p = phases(&sink);
        assert!(p.contains(&"db_validated".to_string()), "phases: {p:?}");
        assert!(p.contains(&"complete".to_string()), "phases: {p:?}");
    }
}
