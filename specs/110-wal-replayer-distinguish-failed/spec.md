# Feature Specification: WAL Replayer Must Distinguish Failed Mutations From Unrecognised Lines

**Feature Branch**: `fabrik/issue-110`
**Created**: 2026-05-28
**Status**: Draft
**Input**: Live observation 2026-05-28 — replaying demo-notebook's 2026-04-11 WAL (1361 lines, 29 files, 14.6 MB) reported `103 mutations replayed, 0 indexes created, 29 WAL files processed`. The remaining ~1258 lines were lumped into `lines_skipped` with no way for the operator to know what happened to them. After local histogram analysis, the breakdown turned out to be: ~872 silently dropped by a too-narrow first-token whitelist (sibling issue), ~386 attempted-and-failed inside `raw_query`. These two failure modes have very different remediation paths — one is a replayer bug, the other is a per-line data/schema problem — but the existing `ReplayStats` API makes them indistinguishable.

## Background

`liminis-graph-core/src/replay.rs` exposes `ReplayStats { lines_replayed, lines_skipped }` (plus per-file counters). The `lines_skipped` field is incremented from at least three distinct code paths:

1. **Unreadable line** (I/O error reading the JSONL line) — `replay.rs:122-126`
2. **Unparseable line** (JSON parse failure) — `replay.rs:139-143`
3. **Unrecognised first-token** (Cypher doesn't start with a whitelist verb) — `replay.rs:166-169`
4. **`raw_query` execution failure** (Cypher executed but lbug returned an error) — `replay.rs:186`

All four paths increment the same counter. The replayer also logs `[WAL WARN] …` lines to stderr for each, but those are captured by the parent process (liminis-app) and not surfaced to the operator. From the IPC response, the operator sees one number and has no signal about which class dominates.

This conflation is the reason today's Check G surfaced as "103 mutations, 1258 lines_skipped" with no further breakdown — the operator could not tell whether the 1258 represented "the replayer can't handle these shapes" (sibling issue, fixable in the replayer) or "the schema doesn't accept these writes" (a data problem, fixable in the migration tooling) without separately tailing logs and running `wc -l` against the WAL files.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Operator Sees Failure Mode Breakdown in Replay Result (Priority: P1)

When the operator triggers `knowledge_rebuild_from_wal` (or any other replay path), the returned stats distinguish:

- Lines successfully replayed
- Lines whose Cypher shape the replayer doesn't recognise as a mutation
- Lines that were attempted but failed at `raw_query` execution
- Lines that were unreadable or unparseable (combined — both are data corruption)

**Why this priority**: This is the difference between "I need to file a replayer bug" and "I need to fix my workspace's WAL" — diagnostically incompatible cases. Operators must be able to tell them apart without log access.

**Independent Test**: Synthesise a 4-line WAL containing: 1 valid CREATE, 1 `MATCH … RETURN` read, 1 CREATE with a malformed JSON params object, 1 CREATE referencing a node UUID that already conflicts on a unique-constraint. Replay. Assert: `lines_replayed=1, unrecognised_lines=1, failed_lines=1, unparseable_lines=1`.

**Acceptance Scenarios**:

1. **Given** a WAL containing 1 successful mutation, 1 unrecognised shape, 1 failed execution, **When** replay completes, **Then** `ReplayStats` exposes all three counts separately.
2. **Given** the IPC response from `knowledge_rebuild_from_wal`, **When** the operator inspects it, **Then** all four counters appear in the response body (not buried in logs).
3. **Given** an existing client built against today's `ReplayStats` shape, **When** the new shape is deployed, **Then** the client continues to work (new fields are additive; `lines_skipped` remains as the sum for back-compat).

---

### User Story 2 — First-N Failure Reasons Are Sampled in the Response (Priority: P2)

For the `failed_lines` bucket, the IPC response includes a small sample (first N, default 10) of failure reasons — Cypher snippet + lbug error message — so the operator can diagnose without log access.

**Why this priority**: Once US-1 lands, the operator knows *how many* mutations failed but still has to find logs to see *why*. A small embedded sample closes that loop for the common case (homogeneous failure mode).

**Independent Test**: Synthesise a WAL with 50 failures all caused by the same missing column. Trigger replay. Assert the IPC response includes a `failed_samples` array with 10 entries, each carrying a truncated Cypher snippet and the lbug error text.

**Acceptance Scenarios**:

1. **Given** 50 failing mutations, **When** replay completes, **Then** `failed_samples` contains exactly 10 entries (the default cap), each with `cypher` (≤200 chars) and `error` fields.
2. **Given** `LCG_REPLAY_FAILURE_SAMPLES=3` in the environment, **When** replay completes with 50 failures, **Then** `failed_samples` contains exactly 3 entries.
3. **Given** zero failures, **When** replay completes, **Then** `failed_samples` is an empty array (not null).

---

### User Story 3 — Telemetry Emits Per-Class Aggregates (Priority: P3)

The existing `TelemetryEvent::WalReplayProgress` (or equivalent) emits the per-class counters at completion (and optionally at periodic checkpoints during replay), so a centralised sink can compare replay fidelity across workspaces over time.

**Why this priority**: This is fleet-observability, not single-operator diagnostics. Lower priority than per-call surfacing because it only matters once there are multiple operators or multi-workspace replay analytics.

**Independent Test**: Replay a WAL with mixed bucket outcomes. Capture stderr. Assert a telemetry event line is emitted containing all four per-class counters.

---

### Edge Cases

- **Replay against an empty WAL directory**: All counters MUST be zero. `failed_samples` MUST be an empty array (not null).
- **Replay where all lines succeed**: `unrecognised_lines = failed_lines = unparseable_lines = 0`, `failed_samples = []`. No regression in this common case.
- **Replay where 100K lines fail with the same error**: `failed_lines = 100000`, `failed_samples` contains the first 10 (or `LCG_REPLAY_FAILURE_SAMPLES`). Total memory: ~10 × (200 + ~200) = ~4 KB. No unbounded memory growth.
- **Cypher snippet contains sensitive data** (e.g., embeddings, personal content): The truncation to 200 chars limits exposure but doesn't eliminate it. Acceptable trade-off because the IPC channel is local Unix socket and the operator already has full DB access. Redacting `$param` placeholders before sampling is out of scope.
- **Per-file counters** (`stats.files_processed`): unchanged. This issue is per-line, not per-file.
- **Concurrency**: replay is single-threaded today. No counter-update race exists; if replay is later parallelised, counters and sample collection MUST be made safe at that time (out of scope).
- **`from_seq` filter**: Lines skipped because their `seq` is below `from_seq` MUST NOT count against any bucket (already true today per `replay.rs:148`).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `ReplayStats` MUST expose at minimum these counters (names normative; type `u64`):
  - `mutations_replayed` (renamed from `lines_replayed` if convenient — keeping `lines_replayed` for back-compat is also acceptable; pick one)
  - `unrecognised_lines` — Cypher present but didn't match the mutation detector
  - `failed_lines` — Cypher matched the mutation detector, was passed to `raw_query`, returned an error
  - `unparseable_lines` — JSON parse failure or I/O read error on the line (combined; both are data corruption)
  - `lines_skipped` — the sum of `unrecognised_lines + failed_lines + unparseable_lines`, retained for back-compat. Either an explicit field or a computed accessor; clients reading it MUST see the same number they see today.
- **FR-002**: The IPC method `knowledge_rebuild_from_wal` (and any other public replay surface) MUST return all four counters in its `result` object. The on-wire field names MUST be stable and documented in `service_protocol.py` (or wherever the IPC schema lives).
- **FR-003**: When a line fails at `raw_query` execution, the replayer MUST capture (a) a truncated copy of the Cypher (first 200 chars), and (b) the lbug error string. Up to N samples (default 10, configurable via env var `LCG_REPLAY_FAILURE_SAMPLES`) MUST be returned in the IPC response under `failed_samples: [{cypher: "...", error: "..."}]`. The samples MUST be drawn from the first N failures (not random — deterministic for testability).
- **FR-004**: The `[WAL WARN]` stderr log lines MUST remain (operators reading them today should not lose information), but a structured telemetry event (`TelemetryEvent::WalReplayProgress` or a new `WalReplayCompleted`) MUST emit the per-class counters at completion regardless of whether stderr logging is captured.
- **FR-005**: The classification of each line into a bucket MUST be exclusive: every counted line lands in exactly one of `mutations_replayed`, `unrecognised_lines`, `failed_lines`, or `unparseable_lines`. The sum MUST equal the total number of attempted lines (after `from_seq` filtering).
- **FR-006**: Regression test MUST cover all four buckets independently and verify the sum-invariant from FR-005.
- **FR-007**: Existing replay test fixtures MUST continue to pass — the test assertions on `lines_replayed` and `lines_skipped` need either no changes or trivial mechanical changes (just point at the new field names).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Re-running demo-notebook's Check G replay after this lands produces an IPC response whose body distinguishes the ~872 unrecognised lines (or 0, once the sibling MATCH-mutation issue lands) from the ~386 raw_query failures. The operator can read a single response and understand which bucket dominates.
- **SC-002**: `failed_samples` contains at least one usable diagnostic entry — Cypher snippet + lbug error — for at least one common failure mode in the demo-notebook replay.
- **SC-003**: Regression tests assert: each bucket can be triggered independently; the four buckets sum to the total attempted lines; `lines_skipped` back-compat field equals the sum of the three non-success buckets.
- **SC-004**: Existing tests pass with at most trivial field-rename changes.
- **SC-005**: The new telemetry event (or extended existing event) is emitted at replay completion and is observable via the stderr telemetry sink in dev.

## Assumptions

- **A1**: The IPC clients that consume `knowledge_rebuild_from_wal` results are limited to liminis-app (TypeScript) and any test harnesses we maintain. Additive field changes are safe; we are not bound by a stable third-party API contract.
- **A2**: Capturing a 200-char Cypher snippet and an error string per failure-sample-slot is sufficient for diagnostic purposes 95% of the time. If specific failure modes need more context, the operator can re-run with a debug env var (future enhancement).
- **A3**: The replay is reading WAL files in `seq` order. The first-N samples are therefore the earliest failures, which is the natural ordering for diagnostic priority (early failures may cascade into later ones).
- **A4**: No existing code reads `lines_skipped` in a way that would semantically depend on its components being indistinguishable (i.e., no client is summing it into a separate "data corruption" bucket today). Retaining it as a sum is safe.

## Out of Scope

- Improving what counts as a recognised mutation (sibling issue: `replay: accept MATCH-prefixed mutation queries`). This issue is purely about visibility into whatever the current classifier decides.
- Retrying failed mutations.
- Persisting failure samples to disk for post-hoc analysis.
- Cancellation / progress reporting during long replays (orthogonal feature).
- Redacting `$param` placeholders from Cypher snippets before sampling.
- Parallelising replay (counter thread-safety at that point is deferred).

## Source References

- `liminis-graph-core/src/replay.rs` — `ReplayStats` struct definition and the four skip paths
- `liminis-graph-core/src/handlers.rs` — `knowledge_rebuild_from_wal` handler; the IPC return shape lives here
- `liminis-graph-core/src/telemetry.rs` — existing telemetry event types; `WalReplayProgress` exists for in-progress emission
- Issue #84 — WAL file rotation (context for WAL layout)
- Issue #29 — Tier 2 WAL admin (`knowledge_rebuild_from_wal` IPC method being extended here)
- Sibling issue: `replay: accept MATCH-prefixed mutation queries` — once that lands, `unrecognised_lines` should drop to near-zero for graphiti-authored WALs; the stats-split is what lets us verify it
