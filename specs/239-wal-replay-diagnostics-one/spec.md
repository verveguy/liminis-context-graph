# Feature Specification: WAL replay diagnostics — deduplicated failure samples, safe rebuild semantics, honest fidelity warnings

**Feature Branch**: `fabrik/issue-239`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "WAL replay diagnostics: one bad template blinds the failure report; rebuild_from_wal is non-idempotent"

## Background

Two defects in WAL replay make failures hard to diagnose and make a routine rebuild look catastrophic.

**A. One bad template blinds the whole failure report.** When `prepare()` fails for a batch, every row in that batch is classified individually (`replay.rs:528-538` as of this writing) — arithmetically correct, since `prepare()` is a pure function of the template. But `classify_replay_failure` consumes the failure-sample cap **per row**, not per template (`replay.rs:589-594`). With a default cap of 10 and a default batch size of 64, the first template that fails to prepare fills `failed_samples` with ten byte-identical entries, and every other distinct failure category becomes invisible in the `rebuild_from_wal` result payload for the rest of the run.

Concretely: a schema gap on `Entity` MERGE plus an unrelated genuine bug on `MENTIONS` both occur in the same WAL. The operator sees ten copies of the `Entity` error and never learns the `MENTIONS` failures exist at all.

**B. `knowledge_rebuild_from_wal` is non-idempotent against a non-empty database.** The native write path emits `CREATE`, not `MERGE`, for `Entity` / `Episodic` / `RelatesToNode_` (`db.rs:272-334`). `handle_rebuild_from_wal` drops indexes but does not clear the database (`handlers.rs:1378-1381`) — unlike `recover_rebuild_from_workspace_wal`, which deletes the database first (`handlers.rs:2529-2545`, `recover_rebuild_from_workspace_wal` at `handlers.rs:2590`).

Invoking `knowledge_rebuild_from_wal` with the default `from_seq: 0` (a full rebuild) against a populated database therefore produces a duplicate-primary-key failure for every node `CREATE`. That yields a large, benign-looking `failed_lines` count that trips `fidelity_warning` and — via defect A — floods the sample buffer, hiding any real problem underneath it. A user following the documented "rebuild the database from the log" guidance (README: "delete it and `knowledge_rebuild_from_wal` reconstructs the entire graph from the log") hits this immediately if they run the rebuild without first clearing the database by hand.

Note `from_seq > 0` is a distinct, intentional use case (incremental replay resuming from a WAL sequence number after e.g. a checkpoint) where the database is *expected* to already contain state — that path must keep working and is not the defect this issue addresses.

**C. `fidelity_warning`'s denominator silently zeroes out on an all-unrecognised WAL.** The ratio is computed as `failed_lines / (lines_replayed + failed_lines)` (`replay.rs:426`), excluding `unrecognised_lines`, `unparseable_lines`, and `legacy_skipped_lines` from the denominator entirely. A WAL that is 100% unrecognised (e.g. pointed at the wrong directory, or a format the parser doesn't understand) produces `lines_replayed: 0` and `failed_lines: 0`, so the denominator is 0, the `if total > 0` guard short-circuits, and `mutations_replayed: 0` is reported with **no warning at all** — indistinguishable from "nothing to do."

Together these defects mean: the one scenario most likely to occur first (rebuilding against a populated database, or pointing at the wrong WAL) is also the scenario where diagnostics fail the operator worst — either by drowning the real signal in duplicate noise (A+B) or by reporting silent success (C).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Distinct failure categories are all visible after a replay (Priority: P1)

An operator runs `knowledge_rebuild_from_wal` against a WAL that contains two unrelated defects: a schema gap affecting every `Entity` MERGE, and a genuine bug affecting every `MENTIONS` relationship write. Today they see ten identical `Entity` samples and never learn the `MENTIONS` category exists. After this change, the result payload surfaces both categories.

**Why this priority**: This is the core diagnosability defect — without it, operators cannot trust the failure report to tell them what's actually wrong, and fix cycles waste time chasing an incomplete picture.

**Independent Test**: Feed a replay a batch mix where at least two distinct `(template, error)` pairs each occur more than the sample cap's worth of times; assert both appear in `failed_samples` with correct occurrence counts, under the existing default cap (10) and batch size (64).

**Acceptance Scenarios**:

1. **Given** a WAL whose replay produces two distinct failing templates, each occurring more than 10 times, **When** replay completes with the default sample cap of 10, **Then** `failed_samples` contains at least one entry per distinct `(template, error)` pair, each annotated with how many rows it matched, and no single category consumes more than one sample slot.
2. **Given** a WAL whose replay produces more distinct failure categories than the sample cap, **When** replay completes, **Then** the cap still bounds the number of distinct categories reported (not the number of rows), and the total `failed_lines` count is unaffected by the dedup — every failing row is still counted, only sample *storage* is deduplicated.

---

### User Story 2 - Rebuilding against a populated database behaves predictably (Priority: P1)

An operator invokes `knowledge_rebuild_from_wal` with the default `from_seq: 0` against a database that already has data in it (the common case: re-running rebuild without first clearing the DB, per the current README guidance). Today this silently produces a duplicate-key failure for every node, masquerading as a large but vague `failed_lines` count. After this change, the operator gets predictable, documented behavior — either the database is cleared for them automatically, or the call fails immediately with a clear, actionable message telling them what to do instead.

**Why this priority**: This is the scenario most likely to be hit by any operator following the documented rebuild workflow, and today it produces the worst possible diagnostic experience (see Background).

**Independent Test**: Call `knowledge_rebuild_from_wal` with `from_seq: 0` against a database seeded with existing data from a prior successful replay; assert the documented behavior occurs (clean rebuild with no duplicate-key noise, or a fast, clear failure) rather than a flood of duplicate-key failures.

**Acceptance Scenarios**:

1. **Given** a non-empty database and a WAL directory, **When** `knowledge_rebuild_from_wal` is called with `from_seq: 0` (or omitted, since 0 is the default) and no explicit override, **Then** the call either (a) clears the database before replaying, producing a clean rebuild with the graph state matching a rebuild against an empty database, or (b) fails fast before any writes occur, with an error message that names the problem (non-empty database) and the corrective action.
2. **Given** a non-empty database that already reflects WAL sequence numbers up to N, **When** `knowledge_rebuild_from_wal` is called with `from_seq` greater than 0 to resume incremental replay, **Then** the call proceeds without being blocked or triggering a clear — this is the existing, intentional incremental-resume path and must not regress.
3. **Given** the chosen behavior for scenario 1, **When** an operator reads the `knowledge_rebuild_from_wal` tool description (MCP tool registry) or the README, **Then** the documented behavior matches what the tool actually does.

---

### User Story 3 - A wholly-unrecognised WAL is reported as a problem, not a silent no-op (Priority: P2)

An operator runs `knowledge_rebuild_from_wal` against a WAL directory where none of the lines are recognised (e.g. wrong directory, corrupt format, or a WAL from an incompatible version). Today the result reports `mutations_replayed: 0` with no warning — indistinguishable from "there was nothing to replay." After this change, this case produces a `fidelity_warning` so the operator knows something is wrong rather than assuming the rebuild vacuously succeeded.

**Why this priority**: Lower priority than A/B because it's a narrower trigger condition, but still a silent-failure trap with real operator cost — a "successful" rebuild that replayed nothing looks identical to a healthy no-op WAL.

**Independent Test**: Replay a WAL file containing only unrecognised lines; assert the result includes a non-empty `fidelity_warning`.

**Acceptance Scenarios**:

1. **Given** a WAL where every line is unrecognised (none parse as a known mutation type), **When** replay completes, **Then** `fidelity_warning` is populated rather than `None`.
2. **Given** a WAL that replays entirely successfully with zero unrecognised/unparseable/failed lines, **When** replay completes, **Then** `fidelity_warning` remains `None` (no regression on the healthy case).
3. **Given** a WAL containing only legacy-schema constructs that are intentionally skipped (`legacy_skipped_lines`), **When** replay completes with zero genuine failures, **Then** `fidelity_warning` behavior is unchanged from today (legacy-skip is an expected, benign outcome and must not by itself count as a fidelity failure) — this scenario is a regression guard, not a behavior change.

---

### Edge Cases

- A WAL with exactly one failing template occurring many times still gets exactly one sample slot with an accurate occurrence count — not a regression of the existing per-template dedup within a single prepare failure.
- A per-row *execute* failure (as opposed to a batch-level *prepare* failure) that recurs across many rows with the same template and error must also be deduplicated by `(template, error)`, not just prepare-time failures — the fix must cover both call sites of `classify_replay_failure` (`flush_batch`'s prepare-error branch and its per-row execute-error branch).
- Two failures that share a template but have different error strings are distinct categories (e.g. a template that fails with one error on some rows due to a data-dependent constraint violation, and a different error on others) and must each get their own sample slot.
- `dry_run: true` rebuilds must exhibit the same sample-dedup and fidelity-warning fixes as non-dry-run rebuilds, since dry runs are the primary way operators preview a rebuild before committing to it.
- The non-empty-database check/clear (User Story 2) must be scoped to `from_seq: 0` full-rebuild requests; it must not fire for legitimate `from_seq > 0` incremental resume.
- If the chosen behavior for a non-empty database is "fail fast," the failure must occur before any destructive or partial writes happen — not after some rows have already been written.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Failure samples recorded during replay MUST be deduplicated by the pair `(template, error)`, regardless of whether the failure originated from a batch `prepare()` failure or a per-row `execute()` failure.
- **FR-002**: Each deduplicated failure sample MUST include an occurrence count reflecting how many rows produced that `(template, error)` pair during the replay, including occurrences beyond whatever the sample cap is.
- **FR-003**: The failure-sample cap MUST bound the number of distinct `(template, error)` categories retained, not the number of failing rows. The existing default cap value is unchanged by this fix.
- **FR-004**: The total `failed_lines` count MUST continue to reflect every failing row (deduplication affects only sample *storage*, not the failure count used elsewhere in the stats).
- **FR-005**: `knowledge_rebuild_from_wal`, when invoked with `from_seq: 0` (default) against a database that already contains data, MUST NOT silently produce a duplicate-primary-key failure per node. It MUST either (a) clear the database before replaying, or (b) fail before any writes occur, with an error message that identifies the non-empty database as the cause and states the corrective action.
- **FR-006**: `knowledge_rebuild_from_wal` invoked with `from_seq > 0` (incremental resume) against a non-empty database MUST continue to behave as it does today — the FR-005 protection applies only to full (`from_seq: 0`) rebuild requests.
- **FR-007**: The `knowledge_rebuild_from_wal` MCP tool description and the README's documentation of WAL administration MUST accurately describe the chosen FR-005 behavior, including, if a fail-fast design is chosen, how an operator opts into clearing.
- **FR-008**: The fidelity-warning calculation MUST count `unrecognised_lines` and `unparseable_lines` toward the total considered when deciding whether to warn, so that a WAL where every line is unrecognised or unparseable produces a non-`None` `fidelity_warning` rather than a `None` result caused by a zero-length denominator.
- **FR-009**: `legacy_skipped_lines` MUST continue to be excluded from the fidelity-failure ratio's numerator (i.e., legacy-schema skips remain a benign, expected outcome and must not by themselves push a healthy replay over the warning threshold) — this preserves existing, intentional behavior (see `replay.rs` comment on `legacy_skipped_lines`) and is a regression guard, not new behavior.
- **FR-010**: All of the above apply identically whether `rebuild_from_wal` is invoked as a synchronous call, a streaming call with progress notifications, or (per README) a background job polled via `knowledge_rebuild_status` — the fix lives in the shared replay/stats path, not a caller-specific branch.

### Key Entities *(if the feature involves data)*

- **Failure sample**: A record of a distinct replay failure category, keyed by `(template, error)`, carrying the Cypher template preview, the error string, and an occurrence count. Currently a byte-for-byte duplicate is recorded once per row up to the cap; after this change, one entry represents all rows sharing that key.
- **Replay fidelity ratio**: The computed proportion of "bad" WAL lines (failed / unrecognised / unparseable) over lines considered, used to decide whether to attach `fidelity_warning` to a replay result.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Given a WAL replay with two or more distinct failing `(template, error)` categories each exceeding the sample cap in row count, the `rebuild_from_wal` result payload's `failed_samples` includes an entry for each distinct category (up to the cap), each with an occurrence count — verified by an automated test using the documented default cap (10) and batch size (64).
- **SC-002**: Given a non-empty database, invoking `knowledge_rebuild_from_wal` with `from_seq: 0` never produces a duplicate-primary-key failure in its result — verified by an automated test that seeds a database, then rebuilds against it and asserts either a clean result or a fast, explicit failure (not a flood of duplicate-key `failed_samples`).
- **SC-003**: Given a WAL where 100% of lines are unrecognised, the replay result's `fidelity_warning` field is populated — verified by an automated test.
- **SC-004**: Given a WAL that replays with zero failures (a fully healthy replay) or with only benign legacy-skips, `fidelity_warning` remains unset — verified by an automated test guarding against a false-positive regression from FR-008/FR-009.
- **SC-005**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` are all green after the change.
- **SC-006**: The README and the `knowledge_rebuild_from_wal` MCP tool description text are updated to describe the actual non-empty-database behavior; a reader following the documented steps does not hit the duplicate-key failure from Background/B.

## Assumptions

- The choice between "auto-clear the database before a `from_seq: 0` rebuild" and "fail fast, requiring an explicit parameter to clear" is a Plan-stage implementation decision, not fixed by this spec — FR-005 requires one of these two defined behaviors, matching the issue's own framing. Either satisfies this spec provided it is documented (FR-007) and covered by a test (SC-002).
- The FR-005 protection is scoped to `from_seq: 0`; a database populated up to sequence N with a `from_seq > 0` rebuild request is the existing, intentional incremental-resume workflow and is explicitly out of scope for the "non-empty database" protection.
- `legacy_skipped_lines` staying out of the fidelity-failure numerator (FR-009) is existing, intentional behavior per the current code comment, and this issue does not revisit that judgment call — only the total-considered (denominator) gap in defect C is in scope.
- This issue does not change the default failure-sample cap value or default batch size; it changes what a fixed cap of samples represents (distinct categories vs. rows).

## Out of Scope

- Ordering/statistics propagation, statement-cache growth, and transaction boundaries — tracked as separate issues in this series (per the original issue's Scope section).
- Changing the fidelity-warning threshold itself (currently 10%, overridable via `LCG_REPLAY_FIDELITY_THRESHOLD`) — only the denominator/numerator composition (defect C) is in scope.
- Any change to how `recover_rebuild_from_workspace_wal` behaves — it already clears the database and is not affected by this issue; it may serve as a precedent for FR-005's implementation but is not itself being modified by this spec.
- Auditing other callers of `knowledge_rebuild_from_wal` (e.g. the Electron app's lifecycle code) for reliance on the current no-clear behavior — flagged as a risk for the Research/Plan stages to investigate, not resolved here.

## Source References *(optional)*

- `crates/core/src/replay.rs` — `flush_batch`, `classify_replay_failure`, `ReplayStats`, `FailureSample`, fidelity-warning computation.
- `crates/core/src/handlers.rs` — `handle_rebuild_from_wal`, `recover_rebuild_from_workspace_wal` (precedent for database-clearing).
- `crates/core/src/db.rs` — native write path emitting `CREATE` for `Entity` / `Episodic` / `RelatesToNode_`.
- `crates/service/src/mcp/tools.rs` — `knowledge_rebuild_from_wal` tool description and input schema.
- `README.md` — WAL administration section, `knowledge_status` / `indices_built` documentation, rebuild-from-log guidance.
- Prior issue in this series: #237 (WAL replay silently loses data — ordering/statistics), now closed.
