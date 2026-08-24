# Feature Specification: Batch embedding API for entity/fact ingest and WAL replay

**Feature Branch**: `fabrik/issue-445`
**Created**: 2026-08-23
**Status**: Draft
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

This costs round-trips in two places:

1. **Bulk extraction ingest** (`episode.rs`). Phase A's entity-name and edge-fact embedding
   loops are strictly sequential — each name and each fact is embedded one at a time, even
   though the full lists (`entity_names`, `edge_facts`) are already materialized before the
   loop starts. Measured on the #217 real-corpus fixture (228 chunks): 1,506 entity names +
   2,392 edge facts = **3,898 serial round-trips, ~17 per chunk**. These embeddings are
   genuinely needed (they feed dedup via `hybrid_dedup_similar_entity`), so this is necessary
   work done one round-trip at a time, not wasted work. Impact here is modest in relative
   terms: LLM extraction dominates a chunk's wall-clock, so ~17 serial embed calls sit against
   a multi-second extraction call.

   Since this issue was originally filed, two more sequential-embed sites have appeared in the
   same function as part of unrelated work: a per-entity summary-embedding pass (issue #470,
   currently paired with the name embed via `tokio::try_join!` so each *entity* costs one
   round-trip pair, but entities are still processed one at a time) and a salvage-endpoint
   lookup loop (one embed call per off-batch edge endpoint name needing a cosine match against
   the batch's own entities). See the Open Questions below for whether these are in scope here.

2. **WAL replay** (issue #440, now merged). Replay has no LLM — embedding *is* the cost.
   #440 introduced a recompute path that derives each stored embedding vector fresh from its
   co-located source text at replay time, via a synchronous per-row callback:

   ```rust
   pub type RecomputeEmbedFn = Box<dyn Fn(&str) -> Result<Vec<f32>, Error> + Send>;
   ```

   — the same one-text-per-call shape, invoked once per WAL row inside `replay.rs`, which is
   deliberately kept decoupled from tokio/async. On the #217 fixture that is **4,126
   recomputes, of which 4,106 are distinct texts** (an in-memory cache absorbs only ~0.5%,
   because the WAL is post-dedup — there is almost nothing left to hit). Replay time is
   therefore approximately `4,106 / embedder throughput` regardless of the cache. Batching
   would collapse those round-trips to tens, but doing so means restructuring `replay.rs`'s
   row-by-row processing to buffer multiple rows' texts before issuing one batch call and
   correlating results back per row — a materially bigger change than the ingest-side
   conversion, where all texts are already collected into a `Vec` up front. See the Open
   Questions below for whether this is in scope here.

**Sequencing.** The original issue said this should wait for #440's FR-011 benchmark, since
that benchmark measures replay recompute cost directly and its number was meant to set this
issue's priority rather than guessing. #440 has since closed (merged as PR #443). However,
the FR-011 benchmark it shipped (`measure_recompute_overhead_over_real_corpus_wal` in
`crates/core/tests/real_corpus_replay_perf.rs`) is `#[ignore]`d and requires a live,
network-reachable embedder sidecar — it has not actually been run and no timing number has
been recorded anywhere. This spec proceeds on the structural argument laid out above (replay
does no LLM work, so embedding is close to the entire cost there) rather than a measured
multiplier. Actual embedder throughput per call remains unmeasured; wall-clock savings from
batching are expected but not quantified by this spec.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Batch embedding API on the `Embedder` trait (Priority: P1)

As a developer of any code path that already has a list of texts to embed, I can call one
method to embed all of them, instead of writing (or reusing) a loop that calls the
single-text `embed` method once per item.

**Why this priority**: Every other user story in this issue depends on this API existing.
Without it, there is nothing for the ingest or replay call sites to switch to.

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
   partial vector list — matching today's behavior, where a single `embed` failure already
   aborts whatever loop called it.
5. **Given** an empty list of texts, **When** the batch method is called, **Then** it returns
   an empty list of vectors without issuing any network call.

---

### User Story 2 - Bulk extraction ingest uses batched embedding (Priority: P1)

As the bulk-extraction ingest pipeline processing a chunk, I embed all of a chunk's entity
names in one batched call and all of a chunk's edge facts in one batched call, instead of one
call per name and one call per fact.

**Why this priority**: This is where the batch API pays off for the common, everyday path —
every chunk processed by ingest benefits, and this is explicitly the case the original issue
proposed converting.

**Independent Test**: Process a chunk with a known number of entities and edges through the
extraction pipeline with an embed-call-counting test embedder (the existing `CountingEmbedder`
pattern) and confirm the number of embed calls attributable to entity names and edge facts
drops from one-per-item to a small constant number per chunk (accounting for any chunking).

**Acceptance Scenarios**:

1. **Given** a chunk that extracts multiple entities, **When** their name embeddings are
   computed, **Then** exactly one batch call (or the minimum number of chunked sub-calls
   required by request-size limits) is issued for all entity names in that chunk, not one
   call per entity.
2. **Given** a chunk that extracts multiple edges, **When** their fact embeddings are
   computed, **Then** exactly one batch call (or the minimum number of chunked sub-calls) is
   issued for all edge facts in that chunk, not one call per edge.
3. **Given** the chunk's processing is cancelled mid-flight (via the existing cancellation
   token), **When** a batch embed call is in progress, **Then** the call is still abortable and
   the chunk is still counted as cancelled — cancellation behavior is unchanged from today's
   per-item loop.
4. **Given** dedup and edge-validation logic downstream of these embeddings
   (`hybrid_dedup_similar_entity`, endpoint salvage matching), **When** batched embedding
   replaces the sequential loops, **Then** the resulting vectors, their order, and everything
   downstream that consumes them are unaffected — this is a call-shape change only, not a
   behavior change.

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
- An embed call for one item in a batch fails: the entire batch call fails (User Story 1,
  Acceptance Scenario 4) — no per-item partial-success/failure reporting is introduced by this
  issue.

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
  result — matching today's single-call error propagation exactly.
- **FR-006**: Calling the batch method with an empty list of texts MUST return an empty list of
  vectors without issuing any network call.
- **FR-007**: The bulk-extraction ingest path's entity-name embedding loop and edge-fact
  embedding loop (`episode.rs`) MUST each be converted to issue one batch call (or the minimum
  number of chunked sub-calls) per chunk, instead of one call per name or per fact.
- **FR-008**: The ingest conversion in FR-007 MUST preserve existing behavior for everything
  downstream of the embeddings — dedup, edge-endpoint salvage matching, and cancellation via
  the chunk's cancellation token — so this is a call-shape change only.

### Key Entities

- **`Embedder` trait** (`crates/core/src/embedder.rs`): the abstraction gaining the new batch
  method; existing implementors are `OaiEmbedder`, `MockEmbedder`, `NameMapEmbedder`, and the
  test-only `CountingEmbedder` wrapper.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On the #217 real-corpus fixture (228 chunks; 1,506 entity names, 2,392 edge
  facts), the number of distinct embedder round-trips issued during bulk-extraction ingest is
  reduced by at least 90% relative to today's one-call-per-name/fact baseline of 3,898
  round-trips (accounting for any chunking of oversized batches).
- **SC-002**: Every existing `Embedder` implementor's existing test suite continues to pass
  unmodified — the new batch method introduces no behavior change for code that has not been
  converted to call it.
- **SC-003**: A batch call where one input fails surfaces as exactly one error to the caller,
  never a partial or silently-incomplete vector list.

## Assumptions

- The exact chunk size / per-request limit for `OaiEmbedder`'s batching is a Research/Plan-stage
  decision, informed by the embedding endpoint's actual limits (not yet measured). This spec
  requires only that batching transparently respects whatever limit is discovered (FR-004).
- Batch calls fail atomically (FR-005): this matches current per-call error propagation, where
  a single `embed` failure already aborts whichever loop called it. No per-item partial-failure
  API is introduced.
- The dependency issue #440 has closed (merged as PR #443), satisfying the original issue's
  sequencing gate, but the FR-011 benchmark that gate was meant to use for prioritization was
  never actually run with a live embedder — see Background. This issue proceeds without a
  measured wall-clock multiplier.

## Out of Scope

- Deduplicating identical texts within a batch call — WAL replay's existing embedding cache
  (from #440) already addresses redundant text within a replay session; this issue only
  changes the transport shape (array vs. single string) of the calls that remain.
- Changes to the embedder's HTTP/UDS transport mechanics (connection pooling, retry-on-broken-
  connection logic) beyond what's needed to send an array-valued request instead of a
  single-string one.
- Issue #444 (a different defect: work that is discarded, not work done inefficiently).

## Source References

- `crates/core/src/embedder.rs` — `Embedder` trait, `OaiEmbedder`, `MockEmbedder`,
  `NameMapEmbedder`, `CountingEmbedder`.
- `crates/core/src/episode.rs` — bulk-extraction ingest's entity-name and edge-fact embedding
  loops (Phase A).
- `crates/core/src/replay.rs` — WAL replay's `RecomputeEmbedFn` / `ReplayOptions` (issue #440).
- Issue #440 (closed, PR #443) — introduced the replay recompute path.
- Issue #444 — the assert-path waste found in the same investigation (different defect).
