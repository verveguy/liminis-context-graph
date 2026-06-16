# Feature Specification: Batch WAL Replay Writes via UNWIND for Throughput

**Feature Branch**: `fabrik/issue-139`
**Created**: 2026-06-16
**Status**: Draft
**Input**: WAL replay (`WalReplayer::replay_opts` in `replay.rs`) executes one `conn.raw_query(&cypher)` per WAL line — single-statement, no transaction batching, no `UNWIND`. A full replay of a real workspace (~43,821 WAL files / ~2.56M mutations) takes on the order of an hour, almost entirely write-bound.

## Background

WAL replay is the primary recovery path for liminis-graph. After the fidelity fixes in #128, #130, #133, and #136, replay is now *correct* — but it is extremely slow. The bottleneck is that `replay.rs` issues one `raw_query` call per WAL line: one write per mutation, one transaction commit per statement.

On LadybugDB (Kuzu), per-row writes are the known throughput anti-pattern: each write triggers index maintenance (FTS + HNSW) for mutations that include a 768-dimensional `FLOAT[]` embedding. At current throughput, a full workspace recovery takes approximately one hour.

The solution is already demonstrated in this codebase. `liminis-framework/framework/src/skills/knowledge-graph/scripts/backfill_embeddings.py` was written specifically to solve LadybugDB throughput: it batches rows and writes them with a single `UNWIND $rows AS row …` query (`DEFAULT_BATCH_SIZE = 64`, max 1024), with comments noting that per-row writes are too slow. graphiti core mirrors this with its `add_nodes_and_edges_bulk` / UNWIND bulk path.

The Rust replay path never adopted this pattern — it still does one statement per line. With the values now serializing correctly, batching is safe to add.

**Prerequisites satisfied**: #128 (apostrophe escaping), #130 (timestamp literals), #133 (vecf32/episodes/bulk-SET parity), #136 (expired_at schema parity) — all params now serialize correctly.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Full Workspace Recovery Completes in Minutes, Not Hours (Priority: P1)

An operator whose database was lost or corrupted triggers WAL replay to recover their workspace. With batching enabled, the replay throughput is dramatically higher: instead of issuing 2.56M individual write transactions, the replayer groups mutations of the same statement shape into batches and executes each batch as a single `UNWIND` query. The operator sees the recovery complete in minutes rather than waiting an hour.

**Why this priority**: Recovery is already correct after the #128–#136 fidelity fixes. Throughput is now the sole obstacle to WAL replay being usable in practice. An hour-long recovery after a crash is an availability problem.

**Independent Test**: Construct a WAL of at least 1,000 mutations where multiple consecutive mutations share the same Cypher template (the common case for node creates). Replay it with batching enabled. Assert that `mutations_replayed` equals the total mutation count, `failed_lines == 0`, and wall-clock time is substantially less than the per-row equivalent (measurable by timing both code paths with the same WAL).

**Acceptance Scenarios**:

1. **Given** a WAL where 64 consecutive mutations share the same Cypher template (e.g., 64 `Entity` node creates with different params), **When** replay runs with default batch size, **Then** the replayer issues one `UNWIND` query for those 64 mutations rather than 64 individual queries, and `mutations_replayed` is 64.
2. **Given** a WAL where consecutive mutations have different Cypher templates (no opportunity to batch), **When** replay runs, **Then** each mutation is executed individually (batch of 1), and `mutations_replayed` still equals the total line count.
3. **Given** a WAL with N consecutive same-template mutations where N > `LCG_REPLAY_BATCH_SIZE`, **When** replay runs, **Then** the mutations are split into batches of at most `LCG_REPLAY_BATCH_SIZE` and each batch is executed as one `UNWIND` query.
4. **Given** a WAL file boundary is encountered mid-stream, **When** replay transitions to the next WAL file, **Then** any accumulated partial batch is flushed before starting the next file (no mutation from one file is batched with mutations from the next file).

---

### User Story 2 — A Bad Row Does Not Fail the Whole Batch; Failure Attribution Is Preserved (Priority: P1)

A WAL batch that fails — due to a constraint violation, schema error, or any other reason — falls back to per-row execution for every mutation in that batch. This ensures that one invalid mutation cannot silently suppress the writes of 63 valid mutations, and that `failed_lines` / `failed_samples` accurately attribute which specific mutations failed.

**Why this priority**: The entire value of the existing telemetry (`failed_lines`, `failed_samples`, `fidelity_warning`) is failure attribution. Batching that drops this attribution would make silent data loss worse, not better.

**Independent Test**: Construct a WAL batch of 5 mutations where mutation #3 is intentionally invalid (e.g., references a table that does not exist). Replay with batching enabled. Assert: mutations #1, #2, #4, #5 succeed and exist in the rebuilt DB; `failed_lines == 1`; `mutations_replayed == 4`; no `fidelity_warning` (< 10% failure rate); `failed_samples` includes a sample from mutation #3.

**Acceptance Scenarios**:

1. **Given** a batch of N mutations where one fails, **When** the batch-level UNWIND query errors, **Then** the replayer retries all N mutations individually, the valid ones are committed, and only the failing mutation increments `failed_lines`.
2. **Given** a batch fallback is in progress, **When** a legacy-schema mutation (e.g., `Community` node create) is encountered in the batch, **Then** it is counted in `legacy_skipped_lines`, not `failed_lines`, consistent with non-batch behavior.
3. **Given** a batch where all N mutations are invalid, **When** fallback runs, **Then** `failed_lines` increments N times and the `fidelity_warning` logic fires if the total ratio exceeds the threshold.
4. **Given** a successful batch (no error), **When** it completes, **Then** no per-row fallback occurs (the fallback is not a default code path, only triggered on batch error).

---

### User Story 3 — Batch Size Is Configurable for Tuning (Priority: P2)

An operator or developer can tune the replay batch size by setting `LCG_REPLAY_BATCH_SIZE`. The default (64) matches the proven value in `backfill_embeddings.py`. Setting it to 1 is valid and produces exactly the current per-row behavior (useful for debugging or bisecting batch-related regressions).

**Why this priority**: The optimal batch size is hardware-dependent and workload-dependent. The default is proven but not universal. Setting it to 1 is the escape hatch for debugging; setting it higher may improve throughput on hardware with faster index maintenance.

**Independent Test**: Set `LCG_REPLAY_BATCH_SIZE=1`. Run replay. Assert that the behavior is identical to the unbatched path: each mutation issues one query, `mutations_replayed` matches line count, no UNWIND queries are issued.

**Acceptance Scenarios**:

1. **Given** `LCG_REPLAY_BATCH_SIZE` is not set, **When** replay runs, **Then** the default batch size of 64 is used.
2. **Given** `LCG_REPLAY_BATCH_SIZE=256`, **When** replay runs, **Then** batches of up to 256 mutations are issued.
3. **Given** `LCG_REPLAY_BATCH_SIZE=1`, **When** replay runs, **Then** each mutation is executed as a single-statement query (no UNWIND); behavior is identical to the pre-batching code path.
4. **Given** `LCG_REPLAY_BATCH_SIZE=0` or an invalid/non-numeric value, **When** replay starts, **Then** a clear startup error is emitted and replay does not proceed with an undefined batch size.

---

### Edge Cases

- **HNSW serialization constraint (ADR-0047)**: HNSW index writes must remain serialized. Batching is performed serially (one batch at a time, each batch committed before the next begins) — no parallel batch execution. The implementation MUST NOT introduce concurrent batch writes.
- **WAL file boundary flush**: A partial batch at the end of a WAL file MUST be flushed before replay moves to the next file. Mutations from different files MUST NOT be grouped in the same batch.
- **Heterogeneous template sequence**: A run of N mutations where templates alternate (A, B, A, B, …) produces N batches of 1, with no grouping. This is correct and expected; no "lookahead" grouping across template boundaries.
- **Single-mutation WAL**: `mutations_replayed = 1`; the replayer issues one statement (not an UNWIND of 1 — a single-element UNWIND is valid but unnecessary; either form is acceptable).
- **Empty WAL**: No batches issued; all counters zero; no error.
- **Very large params (embeddings)**: Batching 64 mutations each with a 768-dim `FLOAT[]` creates an `$rows` list of 64 × 768 floats. This is a large parameter payload. The Research stage must confirm lbug accepts `$rows` lists of this size without truncation or performance cliff.
- **Batch size exceeds remaining mutations**: If the batch accumulator has fewer mutations than `LCG_REPLAY_BATCH_SIZE` when a template boundary or EOF is hit, the partial batch is flushed immediately.
- **Interaction with `legacy_skipped_lines`**: A mutation classified as a legacy skip MUST flush the current batch (if any) before being classified, then start a fresh accumulator — it cannot be included in an UNWIND batch because it must not execute.
- **Cypher template rewrite**: The UNWIND form requires rewriting param references from `$param` to `row.param` throughout the Cypher template. This transformation must be complete (no unrewritten `$` references remain) and must not corrupt template parts that are not param references (e.g., node labels, relationship types, literal values).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The replay loop in `replay.rs` MUST accumulate consecutive WAL mutations that share an identical Cypher template string (after all existing normalizations from #133 — `strip_vecf32`, `expand_bulk_property_set`) into a batch.
- **FR-002**: When a batch is flushed (on template boundary, WAL file boundary, or EOF), it MUST be executed as a single `UNWIND $rows AS row <rewritten-template>` query if batch size > 1. The param map bound to `$rows` is a list of the individual mutation params maps.
- **FR-003**: The Cypher template MUST be rewritten for UNWIND form: every `$paramName` reference in the template is replaced with `row.paramName`, producing a template that iterates over `row` rather than top-level params. The rewrite MUST be applied only to `$` param references, not to node labels, relationship types, or other template parts.
- **FR-004**: A batch of size 1 MUST be executed as a single-statement query (the existing path), not as a `UNWIND` of one element. This ensures `LCG_REPLAY_BATCH_SIZE=1` and single-mutation batches produce identical behavior to the pre-batching code path.
- **FR-005**: The default batch size MUST be 64. The maximum batch size MUST be 256. `LCG_REPLAY_BATCH_SIZE` env var MUST override the default (integer, 1–256). Values outside the valid range or non-numeric values MUST cause a startup error before any replay begins.
- **FR-006**: Each batch MUST be executed in its own transaction (committed atomically before the next batch begins). No batch spans multiple transactions.
- **FR-007**: Batch execution MUST be strictly serial — one batch committed before the next starts. No concurrent batch writes. (Satisfies ADR-0047 HNSW serialization constraint.)
- **FR-008**: On batch-level execution failure (any lbug error from the UNWIND query), the replayer MUST fall back to per-row execution for every mutation in that batch. Per-row fallback MUST use the existing single-mutation path (with all existing normalizations, legacy-skip classification, `failed_lines` / `failed_samples` attribution).
- **FR-009**: All existing telemetry fields — `mutations_replayed`, `failed_lines`, `failed_samples`, `legacy_skipped_lines`, `fidelity_warning` — MUST remain correct in the batched path. Successful batch execution increments `mutations_replayed` by the batch size. Failed mutations in a per-row fallback increment `failed_lines` individually.
- **FR-010**: A legacy-skip mutation (classified by the `LEGACY_SCHEMA_ERROR_PATTERNS` path from #128) MUST cause the current batch to flush (if non-empty) before the skip is processed, so it is never included in a batch UNWIND query.
- **FR-011**: WAL file boundaries MUST flush and commit any accumulated partial batch before replay advances to the next WAL file.
- **FR-012**: Regression tests MUST cover: (a) a batch of N same-template mutations replays with one UNWIND query and `mutations_replayed == N`; (b) a batch with one failing mutation falls back to per-row, the valid N-1 succeed, and `failed_lines == 1`; (c) `LCG_REPLAY_BATCH_SIZE=1` produces per-row behavior; (d) existing per-mutation replay tests pass unchanged.

### Key Entities

- **`WalReplayer`**: The Rust struct in `replay.rs` that drives WAL replay. This issue adds batch accumulation state to its replay loop.
- **Batch accumulator**: A transient buffer holding consecutive WAL mutations sharing the same Cypher template, flushed when the template changes, a WAL file boundary is hit, or the accumulator reaches `LCG_REPLAY_BATCH_SIZE`.
- **UNWIND template rewrite**: The transformation from `$paramName` → `row.paramName` applied to a Cypher template to produce the batched `UNWIND $rows AS row …` query form.
- **`ReplayStats`**: The struct accumulating `mutations_replayed`, `failed_lines`, `failed_samples`, `legacy_skipped_lines`, `fidelity_warning`. Unchanged in shape; updated in batch increments.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Replay throughput on a representative workspace (≥ 100k same-template mutations) improves by at least 10× over the pre-batching path as measured by wall-clock time. (The batch size of 64 reduces round trips by up to 64×; 10× is the conservative floor after accounting for UNWIND overhead.)
- **SC-002**: `mutations_replayed + failed_lines + legacy_skipped_lines` equals the total WAL line count after a complete replay — no mutations are silently dropped by the batching logic.
- **SC-003**: A test with a batch containing one intentionally invalid mutation produces: the valid mutations present in the rebuilt DB, `failed_lines == 1`, and `mutations_replayed == batch_size - 1`.
- **SC-004**: `LCG_REPLAY_BATCH_SIZE=1` produces results byte-for-byte identical to the pre-batching per-row path (same `mutations_replayed`, `failed_lines`, etc.) when run on the same WAL.
- **SC-005**: All pre-commit gates pass: `cargo fmt --all`, `cargo test`, `cargo clippy --release --all-targets -- -D warnings`.
- **SC-006**: Existing replay regression tests (introduced by #128, #130, #133, #136) pass unchanged. The batching implementation introduces no regressions in correctness.

## Assumptions

- **A1**: Consecutive mutations of the same shape (same Cypher template) are common in production WALs — they are the default output of graphiti node/edge creates, which emit the same template repeatedly with different param values. This is the key premise for batching yielding meaningful throughput gains.
- **A2**: lbug (Kuzu) supports `UNWIND $rows AS row …` with `$rows` bound to a list of param maps, matching the approach used in `backfill_embeddings.py`. The Research stage must confirm this API is available on the replay connection type (`Conn` or equivalent) and verify that large `$rows` payloads (64 × 768-dim embeddings) are handled without truncation.
- **A3**: The `$paramName` → `row.paramName` rewrite is correct for all param reference forms present in production WAL templates. Research should enumerate the actual param reference forms observed (simple `$uuid`, nested `$a.b`, etc.) to confirm the rewrite covers all cases.
- **A4**: Serial batch execution (one committed batch at a time) satisfies ADR-0047's HNSW serialization constraint. No parallel execution is introduced.
- **A5**: The batch size env var default of 64 is appropriate; it matches `backfill_embeddings.py`'s proven default. If production workloads show a different optimal value, the env var allows tuning without a code change.
- **A6**: WAL file boundary is a meaningful semantic boundary (it is the unit of WAL rotation per #84). Flushing at file boundaries is the conservative correct choice and avoids any cross-file ordering ambiguity.

## Out of Scope

- **Progress bar UI** (#135) — complementary to this issue; batched progress is coarser but ETA gets more accurate. The two issues pair naturally but are independent.
- **Replay correctness / dialect fixes** — covered by #128, #130, #133, #136 (prerequisites for this issue).
- **Parallel batch execution** — introducing concurrent batch writes would require resolving ADR-0047 HNSW serialization and thread-safe `ReplayStats` counters. Out of scope; serial batching is the target.
- **Changing the WAL write format** — the write path is not touched. Only the replay path is changed.
- **UNWIND bulk path for non-replay writes** — the live write path (IPC-driven mutations) uses a different code path. Bulk optimization of the live path is a separate concern.
- **Retry logic for failed mutations** — per-row fallback on batch failure is the scope; automatic retry of individual failed mutations is not added here.

## Source References

- `liminis-graph-core/src/replay.rs` — `WalReplayer`, `replay_opts`, `raw_query` call site; the mutation execution hot path
- `liminis-graph-core/src/replay.rs` — `LEGACY_SCHEMA_ERROR_PATTERNS`, `failed_lines`, `legacy_skipped_lines`, `fidelity_warning` — telemetry that must be preserved
- `liminis-framework/framework/src/skills/knowledge-graph/scripts/backfill_embeddings.py` — reference UNWIND batching implementation (`DEFAULT_BATCH_SIZE = 64`, max 1024, comments on LadybugDB throughput)
- Issue #128 — apostrophe escaping (prerequisite; values serialize correctly)
- Issue #130 — timestamp literals (prerequisite)
- Issue #133 — vecf32/episodes/bulk-SET parity (prerequisite)
- Issue #136 — expired_at schema parity (prerequisite)
- Issue #135 — replay progress bar (complementary; pairs with this for ETA accuracy)
- ADR-0042 — per-mutation commit cost (context for transaction batching motivation)
- ADR-0047 — HNSW index writes must stay serialized (constraint on parallelism)
