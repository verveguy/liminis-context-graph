/// Named, retained WAL recovery positions (issue #363).
///
/// A checkpoint labels a WAL sequence position an operator has declared known-good, so a later
/// bounded rebuild (`knowledge_rebuild_from_wal { to_seq }`, issue #362) has a precise target
/// instead of requiring timestamp correlation or bisection. This is distinct from
/// `applied_seq`/`WalPosition` (#353/ADR-0353), which is DB-resident, singleton, and
/// automatically advancing — a checkpoint is operator-chosen, named, and must survive the loss
/// of the database it was taken against.
///
/// Storage is therefore entirely WAL-directory-resident, not a DB table: one JSON file per
/// checkpoint under `<wal_dir>/.checkpoints/<name>.json`. This placement is load-bearing — see
/// `checkpoints_dir` — because every WAL/max-seq scan in this codebase (`wal.rs`'s
/// `count_jsonl_files`/`scan_max_seq`, `replay.rs`'s file collection) is a non-recursive
/// `fs::read_dir` filtered to `.jsonl` extension. A `.checkpoints/` subdirectory of non-`.jsonl`
/// files is invisible to all of them without any change to their logic; a checkpoint store
/// placed directly in `wal_dir` with a `.jsonl` extension would instead be silently replayed as
/// mutations and folded into `global_seq` derivation.
///
/// One file per checkpoint (not a single append-only log) so duplicate-name detection (FR-002)
/// and delete-of-missing-name detection (FR-006) fall out of the filesystem's own atomicity
/// (`create_new`/`remove_file`) rather than requiring an in-process lock this codebase has no
/// existing primitive for (checkpoints may be created by a producer process and inspected by a
/// local reader sharing the same WAL directory).
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::wal::{wal_max_seq, wal_min_seq};

/// A named, retained WAL position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub name: String,
    pub seq: u64,
    pub created_at: String,
    pub note: Option<String>,
}

/// A [`Checkpoint`] plus its derived (not stored) reachability status, as returned by `list`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CheckpointListEntry {
    #[serde(flatten)]
    pub checkpoint: Checkpoint,
    /// Whether `checkpoint.seq` falls within `[wal_min_seq, wal_max_seq]` for the WAL content
    /// currently available for replay (FR-007). A bounds check, not a full existence scan: a
    /// gap inside the range (e.g. a specific file manually removed, neighbors intact) is not
    /// detected and is misreported as reachable. See `is_reachable`.
    pub reachable: bool,
}

/// Returns the directory checkpoints are stored in — the single source of truth for this path.
/// Never `.jsonl`-suffixed and never `wal_dir` itself; see the module doc comment for why.
fn checkpoints_dir(wal_dir: &Path) -> PathBuf {
    wal_dir.join(".checkpoints")
}

fn checkpoint_path(wal_dir: &Path, name: &str) -> PathBuf {
    checkpoints_dir(wal_dir).join(format!("{name}.json"))
}

/// Validates a checkpoint name against a safe filename charset before it touches the
/// filesystem. `name` becomes a path component directly (`<wal_dir>/.checkpoints/<name>.json`),
/// so without this check a name containing `/` or `..` could escape `.checkpoints/` entirely.
fn validate_name(name: &str) -> Result<(), Error> {
    static NAME_RE: OnceLock<Regex> = OnceLock::new();
    let re = NAME_RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,255}$").unwrap());
    if name == "." || name == ".." || !re.is_match(name) {
        return Err(Error::Ipc(format!(
            "invalid checkpoint name '{name}': must match ^[A-Za-z0-9._-]{{1,255}}$ and must \
             not be '.' or '..'"
        )));
    }
    Ok(())
}

/// Captures `seq` under `name` (FR-001). Fails if `name` already exists (FR-002) rather than
/// overwriting it — deleting and recreating is the supported way to redefine a name. Pure
/// filesystem operation: does not read `applied_seq` itself, since the caller (the
/// `knowledge_checkpoint_create` handler) already resolved it and must reject a `None`
/// `applied_seq` before calling this (FR-008).
pub(crate) fn create(
    wal_dir: &Path,
    name: &str,
    seq: u64,
    note: Option<String>,
) -> Result<Checkpoint, Error> {
    validate_name(name)?;
    fs::create_dir_all(checkpoints_dir(wal_dir))?;

    let checkpoint = Checkpoint {
        name: name.to_string(),
        seq,
        created_at: chrono::Utc::now().to_rfc3339(),
        note,
    };
    let json = serde_json::to_string_pretty(&checkpoint)?;

    let path = checkpoint_path(wal_dir, name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Ipc(format!(
                    "checkpoint '{name}' already exists; delete it first if you want to \
                     redefine this name"
                ))
            } else {
                Error::WalIo(e)
            }
        })?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;

    Ok(checkpoint)
}

/// Permanently removes the named checkpoint (FR-005). Fails if `name` does not exist (FR-006)
/// rather than treating it as a no-op.
pub(crate) fn delete(wal_dir: &Path, name: &str) -> Result<(), Error> {
    validate_name(name)?;
    let path = checkpoint_path(wal_dir, name);
    fs::remove_file(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Ipc(format!("no checkpoint named '{name}' exists"))
        } else {
            Error::WalIo(e)
        }
    })
}

/// Returns every retained checkpoint (FR-003), each annotated with its reachability (FR-007).
/// An empty or missing `.checkpoints/` directory returns an empty list, not an error. A file
/// that fails to parse (e.g. truncated by a crash between `create_new` succeeding and the write
/// completing) is skipped rather than failing the whole call — the same corruption-tolerance
/// precedent as `wal::read_last_seq`/`replay::first_seq_in_file`. Results are sorted by `name`
/// for a deterministic order (not otherwise specified by the spec).
pub(crate) fn list(wal_dir: &Path) -> Result<Vec<CheckpointListEntry>, Error> {
    let dir = checkpoints_dir(wal_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let min_seq = wal_min_seq(wal_dir)?;
    let max_seq = wal_max_seq(wal_dir)?;

    let mut entries: Vec<CheckpointListEntry> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|p| {
            let contents = fs::read_to_string(&p).ok()?;
            let checkpoint: Checkpoint = serde_json::from_str(&contents).ok()?;
            let reachable = is_reachable(checkpoint.seq, min_seq, max_seq);
            Some(CheckpointListEntry {
                checkpoint,
                reachable,
            })
        })
        .collect();

    entries.sort_by(|a, b| a.checkpoint.name.cmp(&b.checkpoint.name));
    Ok(entries)
}

/// `seq` is reachable iff it falls within `[min_seq, max_seq]` — the range of seqs the WAL
/// content currently available for replay actually covers. An empty WAL (`min_seq`/`max_seq`
/// both `None`) makes every checkpoint unreachable.
fn is_reachable(seq: u64, min_seq: Option<u64>, max_seq: Option<u64>) -> bool {
    match (min_seq, max_seq) {
        (Some(min), Some(max)) => seq >= min && seq <= max,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wal_line(dir: &Path, file_name: &str, seq: u64) {
        let line = crate::wal::WalLine {
            seq,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "x" }),
        };
        fs::write(
            dir.join(file_name),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn validate_name_accepts_typical_names() {
        assert!(validate_name("pre-migration").is_ok());
        assert!(validate_name("post_migration.v2").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_too_long() {
        let name = "a".repeat(256);
        assert!(validate_name(&name).is_err());
        assert!(validate_name(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn validate_name_rejects_dot_and_dotdot() {
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn validate_name_rejects_path_separators() {
        assert!(validate_name("../../etc/passwd").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
    }

    #[test]
    fn create_list_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();

        let cp = create(
            wal_dir,
            "pre-migration",
            100,
            Some("before the big change".into()),
        )
        .unwrap();
        assert_eq!(cp.name, "pre-migration");
        assert_eq!(cp.seq, 100);
        assert_eq!(cp.note.as_deref(), Some("before the big change"));

        let entries = list(wal_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].checkpoint.name, "pre-migration");

        delete(wal_dir, "pre-migration").unwrap();
        assert!(list(wal_dir).unwrap().is_empty());
    }

    #[test]
    fn create_rejects_duplicate_name() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();

        create(wal_dir, "dup", 1, None).unwrap();
        let err = create(wal_dir, "dup", 2, None).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        // The original must be unchanged (FR-004).
        let entries = list(wal_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].checkpoint.seq, 1);
    }

    #[test]
    fn delete_rejects_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();

        let err = delete(wal_dir, "does-not-exist").unwrap_err();
        assert!(err.to_string().contains("no checkpoint named"));
    }

    #[test]
    fn list_on_empty_set_returns_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn list_skips_an_unparseable_file_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();
        create(wal_dir, "good", 5, None).unwrap();
        fs::write(checkpoints_dir(wal_dir).join("corrupt.json"), "not json").unwrap();

        let entries = list(wal_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].checkpoint.name, "good");
    }

    #[test]
    fn creating_or_deleting_one_checkpoint_does_not_alter_others() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();

        create(wal_dir, "a", 1, None).unwrap();
        create(wal_dir, "b", 2, None).unwrap();
        create(wal_dir, "c", 3, None).unwrap();

        delete(wal_dir, "b").unwrap();

        let entries = list(wal_dir).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.checkpoint.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert_eq!(entries[0].checkpoint.seq, 1);
        assert_eq!(entries[1].checkpoint.seq, 3);
    }

    #[test]
    fn list_marks_a_checkpoint_unreachable_when_seq_outside_wal_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();

        // WAL only covers seqs [50, 60]; a checkpoint at seq 10 is outside that range.
        write_wal_line(wal_dir, "seeded_0000.jsonl", 50);
        write_wal_line(wal_dir, "seeded_0001.jsonl", 60);
        create(wal_dir, "stale", 10, None).unwrap();
        create(wal_dir, "current", 55, None).unwrap();

        let entries = list(wal_dir).unwrap();
        let stale = entries
            .iter()
            .find(|e| e.checkpoint.name == "stale")
            .unwrap();
        let current = entries
            .iter()
            .find(|e| e.checkpoint.name == "current")
            .unwrap();
        assert!(!stale.reachable);
        assert!(current.reachable);
    }

    #[test]
    fn list_marks_all_checkpoints_unreachable_when_wal_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path();
        create(wal_dir, "orphaned", 42, None).unwrap();

        let entries = list(wal_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].reachable);
    }
}
