//! On-disk WAL-layout inspection helpers for FR-003's per-group directory assertions (issue
//! #394). Ported from the reference Python harness's `wal_snapshot` (`multistream_test.py`) —
//! the property this exists to check (which group's `.jsonl` stream received which mutations,
//! and that groups not party to an operation are unchanged) is only visible on disk, never from
//! an MCP response alone.
//!
//! The Python reference's other helper, `group_ids_in` (a recursive walk over a WAL line's
//! `params` JSON for `group_id` keys), was ported too but deliberately dropped after review: for
//! every mutation shape this test's assertions actually depend on, it cannot detect the
//! misattribution it would be used to guard against. A plain entity/relationship CREATE embeds
//! `group_id` as a literal top-level param that trivially matches the directory it's already
//! routed to (the same value drives both), so checking it is tautological. Cross-group pointer
//! data (the foreign `source_group_id` in particular) is serialized into the `attributes`
//! Cypher parameter as an opaque JSON *string* (`pointer::write_pointers`), which a
//! `Value::Object`/`Value::Array`-only walk can never see inside — and the actual
//! pointer-mutating statement, `update_relates_to_attributes` (`MATCH (rn:RelatesToNode_
//! {uuid: $uuid}) SET rn.attributes = $attributes`), carries no `group_id` param at all, foreign
//! or otherwise. Attribution for those mutations is provable only by which directory the line
//! physically landed in (`wal_snapshot`) and by the pointer's reported `binding_state`/
//! `source_group_id` via the MCP API (`c_bindings` in `mcp_multistream_e2e.rs`), which is what
//! this test actually asserts on.

use std::collections::BTreeMap;
use std::path::Path;

/// The `.jsonl` files found directly inside one WAL-root subdirectory, as `(file_name, lines)`
/// pairs. Non-`.jsonl` files are ignored — the reference Python harness's `extras` tracking is
/// dropped since no ported assertion uses it.
#[derive(Debug, Clone, Default)]
pub struct WalDirSnapshot {
    pub jsonl_files: Vec<(String, Vec<String>)>,
}

/// Recursively snapshots every subdirectory under `wal_root` that contains at least one
/// `.jsonl` file, keyed by its path relative to `wal_root` (`"."` for the root itself). Returns
/// an empty map if `wal_root` does not exist yet (e.g. before the first write).
pub fn wal_snapshot(wal_root: &Path) -> BTreeMap<String, WalDirSnapshot> {
    let mut out = BTreeMap::new();
    if wal_root.exists() {
        walk_dir(wal_root, wal_root, &mut out);
    }
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut BTreeMap<String, WalDirSnapshot>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut snapshot = WalDirSnapshot::default();
    let mut subdirs = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let lines = std::fs::read_to_string(&path)
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default();
            let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
            snapshot.jsonl_files.push((file_name, lines));
        }
    }
    if !snapshot.jsonl_files.is_empty() {
        let rel = dir
            .strip_prefix(root)
            .map(|p| {
                if p.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    p.to_string_lossy().into_owned()
                }
            })
            .unwrap_or_else(|_| ".".to_string());
        snapshot.jsonl_files.sort_by(|a, b| a.0.cmp(&b.0));
        out.insert(rel, snapshot);
    }
    for subdir in subdirs {
        walk_dir(root, &subdir, out);
    }
}
