//! On-disk WAL-layout inspection helpers for FR-003's per-group directory assertions (issue
//! #394). Ported from the reference Python harness's `wal_snapshot`/`group_ids_in`
//! (`multistream_test.py`) — the property these exist to check (which group's `.jsonl` stream
//! received which mutations, and that groups not party to an operation are unchanged) is only
//! visible on disk, never from an MCP response alone.

use std::collections::{BTreeMap, HashMap};
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

/// Recursively walks a WAL line's parsed `params` JSON for `group_id` string values, counting
/// occurrences. Mirrors the reference Python harness's `group_ids_in`/`walk` closure exactly —
/// this is the property FR-003's "which group's mutations landed in which stream" assertions
/// depend on, since a single WAL line's `params` can nest a `group_id` at any depth (e.g. a
/// cross-group edge's endpoint spec).
pub fn group_ids_in(lines: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for line in lines {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let mut found = std::collections::HashSet::new();
        walk_value(
            rec.get("params").unwrap_or(&serde_json::Value::Null),
            &mut found,
        );
        for g in found {
            *counts.entry(g).or_insert(0) += 1;
        }
    }
    counts
}

fn walk_value(v: &serde_json::Value, found: &mut std::collections::HashSet<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, vv) in map {
                if k == "group_id" {
                    if let Some(s) = vv.as_str() {
                        found.insert(s.to_string());
                    }
                }
                walk_value(vv, found);
            }
        }
        serde_json::Value::Array(arr) => {
            for vv in arr {
                walk_value(vv, found);
            }
        }
        _ => {}
    }
}
