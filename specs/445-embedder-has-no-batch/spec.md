# Feature Specification: Batch embedding API for bulk-extraction ingest and summary-embedding backfill

**Feature Branch**: `fabrik/issue-445`
**Created**: 2026-08-23
**Status**: Specified
**Input**: User description: "Embedder has no batch API: entity/fact embedding is one round-trip per text at ingest and on WAL replay"

## Background

`Embedder` has no batch API. Every embedding is a separate call, and therefore a separate
HTTP/UDS round-trip, even where the texts are all available up front:

```rust
// crates/core/src/embedder.rs:198
pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, Error>>;
    fn dim(&self) -> usize { 768 }
}
```

`OaiEmbedder` serialises `input: &'a str` — a bare string — when talking to the
OpenAI-compatible `POST /v1/embeddings` endpoint it targets. That endpoint **accepts an
array of inputs**; the codebase never uses that capability.

This costs round-trips in three places. Two are in scope for this issue; the third is not
(see below).

1. **Bulk extraction ingest** (`episode.rs`). Phase A has four sequential-embed sites, all
   working from lists that are fully materialized before their loop starts:
   - The entity-name embedding loop and the edge-fact embedding loop — the two loops the
     original issue named. Measured on the #217 real-corpus fixture (228 chunks): 1,506
     entity names + 2,392 edge facts = **3,898 serial round-trips, ~17 per chunk**.
   - A per-entity summary-embedding pass (issue #470), currently paired with the name embed
     via `tokio::try_join!` so each *entity* costs one round-trip pair rather than two
     sequential round-trips — but entities are still processed one at a time, so this is also
     a serial loop, just of pairs instead of singles.
   - A salvage-endpoint lookup loop: one embed call per off-batch edge endpoint name that
     needs a cosine match against the batch's own entities. All the names needing lookup are
     collected into a map before this loop starts, and each lookup is independent of every
     other (it only compares against the batch's already-computed entity name embeddings) —
     so despite reading as a one-off lookup, it is exactly as batchable as the other three.

   These embeddings are genuinely needed — they feed dedup via `hybrid_dedup_similar_entity`
   and downstream persistence — so this is necessary work done one round-trip at a time, not
   wasted work. Impact here is modest in relative terms: LLM extraction dominates a chunk's
   wall-clock, so a chunk's serial embed calls sit against a multi-second extraction call.

2. **`knowledge_backfill_summary_embeddings`** (issue #470) — an operator-invoked admin tool
   that backfills `summary_embedding` for every `Entity` in a group with a non-empty summary,
   for entities created before that field existed. This is the largest single-run cost in the
   system, and the strongest case for batching, for reasons beyond raw round-trip count: a
   real (non-dry-run) invocation acquires the exclusive write lock *before* reading candidates
   and holds it continuously through the write phase, and every candidate is unconditionally
   re-embedded on every run (no "already embedded" skip exists). The embedding calls happen
   one candidate at a time inside 100-candidate write chunks
   (`crates/core/src/backfill_summary_embeddings.rs`, `WRITE_BATCH_SIZE`), all while every
   other read and write in the service is blocked. Batching here shortens that blocked window
   directly — this is an operational availability improvement, not just a throughput one.

3. **WAL replay** (issue #440, now merged) is **not** part of this issue. Replay has no LLM —
   embedding *is* the cost there, and it is in fact the single largest source of embed calls
   anywhere in the system (4,126 recomputes on the #217 fixture, 4,106 of them distinct
   texts — more than the entire ingest path above). #440's recompute path derives each stored
   embedding vector fresh from its co-located source text at replay time via a synchronous
   per-row callback (`RecomputeEmbedFn = Box<dyn Fn(&str) -> Result<Vec<f32>, Error> + Send>`),
   invoked once per WAL row inside `replay.rs`, which is deliberately kept decoupled from
   tokio/async. Batching it means restructuring that row-by-row processing to buffer multiple
   rows' texts before issuing one batch call and correlating results back per row — a
   materially bigger structural change than the ingest-side conversions above, on the path
   that turns the WAL (the source of truth) back into a graph. It is excluded here **for risk
   isolation, not for lack of value** — #440 landed on `main` very recently and already
   changed replay's behavior materially, and stacking a control-flow restructure of the same
   file on top of that in the same release window would make the two hard to attribute
   independently if something went wrong. It has been filed separately as issue #486, which
   also documents (and corrects) an easy mistake: #440's in-memory embedding cache does **not**
   make replay's embedding cost small. That cache is never persisted, replay overwhelmingly
   runs against a cold cache (at startup or during recovery), and the WAL is post-dedup, so
   there is almost nothing for the cache to hit (#440 measured ~0.5%). Replay cost is
   therefore approximately `distinct texts / embedder throughput` regardless of the cache —
   see #486 for the full reasoning.

**Sequencing.** The original issue said work here should wait for #440's FR-011 benchmark,
since that benchmark measures replay recompute cost directly and its number was meant to set
priority rather than guessing. #440 has since closed (merged as PR #443), and the WAL-replay
half of this issue's original scope — the case that benchmark was about — has been split into
#486 as described above. The FR-011 benchmark itself
(`measure_recompute_overhead_over_real_corpus_wal` in
`crates/core/tests/real_corpus_replay_perf.rs`) is `#[ignore]`d and requires a live,
network-reachable embedder sidecar; it has not actually been run and no timing number has been
recorded anywhere. This issue's remaining scope (ingest and the backfill tool) proceeds on the
structural argument above rather than a measured multiplier. Actual embedder throughput per
call remains unmeasured; wall-clock savings from batching are expected but not quantified by
this spec.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Batch embedding API on the `Embedder` trait (Priority: P1)

As a developer of any code path that already has a list of texts to embed, I can call one
method to embed all of them, instead of writing (or reusing) a loop that calls the
single-text `embed` method once per item.

**Why this priority**: Every other user story in this issue depends on this API existing.
Without it, there is nothing for the ingest or backfill call sites to switch to.

**Independent Test**: Call the new batch method with a list of N texts against `MockEmbedder`
(or any existing test embedder) and confirm it returns N vectors, in input order, matching
what N sequential calls to `embed` would have returned — without any implementor needing code
changes.

**Acceptance Scenarios**:

1. **Given** an `Embedder` implementor that only defines the existing single-text `embed`
   method, **When** its batch method is called with several texts, **Then** it returns the
   same vectors, in the same order, as calling `embed` once per text would have — via a
   default trait-level implementation the implementor did not have to write.
2. **Given** `OaiEmbedder` configured against a live or mock OpenAI-compatible endpoint,
   **When** its batch method is called with several texts, **Then** exactly one HTTP/UDS
   request is sent carrying all texts as an array input (not one request per text).
3. **Given** a batch of texts whose combined size exceeds the embedding endpoint's per-request
   limit, **When** `OaiEmbedder`'s batch method is called, **Then** it transparently issues
   multiple sub-requests and returns the full, correctly-ordered set of vectors — the caller
   does not need to know chunking happened.
4. **Given** a batch where the embedder call fails (endpoint unreachable, bad response, etc.),
   **When** the batch method is called, **Then** the whole call returns a single error and no
   partial vector list — matching today's behavior for every loop this issue converts, where a
   single `embed` failure already aborts whatever loop called it (see Edge Cases).
5. **Given** an empty list of texts, **When** the batch method is called, **Then** it returns
   an empty list of vectors without issuing any network call.

---

### User Story 2 - Bulk extraction ingest uses batched embedding (Priority: P1)

As the bulk-extraction ingest pipeline processing a chunk, I embed all of a chunk's entity
names, entity summaries, salvage-endpoint lookups, and edge facts each in one batched call,
instead of one call per item.

**Why this priority**: This is where the batch API pays off for the common, everyday path —
every chunk processed by ingest benefits, and this is explicitly the case the original issue
proposed converting.

**Independent Test**: Process a chunk with a known number of entities and edges through the
extraction pipeline with an embed-call-counting test embedder (the existing `CountingEmbedder`
pattern) and confirm the number of embed calls attributable to each of the four sites drops
from one-per-item to a small constant number per chunk (accounting for any chunking).

**Acceptance Scenarios**:

1. **Given** a chunk that extracts multiple entities, **When** their name embeddings are
   computed, **Then** exactly one batch call (or the minimum number of chunked sub-calls
   required by request-size limits) is issued for all entity names in that chunk, not one
   call per entity.
2. **Given** a chunk that extracts multiple entities with non-empty summaries, **When** their
   summary embeddings are computed, **Then** exactly one batch call is issued for all
   non-empty summaries in that chunk, run concurrently with the entity-name batch call (as
   today's per-entity `tokio::try_join!` already does per pair, now done once per chunk for
   both lists at once) rather than sequentially.
3. **Given** a chunk whose edges reference endpoint names absent from the chunk's own entity
   list, **When** those off-batch endpoint names are looked up for salvage matching, **Then**
   exactly one batch call is issued for all of that chunk's missing endpoint names, not one
   call per missing name.
4. **Given** a chunk that extracts multiple edges, **When** their fact embeddings are
   computed, **Then** exactly one batch call (or the minimum number of chunked sub-calls) is
   issued for all edge facts in that chunk, not one call per edge.
5. **Given** the chunk's processing is cancelled mid-flight (via the existing cancellation
   token), **When** a batch embed call is in progress, **Then** the call is still abortable and
   the chunk is still counted as cancelled — cancellation behavior is unchanged from today's
   per-item loops.
6. **Given** dedup and edge-validation logic downstream of these embeddings
   (`hybrid_dedup_similar_entity`, endpoint salvage matching), **When** batched embedding
   replaces the sequential loops, **Then** the resulting vectors, their order, and everything
   downstream that consumes them are unaffected — this is a call-shape change only, not a
   behavior change.

---

### User Story 3 - `knowledge_backfill_summary_embeddings` uses batched embedding (Priority: P1)

As an operator running the summary-embedding backfill tool against a group with many
entities, the tool embeds each write-chunk of candidate summaries in one batched call instead
of one call per candidate, so the exclusive write-lock window the tool holds for the whole run
is shorter.

**Why this priority**: This is the single largest embedding-round-trip cost of anything in
scope for this issue, and the only one where the cost is also felt operationally — every other
read and write in the service is blocked for the tool's full duration.

**Independent Test**: Run the backfill against a seeded group with a known number of
non-empty-summary entities using an embed-call-counting test embedder, and confirm the number
of embed calls drops from one-per-candidate to one-per-write-chunk (100 candidates per chunk,
per today's `WRITE_BATCH_SIZE`).

**Acceptance Scenarios**:

1. **Given** a non-dry-run backfill with N candidate entities, **When** Phase C writes
   embeddings in its existing 100-candidate write chunks, **Then** each chunk issues one batch
   embed call for its candidates' summaries instead of one call per candidate.
2. **Given** the same backfill, **When** batching is used, **Then** every other existing
   guarantee is unchanged: the exclusive write lock is still held from Phase A through Phase
   C's index rebuild, every candidate is still unconditionally re-embedded on every run (no
   new "already embedded" skip is introduced), the vector index is still dropped before
   writing and rebuilt after, and each write chunk still flushes its own WAL entry.
3. **Given** a `dry_run` invocation, **When** the backfill runs, **Then** behavior is unchanged
   — Phase C (and therefore batching) is never reached, matching today.

---

### Edge Cases

- A batch containing duplicate texts (e.g., two entities that happen to share a name): each
  position gets its own embedded vector; this issue does not add text-level deduplication
  within a batch call (that is a separate concern from batching the transport).
- A single-item batch behaves identically to calling `embed` directly — no added round-trip
  and no different error behavior.
- A batch larger than the endpoint's per-request limit is chunked transparently (User Story 1,
  Acceptance Scenario 3); the exact limit and chunk size are a Research/Plan-stage decision,
  since the endpoint's actual limits have not been measured.
- **Failure semantics introduce no regression on any path this issue touches.** Every loop
  converted by this issue (`episode.rs`'s four Phase A sites, and
  `knowledge_backfill_summary_embeddings`'s Phase C loop) already fails atomically today: each
  uses `?`/`.await?` to propagate the first embed error and abort, with no existing per-item
  fallback. FR-005's atomic-batch-failure behavior matches this exactly. This is distinct from
  the single-item `embed` calls inside `knowledge_assert_entity` and
  `knowledge_assert_relationship`'s handlers, which *do* degrade a failed embed to a
  same-dimension zero vector with a warning surfaced in the response — those call sites are
  unaffected by this issue, since each handles exactly one text per call and has nothing to
  batch.
- The salvage-endpoint lookup loop's batchability was not obvious from the original issue
  text (it reads like a one-off lookup) — it is batchable because all missing names are
  collected into a map before the loop starts, and each lookup only compares against the
  batch's own already-computed entity embeddings, with no dependency on any other lookup's
  result.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `Embedder` trait MUST provide a method to embed multiple texts in a single
  logical call, returning one vector per input text in the same order as the input.
- **FR-002**: A default implementation of that method MUST be provided on the trait itself,
  implemented in terms of the existing single-text `embed` method, so that every existing
  implementor (`MockEmbedder`, `NameMapEmbedder`, `CountingEmbedder`) continues to compile and
  behave identically without any code change.
- **FR-003**: `OaiEmbedder` MUST override the batch method to send all texts in a batch as a
  single array-valued `input` in one request to the OpenAI-compatible `/v1/embeddings`
  endpoint, over whichever transport (HTTP or UDS) is configured, rather than looping over
  single-text requests.
- **FR-004**: When a batch's size would exceed a request-size limit imposed by the embedding
  endpoint, `OaiEmbedder`'s batch implementation MUST transparently split the batch into
  multiple sub-requests and reassemble the results in original input order. Callers MUST NOT
  need to handle chunking themselves.
- **FR-005**: If any input in a batch call fails to embed (including within any one chunk of a
  multi-chunk batch), the entire batch call MUST return a single error rather than a partial
  result. This matches the existing, already-atomic failure behavior of every call site this
  issue converts (see Edge Cases); it does not change the separate degrade-to-zero-vector
  behavior of the out-of-scope single-item assert handlers.
- **FR-006**: Calling the batch method with an empty list of texts MUST return an empty list of
  vectors without issuing any network call.
- **FR-007**: `episode.rs`'s four Phase A sequential-embed sites — the entity-name embedding
  loop, the per-entity summary-embedding pass, the salvage-endpoint lookup loop, and the
  edge-fact embedding loop — MUST each be converted to issue one batch call (or the minimum
  number of chunked sub-calls) per chunk, instead of one call per item.
- **FR-008**: The ingest conversion in FR-007 MUST preserve existing behavior for everything
  downstream of the embeddings — dedup, edge-endpoint salvage matching, and cancellation via
  the chunk's cancellation token — so this is a call-shape change only.
- **FR-009**: `knowledge_backfill_summary_embeddings`'s Phase C write loop MUST issue one batch
  embed call per existing 100-candidate write chunk, instead of one call per candidate.
- **FR-010**: The backfill conversion in FR-009 MUST preserve every other existing guarantee
  unchanged: the exclusive write-lock hold spanning Phase A through Phase C's index rebuild,
  unconditional re-embedding of every candidate on every run, the vector-index drop/rebuild
  ordering, and each write chunk's own WAL flush.

### Key Entities

- **`Embedder` trait** (`crates/core/src/embedder.rs`): the abstraction gaining the new batch
  method; existing implementors are `OaiEmbedder`, `MockEmbedder`, `NameMapEmbedder`, and the
  test-only `CountingEmbedder` wrapper.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On the #217 real-corpus fixture (228 chunks; 1,506 entity names, 2,392 edge
  facts), the number of distinct embedder round-trips issued during bulk-extraction ingest for
  entity names and edge facts is reduced by at least 90% relative to today's
  one-call-per-name/fact baseline of 3,898 round-trips (accounting for any chunking of
  oversized batches).
- **SC-002**: Every existing `Embedder` implementor's existing test suite continues to pass
  unmodified — the new batch method introduces no behavior change for code that has not been
  converted to call it.
- **SC-003**: A batch call where one input fails surfaces as exactly one error to the caller,
  never a partial or silently-incomplete vector list.
- **SC-004**: For a `knowledge_backfill_summary_embeddings` run backfilling N candidate
  entities, the number of embedder round-trips issued during Phase C is reduced from N to at
  most ⌈N / 100⌉ (accounting for any further chunking), shortening the exclusive write-lock
  hold time proportionally.

## Assumptions

- The exact chunk size / per-request limit for `OaiEmbedder`'s batching is a Research/Plan-stage
  decision, informed by the embedding endpoint's actual limits (not yet measured). This spec
  requires only that batching transparently respects whatever limit is discovered (FR-004).
- Batch calls fail atomically (FR-005): this matches current per-call error propagation on
  every path this issue converts, where a single `embed` failure already aborts whichever loop
  called it. No per-item partial-failure API is introduced.
- The dependency issue #440 has closed (merged as PR #443). The WAL-replay half of the
  original issue's scope — the part that #440's FR-011 benchmark was meant to prioritize — has
  been split into issue #486 for risk-isolation reasons documented in Background; that
  benchmark was never actually run with a live embedder, and this issue's remaining scope does
  not depend on its result.

## Out of Scope

- **WAL-replay embedding recompute batching** (issue #440's `RecomputeEmbedFn` path in
  `replay.rs`) — split into issue #486. Excluded for risk isolation (a fresh control-flow
  restructure of the WAL-rebuild path stacked on #440's recent changes to the same file), not
  because it lacks value — see Background and #486 for the full reasoning, including why
  #440's embedding cache does not make this low-value.
- The single-item `embed` calls inside `knowledge_assert_entity` and
  `knowledge_assert_relationship`'s handlers — each embeds exactly one text per call, so there
  is nothing to batch, and their existing degrade-to-zero-vector-with-warning behavior on
  failure is unrelated to and unaffected by this issue.
- Deduplicating identical texts within a batch call. This issue's batch API embeds one vector
  per input position regardless of duplicates within that batch; text-level dedup (as WAL
  replay's own in-memory cache already does for repeated text within a replay session, per
  #440) is a separate concern from batching the transport.
- Changes to the embedder's HTTP/UDS transport mechanics (connection pooling, retry-on-broken-
  connection logic) beyond what's needed to send an array-valued request instead of a
  single-string one.
- Issue #444 (a different defect: work that is discarded, not work done inefficiently).

## Source References

- `crates/core/src/embedder.rs` — `Embedder` trait, `OaiEmbedder`, `MockEmbedder`,
  `NameMapEmbedder`, `CountingEmbedder`.
- `crates/core/src/episode.rs` — bulk-extraction ingest's four Phase A sequential-embed sites.
- `crates/core/src/backfill_summary_embeddings.rs` — `knowledge_backfill_summary_embeddings`'s
  three-phase implementation, including `WRITE_BATCH_SIZE`.
- `crates/core/src/handlers.rs` — the out-of-scope single-item degrade-on-failure pattern in
  the assert-entity/assert-relationship handlers, for contrast.
- `crates/core/src/replay.rs` — WAL replay's `RecomputeEmbedFn` / `ReplayOptions` (issue #440),
  out of scope; see issue #486.
- Issue #440 (closed, PR #443) — introduced the WAL-replay recompute path.
- Issue #444 — the assert-path waste found in the same investigation (different defect).
- Issue #486 — WAL-replay embedding recompute batching, split from this issue.
