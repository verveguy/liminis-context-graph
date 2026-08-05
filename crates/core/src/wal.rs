use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Error;

/// Metadata captured at the moment a WAL file is closed by rotation.
#[derive(Debug, Clone)]
pub struct WalRotationInfo {
    pub from_file_seq: u32,
    pub to_file_seq: u32,
    pub closed_bytes: u64,
    pub closed_events: usize,
}

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

/// Appends WAL lines atomically per chunk to the configured WAL dir (typically `.lcg/wal/`).
pub struct WalWriter {
    wal_dir: PathBuf,
    global_seq: u64,
    file_seq: u32,
    events_in_current_file: usize,
    max_events_per_file: usize,
    max_bytes_per_file: u64,
    bytes_in_current_file: u64,
    session_id: String,
    pending_lines: Vec<WalLine>,
    current_file: Option<PathBuf>,
    last_rotation: Option<WalRotationInfo>,
}

impl WalWriter {
    /// Opens (or creates) the WAL directory and scans existing files to determine the
    /// starting global sequence number.
    ///
    /// `max_bytes_per_file = 0` disables byte-size rotation (only event-count applies).
    pub fn new(
        wal_dir: impl Into<PathBuf>,
        max_events_per_file: usize,
        max_bytes_per_file: u64,
    ) -> Result<Self, Error> {
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
            max_bytes_per_file,
            bytes_in_current_file: 0,
            session_id,
            pending_lines: Vec::new(),
            current_file: None,
            last_rotation: None,
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
        if is_index_ddl(cypher) {
            return Ok(());
        }

        if !looks_like_mutation(cypher) {
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

        fs::create_dir_all(&self.wal_dir)?;

        // Pre-serialize all lines so we can compute chunk_bytes for byte-size rotation.
        let jsons: Vec<String> = self
            .pending_lines
            .iter()
            .map(|l| serde_json::to_string(l).map_err(|e| Error::WalJson(e.to_string())))
            .collect::<Result<_, _>>()?;
        // Each line occupies json.len() + 1 bytes (the '\n').
        let chunk_bytes: u64 = jsons.iter().map(|s| (s.len() + 1) as u64).sum();

        if self.max_bytes_per_file > 0 && chunk_bytes > self.max_bytes_per_file {
            eprintln!(
                "[WAL WARN] chunk ({chunk_bytes} bytes) exceeds max_bytes_per_file ({}); writing anyway",
                self.max_bytes_per_file
            );
        }

        // Rotate if: no file open, event count would exceed max, or byte size would exceed max.
        let needs_new_file = self.current_file.is_none()
            || (self.events_in_current_file > 0
                && self.events_in_current_file + chunk_len > self.max_events_per_file)
            || (self.max_bytes_per_file > 0
                && self.bytes_in_current_file > 0
                && self.bytes_in_current_file + chunk_bytes > self.max_bytes_per_file);

        if needs_new_file {
            // Capture rotation info before resetting counters.
            if self.current_file.is_some() {
                self.last_rotation = Some(WalRotationInfo {
                    from_file_seq: self.file_seq - 1,
                    to_file_seq: self.file_seq,
                    closed_bytes: self.bytes_in_current_file,
                    closed_events: self.events_in_current_file,
                });
            }
            self.current_file = Some(self.make_new_file_path());
            self.events_in_current_file = 0;
            self.bytes_in_current_file = 0;
        }

        let path = self.current_file.as_ref().unwrap();
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = BufWriter::new(file);
        for json in &jsons {
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;

        self.events_in_current_file += chunk_len;
        self.bytes_in_current_file += chunk_bytes;
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

    /// Force-closes the current WAL file (if open) so the next write opens a fresh file.
    /// Since writes are flushed and fsynced per chunk, rotation only resets `current_file`.
    /// Returns `(files_rotated, files_total)` — `files_rotated` is 0 or 1.
    pub fn rotate(&mut self) -> (u32, u32) {
        let files_rotated = if self.current_file.take().is_some() {
            self.last_rotation = Some(WalRotationInfo {
                from_file_seq: self.file_seq - 1,
                to_file_seq: self.file_seq,
                closed_bytes: self.bytes_in_current_file,
                closed_events: self.events_in_current_file,
            });
            self.bytes_in_current_file = 0;
            self.events_in_current_file = 0;
            1
        } else {
            0
        };
        let files_total = count_jsonl_files(&self.wal_dir);
        (files_rotated, files_total)
    }

    /// Drains and returns rotation info if a rotation occurred since the last call.
    /// Returns `None` if no rotation has happened. Intended to be called after `with_chunk`.
    pub fn take_rotation(&mut self) -> Option<WalRotationInfo> {
        self.last_rotation.take()
    }

    /// Returns pending line count (for tests).
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending_lines.len()
    }

    /// Re-derives `global_seq` after a rebuild/clear that may have replayed a WAL directory
    /// populated after this writer was constructed (issue #352). Combines a fresh on-disk scan
    /// (the same `scan_max_seq` used at startup) with the replay's own `last_committed_seq` —
    /// a failed-to-replay line can still have a higher on-disk `seq` than the last commit, so
    /// neither source alone is trustworthy (FR-003). Monotonic: only ever raises `global_seq`
    /// (`max(current, scanned, last_committed_seq + 1)`), never lowers it (FR-005), so this is a
    /// safe no-op when called over an empty or low-max-seq WAL directory.
    pub fn resync_global_seq(&mut self, last_committed_seq: Option<u64>) -> Result<(), Error> {
        let scanned = scan_max_seq(&self.wal_dir)?;
        let from_commit = last_committed_seq.map(|s| s.saturating_add(1)).unwrap_or(0);
        self.global_seq = self.global_seq.max(scanned).max(from_commit);
        Ok(())
    }

    /// Returns the current `global_seq` (for tests).
    #[cfg(test)]
    pub fn global_seq(&self) -> u64 {
        self.global_seq
    }
}

fn count_jsonl_files(dir: &Path) -> u32 {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .count() as u32
        })
        .unwrap_or(0)
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

/// Returns a copy of `s` with Cypher single-quoted string literals replaced by a single space.
/// Handles `\'` escape sequences inside literals.  Used by `log_mutation` to prevent DML
/// keywords that happen to appear inside stored string values from being misclassified as
/// mutation queries.
///
/// Limitation: Cypher line comments (`//`) and block comments (`/* … */`) are not stripped.
/// A keyword appearing only inside a comment will pass through and trigger a false-positive
/// mutation classification. In practice, neither graphiti nor liminis-graph emits commented
/// Cypher in WAL lines, so this gap is benign.
pub(crate) fn strip_quoted_literals(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            // Consume until the matching closing quote, skipping \X escape sequences.
            loop {
                match chars.next() {
                    None => break,
                    Some('\\') => {
                        chars.next(); // skip the escaped char
                    }
                    Some('\'') => break,
                    _ => {}
                }
            }
            result.push(' ');
        } else {
            result.push(ch);
        }
    }
    result
}

/// Whether `cypher` is index DDL (`CREATE_VECTOR_INDEX`, `CREATE INDEX`, `DROP INDEX`), which
/// `looks_like_mutation` would otherwise misclassify as an `Entity`-mutating write on its
/// `CREATE`/`DROP` keywords. Checked ahead of `looks_like_mutation` wherever the result decides
/// whether to pay for `Entity`-related work (WAL logging, `NameIndex` rebuild) — index DDL never
/// touches `Entity` rows, so neither is needed for it (higher priority per AD-W7).
pub(crate) fn is_index_ddl(cypher: &str) -> bool {
    let upper = cypher.to_uppercase();
    upper.contains("CREATE_VECTOR_INDEX")
        || upper.contains("CREATE INDEX")
        || upper.contains("DROP INDEX")
}

/// Whether `cypher` looks like a write (as opposed to a read-only query), by scanning all
/// tokens outside single-quoted literals for mutation keywords. MATCH-prefixed writes (e.g.
/// `"MATCH (...) DETACH DELETE"` or `"MATCH (...) SET ..."`) don't start with the DML verb,
/// so a first-token check would miss them. Stripping quoted literals first prevents entity
/// names that happen to contain DML words from being misclassified as mutations. Parentheses
/// are also treated as token boundaries (in addition to whitespace) so a keyword directly
/// touching punctuation with no space — `"CREATE(:Entity"`, `"MATCH (n)SET"` — still tokenizes
/// as a standalone word instead of being swallowed into one run that never matches exactly.
/// This can only ever turn a prior false negative into a (correct) match, never the reverse,
/// since it splits existing tokens further rather than merging any together.
///
/// Originally `log_mutation`'s inline check (WAL-logging decision); also used by
/// `handle_query_cypher` (issue #283, FR-004) to decide whether a raw-Cypher call through the
/// `cypher` MCP scope needs a follow-up `NameIndex` rebuild — reusing this heuristic keeps the
/// two "does this look like a write" decisions from silently diverging. Callers that care about
/// index DDL specifically (see `is_index_ddl`) should check that first, since this function
/// alone would classify `CREATE INDEX ...` as a mutation.
pub(crate) fn looks_like_mutation(cypher: &str) -> bool {
    let upper = cypher.to_uppercase();
    let stripped = strip_quoted_literals(&upper);
    let spaced: String = stripped
        .chars()
        .flat_map(|c| {
            if c == '(' || c == ')' {
                vec![' ', c, ' ']
            } else {
                vec![c]
            }
        })
        .collect();
    spaced.split_whitespace().any(|t| {
        matches!(
            t,
            "CREATE" | "MERGE" | "SET" | "DELETE" | "DETACH" | "DROP" | "REMOVE"
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_mutation_detects_keywords_touching_parentheses() {
        // Regression for a punctuation-adjacency gap: split_whitespace alone treats
        // "CREATE(:Entity" and "N)SET" as single opaque tokens that never equal "CREATE"/"SET"
        // exactly, silently missing real mutations that happen to have no space before/after
        // a paren.
        assert!(looks_like_mutation("CREATE(:Entity {uuid: 'x'})"));
        assert!(looks_like_mutation("MATCH (n)SET n.x = 1"));
        assert!(looks_like_mutation("MATCH (n)DETACH DELETE n"));
    }

    #[test]
    fn looks_like_mutation_still_ignores_read_only_queries() {
        assert!(!looks_like_mutation("MATCH (n) RETURN n"));
        assert!(!looks_like_mutation("MATCH(n)RETURN n"));
    }

    fn write_wal_line(dir: &Path, file_name: &str, seq: u64) {
        let line = WalLine {
            seq,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            params: serde_json::json!({ "uuid": "x" }),
        };
        fs::write(
            dir.join(file_name),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn resync_global_seq_is_monotonic_against_empty_wal_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        // Simulate mutations already emitted this process (in-memory global_seq advanced).
        writer.global_seq = 10;

        writer.resync_global_seq(None).unwrap();

        assert_eq!(writer.global_seq, 10, "resync must not lower global_seq");
    }

    #[test]
    fn resync_global_seq_picks_up_files_written_after_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let writer_state = WalWriter::new(tmp.path(), 1000, 0).unwrap();
        assert_eq!(writer_state.global_seq, 0);
        let mut writer = writer_state;

        // WAL dir populated out-of-band after the writer was constructed.
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 41);

        writer.resync_global_seq(None).unwrap();

        assert_eq!(writer.global_seq, 42, "resync must pick up on-disk seqs");
    }

    #[test]
    fn resync_global_seq_uses_last_committed_seq_as_a_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();

        // No on-disk lines beyond what the replay itself committed; last_committed_seq alone
        // must be enough to raise the floor.
        writer.resync_global_seq(Some(99)).unwrap();

        assert_eq!(writer.global_seq, 100);
    }

    #[test]
    fn resync_global_seq_prefers_on_disk_scan_over_lower_last_committed_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = WalWriter::new(tmp.path(), 1000, 0).unwrap();

        // A line that failed to replay still has a higher on-disk seq than the last
        // successfully committed line (FR-003 edge case).
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 200);

        writer.resync_global_seq(Some(5)).unwrap();

        assert_eq!(writer.global_seq, 201);
    }
}
