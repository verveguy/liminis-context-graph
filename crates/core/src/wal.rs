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
        // Apply the last_committed_seq floor first: it requires no I/O and is already the most
        // trustworthy source available (it came directly from the replay that just ran). If the
        // on-disk scan below fails, this floor still stands — a scan failure only forfeits the
        // scan's extra precision, not the guaranteed floor.
        let from_commit = last_committed_seq.map(|s| s.saturating_add(1)).unwrap_or(0);
        self.global_seq = self.global_seq.max(from_commit);
        let scanned = scan_max_seq(&self.wal_dir)?;
        self.global_seq = self.global_seq.max(scanned);
        Ok(())
    }

    /// Returns the current `global_seq` — the next seq to be assigned, i.e. one past the
    /// highest seq actually written so far. Used by `wal_exec::wal_flush_chunk` (issue #353)
    /// to compute the max seq assigned during a chunk via a before/after diff. Crate-visible
    /// only — `WalWriter` is re-exported from the crate root, and nothing outside `lcg-core`
    /// needs this, so `pub(crate)` avoids committing to it as public API.
    pub(crate) fn global_seq(&self) -> u64 {
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

/// Returns the highest WAL `seq` actually present across `wal_dir` (issue #353), or `None` if
/// the WAL is empty — the same units as `applied_seq` (a literal seq value), unlike
/// `scan_max_seq`'s internal "next seq to assign" convention (`scan_max_seq() - 1` when
/// non-empty). This is what makes the `applied_seq == wal_max_seq` "caught up" check in
/// `knowledge_status` well-defined. Safe to call fresh on every `knowledge_status` request —
/// see `read_last_seq`'s bounded tail-read, which keeps this from re-reading the full contents
/// of every WAL file at production scale (ADR-0026 documents ~43,820 files in one deployment).
pub fn wal_max_seq(wal_dir: &Path) -> Result<Option<u64>, Error> {
    let next = scan_max_seq(wal_dir)?;
    Ok(if next == 0 { None } else { Some(next - 1) })
}

/// Returns the lowest WAL `seq` actually present across `wal_dir` (issue #363), or `None` if the
/// WAL directory doesn't exist or contains no `.jsonl` files with a parseable first line.
/// Symmetric to [`wal_max_seq`]: together `[wal_min_seq, wal_max_seq]` bound the range of seqs a
/// checkpoint reachability check (FR-007) can trust — a seq outside this range is definitely
/// unreachable. A seq *inside* the range is only probably reachable: this is a bounds check, not
/// a full existence scan, so a gap caused by a specific file being removed while its neighbors
/// remain is not detected. Uses [`crate::replay::first_seq_in_file`] per file rather than a tail
/// read, since the *first* line (not the last) is what determines a file's minimum seq.
pub(crate) fn wal_min_seq(wal_dir: &Path) -> Result<Option<u64>, Error> {
    if !wal_dir.exists() {
        return Ok(None);
    }
    let files: Vec<PathBuf> = fs::read_dir(wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();

    let mut min_seq: Option<u64> = None;
    for path in &files {
        if let Some(seq) = crate::replay::first_seq_in_file(path) {
            min_seq = Some(match min_seq {
                None => seq,
                Some(m) => m.min(seq),
            });
        }
    }
    Ok(min_seq)
}

/// Bytes read from the tail of a WAL file before falling back to a full read. Generous even
/// for a line carrying a large embedding vector; chosen so `scan_max_seq`/`wal_max_seq` stay
/// cheap to call per `knowledge_status` request at the ~43,820-file scale ADR-0026 documents,
/// without reading each file's entire contents just to find its last line.
const TAIL_READ_WINDOW: u64 = 256 * 1024;

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
///
/// Reads only a bounded tail window (`TAIL_READ_WINDOW`) first — the common case, one complete
/// trailing line well within the window — and falls back to a full-file read only when the
/// window yields no parseable `seq` (e.g. a single line larger than the window, such as one
/// carrying an unusually large embedding vector). This preserves the existing "tolerates
/// truncated final lines" guarantee: truncation can only affect the *end* of the file, and both
/// paths always read through to EOF.
fn read_last_seq(path: &Path) -> Result<Option<u64>, Error> {
    let file_len = fs::metadata(path)?.len();
    if file_len > TAIL_READ_WINDOW {
        if let Some(seq) = read_last_seq_in_range(path, file_len - TAIL_READ_WINDOW)? {
            return Ok(Some(seq));
        }
    }
    read_last_seq_in_range(path, 0)
}

/// Reads `path` from byte offset `start` through EOF and returns the `seq` of the last
/// parseable non-empty line. When `start > 0`, the first (possibly partial) line in the window
/// is discarded, since `start` may land mid-line.
fn read_last_seq_in_range(path: &Path, start: u64) -> Result<Option<u64>, Error> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = fs::File::open(path)?;
    let mut buf = Vec::new();
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);

    let mut lines = text.lines();
    if start > 0 {
        lines.next(); // drop the partial line the seek landed inside
    }

    for raw in lines.rev() {
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

    #[test]
    fn wal_max_seq_is_none_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), None);
    }

    #[test]
    fn wal_max_seq_reports_the_literal_highest_seq() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 41);

        // `scan_max_seq` (next-assignable) would return 42; `wal_max_seq` must report 41 — the
        // same units as `applied_seq`, so `applied_seq == wal_max_seq` can express "caught up".
        assert_eq!(wal_max_seq(tmp.path()).unwrap(), Some(41));
    }

    #[test]
    fn wal_min_seq_is_none_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), None);
    }

    #[test]
    fn wal_min_seq_is_none_for_nonexistent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            wal_min_seq(&tmp.path().join("does_not_exist")).unwrap(),
            None
        );
    }

    #[test]
    fn wal_min_seq_reports_first_seq_of_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 41);
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(41));
    }

    #[test]
    fn wal_min_seq_folds_to_the_lowest_across_multiple_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_wal_line(tmp.path(), "seeded_0000.jsonl", 100);
        write_wal_line(tmp.path(), "seeded_0001.jsonl", 5);
        write_wal_line(tmp.path(), "seeded_0002.jsonl", 250);
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(5));
    }

    #[test]
    fn wal_min_seq_skips_a_file_with_a_corrupt_leading_line() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("corrupt_0000.jsonl"), "not valid json\n").unwrap();
        write_wal_line(tmp.path(), "seeded_0001.jsonl", 30);
        assert_eq!(wal_min_seq(tmp.path()).unwrap(), Some(30));
    }

    /// A single WAL line larger than `TAIL_READ_WINDOW` (e.g. one carrying an unusually large
    /// embedding) must still be found via the full-read fallback.
    #[test]
    fn read_last_seq_falls_back_to_full_read_for_an_oversized_single_line() {
        let tmp = tempfile::tempdir().unwrap();
        let line = WalLine {
            seq: 7,
            ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
            db: "default".to_string(),
            cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
            // Padding comfortably exceeds TAIL_READ_WINDOW so the tail-read pass alone can't
            // find a complete line, forcing the full-read fallback path.
            params: serde_json::json!({ "uuid": "x".repeat((TAIL_READ_WINDOW as usize) + 1024) }),
        };
        fs::write(
            tmp.path().join("oversized_0000.jsonl"),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();

        assert_eq!(
            read_last_seq(&tmp.path().join("oversized_0000.jsonl")).unwrap(),
            Some(7)
        );
    }

    /// A file well beyond the tail window with many prior lines must still resolve to the last
    /// line's `seq`, exercising the tail-read path (not the full-read fallback) end to end.
    #[test]
    fn read_last_seq_tail_read_finds_last_line_in_large_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("large_0000.jsonl");
        let mut content = String::new();
        // Enough short lines to exceed TAIL_READ_WINDOW comfortably.
        for seq in 0..20_000u64 {
            let line = WalLine {
                seq,
                ts: "2026-08-05T00:00:00.000000+00:00".to_string(),
                db: "default".to_string(),
                cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
                params: serde_json::json!({ "uuid": "x" }),
            };
            content.push_str(&serde_json::to_string(&line).unwrap());
            content.push('\n');
        }
        fs::write(&path, &content).unwrap();
        assert!(
            content.len() as u64 > TAIL_READ_WINDOW,
            "test file must exceed the tail-read window to exercise that path"
        );

        assert_eq!(read_last_seq(&path).unwrap(), Some(19_999));
    }

    /// A truncated final line (partial JSON, e.g. from a crash mid-write) must be skipped in
    /// favor of the last complete line before it — preserved by both the tail-read and
    /// full-read paths.
    #[test]
    fn read_last_seq_tolerates_truncated_final_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("truncated_0000.jsonl");
        write_wal_line(tmp.path(), "truncated_0000.jsonl", 3);
        // Append a truncated (invalid JSON) final line, as a crash mid-write would leave.
        let mut existing = fs::read_to_string(&path).unwrap();
        existing.push_str(r#"{"seq":4,"ts":"2026-08-05T00:00:00"#);
        fs::write(&path, existing).unwrap();

        assert_eq!(read_last_seq(&path).unwrap(), Some(3));
    }
}
