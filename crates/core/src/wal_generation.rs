//! WAL stream generation identity (issue #387) — a small, opaque, write-once token persisted
//! per group WAL directory alongside `.checkpoints/` and `.wal-bounds.json`. A generation stays
//! stable for the life of a stream and changes only when the stream is genuinely re-created from
//! scratch (republished from `seq: 0` with entirely new content), giving consumers a way to
//! detect "my publisher reset" that doesn't depend on WAL file bytes, counts, or timestamps
//! (FR-008).
//!
//! On-disk layout: `<wal_dir>/.wal-generation.json` holding `{"generation": "<uuid>"}` — a
//! minimal, self-describing JSON object (not a bare token file) so an external publisher (e.g.
//! orac/zen, which does not depend on lcg's Rust types) can write it with a one-line `json.dump`.
//! The `.json` extension keeps it invisible to every existing non-recursive `.jsonl` scan, same
//! as `.wal-bounds.json` (ADR-0375).
//!
//! Unlike `.wal-bounds.json`, this file is **load-bearing, not a cache**: it is minted once, on
//! first stream creation, and never rewritten or recomputed from content. A missing or corrupt
//! generation record is treated as "unknown," never as a mismatch (see `read_generation`) — the
//! Story 5 upgrade path and the Edge Cases section both require that a damaged-but-harmless
//! artifact can never masquerade as a detected reset.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Error;

/// File name (not full path — the record lives alongside the `.jsonl` files it describes) of the
/// generation sidecar. See the module doc for the on-disk shape.
pub const WAL_GENERATION_FILE: &str = ".wal-generation.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GenerationRecord {
    generation: String,
}

fn generation_file_path(wal_dir: &Path) -> PathBuf {
    wal_dir.join(WAL_GENERATION_FILE)
}

/// Reads the current generation for `wal_dir`, or `None` if no record exists, the file is
/// unreadable, or its contents don't parse as a well-formed, non-empty generation record. A
/// corrupted record is deliberately indistinguishable from "no record" (Edge Cases) — damage to
/// this file must never be misread as a detected reset.
pub fn read_generation(wal_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(generation_file_path(wal_dir)).ok()?;
    let record: GenerationRecord = serde_json::from_str(&text).ok()?;
    if record.generation.trim().is_empty() {
        return None;
    }
    Some(record.generation)
}

/// Number of times a loser of the first-creation race re-reads the winner's file before giving
/// up. The winner's write (a few dozen bytes, `create_new` + `write_all` + `sync_all`) completes
/// far faster than this bound in practice; the retry only covers the brief window between the
/// winner's `create_new` succeeding and its `write_all` landing.
const ADOPT_RETRY_ATTEMPTS: u32 = 50;
const ADOPT_RETRY_DELAY: Duration = Duration::from_millis(2);

/// Ensures `wal_dir` has a recorded generation, minting a fresh one if none exists yet.
///
/// Callers MUST only invoke this when the stream has no prior content (see `WalWriter::new`,
/// which gates this on `scan_max_seq(&wal_dir)? == 0`) — a pre-existing populated directory with
/// no generation file (a pre-#387 stream) must stay `None` rather than being retroactively
/// minted (Out of Scope).
///
/// Race-safe: two processes racing to create the same stream's directory for the first time
/// (Edge Cases) both call this. Exactly one wins an atomic `O_EXCL` create; the loser does not
/// error — it re-reads the winner's file and adopts that value, so both processes agree on one
/// generation for the stream.
pub fn ensure_generation(wal_dir: &Path) -> Result<String, Error> {
    if let Some(existing) = read_generation(wal_dir) {
        return Ok(existing);
    }

    let generation = Uuid::new_v4().to_string();
    let record = GenerationRecord {
        generation: generation.clone(),
    };
    let json = serde_json::to_string(&record)?;
    let path = generation_file_path(wal_dir);

    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            sync_dir(wal_dir);
            Ok(generation)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            for _ in 0..ADOPT_RETRY_ATTEMPTS {
                if let Some(winner) = read_generation(wal_dir) {
                    return Ok(winner);
                }
                thread::sleep(ADOPT_RETRY_DELAY);
            }
            Err(Error::WalIo(std::io::Error::other(
                "generation file exists but could not be read after concurrent creation",
            )))
        }
        Err(e) => Err(Error::WalIo(e)),
    }
}

/// Best-effort directory-entry sync, matching `checkpoint.rs`'s `sync_dir` — flushes the
/// containing directory's own entry so a newly linked generation file survives a crash
/// immediately after creation. Never turns a successful create into a hard error.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Pure comparison: `true` only when both sides are recorded and differ. Opaque equality only —
/// a generation is never ordered or interpreted (spec Assumptions). Either side being `None`
/// (never recorded, or unreadable/corrupt) is never a mismatch — see FR-009 (Story 5) and the
/// Edge Cases section.
pub fn generation_mismatch(recorded: Option<&str>, current: Option<&str>) -> bool {
    matches!((recorded, current), (Some(a), Some(b)) if a != b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_generation_is_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_generation(tmp.path()), None);
    }

    #[test]
    fn read_generation_is_none_for_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(generation_file_path(tmp.path()), "not json").unwrap();
        assert_eq!(read_generation(tmp.path()), None);
    }

    #[test]
    fn read_generation_is_none_for_empty_generation_value() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(generation_file_path(tmp.path()), r#"{"generation":""}"#).unwrap();
        assert_eq!(read_generation(tmp.path()), None);
    }

    #[test]
    fn ensure_generation_mints_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let g1 = ensure_generation(tmp.path()).unwrap();
        assert!(!g1.is_empty());
        assert_eq!(read_generation(tmp.path()), Some(g1));
    }

    #[test]
    fn ensure_generation_is_idempotent_once_minted() {
        let tmp = tempfile::tempdir().unwrap();
        let g1 = ensure_generation(tmp.path()).unwrap();
        let g2 = ensure_generation(tmp.path()).unwrap();
        assert_eq!(g1, g2);
    }

    #[test]
    fn generation_mismatch_requires_both_sides_present_and_different() {
        assert!(!generation_mismatch(None, None));
        assert!(!generation_mismatch(Some("a"), None));
        assert!(!generation_mismatch(None, Some("a")));
        assert!(!generation_mismatch(Some("a"), Some("a")));
        assert!(generation_mismatch(Some("a"), Some("b")));
    }

    #[test]
    fn concurrent_first_creation_exactly_one_generation_is_recorded() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::tempdir().unwrap();
        let wal_dir: Arc<PathBuf> = Arc::new(tmp.path().to_path_buf());
        let n = 8;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let wal_dir = Arc::clone(&wal_dir);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    ensure_generation(&wal_dir)
                })
            })
            .collect();

        let results: Vec<String> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();

        let first = &results[0];
        assert!(
            results.iter().all(|g| g == first),
            "every racing caller must agree on exactly one generation: {results:?}"
        );
        assert_eq!(read_generation(&wal_dir), Some(first.clone()));
    }
}
