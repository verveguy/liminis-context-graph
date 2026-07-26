# Feature Specification: WAL replay transaction boundaries — defined recovery state on failure and cancellation

**Feature Branch**: `fabrik/issue-240`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "WAL replay has no transaction boundaries at all — partial writes on failure, chunk atomicity not honoured"

## Background

WAL replay has **no transaction boundaries at all**. There are zero `BEGIN` / `COMMIT` /
`ROLLBACK` statements anywhere in `crates/` — the only occurrences are two comments
(`db.rs:52,105`) that merely note lbug treats certain DDL as write transactions internally.

Every replayed statement is therefore its own auto-commit transaction: lbug's
`TransactionHelper::runFuncInTransaction` starts and commits one transaction per statement
whenever no transaction is already active (`client_context.cpp:553-617`). A replay "batch" of
64 rows sharing a template (`replay.rs`'s `flush_batch`, introduced by #238's prepared-statement
caching) is still 64 independent transactions and 64 commits to the underlying `db.wal` — the
batching added for query-plan reuse never made those 64 executions atomic as a unit.

Consequences of the current design:

- If a batch fails part-way through, is cancelled (the `cancel_fn` check in `replay.rs`'s main
  loop), or the process dies, **rows before the failure point are durably committed and rows
  after it are not — with no marker anywhere of exactly where the split occurred**. Recovery
  re-derives an approximate resume point from the last `Episodic` node (ADR-0026), which
  mitigates the blast radius but does not make the post-failure state well-defined at the
  statement level.
- The WAL **writer** deliberately establishes chunk atomicity: `wal_exec.rs`'s `wal_flush_chunk`
  wraps a group of mutations in `WalWriter::with_chunk`, which either flushes the whole group to
  the on-disk WAL file or discards all of it on error (`wal.rs`'s `with_chunk`). **Replay does not
  honour this grouping in any way** — the persisted WAL line schema (`WalLine`: `seq, ts, db,
  cypher, params`) has no field recording which lines were written as part of the same chunk, so
  even if replay wanted to preserve chunk atomicity today, it has no way to identify chunk
  boundaries from the WAL file alone.
- Per-statement auto-commit is also the slowest possible mode for a bulk rebuild: every one of
  potentially millions of rows pays a full commit round-trip.

lbug **does** support explicit transactions — not via a Rust API method, but as Cypher issued
through `Connection::query`, which lbug's own test suite exercises directly
(`conn.query("BEGIN TRANSACTION")` … `conn.query("COMMIT")`, `lbug-0.17.0/src/database.rs:335-338`).
The capability exists in the engine and is simply unused by this codebase's replay path.

This is one issue in a series auditing WAL replay correctness (see also #237, #238, #239). The
statement-cache issue (#238) that shares `flush_batch`'s execution region has already merged, so
this issue proceeds against the current `flush_batch` implementation (per-row `prepare()`-once,
execute-many via bound parameters).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A failure mid-replay leaves the database at a defined boundary (Priority: P1)

An operator (or the automated recovery path) runs a WAL replay — via `knowledge_rebuild_from_wal`
or the recovery flow in ADR-0026 — against a WAL that contains a failing statement partway
through. Today, everything before the failure is durably committed to `db.wal` one row at a
time, and everything from the failure point onward is simply not attempted for that batch,
leaving no record of exactly which row the database reflects. After this change, replay executes
inside explicit transactions, so a failure rolls the in-flight transaction back to the last
committed transaction boundary — the database is left in a state that corresponds exactly to
"every transaction up to and including transaction N committed, nothing from N+1 applied,"
rather than an arbitrary mid-batch split.

**Why this priority**: This is the core defect the issue exists to fix — a replay that dies or
errors partway through must leave a state that can be reasoned about and resumed from, not an
arbitrary statement-level cut point with no boundary marker.

**Independent Test**: Run a replay against a WAL where a specific row is engineered to fail
(e.g. a constraint violation or a deliberately malformed statement injected mid-stream); assert
that after the failed replay, the database contains either all effects of the transaction
containing the failing row, or none of them — never a partial subset of that transaction's
statements — and that rows from transactions before the failing one are fully present.

**Acceptance Scenarios**:

1. **Given** a WAL whose replay would apply N transactions successfully before an (N+1)th
   transaction fails, **When** replay runs to that failure, **Then** the database reflects
   exactly the effects of transactions 1..N — no partial effects of transaction N+1 are visible.
2. **Given** the same scenario, **When** the failure is inspected after replay halts or reports
   the error, **Then** the replay result identifies the transaction boundary reached (e.g. by
   `seq` or transaction index), so a resume point can be derived from it.
3. **Given** a replay that completes with zero failures, **When** compared against the current
   (pre-fix) per-statement-commit behavior, **Then** the final database state is identical —
   transaction boundaries change durability/atomicity granularity, not replay semantics or which
   rows get applied.

---

### User Story 2 - Cancellation mid-replay leaves a defined, resumable state (Priority: P1)

An operator disconnects a client mid-rebuild, or the service is asked to cancel a long-running
`knowledge_rebuild_from_wal` call. Today the replay loop's `cancel_fn` check breaks out of the
file loop immediately, after whatever has already been auto-committed row-by-row — an arbitrary
cut point with no defined relationship to any larger unit of work. After this change,
cancellation is detected at a transaction boundary (or triggers a rollback of the in-flight
transaction), so the resulting state is exactly as well-defined as the failure case in User
Story 1.

**Why this priority**: The existing cancellation path (`replay.rs`'s cancel check, already
covered by prior WAL-replay tests) is a normal, expected occurrence — a client disconnecting
during a multi-hour rebuild is routine, not exceptional — so its resulting state must be just as
well-defined as an outright failure.

**Independent Test**: Start a replay with a `cancel_fn` engineered to return `true` partway
through a WAL file (mid-transaction); assert the resulting database state corresponds to a
committed transaction boundary (not a partial transaction), and that resuming replay from the
derived boundary reproduces the same end state as an uninterrupted replay.

**Acceptance Scenarios**:

1. **Given** a replay cancelled partway through applying a transaction's statements, **When**
   cancellation is detected, **Then** that in-flight transaction is rolled back in full — none of
   its statements are left partially applied.
2. **Given** a cancelled replay, **When** replay is resumed using the resume point derivable from
   the last committed boundary, **Then** the resumed replay plus its prior committed state
   produces the same final database content as a single uninterrupted replay of the same WAL.

---

### User Story 3 - Bulk rebuild throughput improves, or at minimum does not regress (Priority: P2)

An operator rebuilds a large workspace from WAL (the multi-hour full-rebuild case described in
ADR-0026's Context). Today every row is its own commit, which is the slowest possible commit
granularity. After this change, replay commits at whatever transaction boundary is chosen (batch-
or chunk-aligned, per User Story 4), which should reduce total commit overhead.

**Why this priority**: Valuable and directly motivated by the issue (per-statement commits are
"the slowest possible mode for a bulk rebuild"), but secondary to correctness — a faster replay
that leaves undefined state on failure would not satisfy this issue.

**Independent Test**: Measure end-to-end replay wall-clock time on the real-corpus WAL fixture
(`crates/core/tests/fixtures/real_corpus_wal/`) before and after the change, using the same
input WAL and hardware.

**Acceptance Scenarios**:

1. **Given** the real-corpus WAL fixture, **When** replayed before and after this change,
   **Then** both throughput numbers are recorded and reported (an improvement is expected but not
   itself a pass/fail gate — a documented neutral or regressed result must be explained).

---

### User Story 4 - Large rebuilds do not accumulate unbounded uncommitted state (Priority: P1)

An operator rebuilds a very large workspace from WAL. Because lbug transactions hold uncommitted
state (and potentially locks) in memory until commit, wrapping *all* of replay in one giant
transaction would make memory grow without bound as the rebuild progresses, and would hold a
single-transaction lock for the entire multi-hour operation. After this change, transaction
boundaries are bounded — coordinated with the existing batch/chunk structure already in
`replay.rs` (see the `batch_size` option, default 64) and `wal_exec.rs`'s chunking — so memory
stays bounded regardless of total WAL size.

**Why this priority**: Explicitly called out as non-optional in the issue's Risks section — an
unbounded transaction is a correctness fix that trades one failure mode (undefined post-failure
state) for another (unbounded memory growth / lock hold time on large rebuilds).

**Independent Test**: Run replay against the real-corpus WAL fixture (and/or a larger synthetic
WAL) while sampling process memory; assert memory does not grow monotonically with total WAL
size in a way attributable to accumulated uncommitted transaction state.

**Acceptance Scenarios**:

1. **Given** a WAL substantially larger than any single transaction boundary, **When** replayed
   to completion, **Then** peak memory attributable to replay does not scale with total WAL size
   — only with the (bounded) transaction boundary size.

---

### Edge Cases

- A WAL file boundary occurs mid-transaction (i.e., a chosen transaction boundary would span two
  `.jsonl` files): replay must still commit at a well-defined point, whether or not that point
  aligns with a file boundary — file boundaries and transaction boundaries are not the same
  concept and must not be conflated.
- The `dry_run: true` mode (`ReplayOptions::dry_run`) counts mutations without executing them —
  transaction boundaries are only meaningful when `dry_run` is false; dry-run behavior must be
  unaffected by this change.
- A WAL line whose statement is `MATCH`-prefixed with a `RETURN count(*)` probe appended
  (`with_match_count_probe`, from the no-op-detection mechanism) must continue to work correctly
  inside an explicit transaction — the probe's fallback-to-unprobed-template behavior on
  prepare/execute failure must not itself corrupt or prematurely end a transaction it doesn't
  need to.
- The existing `seq`-monotonicity regression check (FR-004 from #239) and fidelity-warning
  computation must continue to reflect true outcomes when transaction boundaries change which
  rows commit together — a rolled-back transaction's rows must not be double-counted as both
  "replayed" and "rolled back."
- A WAL directory containing zero mutations, or a `from_seq` filter that skips every line, must
  not attempt to open or commit an empty transaction.
- Interaction with ADR-0026's episode-cursor resume: once replay has well-defined transaction
  boundaries, the resume point after a failure or cancellation must be derivable from the last
  *committed transaction* boundary — whether that fully replaces, complements, or leaves
  unchanged the existing last-episode-node resume heuristic is a Plan-stage design decision (see
  Assumptions), not fixed by this spec.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: WAL replay MUST execute mutations inside explicit transactions issued via Cypher
  (`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` or lbug's equivalent transaction-control
  statements), rather than relying on lbug's per-statement auto-commit behavior.
- **FR-002**: On a replay failure (a statement that fails to prepare or execute in a way
  classified as a genuine failure, not a benign no-op or legacy-skip), the in-flight transaction
  containing the failing statement MUST be rolled back in full — no partial subset of that
  transaction's statements may remain committed.
- **FR-003**: On cancellation via the existing `cancel_fn` mechanism, any in-flight transaction
  MUST be rolled back in full before replay stops, leaving the database at the last fully
  committed transaction boundary.
- **FR-004**: The chosen transaction boundary MUST honour the WAL writer's chunk atomicity
  (`wal_exec.rs`'s `with_chunk` groupings) — a unit of mutations written atomically by the writer
  must be replayed atomically as a unit — OR, if a different boundary is chosen instead (e.g. for
  reasons of WAL-format limitations, since the persisted `WalLine` schema does not currently
  record chunk membership), the deviation and its rationale MUST be documented in the ADR required
  by FR-008.
- **FR-005**: Transaction size MUST be bounded — coordinated with the existing batch/chunk
  structure in `replay.rs`/`wal_exec.rs` — so that a large rebuild does not accumulate unbounded
  uncommitted transaction state or hold a single transaction open for the duration of the entire
  replay.
- **FR-006**: The resume point derivable after a failure or cancellation MUST correspond exactly
  to the last committed transaction boundary — not an approximation that could be ahead of or
  behind what was actually committed.
- **FR-007**: `dry_run: true` replay behavior (counting without executing) MUST be unaffected by
  the introduction of transaction boundaries.
- **FR-008**: The chosen transaction-boundary design (boundary granularity, chunk-atomicity
  decision from FR-004, and interaction with ADR-0026's episode-cursor resume and ADR-0017's
  shutdown/checkpoint sequencing) MUST be documented in a new ADR alongside ADR-0026 and ADR-0017.
- **FR-009**: All existing replay behaviors this issue does not target — batching for prepared-
  statement reuse (#238), failure-sample deduplication (#239), no-op detection via the `RETURN
  count(*)` probe, fidelity-warning computation, `seq`-monotonicity checking — MUST continue to
  function correctly with transaction boundaries introduced; none of these are regressed by this
  change.
- **FR-010**: Rebuild throughput (mutations replayed per second) MUST be measured on the same
  fixture before and after this change and the comparison recorded, since per-statement commits
  are expected to be the slower mode.

### Key Entities *(if the feature involves data)*

- **Transaction boundary**: The unit of mutations committed or rolled back together during
  replay. Currently absent (every statement is its own auto-commit unit); after this change, a
  well-defined, bounded grouping — its exact granularity (batch-aligned, chunk-aligned, or a new
  concept) is a Plan-stage decision per FR-004.
- **WAL chunk**: The writer-side atomicity unit established by `wal_exec.rs`'s `with_chunk` —
  currently not recorded in the persisted `WalLine` schema, so not directly recoverable from a
  WAL file without either inferring it or extending the schema.
- **Resume boundary**: The point from which replay can safely resume after an interruption,
  derived from the last committed transaction. Complements (or, per Plan-stage decision, may
  simplify) the existing episode-cursor resume model in ADR-0026.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An automated test induces a failure partway through a replay and verifies the
  database reflects a full transaction boundary (all-or-nothing per transaction), not a partial
  cut within a transaction.
- **SC-002**: An automated test triggers cancellation mid-replay (via `cancel_fn`) and verifies
  the resulting database state is at a defined transaction boundary, and that resuming replay
  from the derived point reproduces the same end state as an uninterrupted replay.
- **SC-003**: The WAL writer's chunk atomicity (`wal_exec.rs`) is either verified as preserved
  through replay by an automated test, or its documented deviation (FR-004) is captured in the
  ADR with rationale — one of these two outcomes is achieved, not left ambiguous.
- **SC-004**: Memory usage during replay of the real-corpus WAL fixture
  (`crates/core/tests/fixtures/real_corpus_wal/`) does not grow unboundedly with total WAL size
  — measured and recorded.
- **SC-005**: Rebuild throughput on the real-corpus WAL fixture is measured before and after this
  change, and the comparison is recorded (in the PR description, ADR, or a benchmark artifact).
- **SC-006**: A new ADR documenting the chosen transaction-boundary design, its chunk-atomicity
  decision, and its interaction with ADR-0026 and ADR-0017 is added to `docs/adr/`.
- **SC-007**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and
  `cargo test --release` are all green after the change.

## Assumptions

- Whether the transaction boundary is exactly batch-aligned (matching `replay.rs`'s existing
  `flush_batch` grouping), exactly chunk-aligned (matching `wal_exec.rs`'s writer-side chunks), or
  some new grouping, is a Plan-stage implementation decision — FR-004 requires either honouring
  chunk atomicity or documenting why a different boundary was chosen; either satisfies this spec
  provided it is documented (FR-008) and tested (SC-003).
- Whether persisting chunk membership requires extending the on-disk `WalLine` schema (currently
  `seq, ts, db, cypher, params`, matching the Python driver's WAL format) is a Research/Plan-stage
  question. This spec does not mandate a schema change — if the chosen design achieves FR-004's
  chunk-atomicity goal without one (e.g. by re-deriving chunk boundaries from existing fields, or
  redefining the atomicity unit), that satisfies this issue; if a schema change is chosen instead,
  it must preserve backward compatibility with existing WAL files (an older WAL with no chunk
  marker must still replay correctly) and must be called out in the ADR.
- Whether the existing ADR-0026 episode-cursor resume heuristic is replaced, complemented, or
  left unchanged now that a precise transaction-boundary resume point exists is a Plan-stage
  decision, not fixed by this spec (see the issue's own Risks section: "the resume model may
  simplify once boundaries are well-defined" is described as a possible follow-on effect, not a
  requirement of this issue).
- This issue proceeds against the current (post-#238) `flush_batch` implementation; the
  statement-cache issue that previously blocked this one has merged.
- The normal (non-replay) ingest write path is unaffected by this change — this issue is scoped
  entirely to the replay path (`WalReplayer::replay_opts` and its call sites), per the issue's own
  Scope section.

## Out of Scope

- Transactions for the normal ingest write path (`wal_exec.rs`'s `wal_flush_chunk` /
  `wal_flush_ungrouped` as consumed by live write handlers) — a separate concern from replay,
  explicitly excluded by the original issue.
- The other issues in this replay-audit series (#237, #238, #239) — already addressed or tracked
  separately.
- Changing the failure-sample deduplication, fidelity-warning computation, or no-op-detection
  mechanisms introduced by #239 — this issue must not regress them (FR-009) but does not revisit
  their design.
- Deciding definitively whether ADR-0026's episode-cursor resume is replaced or kept — left to the
  Plan stage per Assumptions.

## Source References *(optional)*

- `crates/core/src/replay.rs` — `WalReplayer::replay_opts`, `flush_batch`, `ReplayOptions`
  (`batch_size`, `cancel_fn`), `ReplayStats`.
- `crates/core/src/wal_exec.rs` — `wal_flush_chunk`, `wal_flush_ungrouped`, the writer-side
  chunk-atomicity helpers this issue must honour or document a deviation from.
- `crates/core/src/wal.rs` — `WalWriter::with_chunk`, `WalLine` (the persisted five-field WAL
  schema with no chunk-membership field).
- `crates/core/src/db.rs` — `Conn::raw_query`, `Conn::prepare`, `Conn::execute_prepared`,
  `Conn::execute_prepared_returning_count`; lines 52 and 105 carry the only existing
  transaction-related comments in `crates/`.
- `docs/adr/0026-episode-cursor-wal-resume.md` — the current resume heuristic this issue
  interacts with.
- `docs/adr/0017-replace-process-exit-with-normal-return.md` — shutdown/checkpoint sequencing
  this issue's ADR must stay consistent with.
- `crates/core/tests/fixtures/real_corpus_wal/` — the real-corpus fixture used for the
  memory/throughput measurements in SC-004/SC-005.
- `crates/core/tests/wal_replay.rs` — existing replay test suite this issue's new tests extend.
- Prior issues in this series: #237 (ordering/statistics), #238 (statement-cache eviction,
  merged), #239 (failure-sample dedup and rebuild semantics, merged).
