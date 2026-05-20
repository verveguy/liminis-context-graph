use std::fs;
use std::io::{BufRead, BufReader};
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
            let file = fs::File::open(file_path)?;
            let reader = BufReader::new(file);

            for (i, line_result) in reader.lines().enumerate() {
                // A truncated final line that ends with invalid UTF-8 (crash during write)
                // produces an io::Error here — skip it, satisfying R-05.
                let raw = match line_result {
                    Ok(l) => l,
                    Err(_) => {
                        eprintln!(
                            "[WAL WARN] skipping unreadable line {} in {:?}",
                            i + 1,
                            file_path
                        );
                        stats.lines_skipped += 1;
                        continue;
                    }
                };
                let raw = raw.trim().to_string();
                if raw.is_empty() {
                    continue;
                }

                let wal_line: WalLine = match serde_json::from_str(&raw) {
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
/// corresponding JSON values. Uses a single left-to-right pass so already-substituted literal
/// text is never re-scanned, preventing double-interpolation if a value contains `$key` patterns.
/// Longest-key matching at each `$` prevents `$name` from consuming part of `$name_embedding`.
fn interpolate_params(cypher: &str, params: &serde_json::Value) -> String {
    let serde_json::Value::Object(map) = params else {
        return cypher.to_string();
    };
    if map.is_empty() {
        return cypher.to_string();
    }

    let mut pairs: Vec<(&str, &serde_json::Value)> =
        map.iter().map(|(k, v)| (k.as_str(), v)).collect();
    // Longest key first so that at each `$` position we greedily match the longest param name.
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut result = String::with_capacity(cypher.len());
    let mut remaining = cypher;
    while let Some(dollar_pos) = remaining.find('$') {
        result.push_str(&remaining[..dollar_pos]);
        let after_dollar = &remaining[dollar_pos + 1..];
        // Try each key (longest first) to find a match immediately after `$`.
        if let Some((k, v)) = pairs.iter().find(|(k, _)| after_dollar.starts_with(k)) {
            result.push_str(&json_to_cypher_literal(v));
            remaining = &remaining[dollar_pos + 1 + k.len()..];
        } else {
            // `$` not followed by a known key — emit it literally.
            result.push('$');
            remaining = after_dollar;
        }
    }
    result.push_str(remaining);
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

    #[test]
    fn test_no_double_interpolation_when_value_contains_placeholder() {
        // If param 'a' has value "$b" and param 'b' has value "secret", a multi-pass replace
        // would substitute "$b" → "secret", producing "secret" in the result. Single-pass must not.
        let cypher = "SET n.x = $a, n.y = $b";
        let params = json!({"a": "$b", "b": "secret"});
        let result = interpolate_params(cypher, &params);
        // $a must expand to the literal string '$b' (escaped), not to 'secret'.
        assert!(result.contains("'$b'"), "value containing placeholder must not be re-expanded");
        assert!(result.contains("'secret'"), "$b must still expand to 'secret'");
    }
}
