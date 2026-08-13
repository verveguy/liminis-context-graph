//! Maps a `group_id` to its own WAL directory under a shared WAL **root**, and migrates a
//! pre-378 single-stream `LCG_WAL_DIR` into the default group's subdirectory (issue #378).
//!
//! Before this issue, `LCG_WAL_DIR` named one directory shared by every group. After it,
//! `LCG_WAL_DIR` names a **root** containing one subdirectory per `group_id`:
//!
//! ```text
//! <wal_root>/
//!   liminis/     *.jsonl   .checkpoints/   .wal-bounds.json
//!   group-a/     *.jsonl   .checkpoints/   .wal-bounds.json
//! ```
//!
//! The lower-level WAL machinery (`WalWriter`, `checkpoint.rs`, the bounds manifest) is already
//! `&Path`-parameterized and needs no changes — this module is purely about picking the right
//! directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::checkpoint;
use crate::error::Error;

/// The group every write handler that has no narrower single group in scope falls back to
/// (FR-004), and the group a pre-378 single-stream deployment's entire WAL directory migrates
/// into (FR-001). Also the default for every `group_id`-accepting IPC parameter, preserving
/// FR-009's zero-caller-change single-group parity.
pub const DEFAULT_GROUP_ID: &str = "liminis";

/// File name of the seq-bounds manifest (issue #375) — a sibling artifact `migrate_wal_root_if_needed`
/// must relocate alongside the `.jsonl` files it describes (FR-001). Mirrors `wal::WAL_BOUNDS_MANIFEST_FILE`,
/// duplicated here rather than imported since that constant is private to `wal.rs` and this is the
/// only other place that needs to recognize the file by name (not open or parse it).
const WAL_BOUNDS_MANIFEST_FILE: &str = ".wal-bounds.json";

/// Directory name of the checkpoint store (issue #365) — the other sibling artifact FR-001's
/// migration must relocate as a unit, not only the `.jsonl` files.
const CHECKPOINTS_DIR_NAME: &str = ".checkpoints";

/// Encodes `group_id` into a filesystem-safe, bijective WAL directory name (FR-005).
///
/// A `group_id` that already satisfies [`checkpoint::validate_name`]'s rule (ASCII alphanumeric
/// plus `_`/`-`, non-empty, ≤200 chars) is used as the directory name unchanged — this covers the
/// default `"liminis"` group and every `group_id` chosen with this rule in mind, keeping the
/// common case human-readable on disk.
///
/// Otherwise every byte outside that safe charset is percent-encoded (`%` + two uppercase hex
/// digits), including a literal `%` if present. This makes the two cases self-describing on
/// decode without a side table: a directory name containing no `%` is a literal `group_id` (the
/// safe charset can never itself contain `%`); one containing `%` is percent-decoded to recover
/// the original. Bijective by construction, so unlike lossy sanitization it cannot collide two
/// distinct `group_id` values onto one directory.
///
/// Fails only when `group_id` is empty, or when the encoded name would exceed
/// `checkpoint::validate_name`'s 200-char bound — the two cases FR-005 says have no meaningful
/// directory name.
pub fn encode_group_dir_name(group_id: &str) -> Result<String, Error> {
    if group_id.is_empty() {
        return Err(Error::InvalidGroupId(group_id.to_string()));
    }
    if checkpoint::validate_name(group_id).is_ok() {
        return Ok(group_id.to_string());
    }
    let mut encoded = String::with_capacity(group_id.len());
    for byte in group_id.as_bytes() {
        if byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-' {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    if encoded.is_empty() || encoded.len() > 200 {
        return Err(Error::InvalidGroupId(group_id.to_string()));
    }
    Ok(encoded)
}

/// Inverts [`encode_group_dir_name`]: recovers the original `group_id` from a WAL directory
/// name. A name containing no `%` is returned unchanged (it must have been the "already safe"
/// case, since the safe charset never produces one); otherwise every `%XX` escape is decoded.
/// Used by [`list_group_wal_dirs`] to recover each group's identity for `knowledge_status`'s
/// per-group map (FR-007) without a side table.
pub fn decode_group_dir_name(dir_name: &str) -> Result<String, Error> {
    if !dir_name.contains('%') {
        return Ok(dir_name.to_string());
    }
    let bytes = dir_name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .ok_or_else(|| Error::InvalidGroupId(dir_name.to_string()))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| Error::InvalidGroupId(dir_name.to_string()))?;
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| Error::InvalidGroupId(dir_name.to_string()))
}

/// Resolves the WAL directory for `group_id` under `wal_root`. Does not create it — callers
/// that need the directory to exist (e.g. `WalWriter::new`, which already `create_dir_all`s)
/// handle creation lazily on first use (FR-003).
pub fn group_wal_dir(wal_root: &Path, group_id: &str) -> Result<PathBuf, Error> {
    Ok(wal_root.join(encode_group_dir_name(group_id)?))
}

/// Enumerates every group that currently has a WAL directory under `wal_root` — used by
/// `knowledge_status`'s per-group `applied_seq`/`max_seq` map (FR-007). Returns an empty list
/// (not an error) when `wal_root` doesn't exist yet, matching "no stream" being a normal state
/// (see Edge Cases: "A group with no writes yet has no directory").
///
/// A subdirectory whose name doesn't decode cleanly (foreign/corrupt content placed directly
/// under the WAL root) is silently skipped rather than failing the whole enumeration — one bad
/// entry must not hide every legitimate group's status.
pub fn list_group_wal_dirs(wal_root: &Path) -> Result<Vec<(String, PathBuf)>, Error> {
    if !wal_root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(wal_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Ok(group_id) = decode_group_dir_name(name) {
            out.push((group_id, path));
        }
    }
    Ok(out)
}

/// True if `path` (a direct child of a WAL root) is one of the artifact kinds a pre-378
/// single-stream `LCG_WAL_DIR` held at its top level: a `*.jsonl` WAL file, the `.checkpoints/`
/// store (#365), the `.wal-bounds.json` manifest (#375), or that manifest's write-in-progress
/// `.tmp` sibling. Anything else at the WAL root's top level (in particular, a subdirectory that
/// is itself a per-group WAL directory once multi-stream is in use) is left alone — only these
/// specific legacy-layout artifacts are ever relocated by [`migrate_wal_root_if_needed`].
fn is_legacy_top_level_wal_artifact(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if path.is_dir() {
        return name == CHECKPOINTS_DIR_NAME;
    }
    name.ends_with(".jsonl")
        || name == WAL_BOUNDS_MANIFEST_FILE
        || (name.starts_with(".wal-bounds.") && name.ends_with(".tmp"))
}

/// Migrates a pre-378 single-stream `wal_root` (today's flat `LCG_WAL_DIR`, holding `*.jsonl`
/// files, `.checkpoints/`, and `.wal-bounds.json` directly at its top level) into the multi-stream
/// layout by relocating those artifacts, as a unit, into `<wal_root>/liminis/` (FR-001, FR-009).
///
/// Idempotent and crash-safe by construction: each entry is moved with one atomic [`fs::rename`],
/// so a crash mid-migration leaves some entries already inside `liminis/` (no longer visible to
/// this function's top-level scan) and some still loose. The next call re-lists exactly the
/// unmoved remainder and finishes the job — no separate marker file is needed, and a second call
/// over an already-fully-migrated root is a cheap no-op (the top-level scan finds nothing to
/// move). Must be called before any per-group `WalWriter` is constructed against `wal_root`.
pub fn migrate_wal_root_if_needed(wal_root: &Path) -> Result<(), Error> {
    if !wal_root.exists() {
        // Fresh install: nothing to migrate. Per-group directories are created lazily on
        // first write (FR-003).
        return Ok(());
    }

    let loose_entries: Vec<PathBuf> = fs::read_dir(wal_root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_legacy_top_level_wal_artifact(p))
        .collect();
    if loose_entries.is_empty() {
        return Ok(());
    }

    let default_dir = wal_root.join(DEFAULT_GROUP_ID);
    fs::create_dir_all(&default_dir)?;
    for entry in loose_entries {
        let Some(file_name) = entry.file_name() else {
            continue;
        };
        let dest = default_dir.join(file_name);
        if dest.exists() {
            // A prior partial run already moved this entry; nothing left to do for it.
            continue;
        }
        fs::rename(&entry, &dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_already_safe_group_id_is_unchanged() {
        assert_eq!(encode_group_dir_name("liminis").unwrap(), "liminis");
        assert_eq!(encode_group_dir_name("group-a_1").unwrap(), "group-a_1");
    }

    #[test]
    fn encode_rejects_empty() {
        assert!(encode_group_dir_name("").is_err());
    }

    #[test]
    fn encode_rejects_over_length_safe_name() {
        assert!(encode_group_dir_name(&"a".repeat(201)).is_err());
    }

    #[test]
    fn encode_percent_encodes_unsafe_characters() {
        let encoded = encode_group_dir_name("source.doc:v1").unwrap();
        assert_eq!(encoded, "source%2Edoc%3Av1");
        assert_eq!(decode_group_dir_name(&encoded).unwrap(), "source.doc:v1");
    }

    #[test]
    fn encode_percent_encodes_whitespace() {
        let encoded = encode_group_dir_name("my group").unwrap();
        assert_eq!(decode_group_dir_name(&encoded).unwrap(), "my group");
        // The encoded name is a single, safe path component (no separators), even though it
        // doesn't itself satisfy checkpoint::validate_name's stricter charset (it may contain
        // '%', which is filesystem-safe but outside that rule's scope).
        assert_eq!(std::path::Path::new(&encoded).components().count(), 1);
    }

    #[test]
    fn encode_percent_encodes_a_literal_percent_sign() {
        // A group_id already containing '%' must not collide with the encoding of a group_id
        // that produces the same '%XX' text via escaping some other character.
        let encoded = encode_group_dir_name("50%done").unwrap();
        assert_eq!(decode_group_dir_name(&encoded).unwrap(), "50%done");
    }

    #[test]
    fn encode_decode_roundtrip_is_bijective_and_collision_free() {
        let inputs = [
            "liminis",
            "source.doc:v1",
            "my group",
            "50%done",
            "a/b\\c",
            "\0null",
            "unicode-\u{e9}\u{e8}",
        ];
        let mut seen = std::collections::HashSet::new();
        for input in inputs {
            let encoded = encode_group_dir_name(input).unwrap();
            assert!(
                seen.insert(encoded.clone()),
                "collision: two inputs encoded to {encoded:?}"
            );
            assert_eq!(decode_group_dir_name(&encoded).unwrap(), input);
        }
    }

    #[test]
    fn decode_passes_through_a_name_with_no_percent() {
        assert_eq!(decode_group_dir_name("liminis").unwrap(), "liminis");
    }

    #[test]
    fn group_wal_dir_joins_encoded_name_onto_root() {
        let root = Path::new("/tmp/wal-root");
        let dir = group_wal_dir(root, "liminis").unwrap();
        assert_eq!(dir, root.join("liminis"));
    }

    #[test]
    fn list_group_wal_dirs_is_empty_for_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(list_group_wal_dirs(&missing).unwrap(), Vec::new());
    }

    #[test]
    fn list_group_wal_dirs_enumerates_and_decodes_every_group() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("liminis")).unwrap();
        let encoded_b = encode_group_dir_name("group.b").unwrap();
        fs::create_dir_all(tmp.path().join(&encoded_b)).unwrap();
        // A stray file at the root (not a directory) must be ignored, not misreported as a group.
        fs::write(tmp.path().join("stray.txt"), b"x").unwrap();

        let mut groups: Vec<String> = list_group_wal_dirs(tmp.path())
            .unwrap()
            .into_iter()
            .map(|(g, _)| g)
            .collect();
        groups.sort();
        assert_eq!(groups, vec!["group.b".to_string(), "liminis".to_string()]);
    }

    #[test]
    fn migrate_is_noop_for_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        migrate_wal_root_if_needed(&missing).unwrap();
        assert!(!missing.exists());
    }

    #[test]
    fn migrate_is_noop_for_fresh_multigroup_root_with_no_loose_entries() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("group-a")).unwrap();
        fs::create_dir_all(tmp.path().join("group-b")).unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        // Neither pre-existing group directory was touched or nested into a "liminis" dir.
        assert!(tmp.path().join("group-a").is_dir());
        assert!(tmp.path().join("group-b").is_dir());
        assert!(!tmp.path().join("liminis").exists());
    }

    #[test]
    fn migrate_relocates_jsonl_checkpoints_and_bounds_manifest_as_a_unit() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("20260101_000000_abcdef_0000.jsonl"),
            b"{}\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join(".checkpoints").join("pre-migration")).unwrap();
        fs::write(
            tmp.path()
                .join(".checkpoints")
                .join("pre-migration")
                .join("meta.json"),
            b"{}",
        )
        .unwrap();
        fs::write(tmp.path().join(".wal-bounds.json"), b"{}").unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        let default_dir = tmp.path().join("liminis");
        assert!(default_dir
            .join("20260101_000000_abcdef_0000.jsonl")
            .is_file());
        assert!(default_dir
            .join(".checkpoints")
            .join("pre-migration")
            .join("meta.json")
            .is_file());
        assert!(default_dir.join(".wal-bounds.json").is_file());
        // The root itself no longer holds any loose top-level artifact.
        assert!(!tmp
            .path()
            .join("20260101_000000_abcdef_0000.jsonl")
            .exists());
        assert!(!tmp.path().join(".checkpoints").exists());
        assert!(!tmp.path().join(".wal-bounds.json").exists());
    }

    #[test]
    fn migrate_is_idempotent_when_run_twice() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("20260101_000000_abcdef_0000.jsonl"),
            b"{}\n",
        )
        .unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();
        migrate_wal_root_if_needed(tmp.path()).unwrap();

        let default_dir = tmp.path().join("liminis");
        assert!(default_dir
            .join("20260101_000000_abcdef_0000.jsonl")
            .is_file());
    }

    /// Simulates a crash between creating `liminis/` and finishing every rename: one entry made
    /// it across, one didn't. The next call must finish the job, not skip the remainder because
    /// `liminis/` already exists.
    #[test]
    fn migrate_resumes_a_partially_completed_prior_run() {
        let tmp = tempfile::tempdir().unwrap();
        let default_dir = tmp.path().join("liminis");
        fs::create_dir_all(&default_dir).unwrap();
        // Already-moved entry (simulating the crash landed after this rename).
        fs::write(default_dir.join("moved_0000.jsonl"), b"{}\n").unwrap();
        // Still-loose entry (simulating the crash landed before this rename).
        fs::write(tmp.path().join("unmoved_0001.jsonl"), b"{}\n").unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        assert!(default_dir.join("moved_0000.jsonl").is_file());
        assert!(default_dir.join("unmoved_0001.jsonl").is_file());
        assert!(!tmp.path().join("unmoved_0001.jsonl").exists());
    }

    #[test]
    fn migrate_does_not_move_a_sibling_group_directory_created_after_migration() {
        // Once multi-stream is in use, other groups' subdirectories sit at the WAL root's top
        // level too. A later call (e.g. every AppState::from_env startup) must not mistake them
        // for unmigrated legacy content.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("liminis")).unwrap();
        fs::create_dir_all(tmp.path().join("group-a")).unwrap();
        fs::write(tmp.path().join("group-a").join("stray.jsonl"), b"{}\n").unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        assert!(tmp.path().join("group-a").join("stray.jsonl").is_file());
        assert!(!tmp
            .path()
            .join("liminis")
            .join("group-a")
            .join("stray.jsonl")
            .exists());
    }
}
