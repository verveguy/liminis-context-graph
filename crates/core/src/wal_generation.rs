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

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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

/// Ensures `wal_dir` has a recorded generation, minting a fresh one if none exists yet.
///
/// Callers MUST only invoke this when the stream has no prior content (see `WalWriter::new`,
/// which gates this on `scan_max_seq(&wal_dir)? == 0`) — a pre-existing populated directory with
/// no generation file (a pre-#387 stream) must stay `None` rather than being retroactively
/// minted (Out of Scope), **with one deliberate, narrow exception**: issue #431's
/// `wal_group::migrate_wal_root_if_needed` calls this on the destination directory of a legacy
/// flat-WAL migration, which does have prior content. That call site is not the general
/// retroactive backfill this invariant forbids — it stamps only the exact directory the
/// migration itself just relocated content into, under the *assumption* (not a proof — see issue
/// #431's `## Assumptions`) that a legacy flat WAL is locally owned. It does not extend to any
/// other populated, unstamped stream.
///
/// Race-safe by construction, not by timing: two processes racing to create the same stream's
/// directory for the first time (Edge Cases) both call this. Each writes its full, complete
/// content to its own uniquely-named temp file in `wal_dir` and durably `sync_all`s it, *before*
/// either ever touches the shared target path — then publishes atomically via `hard_link`, which
/// fails with `AlreadyExists` for exactly one side. Because the target only ever comes into
/// existence already fully written (nothing ever links a partial temp file), the loser's `read_generation`
/// immediately after its failed `hard_link` is guaranteed to see the winner's complete content —
/// there is no window, timing-dependent or otherwise, in which the target is observable but
/// incomplete. This is a stronger guarantee than a `create_new`-then-write-in-place approach:
/// that shape leaves a real window between file creation and content landing, and a prior version
/// of this function tried to paper over it with a bounded read-retry loop that self-healed by
/// deleting the target once the loser gave up waiting — which could not distinguish a winner that
/// had genuinely crashed mid-write from one that was merely slow (e.g. fsync contention when many
/// streams mint concurrently at process startup), so it could unlink a still-writing winner's
/// file out from under it and mint a second, different generation for the same stream: a
/// false-positive reset triggered by this function's own internals rather than an actual
/// publisher reset. The hard-link design removes the need for any such heuristic.
///
/// Self-healing still applies to genuinely pre-existing wreckage at the target that did **not**
/// come from a concurrent call to this function — e.g. an externally corrupted sidecar, or a
/// leftover empty file from a binary built before this fix. Because our own writes can no longer
/// produce that state, observing it here safely implies "not a live racer" and it is removed and
/// re-minted once, rather than bricking the stream permanently.
pub fn ensure_generation(wal_dir: &Path) -> Result<String, Error> {
    ensure_generation_impl(wal_dir, true)
}

fn ensure_generation_impl(wal_dir: &Path, self_heal_on_wreckage: bool) -> Result<String, Error> {
    if let Some(existing) = read_generation(wal_dir) {
        return Ok(existing);
    }

    let generation = Uuid::new_v4().to_string();
    let record = GenerationRecord {
        generation: generation.clone(),
    };
    let json = serde_json::to_string(&record)?;
    let path = generation_file_path(wal_dir);
    let tmp_path = wal_dir.join(format!("{WAL_GENERATION_FILE}.tmp-{}", Uuid::new_v4()));

    let write_tmp = (|| -> Result<(), Error> {
        let mut tmp_file = fs::File::create(&tmp_path)?;
        tmp_file.write_all(json.as_bytes())?;
        tmp_file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_tmp {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    // Publish atomically: the target either ends up exactly this fully-written content, or this
    // call learns someone else's fully-written content already won — never anything in between.
    let link_result = fs::hard_link(&tmp_path, &path);
    let _ = fs::remove_file(&tmp_path); // best-effort; the temp file's only job was the link above

    match link_result {
        Ok(()) => {
            sync_dir(wal_dir);
            Ok(generation)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(winner) = read_generation(wal_dir) {
                return Ok(winner);
            }
            if self_heal_on_wreckage {
                let _ = fs::remove_file(&path);
                return ensure_generation_impl(wal_dir, false);
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

/// Pure comparison for an **immutable** recorded generation (a checkpoint's, FR-007): `true`
/// only when both sides are known and differ. Opaque equality only — a generation is never
/// ordered or interpreted (spec Assumptions). Either side being `None` (never recorded — a
/// checkpoint taken before this feature or against a not-yet-generationed stream — or currently
/// unreadable/corrupt) is never a mismatch. A checkpoint has no "adopt on first encounter"
/// concept the way a consumer position does (see [`position_reset_detected`]): its generation is
/// set once, at creation, and never revisited, so a `None` recorded value stays permanently
/// generation-blind rather than ever being compared against a later `Some`.
pub fn generation_mismatch(recorded: Option<&str>, current: Option<&str>) -> bool {
    matches!((recorded, current), (Some(a), Some(b)) if a != b)
}

/// Row-existence-aware reset check for the **consumer-side WAL position** (`WalPosition`,
/// #353/#378/#387) — distinct from [`generation_mismatch`], which is the right (simpler) rule for
/// an immutable checkpoint record but wrong here.
///
/// The two differ because a `WalPosition` row is a *live, self-updating* baseline: every
/// successful completion write (`Conn::set_wal_position`) carries whatever generation is
/// currently on disk at that moment, adopting it — including adopting `None` when the stream
/// itself has no generation recorded yet. That adoption is silent and correct for an unchanged,
/// still-generation-less stream (Story 5, Scenarios 1-2): comparing a freshly-adopted `None`
/// against a still-`None` current value must never itself look like a reset.
///
/// But once a row exists at all (`applied_seq.is_some()` — including a row adopted with
/// `generation: None`), a *later* check where the current on-disk generation has become `Some`
/// must still be caught as a reset (Story 5, Scenario 3): a stream that never had a generation
/// and is later genuinely wiped and republished gains one for the first time, at the exact
/// moment it changes underneath the consumer — `recorded: None` is then a real prior value being
/// compared, not an unset placeholder to skip past. This is exactly the "no recorded generation"
/// case FR-009 says "later changes... are still detected normally" for.
///
/// `current: None` (the on-disk sidecar is itself missing, corrupt, or the directory has reverted
/// to looking like a legacy stream) is always exempt, symmetrically with `generation_mismatch` —
/// a damaged-but-harmless artifact must never fake a reset (Edge Cases).
///
/// `applied_seq: None` (no row recorded yet — literal first encounter for this stream/consumer
/// pair) is always exempt: there is nothing yet to have diverged from, and the replay this check
/// gates is itself what performs the adoption.
///
/// This function's own `current_generation.is_none()` exemption above is still exercised directly
/// by this module's unit tests, but is no longer reachable from `handle_rebuild_from_wal`'s one
/// production call site when `applied_seq.is_some()`: issue #414's unknown-generation guard
/// (`Error::WalGenerationUnknown`, ADR-0414) now returns early for exactly that combination before
/// this function is ever called, so a caller reaching this point with `applied_seq.is_some()` also
/// has `current_generation.is_some()`. Kept for correctness and symmetry with `generation_mismatch`
/// rather than narrowed to match, so the two functions' semantics don't drift apart silently.
pub fn position_reset_detected(
    applied_seq: Option<u64>,
    recorded_generation: Option<&str>,
    current_generation: Option<&str>,
) -> bool {
    applied_seq.is_some()
        && current_generation.is_some()
        && recorded_generation != current_generation
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

    /// Wreckage at the target path that did not come from a concurrent call to this function
    /// (e.g. externally corrupted, or left by a pre-fix binary — never something our own
    /// hard_link-based mint can produce) must not permanently brick the stream: `ensure_generation`
    /// must self-heal by removing it and minting a fresh one, rather than erroring forever.
    #[test]
    fn ensure_generation_self_heals_pre_existing_wreckage_not_caused_by_a_concurrent_mint() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(generation_file_path(tmp.path()), "").unwrap();

        let g = ensure_generation(tmp.path()).unwrap();
        assert!(!g.is_empty());
        assert_eq!(read_generation(tmp.path()), Some(g));
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
    fn position_reset_detected_is_false_with_no_row_yet() {
        // First encounter (applied_seq: None): nothing to compare against, regardless of what
        // current on-disk generation looks like.
        assert!(!position_reset_detected(None, None, None));
        assert!(!position_reset_detected(None, None, Some("a")));
        assert!(!position_reset_detected(None, Some("a"), Some("b")));
    }

    #[test]
    fn position_reset_detected_is_false_when_current_is_unreadable() {
        // A row exists, but the current on-disk generation can't be read (missing/corrupt) —
        // never treated as a mismatch (Edge Cases).
        assert!(!position_reset_detected(Some(5), Some("a"), None));
        assert!(!position_reset_detected(Some(5), None, None));
    }

    #[test]
    fn position_reset_detected_is_false_for_an_unchanged_still_generationless_stream() {
        // Story 5 Scenarios 1-2: a row exists (adopted with generation: None because the stream
        // itself has never had one), and the stream still has none — ordinary continued
        // operation, not a reset.
        assert!(!position_reset_detected(Some(5), None, None));
    }

    #[test]
    fn position_reset_detected_is_true_when_a_generationless_baseline_later_gains_one() {
        // Story 5 Scenario 3: a row exists with an adopted generation: None, and the stream has
        // since been genuinely reset and republished — now carrying a generation for the first
        // time. This is the one case generation_mismatch's simpler both-Some rule would (wrongly,
        // for this use) miss.
        assert!(position_reset_detected(Some(5), None, Some("new-gen")));
    }

    #[test]
    fn position_reset_detected_is_true_for_two_different_known_generations() {
        assert!(position_reset_detected(Some(5), Some("g1"), Some("g2")));
    }

    #[test]
    fn position_reset_detected_is_false_for_matching_known_generations() {
        assert!(!position_reset_detected(Some(5), Some("g1"), Some("g1")));
    }

    #[test]
    fn concurrent_first_creation_exactly_one_generation_is_recorded() {
        use std::sync::{Arc, Barrier};
        use std::thread;

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
