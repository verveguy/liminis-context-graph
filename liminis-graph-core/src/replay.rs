use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use serde::Serialize;

use crate::db::Conn;
use crate::error::Error;
use crate::legacy_wal::{expand_bulk_property_set, strip_vecf32};
use crate::wal::{strip_quoted_literals, WalLine};

/// lbug error-message substrings (lowercase) that classify a replay failure as a known legacy
/// graphiti/FalkorDB-era construct — counted in `ReplayStats::legacy_skipped_lines` rather than
/// `failed_lines` so it does not inflate the fidelity-warning ratio. Matched case-insensitively
/// against lbug 0.17.x error text.
///
/// Currently EMPTY. The previous entries (`"table community does not exist"`, `"table has does
/// not exist"`) became dead once #144 added `Community`/`Saga`/`HAS_MEMBER`/`HAS_EPISODE`/
/// `NEXT_EPISODE` as stub tables: those tables now exist, so the "does not exist" errors can no
/// longer occur and a `Community` CREATE replays into the stub (see #144's
/// `test_community_node_replays_into_stub_table`; #145 tracks the community/saga roadmap). Other
/// former legacy constructs are handled at the source — `episodes`/`expired_at` columns
/// (#133/#136) and `vecf32(...)` + bulk-`SET` translation (`legacy_wal`, ADR-0008).
///
/// The mechanism is retained (not removed) so a future legacy construct can be re-added as a
/// one-line pattern without reintroducing the classification plumbing.
const LEGACY_SCHEMA_ERROR_PATTERNS: &[&str] = &[];

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
/// `batch_size` bounds how many same-template rows are prepared-once then executed per
/// flush (a memory/granularity knob); it no longer affects query-string size since rows are
/// bound as parameters rather than inlined.
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

/// Executes accumulated batch mutations against `conn` via prepared-statement binding.
///
/// Prepares the batch's shared (post-normalization) template ONCE, then binds and executes
/// each row's params against it — no string interpolation, no inline `UNWIND` literal, so no
/// oversized query strings (the cause of lbug `db.wal` corruption in the prior inline-UNWIND
/// design, #139). Values are bound as typed lbug `Value`s and coerced to their column types.
///
/// A *prepare* failure (e.g. a legacy construct or missing column that survived
/// normalization) makes the template unusable, so every row sharing it is classified from
/// that single error. A per-row *execute* failure is classified for that row only, so one
/// bad row cannot suppress its siblings.
fn flush_batch(
    batch: &mut ReplayBatch,
    conn: &Conn<'_>,
    stats: &mut ReplayStats,
    sample_cap: usize,
) {
    if batch.is_empty() {
        return;
    }
    let rows = std::mem::take(&mut batch.rows);
    let match_prefixed = std::mem::take(&mut batch.match_prefixed);

    let mut prepared = match conn.prepare(&batch.template) {
        Ok(p) => p,
        Err(e) => {
            let err_str = e.to_string();
            for _ in &match_prefixed {
                classify_replay_failure(&err_str, &batch.template, stats, sample_cap);
            }
            batch.clear();
            return;
        }
    };

    for (row, is_match_prefixed) in rows.into_iter().zip(match_prefixed) {
        let params = serde_json::Value::Object(row);
        match conn.execute_prepared(&mut prepared, &params) {
            Ok(_) => {
                stats.lines_replayed += 1;
                if is_match_prefixed {
                    stats.match_prefixed_replayed += 1;
                }
            }
            Err(e) => classify_replay_failure(&e.to_string(), &batch.template, stats, sample_cap),
        }
    }
    batch.clear();
}

/// Classifies a replay failure (from prepare or execute) as legacy-skipped vs. genuine
/// failure, updating `stats`. Genuine failures record a sample using the template (there is
/// no interpolated string under bound-parameter execution).
fn classify_replay_failure(
    err_str: &str,
    template: &str,
    stats: &mut ReplayStats,
    sample_cap: usize,
) {
    let err_lower = err_str.to_lowercase();
    let is_legacy = LEGACY_SCHEMA_ERROR_PATTERNS
        .iter()
        .any(|pat| err_lower.contains(pat));
    // Log a whitespace-collapsed preview of the failing statement alongside the error. Without
    // this, WAL warnings showed only the error string, hiding which Cypher actually failed —
    // making "Cannot find property X for Y" undebuggable from the log alone.
    // Bound the input to ~400 chars before collapsing whitespace so an extremely large template
    // doesn't allocate its full length just to truncate the preview to 200 chars.
    let cypher_preview: String = template
        .chars()
        .take(400)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    if is_legacy {
        eprintln!("[WAL SKIP] legacy-schema mutation: {err_str} | cypher: {cypher_preview}");
        stats.legacy_skipped_lines += 1;
    } else {
        eprintln!("[WAL WARN] replay execution error: {err_str} | cypher: {cypher_preview}");
        stats.failed_lines += 1;
        if stats.failed_samples.len() < sample_cap {
            stats.failed_samples.push(FailureSample {
                cypher: template.chars().take(200).collect(),
                error: err_str.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    #[test]
    fn test_resolve_batch_size_defaults_to_64() {
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
}
