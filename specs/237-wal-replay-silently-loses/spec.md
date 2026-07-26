# Feature Specification: Fix Silent Data Loss in WAL Replay (Discarded Stats, Out-of-Sequence Files, Uncounted No-Ops)

**Feature Branch**: `fabrik/issue-237`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "WAL replay silently loses data: startup rebuild discards stats, and files can replay out of sequence"

## Background

Two independent defects in the WAL replay pathway cause **silent data loss** — data is dropped with no error, no counter, and no log an operator would see.

**A. `Db::open_or_rebuild` discards all replay statistics.** `crates/core/src/db.rs:93` calls `crate::replay::WalReplayer::new(wal_dir).replay(&conn)?;` and throws away the returned `ReplayStats`. This is the library's fresh-install / missing-database rebuild entry point. A schema gap that fails 100% of `Entity` MERGE statements returns `Ok(())` — no telemetry event, no `failed_lines` counter, no `fidelity_warning`, only per-line `eprintln!` that nobody is watching at startup. This is exactly the hazard already recorded in this project's `CLAUDE.md` ("a single schema gap can silently drop an entire category of mutations"), left unguarded on the one replay call site where no caller inspects the result payload. Every other replay entry point that was checked during specification (`knowledge_rebuild_from_wal`, `recovery.rs`'s WAL-tail-resume path) already forwards `stats` into a telemetry event; `handlers.rs`'s `recover_rebuild_from_workspace_wal` also emits telemetry but has a pre-existing, separately tracked `TODO` for a related — but distinct and out-of-scope — gap (see Out of Scope).

**B. WAL files can replay out of order, and the resulting loss is invisible.** Two things compound:

1. Replay orders files by filename, not by sequence. `crates/core/src/replay.rs:198` sorts with `files.sort_by(|a, b| a.file_name().cmp(&b.file_name()))`, but filenames are `YYYYMMDD_HHMMSS_<random-6-hex-session>_<file_seq:04>.jsonl` (`crates/core/src/wal.rs:210-218`). The **random session id sorts before the sequence number**, so a service crash-and-restart within the same wall-clock second — a crash loop, or a fast `knowledge_recover` — can sort the newer session's file *ahead* of the older one. An NTP step backwards has the same effect. Nothing cross-checks each line's `seq` field for monotonicity across files; `seq` is currently used only for the `from_seq` filter (`replay.rs:287-290`).
2. A `MATCH … SET` that matches nothing is counted as a success. `execute_prepared` returns `Ok` for a zero-row match, so `stats.lines_replayed += 1` unconditionally (`replay.rs:540-551`). `match_prefixed_replayed` counts attempts, not effects.

Together: an out-of-order `SET` (embedding enrichment, edge invalidation, entity-type relabelling) targets a node that does not exist yet, matches nothing, and is recorded as replayed. **Zero failure counters, no log line, data silently absent** — and this holds even after (A) is fixed, since a rebuild can "succeed" with a clean `fidelity_warning` while still having quietly dropped every effect of an out-of-order write.

This is the first issue in a four-issue series auditing the replay pathway; the others (transaction boundaries, statement-cache growth, sample-cap/idempotency) are sequenced behind this one because they touch the same code.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator trusts a fresh-install rebuild's outcome (Priority: P1)

An operator (or an embedding host application) calls `Db::open_or_rebuild` against a WAL directory with no existing database file — a fresh install, or a WAL-only recovery. If the rebuild silently drops mutations because a Cypher template no longer matches the schema, the operator needs to find out, rather than running for weeks against a graph that is quietly missing an entire category of data.

**Why this priority**: this is the literal defect reported — the one replay call site where a schema gap can drop every write of a category with zero observable signal.

**Independent Test**: point `open_or_rebuild` at a WAL directory containing mutations whose Cypher template is guaranteed to fail `prepare()` against the current schema (e.g. references a dropped column), and confirm the caller can observe that failure rather than getting back an indistinguishable-from-clean `Db`.

**Acceptance Scenarios**:

1. **Given** a WAL directory whose mutations all use a Cypher template that fails to prepare against the current schema, **When** `Db::open_or_rebuild` runs its fresh-install rebuild, **Then** the caller can observe that a significant fraction of lines failed — surfaced through the return path and/or a log line at a severity that is not missable under default operation.
2. **Given** a WAL directory that replays cleanly, **When** `open_or_rebuild` runs, **Then** no failure signal is emitted and the rebuilt `Db` is returned as it is today.

---

### User Story 2 - Replay reconstructs state in true write order after a crash-restart (Priority: P1)

An operator recovers from a service crash-loop (or a rapid `knowledge_recover`) where two WAL sessions wrote files within the same wall-clock second. Replay must apply mutations in the order they actually happened, so that a `SET` depending on an earlier `CREATE`/`MERGE` is never applied before its target exists.

**Why this priority**: replay order determines correctness of every dependent write, and the current sort key (full filename, which places a random session id ahead of the sequence number) can silently invert the true order without any indication that it happened.

**Independent Test**: construct WAL files across two sessions whose filenames sort lexicographically opposite to their true `seq` order, replay them, and confirm the resulting graph state matches what replaying in true `seq` order would produce.

**Acceptance Scenarios**:

1. **Given** WAL files from two sessions whose filenames sort lexicographically in the reverse of their `seq` order, **When** replay runs, **Then** mutations are applied in `seq` order and the resulting graph state matches full-`seq`-order replay.
2. **Given** a set of WAL files where a later-processed file contains a `seq` value that is not greater than the maximum `seq` already seen, **When** replay encounters it, **Then** the regression is counted and logged rather than silently applied or silently ignored.

---

### User Story 3 - A no-op `SET` is distinguishable from a real write (Priority: P1)

An operator inspecting replay stats needs a `MATCH ... SET` that matched zero rows (for example, because its target node hadn't been created yet, or was legitimately deleted earlier) to be reported as a distinct outcome from a successful write, so "N mutations replayed" is never inflated by writes that had no effect.

**Why this priority**: this is the defect that makes User Story 2's ordering fix insufficient on its own — even with correct ordering, some no-op matches are legitimate (a genuinely deleted target) and some are data loss (an out-of-order write); both currently vanish into the same success counter, so the fidelity assessment can't see either.

**Independent Test**: replay a WAL containing one `MATCH ... SET` whose target node was never created, and confirm the resulting stats report it under a distinct no-op counter rather than as a plain success.

**Acceptance Scenarios**:

1. **Given** a `MATCH ... SET` statement whose match clause affects zero rows, **When** it is replayed, **Then** it is counted in a distinct counter (e.g. `match_prefixed_no_op`) and is not counted as an unqualified `lines_replayed`/`match_prefixed_replayed` success.
2. **Given** a mix of no-op and effective `MATCH`-prefixed writes, **When** the fidelity ratio is computed, **Then** the no-op count is factored into that assessment rather than being invisible to it.

---

### Edge Cases

- A WAL directory with only one session (no crash-restart) must continue to replay in the existing correct order — the ordering fix must not regress the common case.
- A WAL directory with zero files, or a WAL directory that doesn't exist, must continue to behave as today (no-op, `Ok`).
- `LCG_REPLAY_FIDELITY_THRESHOLD` already governs the fidelity-warning ratio computed inside `WalReplayer::replay`; the new zero-row-match counter must feed into that same assessment rather than living as a disconnected, un-thresholded metric.
- A WAL filename that doesn't match the expected `file_seq` pattern (corrupt or foreign filename) must not panic replay — it needs a defined fallback (e.g., fall back to that file's first `seq` value, or to filename order for that file only); the exact fallback is a Plan-stage decision, but replay must neither crash nor silently drop the file.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `Db::open_or_rebuild`'s fresh-install rebuild path MUST NOT discard the `ReplayStats` produced by `WalReplayer::replay`.
- **FR-002**: When `open_or_rebuild`'s rebuild produces a `fidelity_warning` (or otherwise crosses the existing significant-failure threshold), that outcome MUST be surfaced to the caller and/or logged at a severity that is not missable under default operation — at minimum matching the visibility `knowledge_rebuild_from_wal` already gives its caller today.
- **FR-003**: WAL file replay order MUST be derived from sequence information (the `file_seq` filename component and/or each file's first `seq` value), not from full-filename lexicographic comparison. The random per-session id component of the filename MUST NOT influence ordering.
- **FR-004**: Replay MUST validate that `seq` is monotonically non-decreasing across the files it processes, in processing order. A detected regression (a later-processed file/line whose `seq` is not greater than the maximum `seq` already seen) MUST be counted in a dedicated counter and logged — not silently applied, and not silently skipped.
- **FR-005**: A `MATCH`-prefixed mutation (`MATCH ... SET`, `MATCH ... DETACH DELETE`, etc.) whose execution affects zero rows MUST be counted in a counter distinct from `lines_replayed` and from an effective `match_prefixed_replayed` write, and MUST NOT be reported as an unqualified success.
- **FR-006**: The zero-row-match counter introduced by FR-005 MUST be included in the fidelity assessment (the same mechanism that today computes `fidelity_warning` from `failed_lines`), so a high zero-row-match rate is also visible as a fidelity concern.
- **FR-007**: The ordering fix (FR-003, FR-004) and the no-op accounting fix (FR-005, FR-006) MUST apply uniformly across every replay entry point that shares `WalReplayer::replay`/`replay_opts` — `open_or_rebuild`, `knowledge_rebuild_from_wal`, and both recovery paths (`recovery.rs`'s WAL-tail resume and `handlers.rs`'s `recover_rebuild_from_workspace_wal`) — since file ordering and no-op accounting are properties of the shared replayer, not of any one caller. FR-001 and FR-002 apply specifically to the `open_or_rebuild` call site named in this issue.

### Key Entities

- **ReplayStats**: the struct already returned by `WalReplayer::replay`/`replay_opts`. Gains a new zero-row-match counter (FR-005) and must no longer be silently dropped by `open_or_rebuild` (FR-001).
- **WAL file**: a `.jsonl` file named `YYYYMMDD_HHMMSS_<session-id>_<file_seq>.jsonl`. Replay ordering is defined by `file_seq`/`seq`, not by the full filename string (FR-003).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test demonstrates that a `Db::open_or_rebuild` rebuild whose WAL entirely fails to prepare against the schema no longer returns an outcome indistinguishable from success — the failure is observable from the call.
- **SC-002**: A test demonstrates that a WAL fixture whose filenames sort opposite to their `seq` order replays into the same graph state as replaying that WAL in true `seq` order.
- **SC-003**: A test demonstrates that a `seq` regression across files increments a dedicated counter and produces a log line, for a WAL fixture engineered to regress.
- **SC-004**: A test demonstrates that a `MATCH ... SET` targeting a non-existent node is reflected in the distinct no-op counter rather than in `lines_replayed`/`match_prefixed_replayed`.
- **SC-005**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass with these changes.

## Assumptions

- The exact mechanism for FR-002 ("failing loudly" vs. "logging at an unmissable level") is a Plan-stage design decision; this spec requires only that the outcome be observable, not a specific API shape (e.g., whether `open_or_rebuild`'s signature changes to return stats alongside `Db`, or whether it takes a telemetry/logging parameter).
- The exact fallback behavior for a WAL filename that doesn't match the expected `file_seq` pattern is left to Plan/Research; the only hard requirement is that replay neither panics nor silently drops the file (see Edge Cases).
- `handlers.rs`'s `recover_rebuild_from_workspace_wal` has a pre-existing `TODO` (tracked against #128) noting that it emits a `TelemetryEvent::WalReplayComplete` but does not yet forward `fidelity_warning` into `RecoverOutcome`. That gap is out of scope for this issue (see Out of Scope), even though this issue's ordering and no-op-accounting fixes (FR-003–FR-006) still apply to that code path automatically, since it shares the same underlying replayer.
- This spec treats `Db::open_or_rebuild` as the library's documented fresh-install/rebuild contract regardless of whether `crates/service`'s `main.rs` currently calls it. As of specification time, `main.rs`'s actual startup sequence opens the DB via `Db::open` followed by explicit `init_schema`/`build_indices_and_constraints`/`rebuild_name_index` calls, and does not call `open_or_rebuild`. The defect is real against the function's own public contract (exercised directly by `crates/core/tests/wal_replay.rs` and `crates/core/tests/name_index_coherence.rs`, and referenced by `docs/adr/0038-in-process-name-index.md`) independent of whether the shipped service binary currently exercises that code path.
- `wal.rs`'s `scan_max_seq` (used to seed the writer's next `seq` value on startup) was reviewed during specification: it iterates files in an order derived from filename, but reduces to a true max across *all* files regardless of iteration order, so it is not subject to the same silent-loss defect and does not need a corresponding fix here.

## Out of Scope

- Fixing `handlers.rs`'s `recover_rebuild_from_workspace_wal` gap where `RecoverOutcome` doesn't forward `fidelity_warning` — already tracked against #128, and a candidate for a later issue in this same replay-audit series.
- Transaction boundaries, statement-cache growth, sample-cap and idempotency issues — explicitly called out as separate issues in this same four-issue series.
- Wiring `Db::open_or_rebuild` into `crates/service`'s actual startup sequence in `main.rs`. It is not called there today, and this issue only requires that the function's own contract stop discarding stats — not that it become the service's startup path.
- Changing `wal.rs`'s `scan_max_seq` file-iteration order — confirmed during specification to already be unaffected by this defect (see Assumptions).

## Source References

- `crates/core/src/db.rs:93` — `Db::open_or_rebuild` discards `ReplayStats`.
- `crates/core/src/replay.rs:198` — file ordering by full-filename lexicographic comparison.
- `crates/core/src/replay.rs:287-290` — `seq` used only for the `from_seq` filter; no monotonicity check.
- `crates/core/src/replay.rs:540-551` (`flush_batch`) — zero-row `execute_prepared` success folded into `lines_replayed`/`match_prefixed_replayed`.
- `crates/core/src/wal.rs:210-218` (`make_new_file_path`) — filename format `YYYYMMDD_HHMMSS_<session-id>_<file_seq:04>.jsonl`.
- `crates/core/src/handlers.rs:2620-2623` (`recover_rebuild_from_workspace_wal`) — sibling, already-tracked TODO for the related-but-out-of-scope fidelity_warning gap (tracked against #128).
- `crates/core/src/recovery.rs:229-249` — WAL-tail-resume recovery path; already forwards `stats` into `TelemetryEvent::WalAutoRecovery`, unaffected by defect A but in scope for FR-007's ordering/no-op fixes.
- `crates/core/tests/wal_replay.rs`, `crates/core/tests/name_index_coherence.rs` — existing tests exercising `Db::open_or_rebuild`.
- `docs/adr/0038-in-process-name-index.md` — documents `Db::open_or_rebuild` as a call site of interest.
