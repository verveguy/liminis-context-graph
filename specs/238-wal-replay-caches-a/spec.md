# Feature Specification: Bound Prepared-Statement Growth During WAL Replay

**Feature Branch**: `fabrik/issue-238`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "WAL replay caches a query plan per batch with no eviction — unbounded memory growth on large rebuilds"

## Background

*(Line numbers below reflect the codebase state at the time this issue was filed, describing the pre-fix defect; they are not maintained as the implementation evolves. See `docs/adr/0045-wal-replay-prepared-statement-cache-scope.md` for the as-implemented design.)*

WAL replay accumulates cached query plans without bound, so a large rebuild grows RSS monotonically and can exhaust memory mid-replay.

`Conn::prepare` (`crates/core/src/db.rs:153-155`) reaches lbug's `ClientContext::prepare`, which registers the statement in `CachedPreparedStatementManager` (`client_context.cpp:309-312`). That manager has **no eviction policy and no removal API** — its `statementMap` only ever grows (`prepared_statement_manager.cpp:13-20`). Dropping the Rust `PreparedStatement` frees the FFI handle but **not** the cached parsed statement and logical plan held by the connection.

Replay uses a **single connection for the entire run** (`handlers.rs`'s `handle_rebuild_from_wal`, `recovery.rs`'s WAL-tail-resume path) and calls `prepare()` once per `flush_batch` (`replay.rs:528`). Consequences:

- A homogeneous WAL at the default batch size of 64 caches roughly one plan per 64 lines — a 5M-line WAL accumulates ~78k plans.
- A **template-interleaved** WAL degrades batching to one row per flush (batching is adjacency-based: a template change flushes the current batch — see `replay.rs:341-351`), producing **one cached plan per line**.

Failure scenario: a large-workspace rebuild grows memory until the service is OOM-killed part-way through replay — leaving a partially-written database with its vector indexes dropped (`handlers.rs`'s `handle_rebuild_from_wal` drops FTS + HNSW indexes before replay, around `handlers.rs:1379-1384`), i.e. the worst possible interrupted state.

There is also a redundancy, independent of the memory-growth question: `flush_batch` calls `conn.prepare(&batch.template)` on every flush, including when a batch is flushed only because it hit `batch_size` (not because the template changed) — `ReplayBatch::clear()` resets `template` unconditionally, so a long run of identical templates re-prepares the same Cypher once per flush even though nothing about the statement changed. This wastes prepare/plan work in addition to feeding the unbounded cache.

This is the second issue in a four-issue series auditing the WAL replay pathway (the first, #237, fixed silent data loss from discarded stats and out-of-order file replay).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A large homogeneous-WAL rebuild does not grow memory without bound (Priority: P1)

An operator (or an embedding host application) triggers a rebuild-from-WAL over a large workspace whose WAL is dominated by a small number of recurring mutation templates (the common case: repeated `Entity` MERGE, repeated `Edge` MERGE, etc.). Today, because `flush_batch` prepares a fresh statement on every flush regardless of whether the template changed, the connection's internal statement cache grows by roughly one entry per batch for the whole run, and RSS grows with it. The operator needs this rebuild to complete without memory growing in proportion to the number of batches processed.

**Why this priority**: this is the literal defect reported — the failure mode is an OOM-killed rebuild that leaves the database in the worst possible state (partially written, indexes dropped).

**Independent Test**: replay a synthetic WAL of many lines using a small, fixed number of distinct Cypher templates (repeated in long homogeneous runs, batched at the default `batch_size`), and confirm the number of `prepare()` calls made against the connection is bounded by the number of distinct templates rather than growing with the number of batches or lines.

**Acceptance Scenarios**:

1. **Given** a WAL where a single template repeats across many consecutive lines (more lines than one `batch_size`, so multiple flushes occur), **When** replay runs, **Then** `flush_batch` reuses the same prepared statement across those consecutive flushes instead of re-preparing on each one.
2. **Given** a WAL where several distinct templates each repeat in long homogeneous runs, **When** replay runs, **Then** the total number of `prepare()` calls is proportional to the number of distinct templates encountered, not to the number of batches or lines replayed.
3. **Given** the real-corpus fixture (~12.5k lines) used by the existing WAL replay e2e test, **When** it is replayed with this change in place, **Then** replay produces the same result as before (no behavioral change) and does not exhibit memory growth attributable to statement caching.

---

### User Story 2 - A pathological, highly-interleaved WAL does not go unaddressed (Priority: P2)

A WAL whose mutation templates alternate on (nearly) every line degrades batching to one row per flush (per the existing adjacency-based batching), so consecutive-flush statement reuse (User Story 1) cannot help — each flush's template differs from the last. In this pathological case the underlying connection's statement cache can still grow unboundedly over a long enough replay, because lbug's `CachedPreparedStatementManager` has no eviction policy. The operator needs this residual risk to be documented, and the option of periodically recycling the replay connection to bound it should be considered, even if not required for this issue's WAL shapes of interest.

**Why this priority**: this scenario is explicitly a residual-risk case, not the primary defect. It matters less than the homogeneous case (User Story 1) because interleaving to this degree is not the common shape of real workspace WALs, but it must not be silently ignored.

**Independent Test**: review confirms that (a) the residual unbounded-growth risk for highly-interleaved WALs is documented, and (b) a periodic-connection-recycling mitigation has been considered and its adoption (or deliberate deferral) is recorded.

**Acceptance Scenarios**:

1. **Given** a WAL whose distinct-template count is itself unbounded relative to run length (the pathological interleaved case), **When** replay runs, **Then** the residual memory-growth risk is documented (in code comments, ADR, or equivalent) rather than left as an undocumented gap.
2. **Given** that residual risk, **When** the fix for this issue is designed, **Then** periodic replay-connection recycling is considered as a mitigation and a decision (adopt now, or defer with rationale) is recorded.

---

### Edge Cases

- A batch flush triggered by hitting `batch_size` mid-run of an otherwise-identical template (as opposed to a flush triggered by the template changing) must still be recognized as "same template" and must reuse the existing prepared statement rather than re-preparing.
- A `prepare()` failure on a template change (the existing error path in `flush_batch`, which currently classifies every buffered row as failed) must continue to behave as it does today — retaining a prepared statement across flushes must not change error handling for a template that fails to prepare.
- Client cancellation (checked once per mutation inside the replay loop, pre-fix location `replay.rs:362-366`) must continue to be honored; the mechanism used to carry a prepared statement across flushes must not create a borrow or lifetime conflict with the cancellation check.
- A WAL file boundary (the existing per-file flush of any partial batch) does not need special handling beyond what's already required for consecutive same-template flushes — this issue does not require preserving a prepared statement across a file boundary if the next file starts with a different template, but neither does it prohibit reuse if the template happens to match.
- A completely empty WAL, or a WAL with no repeated templates at all (every line a distinct template), must continue to replay correctly — the fix must not change *what* gets replayed, only how prepared statements are cached and reused.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `flush_batch` MUST retain and reuse the prepared statement for a template across consecutive flushes of that same template, calling `prepare()` again only when the template actually changes (or after a connection recycle, if FR-004 is adopted).
- **FR-002**: The number of `prepare()` calls made over the course of a replay run MUST be bounded by the number of *distinct* Cypher templates encountered, not by the number of batches flushed or lines replayed, for WAL shapes where the same template recurs across multiple consecutive flushes.
- **FR-003**: The change MUST NOT alter replay's observable results — the set of mutations applied, the resulting graph state, and `ReplayStats` counters (`lines_replayed`, `failed_lines`, `match_prefixed_replayed`, etc.) must be identical to today's behavior for the same input WAL.
- **FR-004**: For WAL shapes where consecutive-flush template reuse cannot bound growth (a highly-interleaved, pathological WAL where the distinct-template count itself grows without bound over a long replay), the residual unbounded-growth risk MUST be documented, and periodic replay-connection recycling MUST be evaluated as a mitigation — its adoption is optional for this issue, but the evaluation and decision must be recorded.
- **FR-005**: Existing error handling in `flush_batch` for a template that fails to `prepare()` (classifying every buffered row in that batch as a failure) MUST be preserved unchanged.
- **FR-006**: Existing cancellation handling in the replay loop MUST continue to function without a borrow or lifetime conflict introduced by carrying a prepared statement across flushes.
- **FR-007**: A test or benchmark MUST demonstrate that replaying a long homogeneous WAL (a single template repeated across many batches) performs O(distinct templates) `prepare()` calls rather than O(lines / batch_size).

### Key Entities

- **`ReplayBatch`** (`crates/core/src/replay.rs`): the in-memory accumulator of rows sharing one Cypher template between flushes. Its lifecycle (when it is cleared vs. carried forward) is the basis for deciding when a prepared statement can be reused.
- **Prepared statement cache** (lbug's `CachedPreparedStatementManager`, internal to the connection): the underlying unbounded cache whose growth this issue bounds indirectly, by reducing how often distinct entries are added to it.
- **Replay connection** (`Conn`, `crates/core/src/db.rs`): the single connection used for an entire replay run, and the thing whose statement cache accumulates the effect described above.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test or benchmark demonstrates that replaying a long, homogeneous WAL (many lines, few distinct templates, more lines than one `batch_size`) performs a number of `prepare()` calls proportional to the count of distinct templates, not to the number of batches or lines.
- **SC-002**: Replaying the real-corpus fixture (~12.5k lines) shows no monotonic RSS growth attributable to statement caching, and the fixture's existing end-to-end replay test continues to pass unchanged.
- **SC-003**: No behavioral change to replay results for any existing WAL replay test — the full existing test suite for `replay.rs` and its e2e coverage passes without modification to expected outcomes.
- **SC-004**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass with these changes.
- **SC-005**: The residual unbounded-growth risk for pathological highly-interleaved WALs is documented (in-code or in an ADR), and a decision on periodic connection recycling as a mitigation is recorded, whether or not it is implemented in this issue.

## Assumptions

- The exact mechanism for carrying a prepared statement across flushes (e.g., threading it as an argument, storing it alongside `ReplayBatch`, or another shape) is a Plan-stage design decision; this spec requires only the observable behavior in FR-001/FR-002, not a specific API shape.
- Whether periodic connection recycling (FR-004) is implemented now or deferred is a Plan-stage decision informed by the evaluation this issue requires; this issue does not mandate its implementation.
- The choice of test vs. benchmark for FR-007/SC-001 (e.g., a counter asserted by a unit test, a log-based assertion, or a criterion benchmark) is left to Research/Plan.
- This issue is unblocked: its sole listed dependency, #237 (silent data loss in WAL replay ordering/statistics), is closed and merged to `main`.
- Per the issue series' stated scope, transaction boundaries, replay ordering/statistics, and sample-cap/idempotency are handled by separate issues in this series and are not addressed here even where they touch the same `flush_batch` region.

## Out of Scope

- Adding an eviction policy or removal API to lbug's `CachedPreparedStatementManager` itself (that manager is upstream C++ code, not something this issue's Rust-side change can alter).
- Transaction boundaries around replay batches (separate issue in this series).
- Replay ordering/statistics correctness (handled by #237, already merged).
- Sample-cap and idempotency behavior of replay (separate issue in this series).
- Mandatory implementation of periodic connection recycling — only its evaluation and a recorded decision are required (see FR-004).

## Source References

*(All line numbers below are pre-fix locations, as they were when this issue was filed — several have shifted post-implementation. See `docs/adr/0045-wal-replay-prepared-statement-cache-scope.md`'s References section for as-implemented symbol locations.)*

- `crates/core/src/db.rs:153-155` (`Conn::prepare`) — reaches lbug's `ClientContext::prepare`, which caches into `CachedPreparedStatementManager` with no eviction.
- `crates/core/src/replay.rs:341-351` — adjacency-based batching: a template change flushes the current batch.
- `crates/core/src/replay.rs:475-502` (`ReplayBatch` / `ReplayBatch::clear`) — batch accumulator whose `clear()` resets `template` unconditionally on every flush.
- `crates/core/src/replay.rs:516-553` (`flush_batch`) — calls `conn.prepare(&batch.template)` once per flush, regardless of whether the template changed since the last flush.
- `crates/core/src/replay.rs:362-366` — per-mutation cancellation check in the replay loop; must remain compatible with any prepared-statement-carrying mechanism.
- `crates/core/src/handlers.rs`'s `handle_rebuild_from_wal` (connection created once, ~line 1379; FTS + HNSW indexes dropped before replay, ~lines 1379-1384) and `crates/core/src/recovery.rs`'s WAL-tail-resume path (connection created once, ~line 230) — both replay an entire run over a single connection.
- Issue #237 (closed) — prior issue in this same four-issue replay-audit series, fixed silent data loss from discarded stats and out-of-sequence file replay in the same `flush_batch` region.
