# ADR-0445: Embedder Batch API — Wire-Level Batching, Index-Ordered Reassembly, Chunk-Size Knob

**Status**: Accepted
**Date**: 2026-08-23
**Issue**: #445
**Relates to**: ADR-0006 (HTTP embedding sidecar contract), ADR-0016 (OpenAI-compatible embedding
contract / UDS transport), ADR-0030 (batched write-lock for long-running passes), ADR-0470
(entity summary embedding)

## Context

`Embedder` had no batch method: every embedding — entity name, entity summary, salvage-endpoint
lookup, edge fact, and `knowledge_backfill_summary_embeddings`'s per-candidate summary — was one
`embed()` call, and therefore one HTTP/UDS round-trip, even where the caller already held the
full list of texts up front. The OpenAI-compatible `/v1/embeddings` endpoint `OaiEmbedder` talks
to accepts an array-valued `input`; the codebase never exercised that capability.

Four decisions were not obvious from the spec alone and are recorded here.

## Decision 1: A default trait method expressed in terms of `embed`, run concurrently

**Chosen**: `Embedder::embed_batch` ships as a default method on the trait itself, implemented as
`futures::future::try_join_all(texts.iter().map(|t| self.embed(t)))` — every existing implementor
(`MockEmbedder`, `HashEmbedder`, `NameMapEmbedder`, the test-only `CountingEmbedder`) compiles and
behaves identically without any code change (FR-002), and `OaiEmbedder` overrides it with genuine
wire-level batching (Decision 2).

**Rejected**: a sequential `for text in texts { self.embed(text).await? }` default.

**Rationale**: FR-002 only requires order-preserving parity with N sequential `embed()` calls —
both a sequential and a concurrent default satisfy that letter. But a sequential default is a
silent trap: it compiles, passes every test, and *looks* like an optimization was applied at any
call site converted to use it, while actually issuing exactly as many round-trips as before with
added latency from awaiting them one at a time instead of concurrently. Running the default
concurrently means "an implementor forgot to override `embed_batch`" degrades to "no wire-level
batching, but no worse than before" rather than "a batching bug quietly consuming the accounting
this issue's SC-001/SC-004 exist to prove."

## Decision 2: Wire format extends `input` via an untagged enum, not by unifying on `embed_batch(&[text])`

**Chosen**: `OaiEmbedRequest.input` becomes `OaiEmbedInput { Single(&str), Batch(&[&str]) }`
(`#[serde(untagged)]`), so a single-item request still serializes byte-for-byte as
`{"input": "text", ...}` — never `{"input": ["text"], ...}` — while a batch call serializes as
`{"input": ["t1", "t2", ...], ...}`. `OaiEmbedder::embed()` keeps its own hand-written
implementation calling `do_embed_raw(OaiEmbedInput::Single(text))`; it is not reimplemented on top
of `embed_batch(&[text])`.

**Rejected**: always sending an array (`input: [text]`) even for a single text, or routing
`embed()` through `embed_batch` internally.

**Rationale**: the single-item wire shape is exactly what every existing test fixture, cassette
recording (ADR-0044), and — more importantly — any real OpenAI-compatible sidecar already expects
and has been validated against since ADR-0006/ADR-0016. Changing it for every call, even
single-item ones, would be an unforced, unrelated risk to a code path this issue doesn't need to
touch (Out of Scope: "changes to transport mechanics beyond what's needed to send an array-valued
request instead of a single-string one"). The untagged enum shares `do_embed_http_raw`/
`do_embed_uds_raw` between both shapes — only request-body construction differs — so nothing about
connection pooling, retry-on-broken-connection, or the two-transport split changes.

## Decision 3: Batch responses are reassembled by the response's `index` field, not by trusting `data` array order

**Chosen**: `OaiEmbedding` gains an `index: usize` field (already part of ADR-0016's documented
contract, previously deserialized-but-unused). `extract_embeddings_ordered` places each response
entry at `slots[entry.index]` and errors — a single `Error::Ipc`, not a partial result — on a
wrong entry count, an out-of-range or duplicate index, or a still-`None` slot after every entry is
placed.

**Rejected**: returning `resp.data` in received array order.

**Rationale**: trusting array order is simpler and matches every real OpenAI-compatible server's
actual behavior today, but it has no defense against a nonconforming server (or a future bug in
one) that reorders `data` — which would silently misassign entity B's vector to entity A with no
error, corrupting dedup (`hybrid_dedup_similar_entity` depends on the right vector landing on the
right name) and salvage matching with no observable symptom until much later. Validating by
`index` costs nothing beyond the field already being on the wire, and turns that failure class
into FR-005's atomic batch error instead of silent corruption.

## Decision 4: A dedicated, escape-hatch env var for the chunk size, not a hardcoded constant

**Chosen**: `LCG_EMBED_BATCH_SIZE` (default 64, valid range 1–256, `Error::Config` on an invalid
or out-of-range value) — `resolve_embed_batch_size()` mirrors `replay.rs::resolve_batch_size`'s
exact validation shape (same default, same range, same error style) for repo consistency, even
though the two knobs bound unrelated resources (HTTP request payload size here vs. WAL-row
prepare-statement batching there).

**Rejected**: a hardcoded chunk-size constant with no override.

**Rationale**: the real embedding sidecar's actual per-request size limit is unmeasured (spec
Assumptions — FR-004 only requires that *some* limit is transparently respected, not a specific
one). A conservative default with an operator-tunable override is safer than shipping an unverified
guess with no way to correct it in production without a code change and redeploy.

## Consequences

- `Embedder` gains `embed_batch`; every existing implementor keeps compiling with no code change
  (SC-002). `OaiEmbedder` is the only implementor with real wire-level batching; test embedders
  rely on the concurrent default.
- `episode.rs`'s four Phase A sequential-embed sites (entity names, per-entity summaries, the
  salvage-endpoint lookup, edge facts) and `knowledge_backfill_summary_embeddings`'s Phase C write
  loop are converted to call `embed_batch` once per chunk (FR-007, FR-009) — see the two
  non-mechanical conversions below.
- **Two conversions are not a mechanical "collect the list, batch it, zip results back"**:
  - `episode.rs`'s summary-embedding pass excludes empty summaries from the batch entirely (per
    ADR-0314, an empty summary is never sent to the embedder) and reinserts the same-dimension
    zero-vector sentinel at each skipped entity's original index afterward.
  - The salvage-endpoint lookup's `missing_names` was, and remains, collected as a `HashMap`
    (dedup, keyed by normalized name) before batching, but is converted to a stable-ordered `Vec`
    *before* the batch call so the batch's dense, submission-order output can be zipped back to
    the correct key by position — `HashMap` iteration order is unspecified and would otherwise
    risk misassigning an embedding to the wrong endpoint name.
- `CountingEmbedder` (the shared `embedder.rs` test double) gains a second counter,
  `batch_call_count()`, via an explicit `embed_batch` override — without it, the wrapper would
  fall through to the trait's default (which calls `self.embed()` per item internally), silently
  counting per-item calls under a name that implies per-round-trip counting and making this
  issue's own acceptance tests impossible to write correctly. `call_count()` (tracking `embed()`)
  is unchanged, since existing tests (e.g. `empty_summary_entity_skips_embedder_call_for_summary`)
  depend on its current semantics. The separate, locally-defined `CountingEmbedder` in
  `real_corpus_e2e.rs` is deliberately untouched — that file exercises only
  `knowledge_rebuild_from_wal` (pure WAL replay), never `add_episode`, so nothing there calls
  `embed_batch`.
- **WAL-replay embedding recompute (`replay.rs`'s `RecomputeEmbedFn`, from ADR-0440) is
  untouched and explicitly out of scope**, split into issue #486 for risk isolation — not because
  it lacks value; it is in fact the single largest remaining source of embed calls in the system
  (see #445's spec Background). `replay.rs` has its own synchronous, non-async callback,
  deliberately decoupled from `tokio`/the `Embedder` trait, and batching it means restructuring
  row-by-row replay control flow on the path that turns the WAL — the source of truth — back into
  a graph. Stacking that restructure on top of ADR-0440's very recent changes to the same file, in
  the same release window, was judged to compound two hard-to-attribute risks; #486 tracks it
  separately with the reasoning preserved so it isn't re-derived (and doesn't get silently
  deprioritized as "just leftovers").
- No IPC/MCP protocol surface change — `embed_batch` is an internal Rust trait addition with no
  new `knowledge_*` dispatch method and no wire-visible shape change to any existing one.
