//! WAL checkpoints — named, retained WAL positions stored under `<wal_dir>/.checkpoints/`
//! (issue #365). Pure filesystem I/O: no `Conn`/lbug dependency, so `list`/`delete` work even
//! when the database is degraded or unavailable (FR-011), and `create` is O(1) relative to WAL
//! size (FR-003) — this module never scans or replays `.jsonl` WAL files itself.
//!
//! On-disk layout, one directory per checkpoint name, with generation-numbered files:
//!
//! ```text
//! <wal_dir>/.checkpoints/<name>/
//!     g1.create.json   # {"name","seq","created_at_ms"} — first checkpoint under this name
//!     g1.delete.json   # {"name","deleted_at_ms"} — tombstone for generation 1 (if deleted)
//!     g2.create.json   # a later create() reusing the name — new generation, g1 untouched
//! ```
//!
//! A generation's `create.json` is written via an exclusive create-if-absent open
//! (`OpenOptions::create_new`, POSIX `O_EXCL`), which is atomic across processes on a local
//! filesystem. Two concurrent `create` calls for the same name race to write the same
//! generation file; exactly one wins, and the loser's `AlreadyExists` maps directly to
//! [`Error::CheckpointDuplicateName`] — no retry, satisfying FR-011/FR-012 without any new
//! locking. Deletion never rewrites a generation's `create.json` in place (FR-007); it adds a
//! sibling `delete.json` tombstone for that same generation instead.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::telemetry::now_ms;
use crate::wal::{wal_max_seq, wal_min_seq};

/// Subdirectory of the WAL directory holding the checkpoint store. Invisible to the WAL file
/// scans in `wal.rs`/`replay.rs`/`recovery.rs`/`handlers.rs`, which are all non-recursive and
/// `.jsonl`-extension-filtered (see the issue #365 background for the three sites this placement
/// was verified safe against).
pub const CHECKPOINTS_DIR_NAME: &str = ".checkpoints";

/// A named, retained WAL position, as reported by [`list`]. `seq: None` means "nothing has been
/// applied"; `seq: Some(n)` means "through WAL line `n`, inclusive" (FR-005). `reachable` is a
/// cheap min/max bounds check against the WAL content currently on disk (FR-009) — it does not
/// detect a gap in the middle of that range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub name: String,
    pub seq: Option<u64>,
    pub reachable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateRecord {
    name: String,
    seq: Option<u64>,
    created_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeleteRecord {
    name: String,
    deleted_at_ms: u64,
}

/// Rejects a checkpoint `name` that isn't safe to use as a single filesystem path component —
/// no path separators, `.`/`..`, or other characters that could escape `.checkpoints/<name>/`.
/// Also keeps the store human-inspectable (each name maps to one predictably-named directory),
/// serving Story 5's git-friendliness goal.
pub fn validate_name(name: &str) -> Result<(), Error> {
    let valid = !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(Error::CheckpointInvalidName(name.to_string()))
    }
}

fn checkpoints_dir(wal_dir: &Path) -> PathBuf {
    wal_dir.join(CHECKPOINTS_DIR_NAME)
}

fn name_dir(wal_dir: &Path, name: &str) -> PathBuf {
    checkpoints_dir(wal_dir).join(name)
}

fn create_file_path(dir: &Path, generation: u32) -> PathBuf {
    dir.join(format!("g{generation}.create.json"))
}

fn delete_file_path(dir: &Path, generation: u32) -> PathBuf {
    dir.join(format!("g{generation}.delete.json"))
}

/// The highest generation number present under `dir` (across both create and delete files), or
/// `0` if `dir` doesn't exist or holds no generation files yet.
fn highest_generation(dir: &Path) -> u32 {
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| parse_generation(&e.file_name().to_string_lossy()))
        .max()
        .unwrap_or(0)
}

fn parse_generation(file_name: &str) -> Option<u32> {
    let rest = file_name.strip_prefix('g')?;
    let rest = rest
        .strip_suffix(".create.json")
        .or_else(|| rest.strip_suffix(".delete.json"))?;
    rest.parse().ok()
}

/// Creates a new checkpoint named `name` at `seq`, or fails with
/// [`Error::CheckpointDuplicateName`] if `name` currently identifies an active (non-deleted)
/// checkpoint (FR-006). O(1) relative to WAL size — this function never lists or reads `.jsonl`
/// files (FR-003). `seq` must already be resolved by the caller (`None`/`Some(n)` per FR-005);
/// this module has no database access and does not decide that value itself.
pub fn create(wal_dir: &Path, name: &str, seq: Option<u64>) -> Result<(), Error> {
    validate_name(name)?;
    let dir = name_dir(wal_dir, name);
    fs::create_dir_all(&dir)?;

    let g = highest_generation(&dir);
    let target = if g == 0 {
        1
    } else if delete_file_path(&dir, g).exists() {
        // Prior generation was deleted: the name is inactive and reusable (FR-006's
        // "reuse after deletion" case) — target a fresh generation so g's files are
        // never touched.
        g + 1
    } else {
        // Prior generation is still active (no tombstone): deliberately collide on the
        // same generation file so the exclusive-create below fails with AlreadyExists.
        g
    };

    let record = CreateRecord {
        name: name.to_string(),
        seq,
        created_at_ms: now_ms(),
    };
    let json = serde_json::to_string(&record)?;
    let path = create_file_path(&dir, target);

    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            sync_dir(&dir);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::CheckpointDuplicateName(name.to_string()))
        }
        Err(e) => Err(Error::WalIo(e)),
    }
}

/// Deletes the active checkpoint named `name` by writing a tombstone for its current
/// generation — the generation's `create.json` is never modified or removed (FR-007). Fails
/// with [`Error::CheckpointNotFound`] if `name` does not currently identify an active checkpoint
/// (never created, or already deleted; FR-008).
pub fn delete(wal_dir: &Path, name: &str) -> Result<(), Error> {
    validate_name(name)?;
    let dir = name_dir(wal_dir, name);
    let g = highest_generation(&dir);
    if g == 0 {
        return Err(Error::CheckpointNotFound(name.to_string()));
    }
    let delete_path = delete_file_path(&dir, g);
    if delete_path.exists() {
        return Err(Error::CheckpointNotFound(name.to_string()));
    }

    let record = DeleteRecord {
        name: name.to_string(),
        deleted_at_ms: now_ms(),
    };
    let json = serde_json::to_string(&record)?;

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&delete_path)
    {
        Ok(mut file) => {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            sync_dir(&dir);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A concurrent delete won the race first — same user-facing outcome.
            Err(Error::CheckpointNotFound(name.to_string()))
        }
        Err(e) => Err(Error::WalIo(e)),
    }
}

/// Flushes the containing directory's own entry so a newly linked `gN.*.json` file survives a
/// crash immediately after creation — `File::sync_all` on the file itself flushes its contents
/// and inode, but not the directory entry that links it in, which on some POSIX filesystems can
/// still be lost on a crash between the two. Best-effort: the checkpoint store already tolerates
/// unreadable/missing generation files elsewhere (see `list`'s corrupt-file handling), and a
/// platform where opening a directory for sync fails shouldn't turn a successful create/delete
/// into a hard error over this extra durability margin.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Lists every active (non-deleted) checkpoint, each annotated with whether its `seq` is
/// currently reachable given the WAL content on disk (FR-009): a `seq: None` checkpoint is
/// always reachable (its restore path, `knowledge_clear_all`, never touches the WAL); a
/// `seq: Some(n)` checkpoint is reachable iff `n` falls within `[wal_min_seq, wal_max_seq]` —
/// a cheap bounds check that does not detect a gap in the middle of that range. Returns an empty
/// list, not an error, if the store doesn't exist yet (no checkpoints ever created).
pub fn list(wal_dir: &Path) -> Result<Vec<CheckpointInfo>, Error> {
    let dir = checkpoints_dir(wal_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(Vec::new()),
    };

    let min = wal_min_seq(wal_dir)?;
    let max = wal_max_seq(wal_dir)?;

    let mut results = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name_dir = entry.path();
        let g = highest_generation(&name_dir);
        if g == 0 {
            continue;
        }
        if delete_file_path(&name_dir, g).exists() {
            continue;
        }
        // Corrupt/unreadable create files are tolerated as absent, matching this codebase's
        // WAL-corruption tolerance elsewhere (e.g. `wal::read_last_seq`).
        let Some(record) = read_create_record(&name_dir, g) else {
            continue;
        };
        // Identity is the directory name, not the record's own `name` field: `create`/`delete`
        // both resolve a checkpoint by directory name (`name_dir`), so that's the value that
        // must match for a later `delete` to find what `list` just reported. The two could
        // otherwise diverge — e.g. an operator manually copying a checkpoint's directory to a
        // new name inside a git-tracked WAL directory (Story 5) — leaving the record's internal
        // `name` field stale.
        let Some(name) = name_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let reachable = match record.seq {
            None => true,
            Some(n) => matches!((min, max), (Some(lo), Some(hi)) if lo <= n && n <= hi),
        };
        results.push(CheckpointInfo {
            name,
            seq: record.seq,
            reachable,
        });
    }
    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

fn read_create_record(dir: &Path, generation: u32) -> Option<CreateRecord> {
    let contents = fs::read_to_string(create_file_path(dir, generation)).ok()?;
    serde_json::from_str(&contents).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_alphanumeric_underscore_hyphen() {
        assert!(validate_name("pre-migration").is_ok());
        assert!(validate_name("Chunk_01").is_ok());
        assert!(validate_name(&"a".repeat(200)).is_ok());
    }

    #[test]
    fn validate_name_rejects_empty_and_oversized() {
        assert!(validate_name("").is_err());
        assert!(validate_name(&"a".repeat(201)).is_err());
    }

    #[test]
    fn validate_name_rejects_path_unsafe_characters() {
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a.b").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("a\0b").is_err());
    }

    #[test]
    fn create_then_list_reports_the_new_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), "pre-migration", Some(42)).unwrap();

        let checkpoints = list(tmp.path()).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].name, "pre-migration");
        assert_eq!(checkpoints[0].seq, Some(42));
    }

    #[test]
    fn create_with_none_seq_is_always_reachable() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), "fresh", None).unwrap();

        let checkpoints = list(tmp.path()).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].seq, None);
        assert!(checkpoints[0].reachable);
    }

    #[test]
    fn create_rejects_duplicate_active_name() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), "pre-migration", Some(1)).unwrap();

        let err = create(tmp.path(), "pre-migration", Some(2)).unwrap_err();
        assert!(matches!(err, Error::CheckpointDuplicateName(n) if n == "pre-migration"));

        // The original record must be unmodified.
        let checkpoints = list(tmp.path()).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].seq, Some(1));
    }

    #[test]
    fn create_rejects_invalid_name_without_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let err = create(tmp.path(), "bad name", Some(1)).unwrap_err();
        assert!(matches!(err, Error::CheckpointInvalidName(_)));
        assert!(list(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn delete_removes_from_list_and_never_rewrites_create_record() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), "pre-migration", Some(5)).unwrap();
        let name_d = name_dir(tmp.path(), "pre-migration");
        let create_contents_before = fs::read_to_string(create_file_path(&name_d, 1)).unwrap();

        delete(tmp.path(), "pre-migration").unwrap();

        assert!(list(tmp.path()).unwrap().is_empty());
        let create_contents_after = fs::read_to_string(create_file_path(&name_d, 1)).unwrap();
        assert_eq!(
            create_contents_before, create_contents_after,
            "delete must never rewrite the create record in place"
        );
        assert!(delete_file_path(&name_d, 1).exists());
    }

    #[test]
    fn delete_of_nonexistent_name_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let err = delete(tmp.path(), "never-created").unwrap_err();
        assert!(matches!(err, Error::CheckpointNotFound(n) if n == "never-created"));
    }

    #[test]
    fn delete_of_already_deleted_name_fails() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), "pre-migration", Some(1)).unwrap();
        delete(tmp.path(), "pre-migration").unwrap();

        let err = delete(tmp.path(), "pre-migration").unwrap_err();
        assert!(matches!(err, Error::CheckpointNotFound(n) if n == "pre-migration"));
    }

    #[test]
    fn create_after_delete_reuses_the_name_as_a_new_generation() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), "pre-migration", Some(1)).unwrap();
        delete(tmp.path(), "pre-migration").unwrap();

        create(tmp.path(), "pre-migration", Some(99)).unwrap();

        let checkpoints = list(tmp.path()).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].seq, Some(99));

        let name_d = name_dir(tmp.path(), "pre-migration");
        assert!(create_file_path(&name_d, 1).exists());
        assert!(delete_file_path(&name_d, 1).exists());
        assert!(create_file_path(&name_d, 2).exists());
        assert!(!delete_file_path(&name_d, 2).exists());
    }

    #[test]
    fn list_on_empty_store_returns_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(list(tmp.path()).unwrap(), Vec::new());
    }

    #[test]
    fn list_on_missing_checkpoints_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        // Nothing ever created `.checkpoints/` at all.
        assert_eq!(list(tmp.path()).unwrap(), Vec::new());
    }

    #[test]
    fn list_tolerates_a_corrupt_create_file() {
        let tmp = tempfile::tempdir().unwrap();
        let name_d = name_dir(tmp.path(), "corrupt");
        fs::create_dir_all(&name_d).unwrap();
        fs::write(create_file_path(&name_d, 1), "not json").unwrap();

        assert_eq!(list(tmp.path()).unwrap(), Vec::new());
    }

    #[test]
    fn list_reports_reachability_against_wal_content_on_disk() {
        use crate::wal::WalLine;

        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();
        let line = WalLine {
            seq: 5,
            ts: "2026-08-08T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "x" }),
        };
        fs::write(
            wal_dir.join("0000.jsonl"),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();

        create(wal_dir, "covered", Some(5)).unwrap();
        create(wal_dir, "uncovered", Some(999)).unwrap();

        let checkpoints = list(wal_dir).unwrap();
        let covered = checkpoints.iter().find(|c| c.name == "covered").unwrap();
        let uncovered = checkpoints.iter().find(|c| c.name == "uncovered").unwrap();
        assert!(covered.reachable);
        assert!(!uncovered.reachable);
    }

    #[test]
    fn concurrent_create_of_same_name_exactly_one_wins() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = tempfile::tempdir().unwrap();
        let wal_dir: Arc<PathBuf> = Arc::new(tmp.path().to_path_buf());
        let n = 8;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let wal_dir = Arc::clone(&wal_dir);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    create(&wal_dir, "race", Some(i as u64))
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(successes, 1, "exactly one concurrent create must succeed");

        let checkpoints = list(&wal_dir).unwrap();
        assert_eq!(checkpoints.len(), 1);
    }
}
