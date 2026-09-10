//! Removes embedding-vector `params` fields from *existing* on-disk WAL `.jsonl` files (issue
//! #577). 0.14.0 stopped writing these fields (`wal::strip_vector_params`, issue #526) and made
//! replay ignore any vector it finds in an older WAL (`replay::EMBEDDING_TEXT_PAIRS` always
//! recomputes from co-located source text) — so a vector already on disk in a pre-0.14 WAL is
//! dead weight replay already refuses to read. This module applies the same key-removal
//! transform `WalWriter::log_mutation` applies on the write path, but to lines that are already
//! on disk, reclaiming that space without touching what replay actually does.
//!
//! Deliberately out of scope (ADR-0526 § Decision 4 precedent): a vector inlined as a raw Cypher
//! literal (`params: {}`, no JSON key to remove) is not reachable by this key-removal strategy
//! and is left untouched.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::error::Error;
use crate::wal::{strip_vector_params, WalLine, VECTOR_PARAM_KEYS};
use crate::wal_group;

/// One file's error, keyed by path so a caller can tell which file failed without parsing a
/// combined message string (FR-006/FR-009).
#[derive(Debug, Clone, serde::Serialize)]
pub struct StripFileError {
    pub path: String,
    pub error: String,
}

/// Aggregated result of a [`strip_wal_embeddings`] call (FR-006).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StripWalEmbeddingsReport {
    pub dry_run: bool,
    pub files_processed: usize,
    pub files_rewritten: usize,
    pub files_unchanged: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub records_rewritten: u64,
    pub unparseable_lines: u64,
    pub errors: Vec<StripFileError>,
}

/// Discovers every WAL group directory under `wal_root` (or, when `group_id` is given, just that
/// group's directory) and strips embedding-vector `params` fields from every qualifying
/// `.jsonl` file within (FR-001/FR-006/FR-007).
///
/// A missing `wal_root`, or a missing directory for an explicit `group_id`, is an empty report —
/// not an error (there is nothing to strip). A directory or file this process cannot read (e.g.
/// a permission error) is reported per-path in `errors` and does not abort the rest of the run
/// (Edge Cases: "read-only or permission-restricted WAL directory or file").
///
/// `dry_run` computes the same statistics without writing anything to disk (FR-010): every file
/// is still scanned and validated (so a malformed vector value is still reported as an error),
/// but no tmp file is ever created and no rename ever happens.
pub fn strip_wal_embeddings(
    wal_root: &Path,
    group_id: Option<&str>,
    dry_run: bool,
) -> Result<StripWalEmbeddingsReport, Error> {
    // Best-effort: a legacy flat layout that hasn't been migrated yet is handled below via the
    // `wal_root` defense-in-depth scan regardless of whether this succeeds.
    let _ = wal_group::migrate_wal_root_if_needed(wal_root);

    let mut report = StripWalEmbeddingsReport {
        dry_run,
        ..Default::default()
    };

    let mut dirs: Vec<PathBuf> = Vec::new();
    match group_id {
        Some(gid) => {
            let dir = wal_group::group_wal_dir(wal_root, gid)?;
            if dir.is_dir() {
                dirs.push(dir);
            }
        }
        None => {
            if let Ok(group_dirs) = wal_group::list_group_wal_dirs(wal_root) {
                dirs.extend(group_dirs.into_iter().map(|(_, dir)| dir));
            }
            // Defense-in-depth: a legacy flat-layout `.jsonl` directly under `wal_root` that
            // `migrate_wal_root_if_needed` above didn't relocate (e.g. it failed non-fatally).
            dirs.push(wal_root.to_path_buf());
        }
    }

    for dir in dirs {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                // A missing directory (no WAL for this group yet, or the flat-layout
                // defense-in-depth entry when nothing is loose there) is normal, not an error.
                if dir.exists() {
                    report.errors.push(StripFileError {
                        path: dir.display().to_string(),
                        error: e.to_string(),
                    });
                }
                continue;
            }
        };

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries {
            match entry {
                Ok(e) => {
                    let p = e.path();
                    if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        files.push(p);
                    }
                }
                Err(e) => {
                    // A per-entry enumeration failure (e.g. a permission-restricted or
                    // transiently-unreadable entry alongside otherwise-readable ones) is reported
                    // against the directory, not silently dropped — matching the "read-only or
                    // permission-restricted WAL directory or file" edge case's per-path error
                    // contract, which otherwise only covered `fs::read_dir`'s own top-level error.
                    report.errors.push(StripFileError {
                        path: dir.display().to_string(),
                        error: format!("failed to read a directory entry: {e}"),
                    });
                }
            }
        }
        files.sort();

        for file in files {
            report.files_processed += 1;
            match strip_one_file(&file, dry_run) {
                Ok(result) => {
                    report.bytes_before += result.bytes_before;
                    report.bytes_after += result.bytes_after;
                    report.unparseable_lines += result.unparseable_lines;
                    if result.rewritten {
                        report.files_rewritten += 1;
                        report.records_rewritten += result.records_rewritten;
                    } else {
                        report.files_unchanged += 1;
                    }
                }
                Err(e) => {
                    report.errors.push(StripFileError {
                        path: file.display().to_string(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }

    Ok(report)
}

struct FileResult {
    rewritten: bool,
    bytes_before: u64,
    bytes_after: u64,
    records_rewritten: u64,
    unparseable_lines: u64,
}

/// Two-pass per-file handling (see the module's plan for the rationale): [`scan_file`] is a
/// read-only pass that both validates every embedding-vector value (FR-009) and determines
/// whether the file needs rewriting at all. A file needing no rewrite returns here without ever
/// opening a tmp file — this is what makes a re-run over an already-stripped WAL genuinely
/// zero-I/O (FR-004/SC-003), not merely zero-net-change. `dry_run` stops here too, since
/// `scan_file` already computed the would-be `bytes_after`/`records_rewritten`. Only a real,
/// needed rewrite pays for a second read-and-write pass ([`rewrite_file`]).
fn strip_one_file(path: &Path, dry_run: bool) -> Result<FileResult, Error> {
    let scan = scan_file(path)?;

    if !scan.needs_rewrite {
        return Ok(FileResult {
            rewritten: false,
            bytes_before: scan.bytes_before,
            bytes_after: scan.bytes_before,
            records_rewritten: 0,
            unparseable_lines: scan.unparseable_lines,
        });
    }

    if dry_run {
        return Ok(FileResult {
            rewritten: true,
            bytes_before: scan.bytes_before,
            bytes_after: scan.bytes_after,
            records_rewritten: scan.records_rewritten,
            unparseable_lines: scan.unparseable_lines,
        });
    }

    rewrite_file(path)?;
    let bytes_after = fs::metadata(path)?.len();
    Ok(FileResult {
        rewritten: true,
        bytes_before: scan.bytes_before,
        bytes_after,
        records_rewritten: scan.records_rewritten,
        unparseable_lines: scan.unparseable_lines,
    })
}

struct FileScanResult {
    bytes_before: u64,
    bytes_after: u64,
    needs_rewrite: bool,
    records_rewritten: u64,
    unparseable_lines: u64,
}

/// Streams `path` line-by-line (never buffers the whole file, per the "very large individual WAL
/// files" edge case) and applies [`transform_line`] to each line, without writing anywhere. A
/// malformed embedding-vector value (FR-009) aborts immediately with a descriptive error — since
/// nothing has been written yet, the file is guaranteed untouched by this function.
fn scan_file(path: &Path) -> Result<FileScanResult, Error> {
    let bytes_before = fs::metadata(path)?.len();
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut line_no = 0usize;
    let mut bytes_after: u64 = 0;
    let mut needs_rewrite = false;
    let mut records_rewritten = 0u64;
    let mut unparseable_lines = 0u64;

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        let outcome = transform_line(&buf, line_no, path)?;
        bytes_after += outcome.output.len() as u64;
        if outcome.rewritten {
            needs_rewrite = true;
            records_rewritten += 1;
        }
        if outcome.unparseable {
            unparseable_lines += 1;
        }
    }

    Ok(FileScanResult {
        bytes_before,
        bytes_after,
        needs_rewrite,
        records_rewritten,
        unparseable_lines,
    })
}

/// Streams `path` a second time, writing every line's [`transform_line`] output to a UUID-suffixed
/// tmp file in the same directory (so the eventual rename is same-filesystem, FR-005), then
/// atomically replaces the original only after the tmp file is fully written and flushed. The
/// `.tmp` suffix guarantees a crash-orphaned tmp file is never picked up by a later `.jsonl`
/// enumeration or mistaken for a WAL record file (User Story 3, Acceptance Scenario 1). Any
/// failure before the rename removes the tmp file and leaves `path` completely untouched.
fn rewrite_file(path: &Path) -> Result<(), Error> {
    let dir = path.parent().ok_or(Error::InvalidPath)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(Error::InvalidPath)?;
    let tmp_path = dir.join(format!("{file_name}.{}.tmp", Uuid::new_v4().as_simple()));

    let result: Result<(), Error> = (|| {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let tmp_file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(tmp_file);
        let mut buf = Vec::new();
        let mut line_no = 0usize;

        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let outcome = transform_line(&buf, line_no, path)?;
            writer.write_all(&outcome.output)?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            fs::rename(&tmp_path, path)?;
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

struct LineOutcome {
    /// Bytes to write in place of `raw` — identical to `raw` unless `rewritten` is true.
    output: Vec<u8>,
    /// True if this line had at least one embedding-vector field removed.
    rewritten: bool,
    /// True if this line could not be parsed as a [`WalLine`] at all (not a malformed embedding
    /// value — a structurally different failure, mirroring `replay.rs`'s existing tolerant-skip
    /// precedent). Passed through byte-for-byte unchanged, same as a non-embedding line.
    unparseable: bool,
}

/// Transforms one raw WAL line (as read by `BufRead::read_until(b'\n', ..)`, so `raw` includes
/// its trailing `\n` when present — absent only for a final line with no trailing newline).
///
/// - A blank line, or a line that fails to parse as UTF-8 or as a [`WalLine`], passes through
///   byte-for-byte unchanged and is counted `unparseable` (never `rewritten`) — this is a
///   different, weaker failure than a malformed embedding value in an otherwise-valid line, and
///   must never abort the file (mirrors `replay.rs`'s tolerant-skip precedent).
/// - A line whose `params` holds none of [`VECTOR_PARAM_KEYS`] passes through byte-for-byte
///   unchanged — this is what keeps an already-stripped line immune to reformatting drift on a
///   later run (SC-003).
/// - A line whose `params` holds an embedding-vector key with a value that is not a well-formed
///   JSON array of numbers is malformed (FR-009): returns `Err`, which the caller propagates to
///   abort the whole file before anything is written.
/// - Otherwise the line is rewritten: the originally-parsed generic [`Value`] has its `"params"`
///   slot replaced in place by [`strip_vector_params`]'s result, then the whole `Value` is
///   re-serialized. This is deliberately *not* done by deserializing into [`WalLine`] and
///   re-serializing that struct: `WalLine` has no `deny_unknown_fields` and no catch-all field,
///   so any top-level key it doesn't declare would be silently dropped by that round-trip.
///   Mutating the parsed `Value` in place instead preserves every top-level key — including one
///   `WalLine` doesn't know about — and its original relative order (`serde_json`'s
///   `preserve_order` feature), since only the `"params"` entry's value is ever replaced.
fn transform_line(raw: &[u8], line_no: usize, file_path: &Path) -> Result<LineOutcome, Error> {
    let has_trailing_newline = raw.last() == Some(&b'\n');
    let content = if has_trailing_newline {
        &raw[..raw.len() - 1]
    } else {
        raw
    };

    if content.iter().all(u8::is_ascii_whitespace) {
        return Ok(LineOutcome {
            output: raw.to_vec(),
            rewritten: false,
            unparseable: false,
        });
    }

    let text = match std::str::from_utf8(content) {
        Ok(t) => t,
        Err(_) => {
            return Ok(LineOutcome {
                output: raw.to_vec(),
                rewritten: false,
                unparseable: true,
            });
        }
    };

    let mut value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return Ok(LineOutcome {
                output: raw.to_vec(),
                rewritten: false,
                unparseable: true,
            });
        }
    };

    // Validate the line has WalLine's required shape (seq/ts/db/cypher/params, correctly typed)
    // before touching anything — this keeps `unparseable` semantics identical to a direct
    // `WalLine` parse, without requiring the *rewrite* below to go through the typed struct.
    // Deserializes from a borrow (`&Value` implements `serde::Deserializer`), not a clone: this
    // runs on every line of every file, including files that need no rewrite at all, so avoiding
    // an allocation here matters for the scan pass's cost on large WALs.
    if WalLine::deserialize(&value).is_err() {
        return Ok(LineOutcome {
            output: raw.to_vec(),
            rewritten: false,
            unparseable: true,
        });
    }

    let Some(params_obj) = value.get("params").and_then(Value::as_object) else {
        return Ok(LineOutcome {
            output: raw.to_vec(),
            rewritten: false,
            unparseable: false,
        });
    };

    let mut has_embedding_key = false;
    for key in VECTOR_PARAM_KEYS {
        if let Some(v) = params_obj.get(*key) {
            has_embedding_key = true;
            if !is_well_formed_number_array(v) {
                return Err(Error::Ipc(format!(
                    "{}: line {line_no}: embedding-vector param {key:?} is not a well-formed \
                     JSON array of numbers",
                    file_path.display(),
                )));
            }
        }
    }

    if !has_embedding_key {
        return Ok(LineOutcome {
            output: raw.to_vec(),
            rewritten: false,
            unparseable: false,
        });
    }

    let params_slot = value
        .as_object_mut()
        .and_then(|obj| obj.get_mut("params"))
        .expect("presence and object-ness of \"params\" already confirmed above");
    let owned_params = std::mem::take(params_slot);
    *params_slot = strip_vector_params(owned_params);

    let mut output = serde_json::to_vec(&value)?;
    if has_trailing_newline {
        output.push(b'\n');
    }
    Ok(LineOutcome {
        output,
        rewritten: true,
        unparseable: false,
    })
}

fn is_well_formed_number_array(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().all(Value::is_number),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lines(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut content = lines.join("\n");
        content.push('\n');
        fs::write(&path, content).unwrap();
        path
    }

    /// Fixtures below deliberately write under `<wal_root>/liminis/` (the default group's own
    /// directory, ADR-0378) rather than loose at `wal_root`'s top level — a loose top-level
    /// `.jsonl` is legacy-layout content that `strip_wal_embeddings` itself relocates via
    /// `migrate_wal_root_if_needed` before ever scanning for files to strip, which would move
    /// the fixture out from under a path captured before the call.
    fn group_dir(wal_root: &Path) -> PathBuf {
        wal_root.join(wal_group::DEFAULT_GROUP_ID)
    }

    #[test]
    fn strips_embedding_keys_and_preserves_other_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n) SET n.x=$x","params":{"x":1,"name_embedding":[0.1,0.2],"other":"kept"}}"#;
        let path = write_lines(&group_dir(tmp.path()), "0000.jsonl", &[line]);
        let before = fs::metadata(&path).unwrap().len();

        let report = strip_wal_embeddings(tmp.path(), None, false).unwrap();

        assert_eq!(report.files_rewritten, 1);
        assert_eq!(report.files_unchanged, 0);
        assert_eq!(report.records_rewritten, 1);
        assert_eq!(report.bytes_before, before);
        assert!(report.bytes_after < report.bytes_before);
        assert!(report.errors.is_empty());

        let content = fs::read_to_string(&path).unwrap();
        let parsed: WalLine = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed.seq, 0);
        let params = parsed.params.as_object().unwrap();
        assert!(!params.contains_key("name_embedding"));
        assert_eq!(params.get("x"), Some(&Value::from(1)));
        assert_eq!(params.get("other"), Some(&Value::from("kept")));
    }

    /// Regression test: a line with a top-level key `WalLine` doesn't declare must survive a
    /// strip unchanged, not be silently dropped by round-tripping through the typed struct.
    #[test]
    fn strip_preserves_an_unknown_top_level_field_on_a_rewritten_line() {
        let tmp = tempfile::tempdir().unwrap();
        let line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n) SET n.x=$x","params":{"x":1,"name_embedding":[0.1,0.2]},"schema_version":7}"#;
        let path = write_lines(&group_dir(tmp.path()), "0000.jsonl", &[line]);

        let report = strip_wal_embeddings(tmp.path(), None, false).unwrap();
        assert_eq!(report.files_rewritten, 1);
        assert_eq!(report.records_rewritten, 1);
        assert!(report.errors.is_empty());

        let content = fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(
            value.get("schema_version"),
            Some(&Value::from(7)),
            "an unknown top-level field must survive a strip, not be silently dropped: {value}"
        );
        let params = value["params"].as_object().unwrap();
        assert!(!params.contains_key("name_embedding"));
        assert_eq!(params.get("x"), Some(&Value::from(1)));
    }

    #[test]
    fn already_clean_file_is_a_zero_io_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = group_dir(tmp.path());
        let line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n)","params":{"x":1}}"#;
        let path = write_lines(&dir, "0000.jsonl", &[line]);
        let before_bytes = fs::read(&path).unwrap();
        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();

        let report = strip_wal_embeddings(tmp.path(), None, false).unwrap();

        assert_eq!(report.files_rewritten, 0);
        assert_eq!(report.files_unchanged, 1);
        assert_eq!(report.records_rewritten, 0);
        assert_eq!(report.bytes_before, report.bytes_after);

        // No tmp file was ever created, and the original is byte-for-byte and mtime unchanged.
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1, "no tmp file should have been created");
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime
        );
    }

    #[test]
    fn malformed_embedding_value_aborts_the_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = group_dir(tmp.path());
        let line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n)","params":{"name_embedding":"not-an-array"}}"#;
        let path = write_lines(&dir, "0000.jsonl", &[line]);
        let before_bytes = fs::read(&path).unwrap();

        let report = strip_wal_embeddings(tmp.path(), None, false).unwrap();

        assert_eq!(report.files_rewritten, 0);
        assert_eq!(report.files_unchanged, 0);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].error.contains("name_embedding"));
        assert_eq!(
            fs::read(&path).unwrap(),
            before_bytes,
            "file must be untouched on error"
        );

        // No leftover tmp file either.
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn unparseable_line_passes_through_and_other_files_still_process() {
        let tmp = tempfile::tempdir().unwrap();
        let good_embedding_line = r#"{"seq":1,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n)","params":{"fact_embedding":[0.1]}}"#;
        let path = write_lines(
            &group_dir(tmp.path()),
            "0000.jsonl",
            &["not valid json at all", good_embedding_line],
        );

        let report = strip_wal_embeddings(tmp.path(), None, false).unwrap();

        assert_eq!(report.files_rewritten, 1);
        assert_eq!(report.records_rewritten, 1);
        assert_eq!(report.unparseable_lines, 1);
        assert!(report.errors.is_empty());

        let content = fs::read_to_string(&path).unwrap();
        let mut lines = content.lines();
        assert_eq!(lines.next(), Some("not valid json at all"));
        let second: WalLine = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(second.params.as_object().unwrap().is_empty());
    }

    #[test]
    fn dry_run_reports_would_be_stats_without_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = group_dir(tmp.path());
        let line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n)","params":{"content_embedding":[0.1,0.2,0.3]}}"#;
        let path = write_lines(&dir, "0000.jsonl", &[line]);
        let before_bytes = fs::read(&path).unwrap();

        let report = strip_wal_embeddings(tmp.path(), None, true).unwrap();

        assert!(report.dry_run);
        assert_eq!(report.files_rewritten, 1);
        assert_eq!(report.records_rewritten, 1);
        assert!(report.bytes_after < report.bytes_before);

        assert_eq!(
            fs::read(&path).unwrap(),
            before_bytes,
            "dry_run must not touch the file"
        );
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1, "dry_run must never create a tmp file");
    }

    #[test]
    fn scopes_to_a_single_group_when_group_id_given() {
        let tmp = tempfile::tempdir().unwrap();
        let vector_line = r#"{"seq":0,"ts":"2026-01-01T00:00:00Z","db":"d","cypher":"MERGE (n)","params":{"summary_embedding":[0.1]}}"#;

        let group_a = tmp.path().join("group-a");
        fs::create_dir_all(&group_a).unwrap();
        write_lines(&group_a, "0000.jsonl", &[vector_line]);

        let group_b = tmp.path().join("group-b");
        fs::create_dir_all(&group_b).unwrap();
        write_lines(&group_b, "0000.jsonl", &[vector_line]);

        let report = strip_wal_embeddings(tmp.path(), Some("group-a"), false).unwrap();

        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_rewritten, 1);

        // group-b's file must be untouched.
        let b_content = fs::read_to_string(group_b.join("0000.jsonl")).unwrap();
        let parsed: WalLine = serde_json::from_str(b_content.trim()).unwrap();
        assert!(parsed
            .params
            .as_object()
            .unwrap()
            .contains_key("summary_embedding"));
    }

    #[test]
    fn missing_group_directory_is_an_empty_report_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let report = strip_wal_embeddings(tmp.path(), Some("nonexistent-group"), false).unwrap();
        assert_eq!(report.files_processed, 0);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn missing_wal_root_is_an_empty_report_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report = strip_wal_embeddings(&missing, None, false).unwrap();
        assert_eq!(report.files_processed, 0);
        assert!(report.errors.is_empty());
    }
}
