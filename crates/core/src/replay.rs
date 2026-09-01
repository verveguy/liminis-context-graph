use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::db::Conn;
use crate::error::Error;
use crate::legacy_wal::{expand_bulk_property_set, strip_vecf32};
use crate::wal::{first_seq_in_file, strip_quoted_literals, WalLine};

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
    /// Count of embedding vector params (`name_embedding`/`fact_embedding`/`content_embedding`/
    /// `summary_embedding`) successfully derived by invoking `ReplayOptions::recompute_embed_fn`
    /// on the row's co-located source text, replacing whatever the WAL record carried for that
    /// key — recompute runs unconditionally, on every replay, for every row whose Cypher
    /// template references a recognized vector placeholder (issue #526, FR-001/FR-002).
    pub embeddings_recomputed: u64,
    /// Count of times `ReplayOptions::recompute_embed_fn` was actually invoked (issue #440,
    /// FR-003). This measures callback invocations, including callback cache hits — it does not
    /// measure underlying embedder computations or cache misses, because caching (e.g.
    /// `EmbeddingCache`) happens inside the caller-supplied closure rather than in `replay.rs`
    /// itself. SC-005's cache-effectiveness bound is verified against this counter via a plain
    /// counting closure that wraps the cache, not against `EmbeddingCache` internals directly.
    pub embed_calls: u64,
    /// Count of embedding vector params that had no co-located source text available to recompute
    /// from (issue #526, FR-002/FR-005) — renamed from the pre-#526 `embeddings_recompute_fallback`
    /// now that there is no stored vector left to fall back to. What happens to the row depends on
    /// whether the vector param is the mutation's only purpose: a vector-only `SET` with no text is
    /// skipped entirely (counted in [`Self::embeddings_skip_rows`], preserving whatever the
    /// entity's own CREATE record already computed for that column), while any other record (most
    /// commonly a CREATE, which must still be created) gets a same-dimension zero vector instead —
    /// see `is_vector_only_set`. Never a failure by itself; this is normal, ongoing WAL shape (e.g.
    /// the pre-existing Python/graphiti-driver SET-only vector updates in
    /// `tests/fixtures/wal/python_produced.jsonl`), not evidence the embedder is unreachable.
    pub embeddings_recompute_skipped_no_text: u64,
    /// Count of embedding vector params left bound to their stored WAL value, verbatim, because
    /// `recompute_embed_fn` was invoked but returned an error (issue #440) — e.g. the embedder
    /// sidecar was transiently unreachable. Never fatal to replay (recompute is explicitly
    /// self-healing, per the spec's Assumptions), but counted separately from
    /// `embeddings_recompute_skipped_no_text` so a transient outage is never silent.
    pub embeddings_recompute_failed: u64,
    /// Count of WAL rows skipped entirely — never executed against the database at all — because
    /// they were a vector-only `SET` mutation (see `is_vector_only_set`) with no co-located source
    /// text to recompute from, and no failure either (issue #526, FR-005). Executing such a row
    /// with a placeholder zero vector would *overwrite* whatever real vector the entity's own
    /// CREATE record already computed for that column, actively degrading it — skipping preserves
    /// it. This is the one WAL record shape confirmed to have no source text co-located in the
    /// same record (the pre-existing Python/graphiti-driver SET-only vector updates already in the
    /// wild); every other vector-bearing record kind either already co-locates its text or, for a
    /// CREATE-type record that fails recompute, gets a zero-vector fallback instead of being
    /// skipped (see `embeddings_recompute_skipped_no_text`). Excluded from `lines_replayed` and
    /// from the `fidelity_warning` ratio, matching `legacy_skipped_lines`'s precedent: this is
    /// benign, expected WAL shape, not a sign of a broken replay.
    pub embeddings_skip_rows: u64,
}

impl ReplayStats {
    /// Sum of `unrecognised_lines + failed_lines + unparseable_lines`.
    /// Retained for back-compat: equals the old `lines_skipped` field.
    pub fn lines_skipped(&self) -> u64 {
        self.unrecognised_lines + self.failed_lines + self.unparseable_lines
    }

    /// True when no embedding recompute *attempt* failed during this replay
    /// (`embeddings_recompute_failed == 0`) — e.g. the embedder sidecar was never unreachable,
    /// no recomputed vector had the wrong dimension, none was non-finite. Deliberately does
    /// **not** require `embeddings_recompute_skipped_no_text == 0`: a row with no co-located
    /// source text (FR-002/FR-005 — e.g. a `SET`-only mutation that updates a field without
    /// re-supplying the text an already-recomputed vector on that node was derived from) is
    /// normal, ongoing WAL shape, not a defect, and is common enough on real corpora that
    /// requiring zero skips would make a replay/rebuild almost never confirm a match. A replay
    /// call site persisting an embedding identity (issue #440, FR-006/FR-007) alongside
    /// `applied_seq` should gate that write on this: persisting `Some(identity)` after a real
    /// recompute failure would make `embedding_model_status` read `"match"` for a group that
    /// silently kept stale vectors because the embedder couldn't be reached — the exact
    /// silent-divergence FR-008 exists to prevent. `embeddings_recompute_skipped_no_text` remains
    /// a purely informational counter.
    pub fn embeddings_recompute_had_no_failures(&self) -> bool {
        self.embeddings_recompute_failed == 0
    }
}

/// Callback invoked during replay to emit progress; returning `false` aborts cleanly.
pub type ProgressFn = Box<dyn Fn(&ReplayProgress) -> bool + Send>;
/// Callback invoked once per mutation; returning `true` aborts immediately.
pub type CancelFn = Box<dyn Fn() -> bool + Send>;
/// Callback that receives a `[WAL PROGRESS]` log line; replaces `eprintln!` in tests.
pub type ProgressLogFn = Box<dyn Fn(&str) + Send>;
/// Synchronous embedding callback invoked once per WAL row whose Cypher template references a
/// recognized embedding vector placeholder (issue #526, FR-001/FR-002). `WalReplayer` itself has
/// no tokio dependency and several call sites run with no ambient runtime at all (its own unit
/// tests, `real_corpus_replay_perf.rs`) — so recompute is expressed as a plain sync closure, and
/// each caller bridges its own `Arc<dyn Embedder>` into this shape however fits its own runtime
/// context (`Handle::current().block_on` from inside `spawn_blocking`, or a small dedicated
/// single-threaded runtime for a bare-sync caller). A caller wanting a cache (FR-003 of issue
/// #440) wraps `EmbeddingCache::get_or_compute` inside this closure — `replay.rs` never
/// constructs or touches a cache itself.
pub type RecomputeEmbedFn = Box<dyn Fn(&str) -> Result<Vec<f32>, Error> + Send>;

/// `(embedding vector param, co-located source-text param)` pairs recognized during replay
/// recompute (issue #526, FR-001/FR-002) — the crate-wide single source of truth for which
/// columns are embedding vectors, shared with [`crate::wal::VECTOR_PARAM_KEYS`] (the writer's
/// strip list; kept in sync by `tests::embedding_text_pairs_key_set_matches_wal_strip_list`).
///
/// A pair's relevance to a given WAL row is decided by whether the row's **Cypher template**
/// contains the vector param's `$placeholder` — not by whether the row's params object happens to
/// carry that key. This is what lets replay recognize a vector-bearing row after the writer has
/// stopped emitting the param at all (issue #526): the template still says `$name_embedding`,
/// even though nothing supplies it any more. It also means an older, vector-bearing WAL replays
/// identically — the template text is unchanged either way, only whether the JSON also happens to
/// carry a stale value differs, and that stale value is always ignored (FR-002/FR-003).
const EMBEDDING_TEXT_PAIRS: &[(&str, &str)] = &[
    ("name_embedding", "name"),
    ("fact_embedding", "fact"),
    ("content_embedding", "content"),
    ("summary_embedding", "summary"),
];

/// Options for `WalReplayer::replay_opts`.
pub struct ReplayOptions {
    /// Skip WAL lines with `seq < from_seq`. Default: 0 (replay all).
    pub from_seq: u64,
    /// Skip WAL lines with `seq > to_seq`. Default: `None` (unbounded — replay to the end of
    /// the WAL). `None` is semantically distinct from `Some(0)`: absence means unbounded, not
    /// "bounded to seq 0".
    pub to_seq: Option<u64>,
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
    /// Mandatory recompute-on-replay callback (issue #526, FR-001/FR-002) — replay never binds a
    /// vector value found in a WAL record. Every row whose Cypher template references a
    /// recognized embedding vector placeholder (`name_embedding`/`fact_embedding`/
    /// `content_embedding`/`summary_embedding`, see [`EMBEDDING_TEXT_PAIRS`]) always has its
    /// vector recomputed by this callback from co-located source text. There is no "disabled"
    /// mode and no `Option` wrapper: once the writer stops emitting vector params
    /// unconditionally, a caller with no embedder has no safe value to bind for a `CREATE`
    /// template's vector placeholder at all, so every caller — production and test alike — must
    /// supply one. See [`ReplayOptions::new`] and `zero_vector_embed_fn` for callers that don't
    /// care about embedding fidelity.
    pub recompute_embed_fn: RecomputeEmbedFn,
    /// The embedder's fixed output dimension (issue #526). Used to size a same-dimension
    /// zero-vector fallback when recompute has no source text (or fails) for a record that isn't
    /// a vector-only `SET` and so must still be executed (see `is_vector_only_set`), and to
    /// reject a recomputed vector whose length doesn't match — `lbug`'s embedding columns are
    /// fixed-width (`FLOAT[dim]`), so binding a wrong-length vector fails at `execute_prepared`
    /// and rolls back the *entire* same-template batch (see `flush_batch`), not just one row.
    pub recompute_embed_dim: usize,
}

impl ReplayOptions {
    /// Builds `ReplayOptions` with `recompute_embed_fn`/`recompute_embed_dim` set and every other
    /// field at its convenience default (unbounded `from_seq`/`to_seq`, not a dry run, no
    /// progress/cancel callbacks, batch size/failure cap/log interval read from their env vars,
    /// no progress log sink).
    pub fn new(recompute_embed_fn: RecomputeEmbedFn, recompute_embed_dim: usize) -> Self {
        Self {
            from_seq: 0,
            to_seq: None,
            dry_run: false,
            progress_fn: None,
            cancel_fn: None,
            failure_sample_cap: None,
            batch_size: None,
            log_interval_override: None,
            progress_log_fn: None,
            recompute_embed_fn,
            recompute_embed_dim,
        }
    }
}

/// A trivial [`RecomputeEmbedFn`] that returns a fixed-size zero vector for any input text,
/// ignoring the text entirely. `ReplayOptions::recompute_embed_fn` is mandatory (issue #526), so
/// every caller needs *some* correctly-sized function to supply — this one is for a caller that
/// just needs a schema-valid vector bound and doesn't itself care about embedding fidelity (most
/// commonly a test replaying a fixture that doesn't exercise recompute correctness). A test that
/// does care about fidelity should bridge a real `Embedder` (e.g. `HashEmbedder`, `NameMapEmbedder`)
/// into a sync closure instead.
pub fn zero_vector_embed_fn(dim: usize) -> RecomputeEmbedFn {
    Box::new(move |_text: &str| Ok(vec![0.0f32; dim]))
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
    ///
    /// `recompute_embed_fn`/`recompute_embed_dim` are mandatory (issue #526) — see
    /// [`ReplayOptions::recompute_embed_fn`] for why there is no "disabled" mode.
    pub fn replay(
        &self,
        conn: &Conn<'_>,
        recompute_embed_fn: RecomputeEmbedFn,
        recompute_embed_dim: usize,
    ) -> Result<ReplayStats, Error> {
        self.replay_opts(
            conn,
            ReplayOptions::new(recompute_embed_fn, recompute_embed_dim),
        )
    }

    /// Like `replay` but with `from_seq`/`to_seq` filtering, dry-run mode, and optional progress
    /// callback.
    ///
    /// - Lines with `seq < opts.from_seq` or `seq > opts.to_seq` (when `Some`) are skipped
    ///   without counting against `lines_skipped`.
    /// - When `opts.dry_run`, mutations are counted but not executed against the DB.
    /// - `opts.progress_fn` is called once per file and once per 1000 mutations within a file;
    ///   returning `false` aborts the replay cleanly.
    pub fn replay_opts(&self, conn: &Conn<'_>, opts: ReplayOptions) -> Result<ReplayStats, Error> {
        // Reject an invalid bound pair before touching any WAL files. `handle_rebuild_from_wal`
        // already validates this for MCP callers (FR-003), but `replay_opts` is a public API
        // reachable directly (recovery.rs, tests, future callers) — without this check, a
        // caller-side bug (to_seq < from_seq) would silently filter out every line and return a
        // "successful" zero-line replay instead of surfacing the mistake.
        if let Some(to_seq) = opts.to_seq {
            if to_seq < opts.from_seq {
                return Err(Error::Config(format!(
                    "invalid ReplayOptions: to_seq ({to_seq}) must not be less than from_seq \
                     ({from_seq})",
                    from_seq = opts.from_seq
                )));
            }
        }

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
            embeddings_recomputed: 0,
            embed_calls: 0,
            embeddings_recompute_skipped_no_text: 0,
            embeddings_recompute_failed: 0,
            embeddings_skip_rows: 0,
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

                // to_seq filter — skip without counting as skipped, symmetric to from_seq
                if let Some(to_seq) = opts.to_seq {
                    if wal_line.seq > to_seq {
                        continue;
                    }
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
                    let mut params_map = match params {
                        serde_json::Value::Object(m) => m,
                        _ => serde_json::Map::new(),
                    };

                    // Recompute embedding vectors from co-located source text (issue #526,
                    // FR-001/FR-002), before this row is ever pushed into a batch — never inside
                    // flush_batch's open transaction, so a slow/unreachable embedder cannot hold
                    // an lbug transaction open across up to `batch_size` network round-trips.
                    // Mandatory: there is no "recompute disabled" mode any more (issue #526) —
                    // every row whose template references a recognized vector placeholder always
                    // goes through this, whether the WAL is fresh (no stored value at all) or
                    // legacy (a stored value present but always ignored, FR-002/FR-003).
                    let outcome = recompute_row_embeddings(
                        &norm_cypher,
                        &mut params_map,
                        opts.recompute_embed_fn.as_ref(),
                        opts.recompute_embed_dim,
                        &mut stats,
                    );

                    if outcome == RowEmbeddingOutcome::Skip {
                        // A vector-only `SET` with no source text to recompute from (issue #526,
                        // FR-005) — executing it with a placeholder would overwrite whatever real
                        // vector the entity's own CREATE record already computed for that column.
                        // Skip the row entirely: never pushed into the batch, never executed.
                        stats.embeddings_skip_rows += 1;
                    } else {
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

/// Caps individual `[WAL WARN] embedding recompute failed` lines (issue #440) — an unreachable
/// embedder sidecar can fail on every recognized row for the rest of the replay, which would
/// otherwise emit one uncapped `eprintln!` per row. `stats.embeddings_recompute_failed` still
/// counts every occurrence; only the per-row detail log is capped, mirroring
/// `SEQ_REGRESSION_LOG_CAP`'s existing precedent for the same reason.
const EMBED_FAILURE_LOG_CAP: u64 = 10;

/// Result of [`recompute_row_embeddings`] for one WAL row.
#[derive(Debug, PartialEq, Eq)]
enum RowEmbeddingOutcome {
    /// Execute the row normally (the common case — either no vector param applied, or every
    /// applicable one was recomputed or zero-filled).
    Proceed,
    /// Skip the row entirely — never execute it against the database at all. Only produced for a
    /// vector-only `SET` mutation (see [`is_vector_only_set`]) with no source text to recompute
    /// from: executing it with a placeholder zero vector would overwrite whatever real vector the
    /// entity's own CREATE record already computed for that column (issue #526, FR-005).
    Skip,
}

/// Recomputes each recognized embedding vector param referenced by `cypher` from its co-located
/// source-text param (issue #526, FR-001/FR-002/SC-005), mutating `params_map` in place and
/// updating `stats`'s recompute counters. Replay never binds a vector value found in a WAL record
/// — every recognized vector param is always either freshly recomputed or explicitly zero-filled,
/// regardless of whether `params_map` happens to carry a stored value at all (FR-002/FR-003).
///
/// For each `(vec_key, text_key)` pair in [`EMBEDDING_TEXT_PAIRS`]: relevance is decided by
/// whether **`cypher`** references `$vec_key` — not by whether `params_map` carries the key —
/// since the writer no longer emits it at all for a freshly-written WAL (issue #526). A row whose
/// template doesn't reference `vec_key` is left alone (most WAL rows reference none of the four).
/// For a row that does:
/// - `text_key` present in `params_map` as a non-empty string → `embed_fn` is invoked once;
///   success replaces `vec_key`'s value and bumps `embeddings_recomputed`. Failure (embedder
///   error, a recomputed vector whose length doesn't match `embed_dim`, or a computed vector
///   containing NaN/infinity that can't round-trip through JSON) falls through to the same
///   zero-fill-or-skip handling as missing text, below, and bumps `embeddings_recompute_failed`
///   (never fatal — recompute is explicitly self-healing).
/// - `text_key` missing/empty, or recompute failed → [`is_vector_only_set`] decides what happens:
///   a vector-only `SET` (no other real content in the same mutation) produces
///   [`RowEmbeddingOutcome::Skip`] so the caller drops the row entirely, preserving whatever
///   value already sits in that column; any other record (most commonly a `CREATE`, which must
///   still be created — dropping it would fail SC-002's count parity) gets a same-dimension zero
///   vector bound instead. Counted in `embeddings_recompute_skipped_no_text`.
fn recompute_row_embeddings(
    cypher: &str,
    params_map: &mut serde_json::Map<String, serde_json::Value>,
    embed_fn: &(dyn Fn(&str) -> Result<Vec<f32>, Error> + Send),
    embed_dim: usize,
    stats: &mut ReplayStats,
) -> RowEmbeddingOutcome {
    let mut outcome = RowEmbeddingOutcome::Proceed;

    for (vec_key, text_key) in EMBEDDING_TEXT_PAIRS {
        if !cypher.contains(&format!("${vec_key}")) {
            continue;
        }

        let text = params_map.get(*text_key).and_then(|v| v.as_str());
        let recomputed = match text {
            Some(text) if !text.is_empty() => {
                stats.embed_calls += 1;
                match embed_fn(text).and_then(|vector| {
                    if vector.len() != embed_dim {
                        return Err(Error::Ipc(format!(
                            "recomputed embedding has {} components but the configured \
                             embedding dimension is {embed_dim}",
                            vector.len()
                        )));
                    }
                    vector_to_json(&vector).ok_or_else(|| {
                        Error::Ipc(
                            "recomputed embedding vector contains a non-finite value".to_string(),
                        )
                    })
                }) {
                    Ok(json_vector) => {
                        stats.embeddings_recomputed += 1;
                        Some(json_vector)
                    }
                    Err(e) => {
                        if stats.embeddings_recompute_failed < EMBED_FAILURE_LOG_CAP {
                            eprintln!("[WAL WARN] embedding recompute failed for {vec_key}: {e}");
                        } else if stats.embeddings_recompute_failed == EMBED_FAILURE_LOG_CAP {
                            eprintln!(
                                "[WAL WARN] embedding recompute: further per-row failure \
                                 details suppressed after {EMBED_FAILURE_LOG_CAP} — see the \
                                 final embeddings_recompute_failed count for the true total"
                            );
                        }
                        stats.embeddings_recompute_failed += 1;
                        None
                    }
                }
            }
            _ => {
                stats.embeddings_recompute_skipped_no_text += 1;
                None
            }
        };

        match recomputed {
            Some(json_vector) => {
                params_map.insert((*vec_key).to_string(), json_vector);
            }
            None if is_vector_only_set(cypher, vec_key) => {
                outcome = RowEmbeddingOutcome::Skip;
            }
            None => {
                params_map.insert((*vec_key).to_string(), zero_vector_json(embed_dim));
            }
        }
    }

    outcome
}

/// Converts a computed embedding vector to a JSON array of numbers, or `None` if any component
/// is NaN or infinite (JSON has no representation for either — `serde_json::Number::from_f64`
/// itself returns `None` in that case).
fn vector_to_json(vector: &[f32]) -> Option<serde_json::Value> {
    vector
        .iter()
        .map(|f| serde_json::Number::from_f64(*f as f64).map(serde_json::Value::Number))
        .collect::<Option<Vec<_>>>()
        .map(serde_json::Value::Array)
}

/// A same-dimension all-zero vector, as a JSON array — never NaN/infinite, so this always
/// succeeds (unlike `vector_to_json`, which can reject a genuinely computed vector).
fn zero_vector_json(dim: usize) -> serde_json::Value {
    serde_json::Value::Array(vec![serde_json::json!(0.0); dim])
}

/// Whether `cypher`'s top-level `SET` clause assigns only `vec_key` — i.e. this mutation exists
/// for no purpose other than writing this one vector value, so nothing else can be silently lost
/// by skipping the whole row when no source text is available (issue #526, FR-005). Detected
/// structurally (counting `SET`-clause assignments) rather than via a fixed record-kind list, so
/// it also covers any future single-purpose vector `SET` this codebase hasn't written yet — not
/// just today's known pre-existing Python/graphiti-driver shape
/// (`tests/fixtures/wal/python_produced.jsonl`).
///
/// A `CREATE`-form record (`insert_entity`/`insert_episodic`/`insert_relates_to_edge`/
/// `insert_cross_group_edge`, and `dump.rs`'s `MERGE ... SET a=$a, b=$b, ...` re-materialization
/// templates) is never vector-only: a bare `CREATE (...)` has no `SET` keyword to find at all, and
/// a `MERGE ... SET` template that also sets other real properties (name, summary, fact, ...) has
/// more than one assignment in its `SET` clause — both cases return `false` here regardless of how
/// many properties the record sets, since the row must still be executed to avoid losing the rest
/// of what it creates.
fn is_vector_only_set(cypher: &str, vec_key: &str) -> bool {
    let upper = cypher.to_uppercase();
    let Some(set_idx) = find_word(&upper, "SET") else {
        return false;
    };
    let set_clause = &cypher[set_idx + 3..];
    set_clause.split(',').count() <= 1 && set_clause.contains(&format!("${vec_key}"))
}

/// Finds the byte offset of `word` as a standalone token in `haystack` (already uppercased),
/// bounded by non-alphanumeric characters (whitespace, parentheses, string start/end) — good
/// enough for the fixed, machine-generated Cypher shapes `is_vector_only_set` inspects, unlike
/// `wal::looks_like_mutation`'s fuller tokenizer, which also has to tolerate hand-typed queries
/// through the `cypher` MCP scope.
fn find_word(haystack: &str, word: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let wlen = word.len();
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(word) {
        let idx = start + rel;
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let after_ok = idx + wlen >= bytes.len() || !bytes[idx + wlen].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(idx);
        }
        start = idx + wlen;
    }
    None
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
/// Unlike the cancellation path's `ROLLBACK` (whose failure is deliberately absorbed — see
/// below), a `BEGIN`/`COMMIT` failure here propagates via `?` and aborts the entire
/// `replay_opts` run, including for `Db::open_or_rebuild`'s from-scratch rebuild and
/// `recovery.rs`'s startup auto-recovery. This is intentional, not an oversight: a `COMMIT`
/// failure leaves the actual on-disk commit state ambiguous (did it commit or not?), so
/// silently continuing and recording the batch as committed in `stats` would risk exactly the
/// undefined-state problem this issue exists to eliminate — the DB-open/recovery flows failing
/// loudly on such a (should-never-happen) engine error is preferable to proceeding on an
/// uncertain foundation. The cancellation-`ROLLBACK` case is safe to absorb specifically because
/// the batch is already being discarded either way regardless of whether the `ROLLBACK` itself
/// succeeds — there's no equivalent ambiguity to protect against.
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
                // Cancellation is a normal outcome, not an exception — unlike the genuine
                // execute-failure path below, the engine has not necessarily already cleared
                // its own transaction state, so an explicit ROLLBACK is issued here. But a
                // ROLLBACK failure (e.g. the engine cleared state some other way) must not
                // abort the whole replay and discard `stats`/`last_committed_seq` for every
                // transaction already committed earlier in this run — the batch is being
                // discarded either way, so this is logged and treated as non-fatal.
                if let Err(e) = conn.exec_transaction_control("ROLLBACK") {
                    eprintln!("[WAL WARN] ROLLBACK after cancellation failed (non-fatal): {e}");
                }
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
            ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
        };
        assert_eq!(resolve_batch_size(&opts).unwrap(), 64);
    }

    #[test]
    fn test_resolve_batch_size_rejects_zero() {
        let opts = ReplayOptions {
            batch_size: Some(0),
            ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
        };
        assert!(resolve_batch_size(&opts).is_err());
    }

    #[test]
    fn test_resolve_batch_size_rejects_over_256() {
        let opts = ReplayOptions {
            batch_size: Some(257),
            ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
        };
        assert!(resolve_batch_size(&opts).is_err());
    }

    #[test]
    fn test_resolve_batch_size_accepts_256() {
        let opts = ReplayOptions {
            batch_size: Some(256),
            ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
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
            embeddings_recomputed: 0,
            embed_calls: 0,
            embeddings_recompute_skipped_no_text: 0,
            embeddings_recompute_failed: 0,
            embeddings_skip_rows: 0,
        }
    }

    // ── wal.rs / replay.rs vector-key drift guard (issue #526) ──────────────────────────────

    #[test]
    fn embedding_text_pairs_key_set_matches_wal_strip_list() {
        let mut replay_keys: Vec<&str> = EMBEDDING_TEXT_PAIRS.iter().map(|(k, _)| *k).collect();
        let mut wal_keys: Vec<&str> = crate::wal::VECTOR_PARAM_KEYS.to_vec();
        replay_keys.sort_unstable();
        wal_keys.sort_unstable();
        assert_eq!(
            replay_keys, wal_keys,
            "replay's recompute pairs and wal's strip list must cover exactly the same \
             vector-param keys, or a key present in one but not the other would either leave a \
             vector un-stripped or leave replay unable to recognize it"
        );
    }

    // --- recompute_row_embeddings (issue #526, FR-001/FR-002/FR-005/SC-005) ---

    const ENTITY_CREATE_CYPHER: &str = "CREATE (:Entity {uuid: $uuid, name: $name, \
         name_embedding: $name_embedding, summary: $summary, \
         summary_embedding: $summary_embedding})";

    #[test]
    fn recompute_row_embeddings_uses_embed_fn_when_text_present() {
        let embed_fn: RecomputeEmbedFn = Box::new(|text: &str| Ok(vec![text.len() as f32, 2.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Alice"));

        let outcome = recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(outcome, RowEmbeddingOutcome::Proceed);
        assert_eq!(params["name_embedding"], serde_json::json!([5.0, 2.0]));
        assert_eq!(stats.embeddings_recomputed, 1);
        assert_eq!(stats.embed_calls, 1);
        assert_eq!(stats.embeddings_recompute_skipped_no_text, 0);
        assert_eq!(stats.embeddings_recompute_failed, 0);
    }

    /// FR-002/FR-005: a stored vector in the WAL row is never bound — even when co-located text
    /// is present and recompute succeeds, the stored value (here, a very different vector) is
    /// fully replaced, not merely ignored.
    #[test]
    fn recompute_row_embeddings_never_binds_a_stored_vector_value() {
        let embed_fn: RecomputeEmbedFn = Box::new(|_text: &str| Ok(vec![1.0, 1.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Alice"));
        // A legacy WAL might still carry a stored vector under this key — it must never survive.
        params.insert("name_embedding".to_string(), serde_json::json!([9.0, 9.0]));

        recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(
            params["name_embedding"],
            serde_json::json!([1.0, 1.0]),
            "the stored value must be fully replaced by the recomputed one (FR-002)"
        );
    }

    /// FR-005: a `CREATE`-type record with no co-located text (embedder hiccup notwithstanding,
    /// this shouldn't happen in practice for a real CREATE template, but the row must still be
    /// created for SC-002 count parity) gets a same-dimension zero vector instead of being
    /// skipped or left unbound.
    #[test]
    fn recompute_row_embeddings_zero_fills_a_create_type_row_when_text_missing() {
        let embed_fn: RecomputeEmbedFn =
            Box::new(|_text: &str| panic!("embed_fn must not be called when text is absent"));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        // No "name" param present at all.

        let outcome = recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            3,
            &mut stats,
        );

        assert_eq!(
            outcome,
            RowEmbeddingOutcome::Proceed,
            "a CREATE-type row must never be skipped — dropping it would lose the whole entity"
        );
        assert_eq!(params["name_embedding"], serde_json::json!([0.0, 0.0, 0.0]));
        assert_eq!(stats.embeddings_recompute_skipped_no_text, 1);
        assert_eq!(stats.embed_calls, 0);
        assert_eq!(stats.embeddings_recomputed, 0);
    }

    /// FR-005: a vector-only `SET` mutation (the pre-existing Python/graphiti-driver shape, and
    /// the pre-#526 `backfill_summary_embeddings.rs` shape) with no co-located text is skipped
    /// entirely — never executed — so it can't overwrite a real vector already computed by the
    /// entity's own CREATE record.
    #[test]
    fn recompute_row_embeddings_skips_a_vector_only_set_when_text_missing() {
        let embed_fn: RecomputeEmbedFn =
            Box::new(|_text: &str| panic!("embed_fn must not be called when text is absent"));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("uuid".to_string(), serde_json::json!("ep1"));

        let outcome = recompute_row_embeddings(
            "MATCH (n:Episodic {uuid:$uuid}) SET n.content_embedding=$content_embedding",
            &mut params,
            embed_fn.as_ref(),
            4,
            &mut stats,
        );

        assert_eq!(outcome, RowEmbeddingOutcome::Skip);
        assert_eq!(stats.embeddings_recompute_skipped_no_text, 1);
        assert_eq!(stats.embed_calls, 0);
        assert_eq!(stats.embeddings_recomputed, 0);
        assert!(
            !params.contains_key("content_embedding"),
            "a skipped row's params are never touched — the caller drops the whole row"
        );
    }

    /// An embed-call failure during replay is treated exactly like missing text: a vector-only
    /// `SET` is skipped rather than binding a placeholder that would overwrite a real vector.
    #[test]
    fn recompute_row_embeddings_skips_a_vector_only_set_on_embed_error() {
        let embed_fn: RecomputeEmbedFn =
            Box::new(|_text: &str| Err(Error::Ipc("embedder unreachable".to_string())));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("content".to_string(), serde_json::json!("some content"));

        let outcome = recompute_row_embeddings(
            "MATCH (n:Episodic {uuid:$uuid}) SET n.content_embedding=$content_embedding",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(outcome, RowEmbeddingOutcome::Skip);
        assert_eq!(stats.embeddings_recompute_failed, 1);
        assert_eq!(stats.embed_calls, 1);
        assert_eq!(stats.embeddings_recomputed, 0);
    }

    /// A recomputed vector whose length differs from the configured embedding dimension (e.g.
    /// the embedder's model changed) must not be bound — `lbug`'s embedding columns are
    /// fixed-width, so a wrong-length bind would fail at `execute_prepared` and roll back the
    /// whole same-template batch, not just this row (see `flush_batch`). Treated the same as any
    /// other recompute failure: for this CREATE-type row, zero-filled rather than skipped.
    #[test]
    fn recompute_row_embeddings_zero_fills_on_length_mismatch() {
        let embed_fn: RecomputeEmbedFn = Box::new(|_text: &str| Ok(vec![1.0, 2.0, 3.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Alice"));

        recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(
            params["name_embedding"],
            serde_json::json!([0.0, 0.0]),
            "a wrong-length recomputed vector must never be bound — zero-fill instead"
        );
        assert_eq!(stats.embeddings_recompute_failed, 1);
        assert_eq!(stats.embed_calls, 1);
        assert_eq!(stats.embeddings_recomputed, 0);
    }

    // --- ReplayStats::embeddings_recompute_had_no_failures (issue #440, FR-006/FR-008 gate) ---

    #[test]
    fn embeddings_recompute_had_no_failures_true_when_all_rows_recomputed_cleanly() {
        let embed_fn: RecomputeEmbedFn = Box::new(|text: &str| Ok(vec![text.len() as f32]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Alice"));

        recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            1,
            &mut stats,
        );

        assert!(stats.embeddings_recompute_had_no_failures());
    }

    #[test]
    fn embeddings_recompute_had_no_failures_true_despite_a_skip() {
        let embed_fn: RecomputeEmbedFn = Box::new(|_text: &str| Ok(vec![9.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        // No co-located "content" text present → FR-002/FR-005 skip, not a failure. This is
        // normal, ongoing WAL shape (a SET-only mutation), not evidence the embedder failed —
        // must not suppress a call site's identity persistence on its own.
        recompute_row_embeddings(
            "MATCH (n:Episodic {uuid:$uuid}) SET n.content_embedding=$content_embedding",
            &mut params,
            embed_fn.as_ref(),
            1,
            &mut stats,
        );

        assert!(stats.embeddings_recompute_had_no_failures());
    }

    #[test]
    fn embeddings_recompute_had_no_failures_false_after_an_embed_failure() {
        let embed_fn: RecomputeEmbedFn =
            Box::new(|_text: &str| Err(Error::Ipc("embedder unreachable".to_string())));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("content".to_string(), serde_json::json!("some content"));

        recompute_row_embeddings(
            "MATCH (n:Episodic {uuid:$uuid}) SET n.content_embedding=$content_embedding",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert!(!stats.embeddings_recompute_had_no_failures());
    }

    #[test]
    fn recompute_row_embeddings_ignores_rows_without_a_recognized_vector_placeholder() {
        let embed_fn: RecomputeEmbedFn =
            Box::new(|_text: &str| panic!("embed_fn must not be called for this row"));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("uuid".to_string(), serde_json::json!("abc"));

        let outcome = recompute_row_embeddings(
            "MERGE (n:MENTIONS {ep: $ep})",
            &mut params,
            embed_fn.as_ref(),
            4,
            &mut stats,
        );

        assert_eq!(outcome, RowEmbeddingOutcome::Proceed);
        assert_eq!(stats.embed_calls, 0);
        assert_eq!(stats.embeddings_recompute_skipped_no_text, 0);
        assert_eq!(stats.embeddings_recomputed, 0);
    }

    /// A legacy WAL row whose Cypher template still references `$name_embedding` but whose
    /// params object no longer carries it (issue #526: the writer stopped emitting the param) is
    /// still recognized and recomputed — relevance is decided by the template, not by whether the
    /// stored param happens to be present.
    #[test]
    fn recompute_row_embeddings_recognizes_a_stripped_wal_row_via_the_template() {
        let embed_fn: RecomputeEmbedFn = Box::new(|text: &str| Ok(vec![text.len() as f32, 0.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Al"));
        // No "name_embedding" key at all — a freshly-written, post-#526 WAL row.

        let outcome = recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(outcome, RowEmbeddingOutcome::Proceed);
        assert_eq!(params["name_embedding"], serde_json::json!([2.0, 0.0]));
        assert_eq!(stats.embeddings_recomputed, 1);
    }

    /// SC-005: replaying repeated identical (text, model) pairs must invoke the embedder fewer
    /// times than there are matching rows. `replay.rs` itself has no cache — the caller-supplied
    /// closure is responsible for one (FR-003 of issue #440) — so this wraps a real
    /// `EmbeddingCache` inside the closure and counts the closure's own compute-path invocations
    /// separately from `stats.embed_calls` (which counts every row `replay.rs` attempted, cache
    /// hit or miss alike).
    #[test]
    fn cache_bounds_embedder_invocations_for_repeated_text() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let cache = Arc::new(crate::embedding_cache::EmbeddingCache::new());
        let real_calls = Arc::new(AtomicUsize::new(0));
        let cache_ref = Arc::clone(&cache);
        let real_calls_ref = Arc::clone(&real_calls);
        let embed_fn: RecomputeEmbedFn = Box::new(move |text: &str| {
            cache_ref.get_or_compute("test-model", 2, text, || {
                real_calls_ref.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1.0, 2.0])
            })
        });

        let mut stats = zero_stats();
        for _ in 0..3 {
            let mut params = serde_json::Map::new();
            params.insert("name".to_string(), serde_json::json!("Alice"));
            recompute_row_embeddings(
                "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
                &mut params,
                embed_fn.as_ref(),
                2,
                &mut stats,
            );
        }

        assert_eq!(
            stats.embed_calls, 3,
            "replay.rs attempts recompute once per matching row"
        );
        assert_eq!(stats.embeddings_recomputed, 3);
        assert_eq!(
            real_calls.load(Ordering::SeqCst),
            1,
            "the cache inside the closure must bound real embedder invocations to the number \
             of distinct texts, not the number of matching rows (SC-005)"
        );
    }

    /// FR-010's tolerance target lives with the recompute path it validates: a computed vector
    /// containing NaN/infinity can't round-trip through JSON, so it must be treated the same as
    /// any other recompute failure (zero-fill for this CREATE-type row), not panic or corrupt
    /// params.
    #[test]
    fn recompute_row_embeddings_zero_fills_on_non_finite_vector() {
        let embed_fn: RecomputeEmbedFn = Box::new(|_text: &str| Ok(vec![f32::NAN, 1.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Alice"));

        recompute_row_embeddings(
            "CREATE (:Entity {uuid: $uuid, name: $name, name_embedding: $name_embedding})",
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(params["name_embedding"], serde_json::json!([0.0, 0.0]));
        assert_eq!(stats.embeddings_recompute_failed, 1);
        assert_eq!(stats.embeddings_recomputed, 0);
    }

    /// The two-vector-params-in-one-row case (`insert_entity`'s CREATE template: both
    /// `name_embedding` and `summary_embedding`) — each pair is recomputed independently from its
    /// own co-located text.
    #[test]
    fn recompute_row_embeddings_handles_multiple_vector_params_in_one_row() {
        let embed_fn: RecomputeEmbedFn = Box::new(|text: &str| Ok(vec![text.len() as f32, 0.0]));
        let mut stats = zero_stats();
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), serde_json::json!("Al"));
        params.insert("summary".to_string(), serde_json::json!("Alice summary"));

        let outcome = recompute_row_embeddings(
            ENTITY_CREATE_CYPHER,
            &mut params,
            embed_fn.as_ref(),
            2,
            &mut stats,
        );

        assert_eq!(outcome, RowEmbeddingOutcome::Proceed);
        assert_eq!(params["name_embedding"], serde_json::json!([2.0, 0.0]));
        assert_eq!(params["summary_embedding"], serde_json::json!([13.0, 0.0]));
        assert_eq!(stats.embeddings_recomputed, 2);
        assert_eq!(stats.embed_calls, 2);
    }

    // ── is_vector_only_set (issue #526, FR-005) ─────────────────────────────────────────────

    #[test]
    fn is_vector_only_set_true_for_a_bare_set_only_mutation() {
        assert!(is_vector_only_set(
            "MATCH (n:Episodic {uuid:$uuid}) SET n.content_embedding=$content_embedding",
            "content_embedding",
        ));
        assert!(is_vector_only_set(
            "MATCH (n:RelatesToNode_ {uuid:$uuid}) SET n.fact_embedding=$fact_embedding",
            "fact_embedding",
        ));
    }

    #[test]
    fn is_vector_only_set_false_for_a_create_form_record() {
        // No `SET` keyword at all — properties are inline in the `CREATE` literal.
        assert!(!is_vector_only_set(ENTITY_CREATE_CYPHER, "name_embedding"));
        assert!(!is_vector_only_set(
            ENTITY_CREATE_CYPHER,
            "summary_embedding"
        ));
    }

    #[test]
    fn is_vector_only_set_false_for_a_multi_assignment_set_clause() {
        // dump.rs's MERGE-form re-materialization shape — a SET clause with several assignments,
        // one of which is the vector — must never be treated as vector-only.
        assert!(!is_vector_only_set(
            "MERGE (n:Entity {uuid: $uuid}) SET n.name = $name, n.name_embedding = \
             $name_embedding, n.summary = $summary",
            "name_embedding",
        ));
    }

    #[test]
    fn is_vector_only_set_false_for_the_fixed_backfill_shape() {
        // Issue #526's own fix to `backfill_summary_embeddings.rs`: now that it also re-sets
        // `summary`, its SET clause has two assignments and is no longer vector-only.
        assert!(!is_vector_only_set(
            "MATCH (n:Entity {uuid: $uuid}) SET n.summary_embedding = $summary_embedding, \
             n.summary = $summary",
            "summary_embedding",
        ));
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
                    ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
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
                    ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
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
                    ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
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
                    ..ReplayOptions::new(zero_vector_embed_fn(4), 4)
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
