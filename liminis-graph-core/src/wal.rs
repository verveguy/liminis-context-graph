use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Error;

/// One WAL record — five-field JSONL schema matching the Python `graphiti_core/driver/wal.py`.
/// Fields are declared in `seq, ts, db, cypher, params` order; serde_json preserves
/// struct field declaration order, matching Python's `json.dumps()` dict insertion order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalLine {
    pub seq: u64,
    pub ts: String,
    pub db: String,
    pub cypher: String,
    pub params: serde_json::Value,
}

/// Appends WAL lines atomically per chunk to `.graphiti/wal/` JSONL files.
pub struct WalWriter {
    wal_dir: PathBuf,
    global_seq: u64,
    file_seq: u32,
    events_in_current_file: usize,
    max_events_per_file: usize,
    session_id: String,
    pending_lines: Vec<WalLine>,
    current_file: Option<PathBuf>,
}

impl WalWriter {
    /// Opens (or creates) the WAL directory and scans existing files to determine the
    /// starting global sequence number.
    pub fn new(wal_dir: impl Into<PathBuf>, max_events_per_file: usize) -> Result<Self, Error> {
        let wal_dir = wal_dir.into();
        fs::create_dir_all(&wal_dir)?;

        let global_seq = scan_max_seq(&wal_dir)?;
        let session_id = Uuid::new_v4()
            .as_simple()
            .to_string()
            .chars()
            .take(6)
            .collect::<String>();

        Ok(Self {
            wal_dir,
            global_seq,
            file_seq: 0,
            events_in_current_file: 0,
            max_events_per_file,
            session_id,
            pending_lines: Vec::new(),
            current_file: None,
        })
    }

    /// Buffers a mutation. Filters out reads and index DDL; must be called inside `with_chunk`.
    pub fn log_mutation(
        &mut self,
        cypher: &str,
        params: serde_json::Value,
        database: &str,
    ) -> Result<(), Error> {
        // Filter index DDL before first-token check (higher priority per AD-W7).
        let upper = cypher.to_uppercase();
        if upper.contains("CREATE_VECTOR_INDEX")
            || upper.contains("CREATE INDEX")
            || upper.contains("DROP INDEX")
        {
            return Ok(());
        }

        let first_token = cypher
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_uppercase();

        let is_mutation = matches!(
            first_token.as_str(),
            "CREATE" | "MERGE" | "SET" | "DELETE" | "DETACH" | "DROP" | "REMOVE"
        );
        if !is_mutation {
            return Ok(());
        }

        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string();
        let line = WalLine {
            seq: self.global_seq,
            ts,
            db: database.to_string(),
            cypher: cypher.to_string(),
            params,
        };
        self.global_seq += 1;
        self.pending_lines.push(line);
        Ok(())
    }

    /// Chunk-atomic write: runs `f`; on `Ok` flushes pending lines to one file; on `Err`
    /// discards the buffer (R-02 invariant).
    pub fn with_chunk<F, T>(&mut self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut WalWriter) -> Result<T, Error>,
    {
        self.pending_lines.clear();
        let result = f(self);
        match result {
            Ok(val) => {
                self.flush_pending()?;
                Ok(val)
            }
            Err(e) => {
                self.pending_lines.clear();
                Err(e)
            }
        }
    }

    fn flush_pending(&mut self) -> Result<(), Error> {
        let chunk_len = self.pending_lines.len();
        if chunk_len == 0 {
            return Ok(());
        }

        // Rotate if: no file open, or appending chunk would exceed max_events_per_file.
        let needs_new_file = self.current_file.is_none()
            || (self.events_in_current_file > 0
                && self.events_in_current_file + chunk_len > self.max_events_per_file);

        if needs_new_file {
            self.current_file = Some(self.make_new_file_path());
            self.events_in_current_file = 0;
        }

        let path = self.current_file.as_ref().unwrap();
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = BufWriter::new(file);
        for line in &self.pending_lines {
            let json = serde_json::to_string(line).map_err(|e| Error::WalJson(e.to_string()))?;
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;

        self.events_in_current_file += chunk_len;
        self.pending_lines.clear();
        Ok(())
    }

    fn make_new_file_path(&mut self) -> PathBuf {
        let now = Utc::now();
        let path = self.wal_dir.join(format!(
            "{}_{}_{:04}.jsonl",
            now.format("%Y%m%d_%H%M%S"),
            self.session_id,
            self.file_seq,
        ));
        self.file_seq += 1;
        path
    }

    /// Returns pending line count (for tests).
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending_lines.len()
    }
}

/// Reads all `.jsonl` files in `wal_dir` (reverse lexicographic) and returns `max_seq + 1`,
/// or 0 if no lines are found. Tolerates truncated final lines.
fn scan_max_seq(wal_dir: &Path) -> Result<u64, Error> {
    let mut files: Vec<PathBuf> = fs::read_dir(wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();

    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    let mut max_seq: Option<u64> = None;
    for path in &files {
        if let Some(seq) = read_last_seq(path)? {
            max_seq = Some(match max_seq {
                None => seq,
                Some(m) => m.max(seq),
            });
        }
    }

    Ok(max_seq.map(|s| s + 1).unwrap_or(0))
}

/// Returns the `seq` from the last parseable non-empty line in the file, or `None`.
fn read_last_seq(path: &Path) -> Result<Option<u64>, Error> {
    let content = fs::read(path)?;
    let text = String::from_utf8_lossy(&content);
    for raw in text.lines().rev() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(seq) = val.get("seq").and_then(|v| v.as_u64()) {
                return Ok(Some(seq));
            }
        }
    }
    Ok(None)
}
