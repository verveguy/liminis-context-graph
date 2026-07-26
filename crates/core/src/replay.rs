use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
/// (#133/#136) and `vecf32(...)` + bulk-`SET` translation (`legacy_wal`, ADR-0023).
///
/// The mechanism is retained (not removed) so a future legacy construct can be re-added as a
/// one-line pattern without reintroducing the classification plumbing.
const LEGACY_SCHEMA_ERROR_PATTERNS: &[&str] = &[];

/// A single captured failure category from a `raw_query` execution error during replay.
///
/// One `FailureSample` represents every row that shared the same `(template, error)` pair, not
/// one row — `classify_replay_failure` deduplicates on that key so a single bad template can no
/// longer consume the entire `sample_cap` and hide every other distinct failure category (FR-001).
#[derive(Serialize)]
pub struct FailureSample {
    /// First 200 chars of the interpolated Cypher that was executed.
    pub cypher: String,
    /// The lbug error message returned by `raw_query`.
    pub error: String,
    /// Number of rows that produced this `(template, error)` pair (FR-002), including
    /// occurrences beyond the sample cap.
    pub count: u64,
    /// Full (untruncated) template string, used only as the dedup key — never serialized.
    /// Using the truncated `cypher` preview as the key risks a false merge between two distinct
    /// long templates that happen to share the same first 200 chars.
    #[serde(skip)]
    template: String,
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
    /// Deduplicated failure details for `failed_lines`, keyed by `(template, error)` and capped
    /// at `ReplayOptions::failure_sample_cap` distinct categories — not capped by row count. Each
    /// entry's `count` reflects every row that shared that `(template, error)` pair, including
    /// occurrences beyond the cap (FR-001–FR-003, issue #239).
    pub failed_samples: Vec<FailureSample>,
    pub files_read: u64,
    /// Always 0 — `WalReplayer` itself never builds indexes; `handle_rebuild_from_wal` builds
    /// FTS + HNSW indexes automatically once replay completes (see the `indices_built` field on
    /// the rebuild result, not this one) and reports that outcome separately.
    pub indexes_created: u64,
    /// Mutations whose Cypher began with MATCH (e.g. MATCH … SET for embedding enrichment).
    pub match_prefixed_replayed: u64,
    /// WAL mutations skipped because they reference legacy-schema constructs
    /// (Community node label, HAS relationship type) that are not present in the current lbug
    /// schema. Counted separately from `failed_lines` so they don't inflate the fidelity failure
    /// ratio. Note: episodes mutations are NOT counted here — `episodes STRING[]` is a real
    /// schema column since #133 and those mutations succeed normally.
    ///
    /// Note: this counter is **excluded** from [`lines_skipped()`] — a direct Rust caller that
    /// wants a total "mutations not applied" count must add `legacy_skipped_lines +
    /// lines_skipped() + match_prefixed_no_op + match_delete_no_op` (the latter two were carved
    /// out of `lines_replayed` by FR-005 and are likewise excluded from `lines_skipped()`).
    /// `knowledge_rebuild_from_wal`'s JSON response reports `lines_skipped` but does not
    /// (yet — a deliberate scoping decision, see ADR-0043) include `match_prefixed_no_op`/
    /// `match_delete_no_op`, so a JSON/IPC caller cannot reconstruct this total from that
    /// response alone; only a direct Rust caller holding the full `ReplayStats` can.
    pub legacy_skipped_lines: u64,
    /// Populated when `(failed_lines + match_prefixed_no_op + unrecognised_lines +
    /// unparseable_lines) / (lines_replayed + failed_lines + match_prefixed_no_op +
    /// unrecognised_lines + unparseable_lines) > threshold` after replay completes (FR-008,
    /// issue #239 — `unrecognised_lines`/`unparseable_lines` join both sides so a wholly
    /// unrecognised/unparseable WAL doesn't leave the denominator at 0). `legacy_skipped_lines`
    /// and `match_delete_no_op` stay excluded from both sides (FR-009). Threshold defaults to 10%
    /// and is overridable via `LCG_REPLAY_FIDELITY_THRESHOLD` (float 0.0–1.0). See
    /// `compute_fidelity_warning` for the exact computation.
    pub fidelity_warning: Option<String>,
    /// SET-form `MATCH`-prefixed mutations (`MATCH ... SET`, entity-type relabelling, edge
    /// invalidation, etc. — anything MATCH-prefixed that is not a DELETE) whose execution
    /// affected zero rows. A SET-form zero-row match has no legitimate cause other than an
    /// out-of-order write targeting a node that doesn't exist yet, so this is the counter that
    /// actually signals data loss. Counted separately from `lines_replayed`/
    /// `match_prefixed_replayed` so a no-op is never reported as a successful write (FR-005);
    /// factored into the `fidelity_warning` ratio (FR-006). Detected via a `RETURN count(*)`
    /// probe appended to the template — see `with_match_count_probe`.
    ///
    /// DELETE-form `MATCH`-prefixed zero-row matches (`MATCH ... DETACH DELETE`) are counted in
    /// [`Self::match_delete_no_op`] instead — see that field for why they're kept separate and
    /// excluded from the fidelity ratio.
    pub match_prefixed_no_op: u64,
    /// DELETE-form `MATCH`-prefixed mutations (`MATCH ... DETACH DELETE`, `MATCH ... DELETE`)
    /// whose execution affected zero rows — e.g. the target was already deleted by an earlier
    /// line. Counted separately from `lines_replayed`/`match_prefixed_replayed` (FR-005) but
    /// **excluded** from the `fidelity_warning` ratio, unlike [`Self::match_prefixed_no_op`]:
    /// `recovery.rs`'s WAL-tail-resume replay intentionally re-applies an overlapping `seq`
    /// range on every startup recovery (ADR-0026), and re-running an already-applied
    /// `DETACH DELETE` matches zero rows on every single healthy recovery — folding that into
    /// the same ratio as SET-form no-ops would make a routine, correct recovery report
    /// "rebuilt graph may be incomplete".
    pub match_delete_no_op: u64,
    /// Count of WAL lines whose `seq` was not strictly greater than the maximum `seq` already
    /// seen in this `replay_opts` call, in processing order (FR-004). A regression means the
    /// file-ordering heuristic (see the file sort in `replay_opts`) placed a file out of true
    /// write order — the affected mutation is still applied (refusing it would convert a rare
    /// ordering miss into new data loss) but the regression is counted and logged so it is never
    /// silent.
    pub seq_regressions: u64,
    /// Count of distinct `(template, error)` failure categories that could not be recorded in
    /// `failed_samples` because `sample_cap` distinct categories were already stored. FR-001–003
    /// eliminated the old row-capping defect where one bad template could hide every other
    /// category, but a WAL with more than `sample_cap` genuinely distinct failure categories can
    /// still drop some at the (unchanged) default cap of 10 — this counter makes that truncation
    /// visible (e.g. "10 of 14 categories shown") instead of silent.
    pub failed_sample_categories_dropped: u64,
    /// Count of real `Conn::prepare()` calls made by `flush_batch` over this run — cache misses
    /// only; a flush that reuses the single-entry `PreparedCache` (see that type's doc comment)
    /// does not increment this. This is the basis for issue #238's FR-002/FR-007 bound: for a WAL
    /// where the same template recurs across consecutive flushes, this count stays proportional
    /// to the number of distinct templates encountered rather than growing with the number of
    /// batches flushed or lines replayed.
    pub prepare_calls: u64,
    /// Rows that belonged to a `flush_batch` transaction that was rolled back — either because
    /// another row in the same transaction threw an execute-time exception (lbug rolls back the
    /// *whole* transaction on any such exception, not just the failing statement — see ADR-0047)
    /// or because `cancel_fn` fired mid-transaction. Excludes the one row that actually triggered
    /// the failure (that row is classified into `failed_lines`/`legacy_skipped_lines` via
    /// `classify_replay_failure` as before) and excludes rows in a batch that committed
    /// successfully. A row counted here must never also be counted in `lines_replayed` — see
    /// `flush_batch`'s doc comment (issue #240, FR-002/FR-009).
    pub rolled_back_lines: u64,
    /// The highest WAL `seq` among rows in the most recent `flush_batch` transaction that
    /// actually committed (FR-006, issue #240). `None` until at least one transaction commits.
    /// This is the resume point a caller can derive after a failure or cancellation — it
    /// corresponds exactly to the last committed transaction boundary, not an approximation.
    /// Additive only in this issue: existing callers (`recovery.rs`'s episode-cursor resume,
    /// ADR-0026) are unchanged; a future issue may choose to consume this instead.
    pub last_committed_seq: Option<u64>,
    /// Count of `flush_batch` transactions that committed successfully (issue #240).
    pub transactions_committed: u64,
    /// Count of `flush_batch` transactions that were rolled back — either by the lbug engine's
    /// auto-rollback-on-exception or by an explicit `ROLLBACK` on cancellation (issue #240).
    pub transactions_rolled_back: u64,
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
/// Callback that receives a `[WAL PROGRESS]` log line; replaces `eprintln!` in tests.
pub type ProgressLogFn = Box<dyn Fn(&str) + Send>;

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
    /// Throttle interval in seconds for `[WAL PROGRESS]` log lines. When `None`, reads
    /// `LCG_REPLAY_LOG_INTERVAL_SECS` env var, defaulting to 30s. Production passes `None`;
    /// tests may pass `Some(0)` to force every progress event to emit a log line.
    pub log_interval_override: Option<u64>,
    /// Optional sink for `[WAL PROGRESS]` log lines (replaces `eprintln!`). When `None`,
    /// lines are written to stderr via `eprintln!`. Production passes `None`; tests inject a
    /// closure capturing to `Arc<Mutex<Vec<String>>>` to verify throttle behaviour.
    pub progress_log_fn: Option<ProgressLogFn>,
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

/// Replays all `.jsonl` WAL files against a LadybugDB connection, ordered by each file's
/// first-line `seq` (see the file sort in `replay_opts`) — not by full-filename comparison,
/// since the random per-session id embedded in the filename does not track write order across
/// sessions (see ADR-0043).
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

        // `[WAL PROGRESS]` throttle — emits a grep-able log line at a configurable interval so
        // a durable record survives crashes and IPC disconnects. Format:
        //   [WAL PROGRESS] elapsed=12s files=1234/43821 mutations=567890 failed=0 legacy_skipped=0 | <message>
        // Grep: `grep '[WAL PROGRESS]' service.log`
        // Interval: `LCG_REPLAY_LOG_INTERVAL_SECS` env var (default 30s). The first progress
        // event always logs regardless of interval, providing a "replay started" anchor.
        let log_interval = Duration::from_secs(opts.log_interval_override.unwrap_or_else(|| {
            std::env::var("LCG_REPLAY_LOG_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30)
        }));
        let replay_start = Instant::now();
        let mut last_log_at: Option<Instant> = None;

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
            match_prefixed_no_op: 0,
            match_delete_no_op: 0,
            seq_regressions: 0,
            failed_sample_categories_dropped: 0,
            prepare_calls: 0,
            rolled_back_lines: 0,
            last_committed_seq: None,
            transactions_committed: 0,
            transactions_rolled_back: 0,
        };

        if !self.wal_dir.exists() {
            return Ok(stats);
        }

        let files: Vec<PathBuf> = fs::read_dir(&self.wal_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();

        // Ordering key (FR-003): each file's first WAL line's `seq`, which is globally
        // monotonic across sessions (`WalWriter::scan_max_seq` reseeds it at every restart).
        // The filename's `file_seq` component is NOT used here — it resets to 0 on every new
        // `WalWriter` session, so it cannot order files written by different sessions; using it
        // would silently reintroduce this same ordering defect in a different column. Files
        // whose first-line seq can't be determined (unreadable, empty, or an unparseable first
        // line) sort after all determinate files, grouped by filename among themselves, with a
        // `[WAL WARN]` log — any resulting misordering is then caught by the seq-monotonicity
        // check below (FR-004), so the two mechanisms are complementary, not redundant.
        let mut keyed_files: Vec<(Option<u64>, PathBuf)> = files
            .into_iter()
            .map(|p| (first_seq_in_file(&p), p))
            .collect();
        for (seq, path) in &keyed_files {
            if seq.is_none() {
                eprintln!(
                    "[WAL WARN] could not determine first seq for {path:?}; \
                     falling back to filename order for this file"
                );
            }
        }
        keyed_files.sort_by(|(seq_a, path_a), (seq_b, path_b)| match (seq_a, seq_b) {
            (Some(a), Some(b)) => a
                .cmp(b)
                .then_with(|| path_a.file_name().cmp(&path_b.file_name())),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => path_a.file_name().cmp(&path_b.file_name()),
        });
        let files: Vec<PathBuf> = keyed_files.into_iter().map(|(_, p)| p).collect();
        let files_total = files.len() as u64;

        let mut batch = ReplayBatch::new();
        // Single-entry prepared-statement cache carried across `flush_batch` calls for this
        // whole run — see `PreparedCache`'s doc comment for the LRU-1 scope decision (#238).
        let mut prepared_cache: Option<PreparedCache> = None;
        // Running max `seq` across the whole call, for FR-004's monotonicity check.
        let mut max_seq_seen: Option<u64> = None;
        // Caps individual `[WAL WARN] seq regression` lines — a WAL dir with two interleaved
        // seq lineages (a restored backup, or two merged wal dirs) can regress on every line for
        // the rest of the replay, which would otherwise emit one uncapped eprintln! per line.
        // stats.seq_regressions still counts every occurrence; only the per-line detail log is
        // capped. A single summary line reports the true total after replay completes.
        const SEQ_REGRESSION_LOG_CAP: u64 = 10;
        let mut seq_regressions_logged: u64 = 0;

        'files: for file_path in &files {
            stats.files_read += 1;

            // Progress: once per file
            let should_log = last_log_at.is_none_or(|t| t.elapsed() >= log_interval);
            if opts.progress_fn.is_some() || should_log {
                let p = ReplayProgress {
                    files_processed: stats.files_read,
                    files_total,
                    mutations_replayed: stats.lines_replayed,
                    failed_lines_so_far: stats.failed_lines,
                    legacy_skipped_lines_so_far: stats.legacy_skipped_lines,
                    message: format!("processing file {}", file_path.display()),
                };
                if let Some(ref f) = opts.progress_fn {
                    if !f(&p) {
                        break 'files;
                    }
                }
                if should_log {
                    let line = format!(
                        "[WAL PROGRESS] elapsed={}s files={}/{} mutations={} failed={} legacy_skipped={} | {}",
                        replay_start.elapsed().as_secs(),
                        p.files_processed,
                        p.files_total,
                        p.mutations_replayed,
                        p.failed_lines_so_far,
                        p.legacy_skipped_lines_so_far,
                        p.message,
                    );
                    match opts.progress_log_fn {
                        Some(ref log_fn) => log_fn(&line),
                        None => eprintln!("{line}"),
                    }
                    last_log_at = Some(Instant::now());
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

                // Monotonicity check (FR-004): a regression here means the file-ordering
                // heuristic above placed a file out of true seq order. The mutation still
                // proceeds into the normal path below — refusing it would convert a rare
                // ordering-heuristic miss into new data loss, exactly what this issue exists to
                // eliminate — but the regression is counted and logged so it is never silent.
                if let Some(max_seen) = max_seq_seen {
                    if wal_line.seq <= max_seen {
                        stats.seq_regressions += 1;
                        if seq_regressions_logged < SEQ_REGRESSION_LOG_CAP {
                            let line = format!(
                                "[WAL WARN] seq regression: line seq={} <= max seen so far={} in {:?}",
                                wal_line.seq, max_seen, file_path
                            );
                            match opts.progress_log_fn {
                                Some(ref log_fn) => log_fn(&line),
                                None => eprintln!("{line}"),
                            }
                            seq_regressions_logged += 1;
                        }
                    }
                }
                max_seq_seen = Some(max_seq_seen.map_or(wal_line.seq, |m| m.max(wal_line.seq)));

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
                        let outcome = flush_batch(
                            &mut batch,
                            conn,
                            &mut stats,
                            sample_cap,
                            &mut prepared_cache,
                            opts.cancel_fn.as_ref(),
                        )?;
                        if outcome.cancelled {
                            break 'files;
                        }
                    }

                    // Push this mutation into the batch.
                    if batch.is_empty() {
                        batch.template = norm_cypher;
                    }
                    batch.rows.push(params_map);
                    batch.match_prefixed.push(is_match_prefixed);
                    batch.seqs.push(wal_line.seq);

                    // Flush when the batch reaches the size limit (FR-002, FR-003). Same
                    // template as the just-flushed batch reuses the cached prepared statement
                    // (issue #238) instead of re-preparing.
                    if batch.len() >= batch_size {
                        let outcome = flush_batch(
                            &mut batch,
                            conn,
                            &mut stats,
                            sample_cap,
                            &mut prepared_cache,
                            opts.cancel_fn.as_ref(),
                        )?;
                        if outcome.cancelled {
                            break 'files;
                        }
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
                    let should_log = last_log_at.is_none_or(|t| t.elapsed() >= log_interval);
                    if opts.progress_fn.is_some() || should_log {
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
                        if let Some(ref f) = opts.progress_fn {
                            if !f(&p) {
                                break 'files;
                            }
                        }
                        if should_log {
                            let line = format!(
                                "[WAL PROGRESS] elapsed={}s files={}/{} mutations={} failed={} legacy_skipped={} | {}",
                                replay_start.elapsed().as_secs(),
                                p.files_processed,
                                p.files_total,
                                p.mutations_replayed,
                                p.failed_lines_so_far,
                                p.legacy_skipped_lines_so_far,
                                p.message,
                            );
                            match opts.progress_log_fn {
                                Some(ref log_fn) => log_fn(&line),
                                None => eprintln!("{line}"),
                            }
                            last_log_at = Some(Instant::now());
                        }
                    }
                }
            }

            // WAL file boundary: flush any partial batch before advancing (FR-011).
            if !opts.dry_run {
                let outcome = flush_batch(
                    &mut batch,
                    conn,
                    &mut stats,
                    sample_cap,
                    &mut prepared_cache,
                    opts.cancel_fn.as_ref(),
                )?;
                if outcome.cancelled {
                    break 'files;
                }
            }
        }

        // Flush any remaining batch after cancel/abort or EOF (FR-007, FR-011).
        if !opts.dry_run {
            flush_batch(
                &mut batch,
                conn,
                &mut stats,
                sample_cap,
                &mut prepared_cache,
                opts.cancel_fn.as_ref(),
            )?;
        }

        // Summary for the seq-regression detail logging capped above — the true total is
        // always in `stats.seq_regressions` regardless of how many detail lines were logged.
        if stats.seq_regressions > 0 {
            let line = format!(
                "[WAL WARN] {} seq regression(s) detected during replay (showing first {}); \
                 replay order may not exactly match true write order",
                stats.seq_regressions, seq_regressions_logged,
            );
            match opts.progress_log_fn {
                Some(ref log_fn) => log_fn(&line),
                None => eprintln!("{line}"),
            }
        }

        let threshold: f64 = std::env::var("LCG_REPLAY_FIDELITY_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.10)
            .clamp(0.0, 1.0);
        stats.fidelity_warning = compute_fidelity_warning(&stats, threshold);

        Ok(stats)
    }
}

/// Computes the `fidelity_warning` message (if any) for a completed replay.
///
/// FR-006: `match_prefixed_no_op` joins both sides of the ratio, not just the numerator —
/// FR-005 removes no-ops from `lines_replayed`, so they must also join the denominator or the
/// ratio would never change.
///
/// FR-008/FR-009 (issue #239): `unrecognised_lines` and `unparseable_lines` join both sides too,
/// following the same symmetric-join pattern — a WAL that is 100% unrecognised (wrong directory,
/// incompatible format) must not leave `total == 0`, which would short-circuit the `total > 0`
/// guard below and report a silent, un-warned `mutations_replayed: 0` that is indistinguishable
/// from a healthy no-op WAL. `legacy_skipped_lines` remains excluded from both sides — a benign,
/// expected outcome that must not by itself push a healthy replay over the warning threshold.
fn compute_fidelity_warning(stats: &ReplayStats, threshold: f64) -> Option<String> {
    let total = stats.lines_replayed
        + stats.failed_lines
        + stats.match_prefixed_no_op
        + stats.unrecognised_lines
        + stats.unparseable_lines;
    if total == 0 {
        return None;
    }
    let ineffective = stats.failed_lines
        + stats.match_prefixed_no_op
        + stats.unrecognised_lines
        + stats.unparseable_lines;
    let ratio = ineffective as f64 / total as f64;
    if ratio > threshold {
        // Break down the bucket counts rather than lumping them under one "failed or had no
        // effect" label: that phrasing undersells FR-008's motivating case (a WAL pointed at the
        // wrong directory, or in an incompatible format) — an all-`unrecognised_lines` WAL is
        // not "failed", it was never recognised as a mutation at all, and an all-`unparseable_lines`
        // WAL was never even parsed. The breakdown lets an operator tell "the schema is broken"
        // (failed/no-op) from "you pointed me at the wrong directory" (unrecognised/unparseable)
        // at a glance instead of having to cross-reference the surrounding JSON fields.
        Some(format!(
            "{:.1}% of mutations failed or had no effect (threshold: {:.1}%); rebuilt graph may be \
             incomplete (failed={}, no-op={}, unrecognised={}, unparseable={})",
            ratio * 100.0,
            threshold * 100.0,
            stats.failed_lines,
            stats.match_prefixed_no_op,
            stats.unrecognised_lines,
            stats.unparseable_lines,
        ))
    } else {
        None
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
    /// Each row's WAL `seq`, positionally aligned with `rows`/`match_prefixed` — used to derive
    /// `ReplayStats::last_committed_seq` (FR-006, issue #240) when this batch's transaction
    /// commits.
    seqs: Vec<u64>,
}

impl ReplayBatch {
    fn new() -> Self {
        Self {
            template: String::new(),
            rows: Vec::new(),
            match_prefixed: Vec::new(),
            seqs: Vec::new(),
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
        self.seqs.clear();
    }
}

/// Single-entry ("LRU-1") prepared-statement cache carried across consecutive `flush_batch`
/// calls within one `replay_opts` run, keyed by the post-normalization Cypher template.
///
/// This is a deliberate scope choice, not an oversight: FR-002 (issue #238) only requires
/// bounding `prepare()` calls for a template that recurs across *consecutive* flushes — the
/// common, homogeneous-WAL case (repeated `Entity` MERGE, repeated `Edge` MERGE, etc., batched
/// at `batch_size`). A cache keyed by every distinct template ever seen would itself grow
/// without bound on a pathological, highly-interleaved WAL where the distinct-template count is
/// unbounded relative to run length — reintroducing the exact unbounded-growth failure mode this
/// issue exists to fix, in a different shape. That residual risk is accepted and documented
/// (FR-004/SC-005) rather than "solved" here — see `docs/adr/0045-wal-replay-prepared-statement-cache-scope.md`
/// for the evaluation and the decision to defer periodic replay-connection recycling as its
/// mitigation. LRU-1 gives the homogeneous case (User Story 1) full benefit and the interleaved
/// case (User Story 2) none, which is the accepted trade-off.
struct PreparedCache {
    /// The post-normalization template this prepared statement was built from.
    template: String,
    /// Whether `statement` is the `RETURN count(*)`-probed variant (see
    /// `with_match_count_probe`) or the plain template — carried alongside the statement itself
    /// so a cache hit doesn't need to re-derive it.
    use_probe: bool,
    statement: lbug::PreparedStatement,
}

/// Reads a WAL file and returns the `seq` value of its first line that parses successfully —
/// the ordering key used to sort files in `replay_opts` (FR-003). Returns `None` only if the
/// file can't be opened, or no line in it parses before EOF; callers fall back to filename order
/// for such files (a rare, honestly-approximate placement — any resulting misordering is caught
/// by the seq-monotonicity check in `replay_opts`, not silently accepted as correct).
///
/// Reads incrementally via `BufReader::lines()` rather than loading the whole file into memory —
/// this runs once per `.jsonl` file up front, before the first progress event, so a full-file
/// read (as an earlier version of this function did, contradicting this fix's own ADR-0043 claim
/// of "one line, not a full-file scan") turns a large WAL directory into an apparent startup hang.
/// In the common case the first line parses immediately and only that one line is read; only a
/// genuinely corrupt prefix costs more.
///
/// Tolerates a corrupt/truncated/non-UTF-8 leading line by skipping it and continuing, mirroring
/// `wal::read_last_seq`'s tolerance: `io::Lines` reports an `Err` only for the one line that
/// failed UTF-8 conversion, having already consumed those bytes, so the next line is unaffected
/// and scanning can continue — an early bail on just the first line would relegate a file with
/// thousands of good lines to the back of the replay order over a single crash-truncated line,
/// exactly the kind of WAL corruption the main replay loop already tolerates line-by-line.
fn first_seq_in_file(path: &std::path::Path) -> Option<u64> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let raw = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(wal_line) = serde_json::from_str::<WalLine>(trimmed) {
            return Some(wal_line.seq);
        }
    }
    None
}

/// Appends `RETURN count(*)` to a `MATCH`-prefixed template so its execution result reveals
/// whether the `MATCH` clause found any rows (FR-005). A bare `MATCH ... SET`/`DETACH DELETE`
/// with no `RETURN` clause always reports 0 tuples from `get_num_tuples()` regardless of whether
/// the `MATCH` found anything — this rewrite is the only way to distinguish a real no-op from an
/// effective write. Verified empirically against lbug 0.17: the mutation still applies when the
/// `MATCH` finds rows, and `count(*)` reports the true matched-row count either way.
fn with_match_count_probe(template: &str) -> String {
    let trimmed = template.trim().trim_end_matches(';').trim_end();
    format!("{trimmed} RETURN count(*)")
}

/// Returns true if a `MATCH`-prefixed `template` is DELETE-form (`MATCH ... DETACH DELETE` /
/// `MATCH ... DELETE`) rather than SET-form. Delete-form zero-row matches are routinely
/// legitimate — the target was already deleted by an earlier line, or `recovery.rs`'s
/// WAL-tail-resume replay intentionally re-applies an overlapping `seq` range on every startup
/// (ADR-0026) and re-running an already-applied delete matches nothing — whereas a SET-form
/// zero-row match has no legitimate cause other than an out-of-order write targeting a node that
/// doesn't exist yet. The two are tracked in separate counters (`match_delete_no_op` vs.
/// `match_prefixed_no_op`) so routine delete replay never inflates the fidelity-warning ratio.
///
/// This is a whole-template token scan, not a "does the statement's outermost clause delete"
/// check — a single template containing BOTH a `SET` and a `DELETE` token (reachable via
/// `knowledge_query_cypher`'s arbitrary-Cypher WAL logging, the same source the probe-prepare
/// fallback above exists for) is classified DELETE-form, exempting a genuinely lost SET from the
/// fidelity ratio. Judged low-likelihood (none of this codebase's own templates mix the two) and
/// left as-is rather than hand-rolling a Cypher parser; a future contributor tightening this
/// should keep the ADR-0043 discussion of this limitation in sync.
fn is_delete_form(template: &str) -> bool {
    let upper = template.to_uppercase();
    strip_quoted_literals(&upper)
        .split_whitespace()
        .any(|t| t == "DELETE")
}

/// Outcome of a `flush_batch` call. `replay_opts` only needs to know whether `cancel_fn` fired
/// mid-batch (causing an explicit rollback) so it can stop reading further WAL lines — a
/// committed or ordinarily-failed batch requires no special handling by the caller, since
/// `stats` already reflects the outcome.
struct FlushOutcome {
    cancelled: bool,
}

/// Executes accumulated batch mutations against `conn` inside one explicit transaction (issue
/// #240) — `BEGIN TRANSACTION`, then bind-and-execute each row against a template prepared ONCE,
/// then `COMMIT`. No string interpolation, no inline `UNWIND` literal, so no oversized query
/// strings (the cause of lbug `db.wal` corruption in the prior inline-UNWIND design, #139).
/// Values are bound as typed lbug `Value`s and coerced to their column types.
///
/// A *prepare* failure (e.g. a legacy construct or missing column that survived normalization)
/// makes the template unusable — no transaction is opened, every row sharing it is classified
/// from that single error via `classify_replay_failure`, same as before this issue.
///
/// Once a transaction is open, a per-row *execute* failure is a different matter: lbug's engine
/// rolls back the **whole transaction** on any execute-time exception, not just the failing
/// statement (verified against the vendored C++ source — see
/// `db::lbug_transaction_semantics_pinning_tests` and `docs/adr/0047-wal-replay-transaction-boundaries.md`).
/// So a per-row execute failure here stops the row loop entirely: the triggering row is
/// classified via `classify_replay_failure` exactly as before, every *other* row in the batch —
/// whether it executed successfully earlier in this same transaction (now discarded) or was
/// never attempted — is counted into `stats.rolled_back_lines`, and no explicit `ROLLBACK` is
/// issued (the engine has already rolled back and cleared its transaction state; an explicit
/// `ROLLBACK` at that point would itself error — see the pinning test). This retires the old
/// per-row probe-execute-failure retry-in-place mechanism: that mechanism depended on being able
/// to keep executing later rows in the same batch after an earlier one failed, which whole-
/// transaction rollback makes impossible to do safely. The prepare-time probe-rejection fallback
/// below (tried before `BEGIN` is issued) is unaffected — it's about which template gets prepared,
/// not about recovering from an execute-time failure inside an open transaction.
///
/// `cancel_fn` is checked once per row, before executing it (issue #240, User Story 2). A
/// mid-batch cancellation issues an explicit `ROLLBACK` (there is no engine-side auto-rollback
/// for cancellation, since it isn't an exception), counts the *entire* batch — including any
/// rows that already executed successfully earlier in this same transaction — into
/// `stats.rolled_back_lines`, and returns a `cancelled` outcome so `replay_opts` stops reading
/// further WAL lines.
///
/// `cache` (issue #238) carries a prepared statement across consecutive calls to this function
/// within one `replay_opts` run. When the batch's template matches `cache`'s, the cached
/// statement is reused and no `conn.prepare()` call is made at all — this is what bounds
/// `stats.prepare_calls` to the distinct-template count for a homogeneous WAL (FR-001/FR-002).
/// On a cache miss, the existing prepare logic runs unchanged (including error classification,
/// FR-005). The *settled* `(template, use_probe, statement)` is written back into `cache` only
/// when this batch's transaction actually commits; on any failure or cancellation the cache is
/// dropped instead, so the next flush of the same template re-probes fresh rather than
/// potentially reusing a statement whose most recent use ended in a rolled-back transaction.
fn flush_batch(
    batch: &mut ReplayBatch,
    conn: &Conn<'_>,
    stats: &mut ReplayStats,
    sample_cap: usize,
    cache: &mut Option<PreparedCache>,
    cancel_fn: Option<&CancelFn>,
) -> Result<FlushOutcome, Error> {
    if batch.is_empty() {
        return Ok(FlushOutcome { cancelled: false });
    }
    let rows = std::mem::take(&mut batch.rows);
    let match_prefixed = std::mem::take(&mut batch.match_prefixed);
    let seqs = std::mem::take(&mut batch.seqs);
    let batch_len = rows.len();
    let max_seq_in_batch = seqs.iter().max().copied();

    let is_match_prefixed_batch = match_prefixed.first().copied().unwrap_or(false);
    let is_delete_form_batch = is_match_prefixed_batch && is_delete_form(&batch.template);

    let cached = cache.take().filter(|c| c.template == batch.template);

    let (mut prepared, use_probe) = if let Some(c) = cached {
        (c.statement, c.use_probe)
    } else if is_match_prefixed_batch {
        let probed_template = with_match_count_probe(&batch.template);
        stats.prepare_calls += 1;
        match conn.prepare(&probed_template) {
            Ok(p) => (p, true),
            Err(probe_err) => {
                eprintln!(
                    "[WAL WARN] RETURN count(*) probe rejected by prepare() ({probe_err}); \
                     falling back to unprobed replay for this batch (no-op detection \
                     unavailable for these rows) — probed template: {probed_template:?}"
                );
                stats.prepare_calls += 1;
                match conn.prepare(&batch.template) {
                    Ok(p) => (p, false),
                    Err(e) => {
                        let err_str = e.to_string();
                        for _ in &match_prefixed {
                            classify_replay_failure(&err_str, &batch.template, stats, sample_cap);
                        }
                        batch.clear();
                        return Ok(FlushOutcome { cancelled: false });
                    }
                }
            }
        }
    } else {
        stats.prepare_calls += 1;
        match conn.prepare(&batch.template) {
            Ok(p) => (p, false),
            Err(e) => {
                let err_str = e.to_string();
                for _ in &match_prefixed {
                    classify_replay_failure(&err_str, &batch.template, stats, sample_cap);
                }
                batch.clear();
                return Ok(FlushOutcome { cancelled: false });
            }
        }
    };

    conn.exec_transaction_control("BEGIN TRANSACTION")?;

    let mut local_lines_replayed: u64 = 0;
    let mut local_match_prefixed_replayed: u64 = 0;
    let mut local_match_prefixed_no_op: u64 = 0;
    let mut local_match_delete_no_op: u64 = 0;

    for (row, is_match_prefixed) in rows.into_iter().zip(match_prefixed) {
        if let Some(cancel) = cancel_fn {
            if cancel() {
                conn.exec_transaction_control("ROLLBACK")?;
                stats.rolled_back_lines += batch_len as u64;
                stats.transactions_rolled_back += 1;
                *cache = None;
                batch.clear();
                return Ok(FlushOutcome { cancelled: true });
            }
        }

        let params = serde_json::Value::Object(row);
        let exec_result = if is_match_prefixed && use_probe {
            conn.execute_prepared_returning_count(&mut prepared, &params)
                .map(Some)
        } else {
            conn.execute_prepared(&mut prepared, &params).map(|()| None)
        };

        match exec_result {
            Ok(Some(count)) if count > 0 => {
                local_lines_replayed += 1;
                local_match_prefixed_replayed += 1;
            }
            Ok(Some(_)) => {
                if is_delete_form_batch {
                    local_match_delete_no_op += 1;
                } else {
                    local_match_prefixed_no_op += 1;
                }
            }
            Ok(None) => {
                local_lines_replayed += 1;
                if is_match_prefixed {
                    local_match_prefixed_replayed += 1;
                }
            }
            Err(e) => {
                // The engine has already rolled back the whole transaction — no explicit
                // ROLLBACK here (it would itself error; see this function's doc comment).
                classify_replay_failure(&e.to_string(), &batch.template, stats, sample_cap);
                stats.rolled_back_lines += (batch_len - 1) as u64;
                stats.transactions_rolled_back += 1;
                *cache = None;
                batch.clear();
                return Ok(FlushOutcome { cancelled: false });
            }
        }
    }

    conn.exec_transaction_control("COMMIT")?;
    stats.lines_replayed += local_lines_replayed;
    stats.match_prefixed_replayed += local_match_prefixed_replayed;
    stats.match_prefixed_no_op += local_match_prefixed_no_op;
    stats.match_delete_no_op += local_match_delete_no_op;
    stats.transactions_committed += 1;
    if let Some(max_seq) = max_seq_in_batch {
        stats.last_committed_seq =
            Some(stats.last_committed_seq.map_or(max_seq, |m| m.max(max_seq)));
    }

    // Carry the settled (template, use_probe, statement) forward for the next flush of the
    // same template to reuse (see this function's doc comment) — only on a committed batch.
    *cache = Some(PreparedCache {
        template: std::mem::take(&mut batch.template),
        use_probe,
        statement: prepared,
    });
    batch.clear();
    Ok(FlushOutcome { cancelled: false })
}

/// Classifies a replay failure (from prepare or execute) as legacy-skipped vs. genuine
/// failure, updating `stats`. Genuine failures record — or bump the count of — a sample keyed
/// by the full `(template, error)` pair (there is no interpolated string under bound-parameter
/// execution), so a template that fails on every row in a batch consumes exactly one sample slot
/// regardless of how many rows share it (FR-001–FR-003). `failed_lines` is incremented
/// unconditionally, independent of sample-cap/dedup bookkeeping (FR-004).
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
        stats.failed_lines += 1;
        // Log only on a category's first occurrence, not once per row: the JSON `failed_samples`
        // payload was already deduplicated by (template, error) (FR-001), but before this, the
        // `eprintln!` fired unconditionally per row — so a template failing on every row of a
        // multi-hour replay still flooded the service log (the primary post-mortem artifact for
        // a long-running rebuild) with millions of byte-identical `[WAL WARN]` lines even though
        // the JSON response no longer did.
        if let Some(existing) = stats
            .failed_samples
            .iter_mut()
            .find(|s| s.template == template && s.error == err_str)
        {
            existing.count += 1;
        } else if stats.failed_samples.len() < sample_cap {
            eprintln!("[WAL WARN] replay execution error: {err_str} | cypher: {cypher_preview}");
            stats.failed_samples.push(FailureSample {
                cypher: cypher_preview,
                error: err_str.to_string(),
                count: 1,
                template: template.to_string(),
            });
        } else {
            // A genuinely new (template, error) category, but `sample_cap` distinct categories
            // are already stored — this is the same "one category hides another" failure mode
            // FR-001 targeted, just resurfacing at a higher threshold. Log its first occurrence
            // once (so it isn't silently absent from both the log and the JSON payload) and
            // count it so the caller can report e.g. "10 of 14 categories shown" instead of
            // truncating without a trace.
            eprintln!(
                "[WAL WARN] replay execution error (new category dropped, cap={sample_cap} \
                 reached): {err_str} | cypher: {cypher_preview}"
            );
            stats.failed_sample_categories_dropped += 1;
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

    fn zero_stats() -> ReplayStats {
        ReplayStats {
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
            match_prefixed_no_op: 0,
            match_delete_no_op: 0,
            seq_regressions: 0,
            failed_sample_categories_dropped: 0,
            prepare_calls: 0,
            rolled_back_lines: 0,
            last_committed_seq: None,
            transactions_committed: 0,
            transactions_rolled_back: 0,
        }
    }

    /// FR-008/SC-003: a wholly-unrecognised WAL (`total` driven entirely by
    /// `unrecognised_lines`) must not leave the denominator at 0 — it must warn.
    #[test]
    fn test_compute_fidelity_warning_all_unrecognised() {
        let stats = ReplayStats {
            unrecognised_lines: 5,
            ..zero_stats()
        };
        let warning = compute_fidelity_warning(&stats, 0.10);
        assert!(
            warning.is_some(),
            "a wholly-unrecognised WAL must produce a fidelity_warning, not a zero-denominator no-op"
        );
        let msg = warning.unwrap();
        assert!(
            msg.contains("unrecognised=5"),
            "message must break down bucket counts so an operator can tell 'wrong directory' \
             (unrecognised) from 'schema is broken' (failed/no-op): {msg}"
        );
    }

    /// FR-008/SC-003: same as above but for `unparseable_lines` (corrupt JSON), the other
    /// counter defect C names.
    #[test]
    fn test_compute_fidelity_warning_all_unparseable() {
        let stats = ReplayStats {
            unparseable_lines: 5,
            ..zero_stats()
        };
        assert!(
            compute_fidelity_warning(&stats, 0.10).is_some(),
            "a wholly-unparseable WAL must produce a fidelity_warning"
        );
    }

    /// SC-004 regression guard: a fully healthy replay (zero failures/unrecognised/unparseable)
    /// must not warn.
    #[test]
    fn test_compute_fidelity_warning_healthy_replay_none() {
        let stats = ReplayStats {
            lines_replayed: 100,
            ..zero_stats()
        };
        assert!(compute_fidelity_warning(&stats, 0.10).is_none());
    }

    /// SC-004 / FR-009 regression guard: `legacy_skipped_lines` alone must never push a replay
    /// over the warning threshold — it stays excluded from both sides of the ratio, unchanged by
    /// the FR-008 fix.
    #[test]
    fn test_compute_fidelity_warning_legacy_skip_only_none() {
        let stats = ReplayStats {
            legacy_skipped_lines: 1000,
            ..zero_stats()
        };
        assert!(
            compute_fidelity_warning(&stats, 0.10).is_none(),
            "legacy_skipped_lines must not by itself trigger a fidelity_warning"
        );
    }

    /// Regression guard for the pre-existing `match_delete_no_op` exclusion (ADR-0026): it must
    /// stay excluded from both sides of the ratio, same as `legacy_skipped_lines`.
    #[test]
    fn test_compute_fidelity_warning_match_delete_no_op_only_none() {
        let stats = ReplayStats {
            match_delete_no_op: 1000,
            ..zero_stats()
        };
        assert!(compute_fidelity_warning(&stats, 0.10).is_none());
    }

    // ── Prepared-statement cache tests (issue #238) ─────────────────────────

    use std::io::Write;

    use crate::db::Db;

    fn make_db_with_schema(dir: &tempfile::TempDir) -> Db {
        let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
        let db = Db::open(&db_path).unwrap();
        {
            let conn = db.connect().unwrap();
            conn.init_schema(4).unwrap();
        }
        db
    }

    /// Appends one WAL line to `wal_dir/filename` using a caller-chosen `cypher` template so
    /// tests can control how many distinct templates a run sees.
    fn write_wal_cypher_line(
        wal_dir: &std::path::Path,
        filename: &str,
        seq: u64,
        cypher: &str,
        uuid: &str,
    ) {
        let content = format!(
            "{{\"seq\":{seq},\"ts\":\"2026-01-01T00:00:00Z\",\"db\":\"test\",\
             \"cypher\":{cypher_json},\"params\":{{\"uuid\":\"{uuid}\"}}}}\n",
            cypher_json = serde_json::to_string(cypher).unwrap(),
        );
        let path = wal_dir.join(filename);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // FR-002/FR-007/SC-001: a single template repeated across many lines — enough to trigger
    // several batch_size-driven flushes — must reuse the same prepared statement across those
    // flushes instead of re-preparing on each one.
    #[test]
    fn prepare_calls_bounded_for_homogeneous_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        let total_lines = 10;
        for i in 0..total_lines {
            write_wal_cypher_line(
                &wal_dir,
                "0001.jsonl",
                i + 1,
                "CREATE (:Episodic {uuid: $uuid})",
                &format!("ep-{i}"),
            );
        }

        let conn = db.connect().unwrap();
        let stats = WalReplayer::new(&wal_dir)
            .replay_opts(
                &conn,
                ReplayOptions {
                    batch_size: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();

        // batch_size=3 over 10 lines forces 4 flushes (3, 3, 3, 1) of the same template — all
        // but the first must be a cache hit.
        assert_eq!(stats.prepare_calls, 1);
        assert_eq!(stats.lines_replayed, total_lines);
        assert_eq!(stats.failed_lines, 0);
    }

    // FR-002/FR-007/SC-001: several distinct templates, each repeating in its own long
    // consecutive (non-interleaved) run, must bound prepare_calls by the distinct-template
    // count, not by the number of batches or lines.
    #[test]
    fn prepare_calls_proportional_to_distinct_templates() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        let templates = [
            "CREATE (:Episodic {uuid: $uuid})",
            "CREATE (:Episodic {uuid: $uuid, name: 'a'})",
            "CREATE (:Episodic {uuid: $uuid, name: 'b'})",
        ];
        let lines_per_template = 7;
        let mut seq = 0u64;
        for (t_idx, template) in templates.iter().enumerate() {
            for i in 0..lines_per_template {
                seq += 1;
                write_wal_cypher_line(
                    &wal_dir,
                    "0001.jsonl",
                    seq,
                    template,
                    &format!("ep-{t_idx}-{i}"),
                );
            }
        }

        let conn = db.connect().unwrap();
        let stats = WalReplayer::new(&wal_dir)
            .replay_opts(
                &conn,
                ReplayOptions {
                    batch_size: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(stats.prepare_calls, templates.len() as u64);
        assert_eq!(
            stats.lines_replayed,
            templates.len() as u64 * lines_per_template
        );
        assert_eq!(stats.failed_lines, 0);
    }

    // ADR-0045: pins the accepted LRU-1 limitation so a future change to it (e.g. a bounded
    // multi-entry cache) doesn't silently alter this documented trade-off. An interleaved WAL
    // (A, B, A, B, ...) never has two consecutive same-template flushes, so every flush is a
    // cache miss and prepare_calls must equal the number of lines, not the number of distinct
    // templates.
    #[test]
    fn prepare_calls_unbounded_for_fully_interleaved_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        let templates = [
            "CREATE (:Episodic {uuid: $uuid})",
            "CREATE (:Episodic {uuid: $uuid, name: 'a'})",
        ];
        let total_lines = 10;
        for i in 0..total_lines {
            write_wal_cypher_line(
                &wal_dir,
                "0001.jsonl",
                i + 1,
                templates[(i % 2) as usize],
                &format!("ep-{i}"),
            );
        }

        let conn = db.connect().unwrap();
        let stats = WalReplayer::new(&wal_dir)
            .replay_opts(
                &conn,
                ReplayOptions {
                    batch_size: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            stats.prepare_calls, total_lines,
            "an alternating template never has consecutive same-template flushes, so LRU-1 \
             provides zero benefit by design — see ADR-0045"
        );
        assert_eq!(stats.lines_replayed, total_lines);
    }

    // Edge case (spec.md): the cache is not required to survive a WAL file boundary, but is
    // also not prohibited from doing so if the next file happens to start with the same
    // template — pin the latter, since it's a real efficiency win when it happens naturally.
    #[test]
    fn prepare_calls_bounded_across_wal_file_boundary_for_same_template() {
        let dir = tempfile::TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let db = make_db_with_schema(&dir);
        let template = "CREATE (:Episodic {uuid: $uuid})";
        for i in 0..5 {
            write_wal_cypher_line(&wal_dir, "0001.jsonl", i + 1, template, &format!("a-{i}"));
        }
        for i in 0..5 {
            write_wal_cypher_line(
                &wal_dir,
                "0002.jsonl",
                5 + i + 1,
                template,
                &format!("b-{i}"),
            );
        }

        let conn = db.connect().unwrap();
        let stats = WalReplayer::new(&wal_dir)
            .replay_opts(
                &conn,
                ReplayOptions {
                    batch_size: Some(64),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            stats.prepare_calls, 1,
            "the same template recurring immediately across a file boundary should still hit \
             the cache — this is a bonus, not a requirement (spec.md edge cases)"
        );
        assert_eq!(stats.lines_replayed, 10);
    }
}
