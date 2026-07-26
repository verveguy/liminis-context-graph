//! Loads the #217 public Wikipedia corpus fixture (`corpus_prose.jsonl`) that this
//! harness reuses rather than curating a second corpus (FR-009).

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CorpusChunk {
    pub title: String,
    pub revision_id: u64,
    pub prose: String,
}

/// The #217 fixture's committed path, resolved relative to this crate's manifest
/// directory so the harness works regardless of the caller's current directory.
pub fn default_corpus_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl")
}

/// Loads every non-header record from a `corpus_prose.jsonl`-formatted file. The first
/// line (`{"_header": true, ...}`) is metadata, not a corpus chunk, and is skipped.
pub fn load_corpus(path: impl AsRef<Path>) -> Result<Vec<CorpusChunk>, String> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read corpus {}: {e}", path.display()))?;

    let mut chunks = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "corpus {} line {}: invalid JSON: {e}",
                path.display(),
                i + 1
            )
        })?;
        if value
            .get("_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let chunk: CorpusChunk = serde_json::from_value(value)
            .map_err(|e| format!("corpus {} line {}: {e}", path.display(), i + 1))?;
        chunks.push(chunk);
    }
    Ok(chunks)
}

/// `limit = None` returns every loaded chunk (`--all`); `Some(n)` deterministically
/// takes the first `n` (file order is already fixed/committed) — the affordable-default
/// behavior (Plan: "Default corpus subset = first 50 chunks, not all 228").
pub fn select_subset(chunks: Vec<CorpusChunk>, limit: Option<usize>) -> Vec<CorpusChunk> {
    match limit {
        Some(n) => chunks.into_iter().take(n).collect(),
        None => chunks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    #[test]
    fn skips_header_line() {
        let f = write_fixture(&[
            r#"{"_header": true, "record_count": 2}"#,
            r#"{"title": "A", "revision_id": 1, "prose": "prose a"}"#,
            r#"{"title": "B", "revision_id": 2, "prose": "prose b"}"#,
        ]);
        let chunks = load_corpus(f.path()).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].title, "A");
        assert_eq!(chunks[1].title, "B");
    }

    #[test]
    fn skips_blank_lines() {
        let f = write_fixture(&[
            r#"{"_header": true}"#,
            "",
            r#"{"title": "A", "revision_id": 1, "prose": "prose a"}"#,
            "   ",
        ]);
        let chunks = load_corpus(f.path()).unwrap();
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn invalid_json_line_is_a_descriptive_error() {
        let f = write_fixture(&[r#"{"_header": true}"#, "not json"]);
        let err = load_corpus(f.path()).unwrap_err();
        assert!(err.contains("line 2"));
    }

    #[test]
    fn missing_file_is_a_descriptive_error() {
        let err = load_corpus("/nonexistent/path/corpus_prose.jsonl").unwrap_err();
        assert!(err.contains("failed to read corpus"));
    }

    #[test]
    fn select_subset_limit_is_deterministic_prefix() {
        let chunks: Vec<CorpusChunk> = (0..10)
            .map(|i| CorpusChunk {
                title: format!("T{i}"),
                revision_id: i,
                prose: String::new(),
            })
            .collect();
        let subset = select_subset(chunks.clone(), Some(3));
        assert_eq!(subset.len(), 3);
        assert_eq!(subset[0].title, "T0");
        assert_eq!(subset[2].title, "T2");
    }

    #[test]
    fn select_subset_none_returns_everything() {
        let chunks: Vec<CorpusChunk> = (0..5)
            .map(|i| CorpusChunk {
                title: format!("T{i}"),
                revision_id: i,
                prose: String::new(),
            })
            .collect();
        assert_eq!(select_subset(chunks, None).len(), 5);
    }

    #[test]
    fn select_subset_limit_larger_than_corpus_is_clamped() {
        let chunks: Vec<CorpusChunk> = (0..3)
            .map(|i| CorpusChunk {
                title: format!("T{i}"),
                revision_id: i,
                prose: String::new(),
            })
            .collect();
        assert_eq!(select_subset(chunks, Some(100)).len(), 3);
    }

    #[test]
    fn default_corpus_path_points_at_the_217_fixture() {
        let path = default_corpus_path();
        let canonical = path
            .canonicalize()
            .unwrap_or_else(|e| panic!("default corpus path {} not found: {e}", path.display()));
        assert!(
            canonical.ends_with("crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl"),
            "unexpected default corpus path: {}",
            canonical.display()
        );
    }

    #[test]
    fn loads_the_real_217_fixture() {
        let chunks = load_corpus(default_corpus_path()).unwrap();
        assert_eq!(
            chunks.len(),
            228,
            "expected the committed #217 corpus to have 228 chunks"
        );
        assert!(!chunks[0].prose.is_empty());
    }
}
