# Feature Specification: Batch WAL-replay embedding recompute calls

**Feature Branch**: `fabrik/issue-486`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "Batch WAL-replay embedding recompute calls (split from #445)"

## Background

Issue #445 added a batch API to `Embedder` (`embed_batch`, `crates/core/src/embedder.rs`) and
converted every batchable ingest-side embed loop (entity names, edge facts, salvage-endpoint
lookups, per-entity summaries computed at ingest time, and the
`knowledge_backfill_summary_embeddings` admin backfill) to use it. **#445 has since merged** —
`Embedder::embed_batch` exists on `main` today. WAL replay's recompute path, introduced by #440
and since modified by #526, was deliberately excluded from #445's scope. This issue is that
exclusion's follow-up, and its stated prerequisite (#445 merging) is now satisfied.

### Why WAL replay needs batching too — and why it's the biggest win, not an afterthought

`replay.rs`'s recompute path re-derives every stored embedding vector from its co-located
source text at replay time, via a synchronous callback invoked once per WAL row. On the #217
real-corpus fixture, `crates/core/tests/real_corpus_replay_perf.rs` measures **4,126 recognized
embedding-vector placeholders across 4,106 distinct texts** — the single largest source of
embed calls anywhere in the system, larger than the entire ingest path's ~3,898 calls across
228 chunks that #445 converted.

**The in-memory embedding cache does not make this cheap.** `embedding_cache.rs` documents that
`EmbeddingCache` lives entirely in-process memory and is never persisted; losing it (a process
restart, an explicit clear) means the next lookup recomputes at full cost. WAL replay
overwhelmingly runs at startup or during recovery — i.e. against a cold cache — and
`crates/core/tests/real_corpus_replay_perf.rs`'s own cold-vs-warm test on this fixture confirms
the cache dedupes only *repeated* texts within one replay's lifetime: since the WAL is post-dedup,
there is almost nothing to hit on a cold pass. Replay cost is therefore approximately
`(distinct texts) / (embedder throughput)`, with or without the cache. Batching would collapse
that to tens of round-trips instead of thousands.

### Current architecture (as of this spec, post-#526)

- The recompute callback type is `RecomputeEmbedFn = Box<dyn Fn(&str) -> Result<Vec<f32>, Error>
  + Send>` (`crates/core/src/replay.rs`) — one text in, one vector out, invoked once per WAL row
  via `recompute_row_embeddings`. Two production call sites build this closure today, each
  wrapping the shared `EmbeddingCache::get_or_compute` around a single-text `embedder.embed(text)`
  call: `Db::open_or_rebuild`'s `build_sync_recompute_fn` (`crates/core/src/db.rs`, which builds
  its own dedicated single-threaded tokio runtime since it has no ambient one) and
  `EmbedderContext::recompute_fn_via_handle` (`crates/core/src/embedding_cache.rs`, used from
  inside `tokio::task::spawn_blocking` by `recovery::run_full_recovery_sequence` and two
  `handlers.rs` rebuild paths). `WalReplayer` itself has no tokio dependency and several call
  sites (its own unit tests, `real_corpus_replay_perf.rs`) run with no ambient runtime at all —
  this is why the callback is a plain sync closure rather than an async method, and why each
  caller bridges its own runtime context individually.
- Recompute runs **before** a row is pushed into the existing, separate same-template Cypher
  `UNWIND` execution batch (`ReplayOptions.batch_size` / `LCG_REPLAY_BATCH_SIZE`, issues
  #238/#240) — deliberately, so that a slow or unreachable embedder never holds an open lbug
  transaction across a network round-trip. **This is a different batching mechanism from the one
  this issue proposes**: it batches multiple WAL rows' Cypher execution into one `UNWIND`
  statement per flush, independent of how many embedder round-trips those rows' recompute step
  cost. This issue's embedding-batch window and that existing execution-batch window are not the
  same thing and are not required to share size or boundaries.
- Per-row fallback (issue #526, `is_vector_only_set`): a row with no co-located source text (or
  whose recompute call fails) is either zero-vector-filled (if the row must still execute, e.g.
  a `CREATE`) or skipped entirely (if it's a vector-only `SET` with nothing else to do — executing
  it with a placeholder would overwrite a real vector the entity's own `CREATE` record already
  computed). `ReplayStats` tracks this via five counters: `embeddings_recomputed`, `embed_calls`,
  `embeddings_recompute_skipped_no_text` (renamed from the pre-#526
  `embeddings_recompute_fallback` — the issue that originally proposed this work predates that
  rename), `embeddings_recompute_failed`, and `embeddings_skip_rows`.
- `dry_run` mode skips embedding recompute entirely today — no embed calls are issued when
  `ReplayOptions.dry_run` is set.
- Model-identity mismatch detection (issue #440, FR-006/FR-007) persists the `(model, dim)`
  identity from `EmbedderContext::identity()` for later comparison via
  `embedding_model_status`-style reads; this mechanism sits alongside recompute but is not itself
  part of the per-row embed loop.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - WAL replay batches embedding recompute across a window of rows (Priority: P1)

As a developer or operator running a WAL rebuild or startup/crash recovery, replay's embedding
recompute step issues a small number of batched embedder round-trips for a window of rows'
source texts, instead of one round-trip per row, so that replay's embedding cost approaches
`(distinct texts) / (embedder throughput)` with tens of round-trips rather than thousands.

**Why this priority**: This is the entire point of the issue — the WAL-replay recompute path is
the single largest source of embed calls anywhere in the system (larger than the whole ingest
path #445 already converted), and it is currently the *only* embed call site left issuing one
round-trip per item.

**Independent Test**: Replay a WAL fixture with a known number of distinct recognized-embedding
texts through an embed-call-counting test embedder (the existing `CountingEmbedder` pattern used
by `real_corpus_replay_perf.rs`) and confirm the number of real embedder calls drops from
one-per-row to a small number of batch calls (accounting for window-size chunking), while
`ReplayStats.embeddings_recomputed` and `ReplayStats.embed_calls` report the same totals as
today's per-row implementation would for the same input.

**Acceptance Scenarios**:

1. **Given** a WAL containing multiple rows whose Cypher template references a recognized
   embedding vector placeholder, each with distinct co-located source text, **When** replay
   processes them, **Then** the underlying embedder is invoked via one or more batch calls
   covering multiple rows' texts each, rather than one call per row.
2. **Given** the same WAL, **When** two or more rows share identical co-located source text
   (a same-text repeat within one replay), **Then** the existing `EmbeddingCache` behavior is
   preserved — a text already resolved earlier in the same replay does not require a further
   embedder round-trip inside any subsequent batch call.
3. **Given** `ReplayOptions.dry_run` is set, **When** replay runs, **Then** no embed calls
   (batched or otherwise) are issued, matching today's dry-run behavior exactly.
4. **Given** a live replay in progress with `ReplayOptions.cancel_fn` set, **When** cancellation
   fires, **Then** replay stops within a latency comparable to today's per-row check — it is not
   forced to wait for an arbitrarily large in-flight batch window to finish first.

---

### User Story 2 - All of #440/#526's existing guarantees are preserved exactly (Priority: P1)

As a developer relying on `ReplayStats`'s embedding-related counters or on
model-identity-mismatch detection after a replay, every one of those signals means exactly what
it means today — the batching restructure changes only how many network round-trips replay's
embedding step costs, never what gets counted, skipped, zero-filled, or persisted.

**Why this priority**: This is a rewrite of the path that turns the WAL — the system's source of
truth — back into a graph. A silent behavior change here (a miscounted stat, a fallback that no
longer fires, a mismatch detector that stops firing) produces a rebuild that silently yields a
different graph, which the original issue correctly identifies as one of the worst failure
classes in this system.

**Independent Test**: Run the existing `replay.rs` unit test suite and
`real_corpus_replay_perf.rs`'s validation test
(`validate_recompute_matches_stored_vectors_for_real_corpus_wal`) unmodified in their assertions
against the batched implementation; every existing pass/fail expectation for counters, fallback
behavior, and per-kind vector agreement continues to hold.

**Acceptance Scenarios**:

1. **Given** a row with no co-located source text that must still execute (e.g. a `CREATE`),
   **When** batched recompute processes the window containing it, **Then** that row still
   receives a same-dimension zero-vector fallback and is still counted in
   `embeddings_recompute_skipped_no_text`, exactly as today — independent of whatever else is in
   its batch window.
2. **Given** a vector-only `SET` row with no co-located source text, **When** batched recompute
   processes the window containing it, **Then** that row is still skipped entirely (never
   executed against the database) and still counted in `embeddings_skip_rows`, exactly as today.
3. **Given** a batch/window embed call where one or more (but not all) of the window's texts
   fail to embed, **When** replay handles the failure, **Then** only the affected row(s) are
   routed to `embeddings_recompute_failed` and their existing fallback path — unaffected rows in
   the same window are unaffected, and the whole replay does not abort because of the partial
   failure.
4. **Given** the same WAL and embedder replayed before and after this change, **When** replay
   completes, **Then** `ReplayStats.embeddings_recomputed`, `embed_calls`,
   `embeddings_recompute_skipped_no_text`, `embeddings_recompute_failed`, and
   `embeddings_skip_rows` all report identical totals, and any persisted model-identity value
   (`EmbedderContext::identity()` / `embedding_model_status`) is identical.
5. **Given** replay's existing invariant that recompute happens outside of any open lbug
   transaction (so a slow or unreachable embedder cannot hold a `flush_batch` transaction open
   across a network round-trip), **When** recompute is batched, **Then** that invariant still
   holds — a batch's network round-trip(s) never occur while a transaction is open.

---

### Edge Cases

- A window whose rows span a change in Cypher template (the boundary the existing Cypher
  execution batch already flushes on) is not required to align with the embedding-batch window —
  the two batching concepts are independent (see Background).
- A window containing only rows with no co-located source text (all fallback/skip) issues no
  network round-trip at all for that window.
- A batch call that fails entirely (e.g. embedder unreachable for the whole call, not just some
  texts) degrades every row in that window to its existing per-row fallback path — none of them
  abort the replay, matching today's single-row failure semantics applied per row in the window.
- The last window in a replay (or in a file) may be smaller than the configured window size; it
  must still be flushed and processed, not silently dropped.
- A duplicate text appearing twice within the *same* window (not just across windows) must not
  require two embedder round-trips for that text — whether this is handled via the existing
  cache, via batch-call deduplication, or both, is a Research/Plan-stage decision; the guarantee
  is the outcome (no redundant round-trip), not the mechanism.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: WAL replay's row-processing MUST buffer a window of multiple rows' co-located
  source texts and issue embedding recompute via `Embedder::embed_batch` (added by #445) once
  per window, instead of invoking a single-text recompute callback once per row.
- **FR-002**: The batching restructure MUST NOT change the meaning, triggering conditions, or
  presence of any existing `ReplayStats` embedding-related counter: `embeddings_recomputed`,
  `embed_calls`, `embeddings_recompute_skipped_no_text`, `embeddings_recompute_failed`, and
  `embeddings_skip_rows`. No rename, removal, or redefinition of any of these is in scope.
- **FR-003**: The per-row fallback decision governed by `is_vector_only_set` (zero-vector fill
  for a row that must still execute vs. skip entirely for a vector-only `SET` with no source
  text) MUST continue to be evaluated independently per row — a window MUST NOT force a uniform
  outcome (all-fallback or all-skip) across its rows just because one row lacks source text or
  fails to embed.
- **FR-004**: A failure affecting one or more rows within a batch/window embed call (whether the
  whole call fails, or the embedder rejects/mismatches a subset) MUST degrade exactly like
  today's single-row failure: affected rows are routed to `embeddings_recompute_failed` and
  their existing fallback path (FR-003); unaffected rows in the same window are unaffected; the
  whole replay MUST NOT abort because of a batch/window-level failure.
- **FR-005**: Model-identity capture and persistence (`EmbedderContext::identity()`'s `(model,
  dim)` pair, and whatever consumes it for `embedding_model_status`-style comparison) MUST be
  unaffected by this restructure.
- **FR-006**: When the existing `EmbeddingCache` is in use, a text already cached for the active
  `(model, dim)` identity MUST NOT require a further embedder round-trip when it recurs later in
  the same replay — including recurrence within a single batch window, not only across windows.
- **FR-007**: `ReplayOptions.dry_run` MUST continue to skip embedding recompute entirely — no
  batch (or single) embed calls issued during a dry run, matching today's behavior.
- **FR-008**: `ReplayOptions.cancel_fn` cancellation MUST remain responsive at a granularity
  comparable to today's per-row check — the batch/window size chosen MUST NOT make cancellation
  latency unboundedly worse than today's.
- **FR-009**: This issue's embedding-batch window is independent of the existing same-template
  Cypher `UNWIND` execution batch (`ReplayOptions.batch_size` / `LCG_REPLAY_BATCH_SIZE`, issues
  #238/#240). This issue MUST NOT require the two windows to share size or boundaries, though
  Research/Plan MAY choose to align them if that turns out to simplify the implementation.
- **FR-010**: Replay's existing invariant that embedding recompute happens entirely outside of
  any open lbug transaction (so a slow or unreachable embedder cannot hold a `flush_batch`
  transaction open across a network round-trip) MUST be preserved under batching.
- **FR-011**: Every call site that currently builds a `RecomputeEmbedFn`-shaped callback —
  `Db::open_or_rebuild`'s `build_sync_recompute_fn`, `EmbedderContext::recompute_fn_via_handle`,
  and `replay.rs`'s own unit tests — MUST be migrated to whatever batch-capable shape this issue
  introduces. This issue MUST NOT leave any production call site still bridging the embedder one
  text at a time as an unbatched bypass.

### Key Entities

- **`RecomputeEmbedFn` / `ReplayOptions.recompute_embed_fn`** (`crates/core/src/replay.rs`): the
  per-row synchronous embedding callback this issue restructures for batched invocation. Its
  exact resulting shape (signature, whether it stays a callback or becomes something else) is a
  Research/Plan-stage decision.
- **`ReplayStats`** (`crates/core/src/replay.rs`): the counters this issue's restructure must
  leave semantically unchanged (FR-002).
- **`EmbeddingCache` / `EmbedderContext`** (`crates/core/src/embedding_cache.rs`): the existing
  content-addressed cache and identity bundle that batching must keep working alongside (FR-005,
  FR-006).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On the #217 real-corpus fixture (4,126 recognized embedding-vector placeholders
  across 4,106 distinct texts, per `real_corpus_replay_perf.rs`), the number of real embedder
  round-trips issued during a cold-cache WAL replay is reduced by at least 90% relative to
  today's approximately one-round-trip-per-distinct-text baseline, accounting for any
  window-size chunking.
- **SC-002**: Every existing `replay.rs` unit test and every existing `ReplayStats`-based
  assertion in `real_corpus_replay_perf.rs` and other replay/recovery integration tests continues
  to pass with its existing counter-value expectations, unmodified — only the number of
  underlying embedder network calls changes, never a counted outcome.
- **SC-003**: A window/batch call where one or more rows fail to embed or lack source text
  surfaces those specific rows via existing per-row counters and fallback behavior; it never
  aborts the whole replay and never silently drops or mis-attributes an unrelated row in the same
  window.
- **SC-004**: For a fixed WAL and embedder configuration, `embedding_model_status` (or equivalent
  identity-mismatch detection) reports identically before and after this change.

## Assumptions

- Issue #445 has merged and `Embedder::embed_batch` exists on `main`; the original issue's
  "land after #445 merges" sequencing note is satisfied and no longer a blocker.
- The exact batch/window size — whether a fixed constant, an env-var-configurable knob (possibly
  mirroring `LCG_REPLAY_BATCH_SIZE`'s pattern), or something else — is a Research/Plan-stage
  decision. This spec requires only that some bounded window is used (FR-008 bounds cancellation
  latency; the Edge Cases note the last, possibly-partial window must still be processed).
- The exact resulting shape of the `RecomputeEmbedFn` callback (or its replacement) — including
  whether `WalReplayer` gains new API surface or whether batching is layered above the existing
  callback boundary — is a Research/Plan-stage decision. `WalReplayer`'s decoupling from
  tokio/async (per #440's original design) is a real constraint on that decision but not one this
  spec resolves.
- Whether the embedding-batch window is coupled to or independent of the existing Cypher
  `UNWIND` execution batch is a Research/Plan-stage decision (FR-009).

## Out of Scope

- Persisting `EmbeddingCache` across process restarts — a different concern, not proposed by
  this issue.
- Any change to the existing Cypher `UNWIND` execution batching mechanism itself
  (`ReplayOptions.batch_size` / `LCG_REPLAY_BATCH_SIZE`, issues #238/#240), beyond whatever
  minimal interaction is needed to keep it working alongside the new embedding-batch window
  (FR-009).
- Ingest-side or `knowledge_backfill_summary_embeddings` embedding batching — already shipped by
  #445.
- Changes to `OaiEmbedder`'s HTTP/UDS transport mechanics beyond what #445 already introduced.
- Any change to model-identity-mismatch *policy* (e.g. automatically re-embedding on a detected
  mismatch) — this issue only requires that today's detection continues to fire identically
  (FR-005, SC-004), not that its consequences change.
- Deduplicating identical texts *across* a replay in any way beyond what `EmbeddingCache` already
  does — this issue's within-window dedup guarantee (FR-006) is about not regressing existing
  cache behavior, not adding new dedup semantics.

## Source References

- `crates/core/src/replay.rs` — `WalReplayer`, `ReplayOptions`, `ReplayStats`,
  `RecomputeEmbedFn`, `recompute_row_embeddings`, `is_vector_only_set`, `EMBEDDING_TEXT_PAIRS`.
- `crates/core/src/embedding_cache.rs` — `EmbeddingCache`, `EmbedderContext`.
- `crates/core/src/db.rs` — `build_sync_recompute_fn` (`Db::open_or_rebuild`'s bare-sync call
  site).
- `crates/core/src/recovery.rs` — `run_full_recovery_sequence`'s two replay call sites.
- `crates/core/src/handlers.rs` — `handle_rebuild_from_wal`, `recover_rebuild_from_workspace_wal`.
- `crates/core/src/embedder.rs` — `Embedder::embed_batch` (issue #445), the batch API this issue
  consumes.
- `crates/core/tests/real_corpus_replay_perf.rs` — the #217 real-corpus fixture's existing
  benchmarks (`measure_replay_throughput_over_real_corpus_wal`,
  `measure_cold_vs_warm_cache_replay_over_real_corpus_wal`,
  `validate_recompute_matches_stored_vectors_for_real_corpus_wal`), the empirical basis for
  SC-001's 4,126/4,106 figures and for SC-002's regression-test expectations.
- Issue #440 (closed) — introduced the WAL-replay recompute path.
- Issue #445 (closed) — added `Embedder::embed_batch` and converted ingest/backfill call sites;
  this issue's prerequisite, now satisfied.
- Issue #526 (closed) — made recompute mandatory, renamed `embeddings_recompute_fallback` to
  `embeddings_recompute_skipped_no_text`, and added `embeddings_skip_rows` /
  `is_vector_only_set` — the current counter and fallback semantics this issue must preserve.
