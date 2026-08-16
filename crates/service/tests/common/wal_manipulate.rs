//! Raw on-disk WAL manipulation helpers for issue #400's generation-reset (#387), ambiguous
//! (#369) and missing-generation-sidecar (#414) phases in `mcp_multistream_e2e.rs`.
//!
//! Ported from `crates/core/tests/wal_generation_reset.rs`'s own local helpers of the same
//! names, which are not exported from `lcg_core` (they're plain `#[cfg(test)]`-free functions
//! defined in that one test file). This is the direct precedent for simulating an
//! externally-republished WAL stream: delete/rewrite `.jsonl`/`.wal-generation.json` files on
//! disk between MCP calls, relying on `knowledge_status`/`knowledge_rebuild_from_wal` always
//! reading WAL-directory state fresh off disk (ADR-0353/0387), never from any cached field.
//!
//! One deliberate difference: `entity_wal_line` here embeds a 768-float embedding literal, not
//! the 4-float one `wal_generation_reset.rs` uses — that suite opens its own 4-dim `Db`, while
//! this e2e test's live process is schema-initialized against `spawn_stub_embedder`'s fixed
//! 768-dim response, so a raw-injected `name_embedding` must match that dimension.

use std::path::{Path, PathBuf};

use lcg_core::{wal_generation, wal_group};

/// A standalone `Entity` WAL line, named after its own uuid unless `name` is given.
pub fn entity_wal_line(seq: u64, uuid: &str, name: &str, group_id: &str) -> String {
    let embedding = vec!["0.0"; 768].join(", ");
    format!(
        r#"{{"seq":{seq},"ts":"2026-08-16T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{name}', n.group_id = '{group_id}', n.labels = ['Entity'], n.created_at = timestamp('2026-08-16 00:00:00'), n.name_embedding = [{embedding}], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

pub fn group_dir(wal_root: &Path, group_id: &str) -> PathBuf {
    wal_group::group_wal_dir(wal_root, group_id).unwrap()
}

/// Writes `lines` into a fresh `.jsonl` file in `group_id`'s directory. Never deletes or touches
/// the generation sidecar — used both for a group's first raw-injected content and for ordinary
/// continued-ingest appends.
pub fn write_group_wal(wal_root: &Path, group_id: &str, file_name: &str, lines: &[String]) {
    let dir = group_dir(wal_root, group_id);
    std::fs::create_dir_all(&dir).unwrap();
    let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(dir.join(file_name), content).unwrap();
}

pub fn set_group_generation(wal_root: &Path, group_id: &str, generation: &str) {
    let dir = group_dir(wal_root, group_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".wal-generation.json"),
        format!(r#"{{"generation":"{generation}"}}"#),
    )
    .unwrap();
}

pub fn read_group_generation(wal_root: &Path, group_id: &str) -> Option<String> {
    wal_generation::read_generation(&group_dir(wal_root, group_id))
}

/// Simulates an external publisher resetting `group_id`'s stream: every existing `.jsonl` file
/// is removed and replaced by a fresh set restarting at `seq: 0`, and the generation sidecar is
/// overwritten with `new_generation` — a genuinely different stream, not a continuation of the
/// old one.
pub fn reset_group_stream(
    wal_root: &Path,
    group_id: &str,
    file_name: &str,
    lines: &[String],
    new_generation: &str,
) {
    let dir = group_dir(wal_root, group_id);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    write_group_wal(wal_root, group_id, file_name, lines);
    set_group_generation(wal_root, group_id, new_generation);
}

/// Simulates a mis-published stream (#414): removes `.wal-generation.json` from `group_id`'s
/// directory while leaving its `.jsonl` content untouched — the shape a publish step has in
/// practice when it globs `*.jsonl` only and silently drops the dot-namespaced sidecar.
pub fn remove_group_generation(wal_root: &Path, group_id: &str) {
    let sidecar = group_dir(wal_root, group_id).join(".wal-generation.json");
    if sidecar.exists() {
        std::fs::remove_file(sidecar).unwrap();
    }
}
