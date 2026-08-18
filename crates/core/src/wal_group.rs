//! Maps a `group_id` to its own WAL directory under a shared WAL **root**, and migrates a
//! pre-378 single-stream `LCG_WAL_DIR` into the default group's subdirectory (issue #378).
//!
//! Before this issue, `LCG_WAL_DIR` named one directory shared by every group. After it,
//! `LCG_WAL_DIR` names a **root** containing one subdirectory per `group_id`:
//!
//! ```text
//! <wal_root>/
//!   liminis/     *.jsonl   .checkpoints/   .wal-bounds.json   .wal-generation.json
//!   group-a/     *.jsonl   .checkpoints/   .wal-bounds.json   .wal-generation.json
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

/// File name of the generation sidecar (issue #387) — a third sibling artifact
/// `migrate_wal_root_if_needed` must relocate alongside the others. Mirrors
/// `wal_generation::WAL_GENERATION_FILE`, duplicated here for the same reason as
/// `WAL_BOUNDS_MANIFEST_FILE` above: this is the only other place that needs to recognize the
/// file by name, not open or parse it. No pre-378 install could actually have this file (this
/// feature postdates #378), but recognizing it keeps this list consistent with the other two
/// sidecars for any future top-level-artifact migration.
const WAL_GENERATION_FILE: &str = ".wal-generation.json";

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

/// Defends against a collision `encode_group_dir_name` cannot see: it is bijective at the
/// **string** level (FR-005), but on a case-insensitive filesystem (the default for macOS APFS
/// and Windows NTFS — both plausible dev/deployment targets), two distinct `group_id`s that
/// already satisfy `checkpoint::validate_name` and differ only by case (e.g. `"Acme"` and
/// `"acme"`) both pass through unchanged and resolve to the *same physical directory*,
/// interleaving both groups' `*.jsonl` files, `.checkpoints/`, and `.wal-bounds.json` and
/// corrupting both streams' `global_seq` numbering. FR-005 mandates that an already-safe
/// `group_id` use itself as the directory name unchanged, so this cannot be closed by changing
/// the encoding scheme without a spec amendment — instead, called once, the first time
/// `dir_name` would be created (see `AppState::with_wal_writer`), it fails loudly if an
/// existing, differently-cased sibling directory would collide, rather than letting both groups
/// silently share one directory.
///
/// A no-op when `dir_name`'s own directory already exists (the ordinary "reopen an existing
/// group's writer" case, not a fresh collision) or when `wal_root` doesn't exist yet.
pub fn check_no_case_insensitive_collision(wal_root: &Path, dir_name: &str) -> Result<(), Error> {
    if !wal_root.exists() {
        return Ok(());
    }
    // Deliberately does not short-circuit via `wal_root.join(dir_name).exists()`: on a
    // case-insensitive filesystem that check itself resolves "acme" to an existing "Acme"
    // directory, so it would wrongly treat every collision as "already exists, this is just a
    // reopen" and never reach the mismatch below. Scanning entries and comparing byte-for-byte
    // instead makes this function's own behavior correct regardless of the host filesystem's
    // case sensitivity — the whole point of the check.
    for entry in fs::read_dir(wal_root)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let Some(existing_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if existing_name == dir_name {
            return Ok(()); // Exact match — reopening this group's own existing directory.
        }
        if existing_name.eq_ignore_ascii_case(dir_name) {
            return Err(Error::Ipc(format!(
                "cannot create WAL directory {dir_name:?} under {wal_root:?}: it would collide \
                 case-insensitively with the existing directory {existing_name:?} on a \
                 case-insensitive filesystem (e.g. macOS APFS, Windows NTFS) — both group_ids \
                 would silently interleave into one physical directory. Choose a group_id that \
                 doesn't differ from an existing one only by letter case."
            )));
        }
    }
    Ok(())
}

/// Enumerates every group that currently has a WAL directory under `wal_root` — used by
/// `knowledge_status`'s per-group `applied_seq`/`max_seq` map (FR-007). Returns an empty list
/// (not an error) when `wal_root` doesn't exist yet, matching "no stream" being a normal state
/// (see Edge Cases: "A group with no writes yet has no directory").
///
/// A subdirectory whose name doesn't decode cleanly, or doesn't round-trip back through
/// [`encode_group_dir_name`] to the exact same directory name, is silently skipped rather than
/// failing the whole enumeration — one bad entry must not hide every legitimate group's status.
/// The round-trip check is what excludes housekeeping directories that can legitimately sit at
/// the WAL root's top level but are never themselves a group's directory — most notably a
/// pre-378 `.checkpoints/` left behind by a migration that hasn't run yet or failed
/// (`migrate_wal_root_if_needed` is non-fatal on error, per `AppState::from_env`): `.checkpoints`
/// decodes to itself (it contains no `%`), but `encode_group_dir_name(".checkpoints")` would
/// percent-encode the leading `.` into a different string, so the round-trip fails and the entry
/// is correctly excluded rather than misreported as a real `group_id` in `knowledge_status`'s
/// `wal_groups` map.
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
            if matches!(encode_group_dir_name(&group_id), Ok(re_encoded) if re_encoded == name) {
                out.push((group_id, path));
            }
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
        || name == WAL_GENERATION_FILE
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
///
/// Also stamps a `.wal-generation.json` for the destination group (issue #431), whenever there is
/// legacy content to relocate: a legacy flat WAL predates generation identity (#387) entirely, and
/// this migration *assumes* it is locally owned (this node wrote it) rather than proving it —
/// nothing on disk distinguishes a flat WAL this node wrote from one copied in from elsewhere; see
/// issue #431's `## Assumptions` for why the assumption holds today and what would invalidate it.
/// Under that assumption, migration mints a generation for it rather than leaving it to be refused
/// by #414's unknown-generation guard on the very next rebuild. `wal_generation::ensure_generation`
/// is itself idempotent (FR-002: never overwrites an existing record) and is only ever called here
/// on `default_dir`, the exact directory this migration just relocated (or is relocating) loose
/// content into — never as a general sweep over every group directory, which would also stamp an
/// unrelated stream that arrived without a generation for other reasons (e.g. one stripped per the
/// ADR-0387 publish contract) and defeat the guard #414 introduced.
///
/// The stamp is minted *before* the relocation loop below, not after: if it ran last and failed
/// (disk full, permissions) on a call where every loose entry had already been renamed, the next
/// call's `loose_entries` scan would find nothing left at the root and take the early return above
/// without ever retrying the stamp, leaving the group permanently generationless. Minting first
/// keeps the stamp attempt covered by the same retry the relocation loop already gets: as long as
/// any loose entry remains unmoved, this function is re-entered past the early return on the next
/// call, and `ensure_generation`'s own idempotency (FR-002) makes re-attempting it there a no-op
/// once it has actually succeeded.
///
/// A loose `.wal-generation.json` — recognized by [`is_legacy_top_level_wal_artifact`] for
/// consistency with the other sidecars, though no genuine pre-378 install can actually have one —
/// is relocated ahead of both the mint and the general loop, specifically so the mint never gets a
/// chance to occupy `default_dir`'s sidecar path first and cause the general loop to skip (and
/// thereby orphan) the loose original. If `default_dir` already has its own sidecar (FR-002's
/// "don't overwrite" case) *and* a loose one is also present at the root, and the two disagree,
/// they cannot be silently reconciled — one of them is stale, and neither an unconditional
/// overwrite nor a silent skip is safe — so this returns an error instead of relocating anything,
/// including the other loose artifacts, until an operator resolves which sidecar is authoritative.
/// If instead the two are byte-identical, that is not a conflict: it is this function's own
/// leftover from a prior call whose `hard_link` succeeded but whose following `remove_file` failed
/// (e.g. a permission/mount change racing between the two calls) — that case self-heals by
/// removing the loose duplicate, consistent with every other crash point in this migration.
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

    // A loose `.wal-generation.json` (if one is somehow present — see `WAL_GENERATION_FILE`'s own
    // doc comment on why no genuine pre-378 install can have one) must be relocated *before*
    // `ensure_generation` mints a fresh record below. Otherwise `ensure_generation` would create
    // `default_dir`'s sidecar first, the relocation loop's `dest.exists()` check would then see it
    // and skip the loose file, and the loose file's real content would be silently orphaned at the
    // WAL root forever in favor of a freshly-minted, unrelated UUID.
    if let Some(loose_generation) = loose_entries
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(WAL_GENERATION_FILE))
    {
        let dest = default_dir.join(WAL_GENERATION_FILE);
        // Publish via `hard_link` + `remove_file`, not `rename`: a plain rename would silently
        // replace an already-existing `dest` if another process raced this migration on the same
        // `wal_root` between an `exists()` check and the rename call. `hard_link` fails closed
        // with `AlreadyExists` instead, giving the same conflict error with no TOCTOU window —
        // the same publish pattern `wal_generation::ensure_generation` itself uses for its mint.
        match fs::hard_link(loose_generation, &dest) {
            Ok(()) => fs::remove_file(loose_generation)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Not necessarily a genuine conflict: a prior call may have hard_link'd
                // successfully and then failed on this same remove_file (a permission/mount
                // change racing between the two calls), leaving `loose_generation` and `dest`
                // as two paths to identical content. That is this function's own unfinished
                // cleanup, not a disagreement between two authorities — recognize it by content
                // equality and self-heal by removing the leftover loose file, matching every
                // other crash point in this migration's "idempotent and crash-safe" guarantee.
                // Only a genuine content mismatch is left as a real conflict.
                let loose_bytes = fs::read(loose_generation)?;
                let dest_bytes = fs::read(&dest)?;
                if loose_bytes == dest_bytes {
                    fs::remove_file(loose_generation)?;
                } else {
                    return Err(Error::Ipc(format!(
                        "cannot migrate WAL root {wal_root:?}: a loose {loose_generation:?} and \
                         a destination {dest:?} both exist and disagree — reconcile which one is \
                         authoritative (delete whichever is stale) before retrying. No artifacts \
                         were relocated."
                    )));
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    crate::wal_generation::ensure_generation(&default_dir)?;

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

    /// A pre-378 `.checkpoints/` directory left loose at the WAL root's top level (migration
    /// hasn't run yet, or failed and was left in place non-fatally) must never be misreported as
    /// a real `group_id` in `knowledge_status`'s per-group map — it decodes to itself (no `%`),
    /// but fails the round-trip re-encode check since a leading `.` is outside the safe charset.
    #[test]
    fn list_group_wal_dirs_excludes_a_loose_legacy_checkpoints_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("liminis")).unwrap();
        fs::create_dir_all(tmp.path().join(CHECKPOINTS_DIR_NAME)).unwrap();

        let groups: Vec<String> = list_group_wal_dirs(tmp.path())
            .unwrap()
            .into_iter()
            .map(|(g, _)| g)
            .collect();
        assert_eq!(
            groups,
            vec!["liminis".to_string()],
            "a loose .checkpoints/ directory must never appear as a reported group"
        );
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

    /// FR-001/SC-002: relocating loose legacy content into the destination group's directory
    /// must also stamp a readable generation for it, so #414's unknown-generation guard doesn't
    /// refuse the very next rebuild.
    #[test]
    fn migrate_stamps_a_generation_for_the_relocated_group() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("20260101_000000_abcdef_0000.jsonl"),
            b"{}\n",
        )
        .unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        let default_dir = tmp.path().join("liminis");
        let generation = crate::wal_generation::read_generation(&default_dir);
        assert!(
            generation.is_some(),
            "migration must mint a generation for the group it relocated content into"
        );
    }

    /// FR-002: if the destination group already has a generation recorded (e.g. a partially
    /// completed prior migration, or the group already received writes through some other path),
    /// migration must not overwrite it with a fresh one.
    #[test]
    fn migrate_does_not_overwrite_an_existing_generation() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("20260101_000000_abcdef_0000.jsonl"),
            b"{}\n",
        )
        .unwrap();
        let default_dir = tmp.path().join("liminis");
        fs::create_dir_all(&default_dir).unwrap();
        let pre_existing = crate::wal_generation::ensure_generation(&default_dir).unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        assert_eq!(
            crate::wal_generation::read_generation(&default_dir),
            Some(pre_existing),
            "migration must not overwrite an already-recorded generation"
        );
    }

    /// Guards against the over-broad "stamp any unstamped group" sweep the spec explicitly
    /// rejects: a sibling group directory migration doesn't touch must not gain a generation
    /// stamp as a side effect of an unrelated migration run.
    #[test]
    fn migrate_does_not_stamp_a_sibling_group_it_did_not_relocate_content_into() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("20260101_000000_abcdef_0000.jsonl"),
            b"{}\n",
        )
        .unwrap();
        let sibling_dir = tmp.path().join("group-a");
        fs::create_dir_all(&sibling_dir).unwrap();
        fs::write(sibling_dir.join("0000.jsonl"), b"{}\n").unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        assert!(
            crate::wal_generation::read_generation(&sibling_dir).is_none(),
            "an untouched sibling group must not be stamped by this migration"
        );
    }

    /// Failure-injection regression for the recoverability gap CodeRabbit flagged on this PR: if
    /// `ensure_generation` fails (here, simulated via a read-only destination directory) on a run
    /// where it would otherwise be the last step, a naive "stamp after relocating" ordering would
    /// leave the group permanently generationless — the next call's top-level scan finds no loose
    /// entries left and takes the early return before ever retrying the stamp. Stamping *before*
    /// the relocation loop (this function's actual order) means the loose entries are still at the
    /// root when the stamp fails, so the next call is guaranteed to retry both.
    #[test]
    #[cfg(unix)]
    fn migrate_recovers_after_generation_stamp_fails_on_a_prior_call() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let loose_file = tmp.path().join("20260101_000000_abcdef_0000.jsonl");
        fs::write(&loose_file, b"{}\n").unwrap();

        let default_dir = tmp.path().join("liminis");
        fs::create_dir_all(&default_dir).unwrap();
        let mut perms = fs::metadata(&default_dir).unwrap().permissions();
        perms.set_mode(0o555); // read + execute, no write: blocks ensure_generation's tmp-file create
        fs::set_permissions(&default_dir, perms).unwrap();

        let result = migrate_wal_root_if_needed(tmp.path());
        assert!(
            result.is_err(),
            "the stamp attempt must fail while the destination directory is read-only"
        );
        assert!(
            loose_file.exists(),
            "relocation must not have happened yet — the stamp runs first and failed"
        );
        assert!(
            crate::wal_generation::read_generation(&default_dir).is_none(),
            "no generation should have been minted by the failed attempt"
        );

        let mut perms = fs::metadata(&default_dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&default_dir, perms).unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        assert!(
            !loose_file.exists(),
            "the retried migration must finish relocating the loose entry"
        );
        assert!(
            default_dir
                .join("20260101_000000_abcdef_0000.jsonl")
                .is_file(),
            "the loose entry must have landed in the destination group directory"
        );
        assert!(
            crate::wal_generation::read_generation(&default_dir).is_some(),
            "the retried migration must finish stamping a generation"
        );
    }

    /// Regression for the review finding on stamp-before-relocate ordering: a loose
    /// `.wal-generation.json` at the WAL root's top level (structurally impossible for a genuine
    /// pre-378 install, per `WAL_GENERATION_FILE`'s doc comment, but still recognized by
    /// `is_legacy_top_level_wal_artifact` "for consistency") must have its real content preserved
    /// by relocation, not silently discarded in favor of a freshly-minted UUID because
    /// `ensure_generation` got to occupy the destination path first.
    #[test]
    fn migrate_relocates_a_loose_generation_sidecar_instead_of_orphaning_it() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("20260101_000000_abcdef_0000.jsonl"),
            b"{}\n",
        )
        .unwrap();
        let loose_generation_file = tmp.path().join(".wal-generation.json");
        fs::write(&loose_generation_file, r#"{"generation":"original-value"}"#).unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        let default_dir = tmp.path().join("liminis");
        assert!(
            !loose_generation_file.exists(),
            "the loose sidecar must have been relocated, not left orphaned at the WAL root"
        );
        assert_eq!(
            crate::wal_generation::read_generation(&default_dir),
            Some("original-value".to_string()),
            "the relocated sidecar's original content must be preserved, not overwritten by a fresh mint"
        );
    }

    /// CodeRabbit finding on PR #433: a loose sidecar coexisting with an already-populated
    /// destination sidecar must not be silently reconciled by discarding one of them. Migration
    /// must refuse instead, and leave every artifact — including the other loose entries that
    /// have nothing to do with the conflict — exactly where it found them, so an operator can
    /// inspect and choose which value is authoritative.
    #[test]
    fn migrate_fails_instead_of_silently_reconciling_conflicting_generation_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let loose_jsonl = tmp.path().join("20260101_000000_abcdef_0000.jsonl");
        fs::write(&loose_jsonl, b"{}\n").unwrap();
        let loose_generation_file = tmp.path().join(".wal-generation.json");
        fs::write(&loose_generation_file, r#"{"generation":"loose-value"}"#).unwrap();

        let default_dir = tmp.path().join("liminis");
        fs::create_dir_all(&default_dir).unwrap();
        fs::write(
            default_dir.join(".wal-generation.json"),
            r#"{"generation":"destination-value"}"#,
        )
        .unwrap();

        let result = migrate_wal_root_if_needed(tmp.path());
        assert!(
            result.is_err(),
            "migration must refuse when the loose and destination generations conflict"
        );

        assert!(
            loose_jsonl.exists(),
            "unrelated loose artifacts must not be relocated while the conflict is unresolved"
        );
        assert!(
            loose_generation_file.exists(),
            "the loose generation sidecar must be left in place, not discarded"
        );
        assert_eq!(
            crate::wal_generation::read_generation(&default_dir),
            Some("destination-value".to_string()),
            "the destination's existing generation must be untouched"
        );
    }

    /// handarbeit-pruefer finding on PR #433: a loose sidecar and destination sidecar that are
    /// byte-identical are not a genuine conflict — they are this function's own leftover from a
    /// prior call whose `hard_link` succeeded but whose following `remove_file` failed. That case
    /// must self-heal (remove the loose duplicate and proceed) rather than permanently refusing
    /// with a "reconcile which is authoritative" error an operator cannot actually act on, since
    /// there is nothing to reconcile: the two paths already agree.
    #[test]
    fn migrate_self_heals_a_leftover_loose_sidecar_identical_to_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let loose_jsonl = tmp.path().join("20260101_000000_abcdef_0000.jsonl");
        fs::write(&loose_jsonl, b"{}\n").unwrap();
        let loose_generation_file = tmp.path().join(".wal-generation.json");
        fs::write(&loose_generation_file, r#"{"generation":"shared-value"}"#).unwrap();

        let default_dir = tmp.path().join("liminis");
        fs::create_dir_all(&default_dir).unwrap();
        // Simulates a prior call's hard_link succeeding and its remove_file failing: the two
        // paths hold identical content, as real hard-linked files would.
        fs::write(
            default_dir.join(".wal-generation.json"),
            r#"{"generation":"shared-value"}"#,
        )
        .unwrap();

        migrate_wal_root_if_needed(tmp.path()).unwrap();

        assert!(
            !loose_generation_file.exists(),
            "the leftover loose duplicate must be cleaned up, not left in place forever"
        );
        assert!(
            !loose_jsonl.exists(),
            "once the leftover is recognized as benign, the other loose artifacts must still be relocated"
        );
        assert_eq!(
            crate::wal_generation::read_generation(&default_dir),
            Some("shared-value".to_string()),
            "the destination's generation must be preserved unchanged"
        );
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

    #[test]
    fn collision_check_rejects_a_differently_cased_existing_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("Acme")).unwrap();

        let err = check_no_case_insensitive_collision(tmp.path(), "acme").unwrap_err();
        assert!(
            err.to_string().contains("collide"),
            "error must explain the case-insensitive collision: {err}"
        );
    }

    #[test]
    fn collision_check_allows_reopening_its_own_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("acme")).unwrap();

        // Same name, byte-identical — this is the ordinary "writer already exists" case, not a
        // fresh collision, and must not be rejected.
        check_no_case_insensitive_collision(tmp.path(), "acme").unwrap();
    }

    #[test]
    fn collision_check_allows_unrelated_sibling_names() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("group-a")).unwrap();

        check_no_case_insensitive_collision(tmp.path(), "group-b").unwrap();
    }

    #[test]
    fn collision_check_is_a_noop_for_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");

        check_no_case_insensitive_collision(&missing, "acme").unwrap();
    }
}
