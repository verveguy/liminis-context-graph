# ADR-0047: WAL Replay Transaction Boundaries — Batch-Aligned, Not Chunk-Aligned

**Status**: Accepted
**Date**: 2026-07-26
**Issue**: #240

## Context

WAL replay (`crates/core/src/replay.rs`) had **no transaction boundaries at all**. Every
replayed statement was its own lbug auto-commit transaction:
`TransactionHelper::runFuncInTransaction` (lbug 0.17.0's C++ engine, `client_context.cpp:553-617`)
begins and commits one transaction per statement whenever no transaction is already active. The
prepared-statement batching introduced by #238/ADR-0045 (`flush_batch`, grouping up to
`batch_size` — default 64, max 256 — consecutive same-template rows to amortize `prepare()`) never
changed this: a 64-row batch was still 64 independent commits to the underlying `db.wal`, not one
atomic unit.

Consequences: a failure or cancellation partway through a batch left rows before the failure
point durably committed and rows after it simply not attempted, with no marker anywhere of
exactly where the split occurred. The WAL **writer** side already establishes chunk atomicity
(`wal_exec.rs`'s `wal_flush_chunk` / `WalWriter::with_chunk`, `wal.rs`) — a group of mutations
either flushes to `db.wal` as a whole or is discarded on error — but replay honoured none of it,
and the persisted `WalLine` schema (`seq, ts, db, cypher, params`) has no field recording chunk
membership, so chunk boundaries aren't even recoverable from a WAL file alone. Per-statement
auto-commit was also the slowest possible mode for a bulk rebuild.

lbug does support explicit transactions as plain Cypher (`BEGIN TRANSACTION` / `COMMIT` /
`ROLLBACK`, issuable via `Connection::query`) — the engine's own test suite exercises this
directly (`lbug-0.17.0/src/database.rs:335-338`) — but this codebase's replay path never used it.

### The engine-forced constraint that shapes this design

Verified against the vendored lbug 0.17.0 C++ source (not just the Rust binding, which documents
none of this): once a transaction is open, **any** row's execute-time exception causes the engine
itself to roll back the **entire** transaction — every statement already applied earlier in it,
not just the failing one (`client_context.cpp:598-604`). lbug has no savepoints anywhere in its
source tree and does not allow nested transactions (`BEGIN` while a transaction is active throws;
`COMMIT`/`ROLLBACK` with no active transaction throws `"No active transaction for {action}."`,
plain text with no distinguishing prefix). After an engine auto-rollback, the connection has no
active transaction — issuing an explicit `ROLLBACK` at that point throws. This is not a choice
Rust-side code can opt out of or make more granular; it dictated most of the decisions below. See
`crates/core/src/db.rs`'s `lbug_transaction_semantics_pinning_tests` module for regression tests
pinning this exact behavior in case a future lbug version changes it.

## Decision

### Transaction boundary = one `flush_batch` call, not one WAL chunk (FR-004 deviation)

Each call to `flush_batch` — i.e., each same-template run up to `batch_size` rows — now runs
inside one explicit transaction: `BEGIN TRANSACTION`, the existing prepare-once/execute-many row
loop, then `COMMIT`. This is **batch-aligned, not chunk-aligned**: a single write-side chunk
(`wal_flush_chunk`, one per episode) spans many distinct Cypher templates — `insert_entity`,
`insert_relates_to_edge` (3 templates per edge), `insert_episodic`, `insert_mentions_edge` (N
templates) — so one chunk becomes many transactions under this design, not one.

This is the FR-004 documented deviation: exact chunk-alignment was rejected because it would
require either buffering multiple distinct templates inside one transaction (losing #238's
prepared-statement-reuse win, since each template still needs its own `prepare()` inside that
transaction) or extending the persisted `WalLine` schema to record chunk membership (a schema
change the spec permits but doesn't mandate, and which would need backward-compatibility handling
for pre-existing WAL files with no chunk marker). Batch-aligned transactions need no schema
change, reuse the existing `batch_size`-driven flush points unmodified, and — since `batch_size`
is already bounded (1–256) — get User Story 4's memory-boundedness requirement for free, with no
new tunable.

### Accounting is deferred to commit time, not applied inline

Before this change, `flush_batch` mutated `ReplayStats` fields row-by-row as it executed. Under
whole-transaction rollback, an early row's "success" can be invalidated by a later row's failure
in the same transaction, so per-row outcomes (`lines_replayed`, `match_prefixed_replayed`,
`match_prefixed_no_op`, `match_delete_no_op` deltas) are now accumulated in local counters inside
`flush_batch` and merged into `stats` only after a successful `COMMIT`.

On a genuine execute failure, the triggering row is classified via the existing
`classify_replay_failure` (unchanged — same dedup/sample-cap logic from ADR-0046), and every
*other* row in that same batch — whether it already executed successfully earlier in the
now-discarded transaction, or was never attempted — is counted into a new
`ReplayStats::rolled_back_lines` counter (`batch_len - 1`, since exactly one row triggered the
classified failure). This is the fix for Research's top-flagged risk: a rolled-back batch's
earlier-successful rows must never be double-counted as both `lines_replayed` and rolled back.
No explicit `ROLLBACK` is issued in this path — the engine has already rolled back and cleared its
transaction state by the time the exception is caught; issuing one would itself error.

`rolled_back_lines` is a pure observability counter and is **deliberately excluded** from the
`fidelity_warning` ratio (`compute_fidelity_warning`, ADR-0043/ADR-0046) in this change.
`compute_fidelity_warning` is extensively pinned by existing tests, and folding a new counter into
it risks unintended regressions for comparatively low value in this pass — noted here as a
candidate for a future issue, not decided against permanently.

### The probe-execute-failure retry-in-place mechanism is retired

Before this change, when a `RETURN count(*)`-probed statement (the no-op-detection mechanism from
ADR-0043) prepared fine but failed to *execute*, `flush_batch` re-prepared the unmodified template
and retried the *same row inline*, continuing the rest of the batch under the fallback statement.
That pattern cannot coexist with whole-transaction rollback: the moment the probed execute throws,
the engine has already discarded the transaction, so retrying and continuing would either error
(`COMMIT` against no active transaction) or silently degrade to per-statement auto-commit for the
rest of the batch — reintroducing exactly the undefined partial-commit behavior this issue exists
to eliminate.

A probe-execute failure is now classified identically to any other execute failure: stop the row
loop, classify the one triggering row, roll the rest of the batch into `rolled_back_lines`. This
is an engine-forced behavior change, not a design preference — a `MATCH`-prefixed WAL line whose
probe fails only because of the appended `RETURN` (not the underlying write) is now rolled back
along with its batch siblings rather than recovered inline. This is safe because replay is
idempotent (MERGE-based writes, and CREATE-based writes are re-derivable via
`knowledge_rebuild_from_wal`'s `from_seq` resume) — a later replay or recovery pass re-applies the
rolled-back rows. The separate *prepare-time* probe-rejection fallback (tried before `BEGIN` is
even issued, when the probed template itself doesn't parse) is unaffected and unchanged — it's a
property of the template, not a per-row execute outcome, and doesn't interact with an open
transaction at all.

### Cancellation is checked per-row, inside the transaction

`cancel_fn` (real, production-wired via `handle_rebuild_from_wal`'s composite client-disconnect /
shutdown closure) is now checked once per row inside `flush_batch`'s loop, in addition to the
pre-existing once-per-WAL-line check in `replay_opts`'s outer loop. When it fires mid-transaction,
`flush_batch` issues an explicit `ROLLBACK` (cancellation isn't an exception, so there's no engine
auto-rollback to rely on), counts the **entire** batch — including any rows that already executed
successfully earlier in the same transaction — into `rolled_back_lines`, and returns a `cancelled`
outcome that `replay_opts` uses to stop reading further WAL lines. This keeps the maximum
uncommitted-then-discarded window bounded by `batch_size`, not by however long cancellation takes
to propagate through the whole file loop.

### New `ReplayStats` fields, additive only

- `rolled_back_lines: u64` — see above.
- `transactions_committed: u64` / `transactions_rolled_back: u64` — per-transaction counters,
  useful for diagnosing how batch-aligned this WAL's actual transaction cadence turned out to be
  (a heavily-interleaved-template WAL yields many small transactions; a homogeneous WAL yields
  large `batch_size`-capped ones — see the SC-005 measurement below).
- `last_committed_seq: Option<u64>` — the max WAL `seq` among rows in the most recent transaction
  that actually committed, updated only on `COMMIT`. This is the FR-006 resume-point value: it
  corresponds *exactly* to the last committed transaction boundary, not an approximation. `None`
  until at least one transaction commits.

All four are surfaced in `knowledge_rebuild_from_wal`'s JSON result (streaming, non-streaming
dry-run, and background-job paths) and in the shutdown-cancelled progress event, so a caller can
observe them without holding a `ReplayStats` value directly.

**`last_committed_seq` is surfaced but intentionally not wired into ADR-0026's episode-cursor
resume heuristic.** That heuristic (`recovery.rs`'s `derive_episode_cursor`, used by the
startup/recovery path, not by `handle_rebuild_from_wal`) re-derives `from_seq` from the last
`Episodic` node already in the database and is tolerant of re-applying an overlapping `seq` range,
by design, since replay is idempotent. `last_committed_seq` is a strictly more precise value than
that heuristic ever needed to be — but rewiring `recovery.rs` to consume it is out of scope for
this issue (per the spec's own Assumptions/Out-of-Scope) and is left as a candidate simplification
for a future issue. Today, the two resume mechanisms simply coexist: `recovery.rs`'s startup path
keeps using its existing episode-cursor heuristic unchanged, while any caller of `replay_opts`
directly (or via `knowledge_rebuild_from_wal`) now additionally has access to the exact
`last_committed_seq` value if it wants a tighter resume point.

### Ordering with ADR-0017 (shutdown sequencing)

A `cancel_fn`-triggered mid-transaction `ROLLBACK` completes synchronously inside the
`spawn_blocking` closure that runs `replay_opts`, before that closure returns control to the async
caller. `handle_rebuild_from_wal` already awaits this `spawn_blocking` task to completion before
proceeding, and ADR-0017's shutdown sequencing already drains all `spawn_blocking` work before the
lbug checkpoint fires on exit. No new ordering risk is introduced: the rollback is guaranteed to
have completed (successfully or not — its own `Result` propagates via `?`) before shutdown's
checkpoint step runs.

### New `Conn::exec_transaction_control` helper, not `Conn::raw_query`

`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` are issued via a new
`Conn::exec_transaction_control(&self, sql: &str) -> Result<(), Error>` (`db.rs`), not the
existing `Conn::raw_query`. `raw_query` appends every call to `Conn::executed_mutations`, a buffer
meant for live-write WAL recording that nothing drains for a replay connection — using it for
transaction-control statements would silently accumulate one entry per transaction for the life of
a multi-hour replay, an unbounded-memory regression introduced by this very fix and directly
undermining User Story 4. `exec_transaction_control` calls `Connection::query` directly, skipping
that recording. (Issuing `BEGIN`/`COMMIT`/`ROLLBACK` this way was also verified in Research to not
feed lbug's own `CachedPreparedStatementManager` leak that ADR-0045 bounded — `queryNoLock` never
registers into that map, unlike the `prepare()` entry point `PreparedCache` already governs.)

## Consequences

- **Correctness (User Stories 1 & 2, SC-001/SC-002)**: a replay failure or cancellation now leaves
  the database at a well-defined transaction boundary — every transaction up to and including the
  last committed one is fully present, and the transaction containing the failure/cancellation is
  fully absent, never a partial subset. `crates/core/tests/wal_replay.rs`'s
  `sc001_prior_committed_transactions_survive_a_later_rollback` and
  `sc002_cancel_mid_transaction_rolls_back_and_resume_matches_uninterrupted_replay` pin this
  end-to-end, including that resuming from `last_committed_seq + 1` reproduces the same end state
  as an uninterrupted replay.
- **Accounting semantics changed, not just durability granularity**: a mid-batch failure that
  previously left some rows in that batch counted as `lines_replayed` (because each row committed
  independently) now counts them as `rolled_back_lines` instead, since the whole transaction
  discards them. Several pre-existing tests in `wal_replay.rs` that asserted the old per-row
  isolation behavior were rewritten to assert the new atomic-batch semantics instead
  (`batch_fallback_rolls_back_whole_batch_on_bad_row`, and three failure-sample-dedup tests
  updated to use `batch_size: 1` — see their inline comments — so they continue to isolate the
  dedup mechanism they were actually written to test, independent of this issue's atomicity
  change).
- **Throughput improved (User Story 3, SC-005)**: measured replaying
  `crates/core/tests/fixtures/real_corpus_wal/` (71 MB, ~12.5k lines, 12,482 real mutations)
  directly through `WalReplayer::replay` (no IPC layer, no index build) on the same machine,
  before (`main` @ `ac35797`, per-statement auto-commit) vs. after (this change):

  | | wall-clock | mutations/s |
  |---|---|---|
  | before | 73.3s | 170.3 |
  | after | 51.4s | 242.7 |

  ~30% faster, ~43% higher throughput. See `crates/core/tests/real_corpus_replay_perf.rs`
  (`#[ignore]`d — run explicitly per its own doc comment).
- **Memory did not regress (User Story 4, SC-004)**: peak RSS (`maximum resident set size` via
  `/usr/bin/time -l`) for the same before/after run was ~1.31 GB before and ~1.25 GB after — flat,
  not growing, consistent with the structural argument that transaction size is capped by
  `batch_size` independent of total WAL size. This fixture (71 MB) is not large enough on its own
  to demonstrate a monotonic-growth trend distinctly from noise across WAL sizes; the argument for
  boundedness here is structural (transaction size is bounded by construction, not measured to
  scale with WAL size), not purely empirical.
- **`FailureSample`/`fidelity_warning`/`seq`-regression/no-op-detection mechanisms (FR-009)** are
  unmodified in their own logic and continue to function — `classify_replay_failure`,
  `compute_fidelity_warning`, the `seq`-monotonicity check, and `with_match_count_probe`/
  `is_delete_form` are called from the same places with the same inputs; only the *caller's*
  transaction-boundary bookkeeping around them changed.
- **`dry_run: true` is completely unaffected (FR-007)** — it never calls `flush_batch` at all, so
  zero code on that path changed.
- A WAL directory with zero mutations, or a `from_seq` filter that skips every line, never causes
  `batch` to become non-empty, so `flush_batch`'s early-return-on-empty-batch guard (unchanged)
  means no empty transaction is ever opened or committed.

## Alternatives Considered

- **Exact chunk-alignment** (buffer all of one `wal_flush_chunk`'s mutations — potentially many
  distinct templates — inside one transaction): rejected. Requires either preparing a fresh
  statement per template inside the transaction anyway (no prepared-statement-reuse loss relative
  to batch-aligned, but no atomicity gain either, since each template's rows still need their own
  `prepare()`+execute loop) or restructuring `flush_batch` to accept heterogeneous templates in one
  call — a substantially larger change for a boundary that still wouldn't be exactly chunk-sized
  without a `WalLine` schema change to record chunk membership in the first place.
- **Persist chunk-id in `WalLine`** to make exact chunk-alignment recoverable from the WAL file:
  rejected as unnecessary for this issue — the spec explicitly permits but does not mandate a
  schema change, and batch-aligned transactions satisfy FR-004's alternative branch (document the
  deviation) without touching the on-disk format or needing backward-compatibility handling for
  older WAL files.
- **Shrink `batch_size` to reduce rollback blast radius** instead of accepting whole-batch
  rollback at the existing default (64): rejected — this trades correctness risk for throughput
  loss without addressing the root tension (per-row accounting under transactional semantics still
  needs the deferred/local-buffering fix regardless of batch size), and the existing `batch_size`
  knob is already user-tunable (`LCG_REPLAY_BATCH_SIZE`) for anyone who wants a smaller blast
  radius.
- **Re-execute a rolled-back batch's rows one-by-one in auto-commit mode** to preserve today's
  "do-your-best" throughput after a failure (only the genuinely bad row would then fail): rejected
  — this reintroduces exactly the undefined, partial-commit granularity this issue exists to
  eliminate, just scoped to "batches that failed once" instead of "every batch." A future replay
  pass (idempotent) is the correct way to recover the good rows, not an in-place per-row fallback.
- **Wire `last_committed_seq` into ADR-0026's `derive_episode_cursor`** now that a precise
  resume point exists: deferred, not rejected — legitimate future simplification, but out of scope
  per the spec's Assumptions (this issue is scoped to the replay path itself, not `recovery.rs`'s
  resume heuristic).
- **Fold `rolled_back_lines` into the `fidelity_warning` ratio**: deferred — `compute_fidelity_warning`
  is heavily pinned by existing tests and the ratio's current definition (ADR-0043/ADR-0046) is
  itself the product of careful, tested tuning; changing it in the same change as this issue's
  atomicity fix risked conflating two independent behavioral changes in one review pass.

## References

- Issue #240 — this fix; the fourth in the WAL-replay-audit series (#237, #238, #239)
- ADR-0043 — WAL Replay: Seq-Based File Ordering and MATCH-Write No-Op Accounting (the
  `with_match_count_probe`/`is_delete_form`/no-op counters this issue's transaction boundary must
  not disturb)
- ADR-0045 — WAL Replay Prepared-Statement Cache — LRU-1 Scope and Deferred Connection Recycling
  (`flush_batch`'s `PreparedCache`, the mechanism this issue's transaction boundary coordinates
  with; verified non-conflicting with `BEGIN`/`COMMIT`/`ROLLBACK` issuance)
- ADR-0046 — WAL Replay — Deduplicated Failure Samples and Fail-Fast Rebuild Idempotency
  (`classify_replay_failure`'s dedup logic and `compute_fidelity_warning`, both unmodified by this
  issue and exercised identically from the new transaction-aware `flush_batch`)
- ADR-0026 — Episode-Cursor WAL Resume for Checkpoint Recovery (the existing resume heuristic this
  issue's `last_committed_seq` complements without replacing)
- ADR-0017 — Replace `process::exit` with Normal Return in async main (the shutdown/`spawn_blocking`
  drain sequencing this issue's cancellation rollback must complete before)
- ADR-0024 — Bound-Parameter DB Access — Retire Cypher String Interpolation (why `exec_transaction_control`
  issues raw, non-parameterized Cypher for transaction control specifically, not a general
  precedent for reintroducing interpolation elsewhere)
- `crates/core/src/db.rs`'s `lbug_transaction_semantics_pinning_tests` — regression tests pinning
  the undocumented engine behavior (whole-transaction rollback on exception; cross-transaction
  prepared-statement reuse) this design depends on
- `crates/core/tests/real_corpus_replay_perf.rs` — the SC-004/SC-005 measurement harness
