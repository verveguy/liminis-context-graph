use std::fs;
use std::path::PathBuf;

use crate::db::Conn;
use crate::error::Error;
use crate::wal::WalLine;

/// Statistics returned from a WAL replay run.
pub struct ReplayStats {
    pub lines_replayed: u64,
    pub lines_skipped:  u64,
    pub files_read:     u64,
}

/// Replays all `.jsonl` WAL files in lexicographic filename order against a LadybugDB connection.
pub struct WalReplayer {
    wal_dir: PathBuf,
}

impl WalReplayer {
    pub fn new(wal_dir: impl Into<PathBuf>) -> Self {
        Self { wal_dir: wal_dir.into() }
    }

    /// Reads all JSONL files, executes known mutations, skips truncated/unknown lines (R-05, R-08).
    pub fn replay(&self, conn: &Conn<'_>) -> Result<ReplayStats, Error> {
        let mut stats = ReplayStats {
            lines_replayed: 0,
            lines_skipped:  0,
            files_read:     0,
        };

        if !self.wal_dir.exists() {
            return Ok(stats);
        }

        let mut files: Vec<PathBuf> = fs::read_dir(&self.wal_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();

        // Lexicographic order — ISO-8601 timestamp prefix ensures chronological order (R-07).
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for file_path in &files {
            stats.files_read += 1;
            let content = fs::read_to_string(file_path)?;
            let raw_lines: Vec<&str> = content.lines().collect();

            for (i, raw) in raw_lines.iter().enumerate() {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }

                let wal_line: WalLine = match serde_json::from_str(raw) {
                    Ok(l) => l,
                    Err(_) => {
                        eprintln!(
                            "[WAL WARN] skipping unparseable line {} in {:?}",
                            i + 1,
                            file_path
                        );
                        stats.lines_skipped += 1;
                        continue;
                    }
                };

                let first_token = wal_line
                    .cypher
                    .trim()
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_uppercase();

                let is_known = matches!(
                    first_token.as_str(),
                    "CREATE" | "MERGE" | "SET" | "DELETE" | "DETACH" | "DROP" | "REMOVE"
                );

                if !is_known {
                    eprintln!("[WAL WARN] skipping unknown op: {first_token}");
                    stats.lines_skipped += 1;
                    continue;
                }

                let cypher = interpolate_params(&wal_line.cypher, &wal_line.params);
                match conn.raw_query(&cypher) {
                    Ok(_) => stats.lines_replayed += 1,
                    Err(e) => {
                        eprintln!(
                            "[WAL WARN] replay execution error at line {} in {:?}: {}",
                            i + 1,
                            file_path,
                            e
                        );
                        stats.lines_skipped += 1;
                    }
                }
            }
        }

        Ok(stats)
    }
}

/// Substitutes `$key` placeholders in `cypher` with Cypher literal representations of the
/// corresponding JSON values. Params are processed longest-name-first to avoid partial
/// substitution of shorter names that are prefixes of longer ones (e.g., `$name` vs
/// `$name_embedding`).
fn interpolate_params(cypher: &str, params: &serde_json::Value) -> String {
    let serde_json::Value::Object(map) = params else {
        return cypher.to_string();
    };
    if map.is_empty() {
        return cypher.to_string();
    }

    let mut pairs: Vec<(&str, &serde_json::Value)> =
        map.iter().map(|(k, v)| (k.as_str(), v)).collect();
    // Longest key first to prevent $name from clobbering $name_embedding.
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut result = cypher.to_string();
    for (key, val) in pairs {
        let placeholder = format!("${key}");
        result = result.replace(&placeholder, &json_to_cypher_literal(val));
    }
    result
}

/// Converts a serde_json::Value to a Cypher literal string.
fn json_to_cypher_literal(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        serde_json::Value::Array(arr) => {
            let items: Vec<_> = arr.iter().map(json_to_cypher_literal).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(obj) => {
            let pairs: Vec<_> = obj
                .iter()
                .map(|(k, v)| format!("{k}: {}", json_to_cypher_literal(v)))
                .collect();
            format!("{{{}}}", pairs.join(", "))
        }
    }
}

#[cfg(test)]
mod interpolate_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_interpolate_string_param() {
        let cypher = "MERGE (n:Entity {uuid: $uuid})";
        let params = json!({"uuid": "abc-123"});
        let result = interpolate_params(cypher, &params);
        assert_eq!(result, "MERGE (n:Entity {uuid: 'abc-123'})");
    }

    #[test]
    fn test_interpolate_longest_first_avoids_partial_match() {
        let cypher = "SET n.name_embedding = $name_embedding, n.name = $name";
        let params = json!({"name": "Alice", "name_embedding": [1.0, 0.0]});
        let result = interpolate_params(cypher, &params);
        assert!(result.contains("[1.0, 0.0]"), "embedding should be an array");
        assert!(result.contains("'Alice'"), "name should be a string");
        assert!(
            !result.contains("'Alice'_embedding"),
            "must not partially replace $name_embedding"
        );
    }

    #[test]
    fn test_interpolate_nested_object() {
        let cypher = "SET n += $props";
        let params = json!({"props": {"name": "Alice", "age": 30}});
        let result = interpolate_params(cypher, &params);
        assert!(result.contains("SET n += {"), "should produce map literal");
    }

    #[test]
    fn test_json_to_cypher_literal_string() {
        let val = serde_json::Value::String("it's here".to_string());
        assert_eq!(json_to_cypher_literal(&val), "'it''s here'");
    }

    #[test]
    fn test_json_to_cypher_literal_array() {
        let val = json!([1.0, 2.0]);
        assert_eq!(json_to_cypher_literal(&val), "[1.0, 2.0]");
    }
}
