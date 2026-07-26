//! On-disk, append-only JSONL cache for judge verdicts (FR-005/SC-003). Re-running the
//! harness against the same corpus and backends must make zero new judge calls for
//! previously-scored comparisons — this is what makes repeated runs affordable (judge
//! calls cost real money against a hosted model).
//!
//! The cache key scheme is ported from the prior Python harness's
//! `sha256(json({"cand": ..., "prompt": ..., "ref": ...}, sort_keys=True))[:24]`, extended
//! with a `judge_model` field: this harness (unlike the ported original, which pinned one
//! fixed judge model) exposes `--judge-model` as a run-time choice, so the key must include
//! it — otherwise switching judge models would silently reuse another model's verdicts
//! from the same cache file.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::judge::JudgeVerdict;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    matched: Vec<(usize, usize)>,
    unmatched_reference: Vec<usize>,
    unmatched_candidate: Vec<usize>,
}

/// Recursively sorts object keys alphabetically, mirroring Python's `json.dumps(...,
/// sort_keys=True)`. `serde_json` here is built with the `preserve_order` feature
/// (workspace-wide), so a `Map` serializes in insertion order — inserting already-sorted
/// key/value pairs makes the output match Python's `sort_keys=True` exactly, including
/// nested objects.
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonical_json(v)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (k, v) in entries {
                sorted.insert(k, v);
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

/// `sha256(json({"cand", "judge_model", "prompt", "ref"}, sort_keys=True))[:24]`.
pub fn cache_key(
    prompt_name: &str,
    judge_model: &str,
    reference: &Value,
    candidate: &Value,
) -> String {
    let value = json!({
        "cand": candidate,
        "judge_model": judge_model,
        "prompt": prompt_name,
        "ref": reference,
    });
    let canonical =
        serde_json::to_string(&canonical_json(&value)).expect("Value serialization is infallible");
    let digest = Sha256::digest(canonical.as_bytes());
    let hex = format!("{digest:x}");
    hex[..24].to_string()
}

pub struct JudgeCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, JudgeVerdict>>,
}

impl JudgeCache {
    /// Loads an existing cache from `path`, or starts empty if the file doesn't exist yet
    /// (a first run against a fresh `--judge-cache` path).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut entries = HashMap::new();

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                for (i, line) in content.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let entry: CacheEntry = serde_json::from_str(line).map_err(|e| {
                        format!(
                            "judge cache {} line {}: invalid JSON: {e}",
                            path.display(),
                            i + 1
                        )
                    })?;
                    entries.insert(
                        entry.key.clone(),
                        JudgeVerdict {
                            matched: entry.matched,
                            unmatched_reference: entry.unmatched_reference,
                            unmatched_candidate: entry.unmatched_candidate,
                        },
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "failed to read judge cache {}: {e}",
                    path.display()
                ))
            }
        }

        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    /// Returns the cached verdict for `key`, if present — a hit here must short-circuit
    /// before any judge call (SC-003).
    pub fn get(&self, key: &str) -> Option<JudgeVerdict> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    /// Records a freshly-judged verdict both durably on disk and in memory (append-only,
    /// matching `CassetteWriter`'s convention — re-opening never truncates). The lock is
    /// held across the whole call, not just the in-memory insert, so concurrent callers
    /// can't interleave partial JSONL lines on disk. The disk write happens *before* the
    /// in-memory insert: if the write fails, the verdict must not become visible to `get`,
    /// or a later cache hit in this same process would report success for a comparison
    /// that was never actually persisted.
    pub fn insert(&self, key: String, verdict: JudgeVerdict) -> Result<(), String> {
        let mut entries = self.entries.lock().unwrap();

        let entry = CacheEntry {
            key: key.clone(),
            matched: verdict.matched.clone(),
            unmatched_reference: verdict.unmatched_reference.clone(),
            unmatched_candidate: verdict.unmatched_candidate.clone(),
        };
        let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{line}").map_err(|e| e.to_string())?;

        entries.insert(key, verdict);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cache_key_is_stable_and_order_independent_of_construction() {
        let a = cache_key(
            "extract_nodes.extract_text",
            "claude-sonnet-4-6",
            &json!({"x": 1, "y": 2}),
            &json!({}),
        );
        let b = cache_key(
            "extract_nodes.extract_text",
            "claude-sonnet-4-6",
            &json!({"y": 2, "x": 1}),
            &json!({}),
        );
        assert_eq!(a, b, "key must not depend on JSON object insertion order");
    }

    #[test]
    fn cache_key_changes_with_prompt_name() {
        let a = cache_key(
            "extract_nodes.extract_text",
            "claude-sonnet-4-6",
            &json!({}),
            &json!({}),
        );
        let b = cache_key(
            "extract_edges.default",
            "claude-sonnet-4-6",
            &json!({}),
            &json!({}),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_with_reference_or_candidate() {
        let a = cache_key("p", "claude-sonnet-4-6", &json!({"a": 1}), &json!({}));
        let b = cache_key("p", "claude-sonnet-4-6", &json!({"a": 2}), &json!({}));
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_with_judge_model() {
        // FR-005/SC-003's cache scope includes judge_model: switching --judge-model must
        // not silently reuse another model's cached verdicts.
        let a = cache_key("p", "claude-sonnet-4-6", &json!({}), &json!({}));
        let b = cache_key("p", "claude-haiku-4-5-20251001", &json!({}), &json!({}));
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_is_24_hex_chars() {
        let k = cache_key("p", "claude-sonnet-4-6", &json!({}), &json!({}));
        assert_eq!(k.len(), 24);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn load_missing_file_starts_empty() {
        let dir = TempDir::new().unwrap();
        let cache = JudgeCache::load(dir.path().join("nonexistent.jsonl")).unwrap();
        assert!(cache.get("anything").is_none());
    }

    #[test]
    fn insert_then_get_roundtrips() {
        let dir = TempDir::new().unwrap();
        let cache = JudgeCache::load(dir.path().join("cache.jsonl")).unwrap();
        let verdict = JudgeVerdict {
            matched: vec![(0, 0)],
            unmatched_reference: vec![],
            unmatched_candidate: vec![],
        };
        cache.insert("k1".to_string(), verdict.clone()).unwrap();
        assert_eq!(cache.get("k1"), Some(verdict));
    }

    #[test]
    fn persists_across_reload_sc003() {
        // SC-003: a second run against the same cache path serves prior entries from disk
        // without needing any fresh judge call.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.jsonl");
        let verdict = JudgeVerdict {
            matched: vec![(0, 0), (1, 1)],
            unmatched_reference: vec![],
            unmatched_candidate: vec![],
        };
        {
            let cache = JudgeCache::load(&path).unwrap();
            cache.insert("k1".to_string(), verdict.clone()).unwrap();
        }
        let reloaded = JudgeCache::load(&path).unwrap();
        assert_eq!(reloaded.get("k1"), Some(verdict));
    }

    #[test]
    fn insert_appends_without_truncating_across_reloads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.jsonl");
        {
            let cache = JudgeCache::load(&path).unwrap();
            cache
                .insert("k1".to_string(), JudgeVerdict::default())
                .unwrap();
        }
        {
            let cache = JudgeCache::load(&path).unwrap();
            cache
                .insert("k2".to_string(), JudgeVerdict::default())
                .unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.lines().count(),
            2,
            "re-opening must append, not truncate"
        );
    }
}
