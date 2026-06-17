use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use serde::Serialize;

use crate::db::Conn;
use crate::error::Error;
use crate::legacy_wal::{expand_bulk_property_set, strip_vecf32};
use crate::wal::{strip_quoted_literals, WalLine};

/// lbug error-message substrings (lowercase) that identify legacy graphiti/FalkorDB-era schema
/// constructs (Community node label, HAS relationship type) not present in the current lbug
/// schema. Mutations matching these patterns are counted in `ReplayStats::legacy_skipped_lines`
/// rather than `failed_lines` so they don't inflate the fidelity-warning ratio. Patterns are
/// compared case-insensitively against the lowercased error string to guard against minor casing
/// variations across lbug versions.
///
/// NOTE: `episodes` was intentionally removed from this list in #133: `RelatesToNode_` now has an
/// `episodes STRING[]` column, so episodes mutations succeed and must NOT be silently skipped.
///
/// NOTE: These patterns are matched against lbug 0.17.x error text. If lbug changes its error
/// message format in a future version these patterns may silently stop matching. See ADR-0007.
const LEGACY_SCHEMA_ERROR_PATTERNS: &[&str] =
    &["table community does not exist", "table has does not exist"];

/// A single captured failure from a `raw_query` execution error during replay.
#[derive(Serialize)]
pub struct FailureSample {
    /// First 200 chars of the interpolated Cypher that was executed.
    pub cypher: String,
    /// The lbug error message returned by `raw_query`.
    pub error: String,
}

/// Statistics returned from a WAL replay run.
pub struct ReplayStats {
    pub lines_replayed: u64,
    /// Lines whose Cypher shape the replayer didn't recognise as a mutation.
    pub unrecognised_lines: u64,
    /// Lines that were attempted but failed at `raw_query` execution.
    pub failed_lines: u64,
    /// Lines that failed JSON parsing or had an I/O read error (both are data corruption).
    pub unparseable_lines: u64,
    /// Sampled failure details for `failed_lines` (first N, capped by `ReplayOptions::failure_sample_cap`).
    pub failed_samples: Vec<FailureSample>,
    pub files_read: u64,
    /// Always 0 — callers invoke `knowledge_build_indices` separately after rebuild.
    pub indexes_created: u64,
    /// Mutations whose Cypher began with MATCH (e.g. MATCH … SET for embedding enrichment).
    pub match_prefixed_replayed: u64,
    /// WAL mutations skipped because they reference legacy-schema constructs
    /// (Community node label, HAS relationship type) that are not present in the current lbug
    /// schema. Counted separately from `failed_lines` so they don't inflate the fidelity failure
    /// ratio. Note: episodes mutations are NOT counted here — `episodes STRING[]` is a real
    /// schema column since #133 and those mutations succeed normally.
    ///
    /// Note: this counter is **excluded** from [`lines_skipped()`] — callers that want a
    /// total "mutations not applied" count must add `legacy_skipped_lines + lines_skipped()`.
    pub legacy_skipped_lines: u64,
    /// Populated when `failed_lines / (lines_replayed + failed_lines) > threshold` after
    /// replay completes. Threshold defaults to 10% and is overridable via
    /// `LCG_REPLAY_FIDELITY_THRESHOLD` (float 0.0–1.0).
    pub fidelity_warning: Option<String>,
}

impl ReplayStats {
    /// Sum of `unrecognised_lines + failed_lines + unparseable_lines`.
    /// Retained for back-compat: equals the old `lines_skipped` field.
    pub fn lines_skipped(&self) -> u64 {
        self.unrecognised_lines + self.failed_lines + self.unparseable_lines
    }
}

/// Callback invoked during replay to emit progress; returning `false` aborts cleanly.
pub type ProgressFn = Box<dyn Fn(&ReplayProgress) -> bool + Send>;
/// Callback invoked once per mutation; returning `true` aborts immediately.
pub type CancelFn = Box<dyn Fn() -> bool + Send>;

/// Options for `WalReplayer::replay_opts`.
#[derive(Default)]
pub struct ReplayOptions {
    /// Skip WAL lines with `seq < from_seq`. Default: 0 (replay all).
    pub from_seq: u64,
    /// Count mutations without applying them. Default: false.
    pub dry_run: bool,
    /// Called once per file and once per 1000 mutations.
    pub progress_fn: Option<ProgressFn>,
    /// Called once per mutation to detect client disconnection faster than the 1000-mutation cadence.
    pub cancel_fn: Option<CancelFn>,
    /// Maximum number of `raw_query` failure samples to collect in `ReplayStats::failed_samples`.
    /// When `None`, reads `LCG_REPLAY_FAILURE_SAMPLES` env var, defaulting to 10.
    pub failure_sample_cap: Option<usize>,
    /// Maximum number of same-template mutations to batch into a single UNWIND query.
    /// Valid range: 1–256. When `None`, reads `LCG_REPLAY_BATCH_SIZE` env var, defaulting to 64.
    /// Set to `Some(1)` to disable batching and reproduce the pre-batching per-row behavior.
    pub batch_size: Option<usize>,
}

/// Progress snapshot passed to the `ReplayOptions::progress_fn` callback.
pub struct ReplayProgress {
    pub files_processed: u64,
    pub files_total: u64,
    pub mutations_replayed: u64,
    pub failed_lines_so_far: u64,
    pub legacy_skipped_lines_so_far: u64,
    pub message: String,
}

/// Replays all `.jsonl` WAL files in lexicographic filename order against a LadybugDB connection.
pub struct WalReplayer {
    wal_dir: PathBuf,
}

impl WalReplayer {
    pub fn new(wal_dir: impl Into<PathBuf>) -> Self {
        Self {
            wal_dir: wal_dir.into(),
        }
    }

    /// Reads all JSONL files, executes known mutations, skips truncated/unknown lines (R-05, R-08).
    pub fn replay(&self, conn: &Conn<'_>) -> Result<ReplayStats, Error> {
        self.replay_opts(conn, ReplayOptions::default())
    }

    /// Like `replay` but with `from_seq` filtering, dry-run mode, and optional progress callback.
    ///
    /// - Lines with `seq < opts.from_seq` are skipped without counting against `lines_skipped`.
    /// - When `opts.dry_run`, mutations are counted but not executed against the DB.
    /// - `opts.progress_fn` is called once per file and once per 1000 mutations within a file;
    ///   returning `false` aborts the replay cleanly.
    pub fn replay_opts(&self, conn: &Conn<'_>, opts: ReplayOptions) -> Result<ReplayStats, Error> {
        // Validate batch size before touching any WAL files (FR-005).
        let batch_size = resolve_batch_size(&opts)?;

        let sample_cap = opts.failure_sample_cap.unwrap_or_else(|| {
            std::env::var("LCG_REPLAY_FAILURE_SAMPLES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10)
        });
        let mut stats = ReplayStats {
            lines_replayed: 0,
            unrecognised_lines: 0,
            failed_lines: 0,
            unparseable_lines: 0,
            failed_samples: Vec::new(),
            files_read: 0,
            indexes_created: 0,
            match_prefixed_replayed: 0,
            legacy_skipped_lines: 0,
            fidelity_warning: None,
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
        let files_total = files.len() as u64;

        // Drop FTS and HNSW indexes before batched replay to avoid the lbug
        // FTSIndex::deleteFromTermsTable SIGBUS triggered by batched UNWIND SET on indexed columns.
        // Per-row replay (batch_size == 1) is not affected by this bug and is left unchanged (FR-005).
        if batch_size > 1 && !opts.dry_run {
            crate::schema::drop_fts_indexes(conn);
            conn.drop_vector_indexes();
        }

        let mut batch = ReplayBatch::new();

        'files: for file_path in &files {
            stats.files_read += 1;

            // Progress: once per file
            if let Some(ref f) = opts.progress_fn {
                let p = ReplayProgress {
                    files_processed: stats.files_read,
                    files_total,
                    mutations_replayed: stats.lines_replayed,
                    failed_lines_so_far: stats.failed_lines,
                    legacy_skipped_lines_so_far: stats.legacy_skipped_lines,
                    message: format!("processing file {}", file_path.display()),
                };
                if !f(&p) {
                    break 'files;
                }
            }

            let file = match fs::File::open(file_path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!(
                        "[WAL WARN] skipping unreadable WAL file {:?}: {e}",
                        file_path
                    );
                    continue;
                }
            };
            let reader = BufReader::new(file);
            let mut mutations_in_file: u64 = 0;

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
                        stats.unparseable_lines += 1;
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
                        stats.unparseable_lines += 1;
                        continue;
                    }
                };

                // from_seq filter — skip without counting as skipped
                if wal_line.seq < opts.from_seq {
                    continue;
                }

                // Mirror the writer's mutation detection: scan all tokens outside
                // single-quoted literals so MATCH-prefixed writes (MATCH ... DETACH DELETE,
                // MATCH ... SET) are replayed correctly.
                let upper = wal_line.cypher.to_uppercase();
                let is_known = strip_quoted_literals(&upper).split_whitespace().any(|t| {
                    matches!(
                        t,
                        "CREATE" | "MERGE" | "SET" | "DELETE" | "DETACH" | "DROP" | "REMOVE"
                    )
                });

                if !is_known {
                    let end = wal_line
                        .cypher
                        .char_indices()
                        .nth(80)
                        .map_or(wal_line.cypher.len(), |(i, _)| i);
                    eprintln!(
                        "[WAL WARN] skipping unrecognised mutation: {}",
                        &wal_line.cypher[..end]
                    );
                    stats.unrecognised_lines += 1;
                    continue;
                }

                let trimmed = upper.trim_start();
                let is_match_prefixed = trimmed.starts_with("MATCH")
                    && trimmed
                        .get(5..)
                        .and_then(|s| s.chars().next())
                        .is_none_or(|c| !c.is_alphanumeric() && c != '_');

                if opts.dry_run {
                    stats.lines_replayed += 1;
                    if is_match_prefixed {
                        stats.match_prefixed_replayed += 1;
                    }
                } else {
                    // Normalize the template and params (strip_vecf32, expand_bulk_property_set).
                    let norm_cypher = strip_vecf32(&wal_line.cypher);
                    let (norm_cypher, params) =
                        expand_bulk_property_set(&norm_cypher, wal_line.params);

                    // Extract the params map for batch accumulation.
                    let params_map = match params {
                        serde_json::Value::Object(m) => m,
                        _ => serde_json::Map::new(),
                    };

                    // Flush the current batch when the template changes (FR-001).
                    if !batch.is_empty() && batch.template != norm_cypher {
                        flush_batch(&mut batch, conn, &mut stats, sample_cap);
                    }

                    // Push this mutation into the batch.
                    if batch.is_empty() {
                        batch.template = norm_cypher;
                    }
                    batch.rows.push(params_map);
                    batch.match_prefixed.push(is_match_prefixed);

                    // Flush when the batch reaches the size limit (FR-002, FR-003).
                    if batch.len() >= batch_size {
                        flush_batch(&mut batch, conn, &mut stats, sample_cap);
                    }
                }

                mutations_in_file += 1;

                // Cancel check: abort immediately if client disconnected
                if let Some(ref cancel) = opts.cancel_fn {
                    if cancel() {
                        break 'files;
                    }
                }

                // Progress: once per 1000 mutations within a file
                if mutations_in_file.is_multiple_of(1000) {
                    if let Some(ref f) = opts.progress_fn {
                        let p = ReplayProgress {
                            files_processed: stats.files_read,
                            files_total,
                            mutations_replayed: stats.lines_replayed,
                            failed_lines_so_far: stats.failed_lines,
                            legacy_skipped_lines_so_far: stats.legacy_skipped_lines,
                            message: format!(
                                "replayed {} mutations in file {}",
                                mutations_in_file,
                                file_path.display()
                            ),
                        };
                        if !f(&p) {
                            break 'files;
                        }
                    }
                }
            }

            // WAL file boundary: flush any partial batch before advancing (FR-011).
            if !opts.dry_run {
                flush_batch(&mut batch, conn, &mut stats, sample_cap);
            }
        }

        // Flush any remaining batch after cancel/abort or EOF (FR-007, FR-011).
        if !opts.dry_run {
            flush_batch(&mut batch, conn, &mut stats, sample_cap);
        }

        // Rebuild FTS and HNSW indexes after batched replay completes (mirrors the drop above).
        // Non-fatal: index build failure is logged but does not propagate as a replay error.
        if batch_size > 1 && !opts.dry_run {
            if let Err(e) = conn.build_indices_and_constraints() {
                eprintln!("[WAL WARN] index rebuild after batched replay failed: {e} (non-fatal)");
            }
        }

        let threshold: f64 = std::env::var("LCG_REPLAY_FIDELITY_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.10)
            .clamp(0.0, 1.0);
        let total = stats.lines_replayed + stats.failed_lines;
        if total > 0 {
            let ratio = stats.failed_lines as f64 / total as f64;
            if ratio > threshold {
                stats.fidelity_warning = Some(format!(
                    "{:.1}% of mutations failed (threshold: {:.1}%); rebuilt graph may be incomplete",
                    ratio * 100.0,
                    threshold * 100.0,
                ));
            }
        }

        Ok(stats)
    }
}

/// Resolves the batch size from `opts.batch_size` or the `LCG_REPLAY_BATCH_SIZE` env var.
///
/// Valid range is 1–256. Values outside this range, or non-numeric env strings, cause a
/// `Error::Config` so that invalid configuration aborts before any WAL files are processed.
/// Note: batch_size=64 with 768-dim embeddings produces ~400 KB inline query strings; reduce
/// via `LCG_REPLAY_BATCH_SIZE` if lbug rejects oversized queries.
fn resolve_batch_size(opts: &ReplayOptions) -> Result<usize, Error> {
    let size = if let Some(s) = opts.batch_size {
        s
    } else {
        std::env::var("LCG_REPLAY_BATCH_SIZE")
            .ok()
            .map(|v| {
                v.parse::<usize>().map_err(|_| {
                    Error::Config(format!(
                        "LCG_REPLAY_BATCH_SIZE={v:?} is not a valid integer; \
                         expected 1–256"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(64)
    };
    if size == 0 || size > 256 {
        return Err(Error::Config(format!(
            "batch size {size} is out of range; expected 1–256"
        )));
    }
    Ok(size)
}

/// Rewrites `$key` placeholders in `template` to `row.key` for use in an UNWIND query.
///
/// Uses the same longest-key-first strategy as `interpolate_params` so that `$name_embedding`
/// is matched before `$name` when both keys exist.
fn rewrite_params_for_unwind(template: &str, keys: &[&str]) -> String {
    if keys.is_empty() {
        return template.to_string();
    }
    let mut sorted_keys: Vec<&str> = keys.to_vec();
    sorted_keys.sort_by_key(|k| std::cmp::Reverse(k.len()));

    let mut result = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(dollar_pos) = remaining.find('$') {
        result.push_str(&remaining[..dollar_pos]);
        let after_dollar = &remaining[dollar_pos + 1..];
        if let Some(k) = sorted_keys.iter().find(|&&k| {
            after_dollar.starts_with(k)
                && after_dollar[k.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        }) {
            result.push_str("row.");
            result.push_str(k);
            remaining = &remaining[dollar_pos + 1 + k.len()..];
        } else {
            result.push('$');
            remaining = after_dollar;
        }
    }
    result.push_str(remaining);
    result
}

/// Serializes a JSON object map directly to a Cypher map literal without wrapping in Value::Object.
fn map_to_cypher_literal(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let pairs: Vec<String> = obj
        .iter()
        .map(|(k, v)| format!("{k}: {}", json_to_cypher_literal(v)))
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

/// Builds an inline-literal UNWIND Cypher query from a rewritten template and a list of row maps.
///
/// lbug (Kuzu) has no parameterized query API (ADR-001), so the row list is inlined as a Cypher
/// list literal: `UNWIND [{k: v, ...}, ...] AS row <rewritten_template>`.
fn build_unwind_query(
    rewritten_template: &str,
    rows: &[serde_json::Map<String, serde_json::Value>],
) -> String {
    let row_literals: Vec<String> = rows.iter().map(map_to_cypher_literal).collect();
    format!(
        "UNWIND [{}] AS row\n{}",
        row_literals.join(", "),
        rewritten_template
    )
}

/// Accumulator for consecutive WAL mutations sharing an identical post-normalization template.
struct ReplayBatch {
    template: String,
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
    match_prefixed: Vec<bool>,
}

impl ReplayBatch {
    fn new() -> Self {
        Self {
            template: String::new(),
            rows: Vec::new(),
            match_prefixed: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn len(&self) -> usize {
        self.rows.len()
    }

    fn clear(&mut self) {
        self.template.clear();
        self.rows.clear();
        self.match_prefixed.clear();
    }
}

/// Executes accumulated batch mutations against `conn`, updating `stats`.
///
/// - Empty batch: no-op.
/// - Batch of 1: uses the existing single-statement path (`interpolate_params` + `raw_query`).
/// - Batch of N > 1: builds an UNWIND query. On UNWIND failure, falls back to per-row execution
///   so that one invalid mutation cannot suppress N-1 valid writes.
fn flush_batch(
    batch: &mut ReplayBatch,
    conn: &Conn<'_>,
    stats: &mut ReplayStats,
    sample_cap: usize,
) {
    if batch.is_empty() {
        return;
    }

    if batch.len() == 1 {
        // Single-entry path: use interpolate_params to produce the final Cypher.
        let row = std::mem::take(&mut batch.rows[0]);
        let params = serde_json::Value::Object(row);
        let cypher = interpolate_params(&batch.template, &params);
        execute_single(
            &cypher,
            batch.match_prefixed[0],
            conn,
            stats,
            sample_cap,
            "batch(1)",
        );
        batch.clear();
        return;
    }

    // Multi-entry UNWIND path.
    let keys: Vec<&str> = batch.rows[0].keys().map(|k| k.as_str()).collect();
    let rewritten = rewrite_params_for_unwind(&batch.template, &keys);
    let unwind_cypher = build_unwind_query(&rewritten, &batch.rows);

    match conn.raw_query(&unwind_cypher) {
        Ok(_) => {
            stats.lines_replayed += batch.len() as u64;
            stats.match_prefixed_replayed +=
                batch.match_prefixed.iter().filter(|&&b| b).count() as u64;
        }
        Err(e) => {
            eprintln!(
                "[WAL WARN] UNWIND batch of {} failed ({}); falling back to per-row: {}",
                batch.len(),
                &batch.template[..batch.template.len().min(80)],
                e
            );
            // Fallback: re-execute each row individually so valid mutations still succeed.
            let rows = std::mem::take(&mut batch.rows);
            let match_prefixed = std::mem::take(&mut batch.match_prefixed);
            for (row, is_match) in rows.into_iter().zip(match_prefixed) {
                let params = serde_json::Value::Object(row);
                let cypher = interpolate_params(&batch.template, &params);
                execute_single(&cypher, is_match, conn, stats, sample_cap, "batch fallback");
            }
        }
    }
    batch.clear();
}

/// Executes a single interpolated Cypher mutation and updates `stats`.
fn execute_single(
    cypher: &str,
    is_match_prefixed: bool,
    conn: &Conn<'_>,
    stats: &mut ReplayStats,
    sample_cap: usize,
    context: &str,
) {
    match conn.raw_query(cypher) {
        Ok(_) => {
            stats.lines_replayed += 1;
            if is_match_prefixed {
                stats.match_prefixed_replayed += 1;
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            let err_lower = err_str.to_lowercase();
            let is_legacy = LEGACY_SCHEMA_ERROR_PATTERNS
                .iter()
                .any(|pat| err_lower.contains(pat));
            if is_legacy {
                eprintln!("[WAL SKIP] legacy-schema mutation ({context}): {}", err_str);
                stats.legacy_skipped_lines += 1;
            } else {
                eprintln!("[WAL WARN] replay execution error ({context}): {}", err_str);
                stats.failed_lines += 1;
                if stats.failed_samples.len() < sample_cap {
                    stats.failed_samples.push(FailureSample {
                        cypher: cypher.chars().take(200).collect(),
                        error: err_str,
                    });
                }
            }
        }
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
    pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));

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
        serde_json::Value::String(s) => {
            // Detect RFC-3339 datetime strings and emit the typed timestamp() constructor
            // that lbug requires. Without this, STRING→TIMESTAMP implicit casts fail and
            // every subsequent MATCH on the node/edge yields "Cannot find property" cascades.
            // Pre-filter: a valid RFC-3339 datetime is at least 20 chars, starts with a digit
            // (year), and has 'T' at index 10. This skips the chrono parser for the majority
            // of params (UUIDs, names, content) without changing correctness.
            if s.len() >= 20
                && s.as_bytes()[0].is_ascii_digit()
                && (s.as_bytes()[10] == b'T' || s.as_bytes()[10] == b't')
                && chrono::DateTime::<chrono::FixedOffset>::parse_from_rfc3339(s).is_ok()
            {
                format!("timestamp('{}')", crate::db::escape_pub(s))
            } else {
                format!("'{}'", crate::db::escape_pub(s))
            }
        }
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
        assert!(
            result.contains("[1.0, 0.0]"),
            "embedding should be an array"
        );
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
        assert_eq!(json_to_cypher_literal(&val), "'it\\'s here'");
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
        assert!(
            result.contains("'$b'"),
            "value containing placeholder must not be re-expanded"
        );
        assert!(
            result.contains("'secret'"),
            "$b must still expand to 'secret'"
        );
    }

    #[test]
    fn test_timestamp_with_offset_emits_typed_literal() {
        let val = serde_json::Value::String("2026-03-25T16:58:57.761788+00:00".to_string());
        assert_eq!(
            json_to_cypher_literal(&val),
            "timestamp('2026-03-25T16:58:57.761788+00:00')"
        );
    }

    #[test]
    fn test_timestamp_utc_z_emits_typed_literal() {
        let val = serde_json::Value::String("2026-03-25T16:58:57Z".to_string());
        assert_eq!(
            json_to_cypher_literal(&val),
            "timestamp('2026-03-25T16:58:57Z')"
        );
    }

    #[test]
    fn test_timestamp_nonzero_offset_emits_typed_literal() {
        let val = serde_json::Value::String("2026-03-25T16:58:57+05:30".to_string());
        assert_eq!(
            json_to_cypher_literal(&val),
            "timestamp('2026-03-25T16:58:57+05:30')"
        );
    }

    #[test]
    fn test_space_separated_datetime_no_tz_emits_bare_string() {
        // "2026-05-19 00:00:00" is the format used in existing fixtures where the template
        // already wraps with timestamp(...). parse_from_rfc3339 rejects it (no T, no tz).
        let val = serde_json::Value::String("2026-05-19 00:00:00".to_string());
        assert_eq!(json_to_cypher_literal(&val), "'2026-05-19 00:00:00'");
    }

    #[test]
    fn test_ordinary_string_emits_bare_string() {
        let val = serde_json::Value::String("not a timestamp".to_string());
        assert_eq!(json_to_cypher_literal(&val), "'not a timestamp'");
    }

    #[test]
    fn test_rewrite_basic_param() {
        let result = rewrite_params_for_unwind("MERGE (n:Entity {uuid: $uuid})", &["uuid"]);
        assert_eq!(result, "MERGE (n:Entity {uuid: row.uuid})");
    }

    #[test]
    fn test_rewrite_longest_key_first() {
        // $name_embedding must be matched before $name
        let template = "SET n.name_embedding = $name_embedding, n.name = $name";
        let result = rewrite_params_for_unwind(template, &["name", "name_embedding"]);
        assert_eq!(
            result,
            "SET n.name_embedding = row.name_embedding, n.name = row.name"
        );
    }

    #[test]
    fn test_rewrite_no_params_unchanged() {
        let template = "MERGE (n:Entity {uuid: 'literal'})";
        let result = rewrite_params_for_unwind(template, &[]);
        assert_eq!(result, template);
    }

    #[test]
    fn test_rewrite_no_dollar_refs_unchanged() {
        let template = "MERGE (n:Entity {uuid: 'no-params'}) ON CREATE SET n.x = 1";
        let result = rewrite_params_for_unwind(template, &["uuid"]);
        assert_eq!(result, template);
    }

    #[test]
    fn test_rewrite_boundary_check_prevents_partial_match() {
        // Keys: only "name". Template has $name_embedding.
        // Without boundary check, "name" would greedily match $name from $name_embedding.
        // With boundary check, "_" after "name" stops the match, so $name_embedding is untouched.
        let template = "SET n.name_embedding = $name_embedding";
        let result = rewrite_params_for_unwind(template, &["name"]);
        assert_eq!(
            result, template,
            "$name_embedding must not be rewritten when only 'name' is in keys"
        );
    }

    #[test]
    fn test_resolve_batch_size_defaults_to_64() {
        // When batch_size is None and env var is unset, default is 64.
        // We can't safely unset env vars in parallel tests, so just test via opts.
        let opts = ReplayOptions {
            batch_size: Some(64),
            ..Default::default()
        };
        assert_eq!(resolve_batch_size(&opts).unwrap(), 64);
    }

    #[test]
    fn test_resolve_batch_size_rejects_zero() {
        let opts = ReplayOptions {
            batch_size: Some(0),
            ..Default::default()
        };
        assert!(resolve_batch_size(&opts).is_err());
    }

    #[test]
    fn test_resolve_batch_size_rejects_over_256() {
        let opts = ReplayOptions {
            batch_size: Some(257),
            ..Default::default()
        };
        assert!(resolve_batch_size(&opts).is_err());
    }

    #[test]
    fn test_resolve_batch_size_accepts_256() {
        let opts = ReplayOptions {
            batch_size: Some(256),
            ..Default::default()
        };
        assert_eq!(resolve_batch_size(&opts).unwrap(), 256);
    }

    #[test]
    fn test_build_unwind_query_shape() {
        use serde_json::json;
        let rows = vec![
            match json!({"uuid": "a-1", "name": "A"}) {
                serde_json::Value::Object(m) => m,
                _ => unreachable!(),
            },
            match json!({"uuid": "b-2", "name": "B"}) {
                serde_json::Value::Object(m) => m,
                _ => unreachable!(),
            },
        ];
        let q = build_unwind_query("MERGE (n:Entity {uuid: row.uuid})", &rows);
        assert!(q.starts_with("UNWIND ["), "must start with UNWIND [");
        assert!(q.contains("] AS row\n"), "must have ] AS row");
        assert!(q.contains("'a-1'"), "must inline first uuid");
        assert!(q.contains("'b-2'"), "must inline second uuid");
    }
}
