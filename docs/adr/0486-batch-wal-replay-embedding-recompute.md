# ADR-0486: Batch WAL-Replay Embedding Recompute — An Independent, Unaligned Window Upstream of the Cypher Batch

**Status**: Accepted
**Date**: 2026-09-01
**Issue**: #486
**Relates to**: ADR-0440 (recompute embeddings on WAL replay, sync bridge), ADR-0445 (embedder
batch API — explicitly split WAL-replay recompute out as this issue), ADR-0526 (vectors are a
local cache — mandatory recompute, `is_vector_only_set`), ADR-0047 (WAL replay transaction
boundaries), ADR-0045 (prepared-statement cache scope)

## Context

`replay.rs`'s recompute path (ADR-0440, made mandatory by ADR-0526) recomputed one embedding
vector at a time: `recompute_row_embeddings` ran inline, once per WAL row, invoking a single-text
`RecomputeEmbedFn` for each recognized vector placeholder. On the #217 real-corpus fixture this
issues 4,126 embedder round-trips for 4,106 distinct texts — the single largest source of embed
calls anywhere in the system, larger than the entire ingest path ADR-0445 already converted to
`Embedder::embed_batch`. ADR-0445 deliberately left this path untouched, splitting it into this
issue for risk isolation.

Four decisions were not obvious from the spec alone and are recorded here.

## Decision 1: A second, independent buffering window — not reuse of the existing Cypher execution batch

**Chosen**: A new `pending_window: Vec<PendingRow>` buffers normalized-but-not-yet-recomputed rows
as WAL lines are read, sized by a new, independent `embed_window_size` (FR-009). Once full (or at
EOF, or dropped on cancellation), `resolve_embed_window` issues one batched `RecomputeEmbedFn`
call for the whole window, then only the surviving, resolved rows are handed to the existing,
**unmodified** `push_resolved_rows`/`flush_batch` pipeline — the same template-change/size-flush
logic the Cypher `UNWIND` execution batch (ADR-0047, issues #238/#240) already used.

**Rejected**: coupling the embedding window to `ReplayOptions.batch_size`/`LCG_REPLAY_BATCH_SIZE`,
or recomputing embeddings inside `flush_batch` itself.

**Rationale**: the two batching concepts bound genuinely different resources — network round-trips
to an embedder vs. same-template Cypher execution grouped into one transaction — and the spec
(FR-009) explicitly permits them to have unrelated size or boundaries. Coupling them would force a
choice that's wrong for one side no matter which value wins: a `batch_size` tuned for transaction
granularity (default 64, informed by memory/prepared-statement concerns) has no principled
relationship to how many embedder round-trips are efficient, and `OaiEmbedder::do_embed_batch`
already re-chunks internally at its own `LCG_EMBED_BATCH_SIZE`. Recomputing inside `flush_batch`
was rejected even more directly by FR-010/ADR-0047: it would reintroduce a slow-or-unreachable
embedder call inside an open lbug transaction, the exact hazard ADR-0047's transaction-boundary
design exists to prevent. Keeping the window as a distinct, temporally-upstream buffer makes
FR-010 hold *by construction* — a window is always fully resolved before any of its rows ever
reach `ReplayBatch`.

## Decision 2: The embedding window is not flushed at WAL-file boundaries

**Chosen**: Unlike the Cypher execution batch (which already flushes at each file boundary),
`pending_window` persists across files — it is resolved only when full, at EOF across the whole
run, or dropped on cancellation.

**Rejected**: flushing the pending embedding window at every file boundary, mirroring the Cypher
batch's existing behavior.

**Rationale**: maximizing the window's fill rate is what actually delivers SC-001's round-trip
reduction — a typical multi-file WAL directory has far more files than `embed_window_size`'s
default of 64, so a per-file flush would frequently emit small, inefficient windows for no
correctness benefit. FR-009 explicitly permits the two windows to have independent boundaries, and
nothing about embedding recompute cares which file a row's source text came from.

## Decision 3: Cancellation drops the pending window rather than flushing it

**Chosen**: The existing per-row `cancel_fn` check (unchanged position, right after a row is
buffered) gains one action on a positive cancellation: `pending_window.clear()` before
`break 'files`. Every `ReplayStats` embedding counter mutation happens *inside*
`resolve_embed_window`, which — being an ordinary synchronous function call, never itself
interrupted — either runs to full completion or is never invoked at all for a given window. A
dropped window has therefore touched zero embedding counters for its buffered rows.

**Rejected**: resolving (flushing) the pending window before honoring cancellation, so no buffered
row's recompute work is "wasted."

**Rationale**: this is the most direct reading of the spec's own edge case — cancellation "is not
forced to wait for an arbitrarily large in-flight batch window to finish first" (FR-008). Flushing
on cancellation would make cancellation latency scale with `embed_window_size` (up to 256 rows'
worth of embedder round-trip time), regressing today's per-row bound. Dropping is safe: those rows
were never executed against the database (dropped before `push_resolved_rows`), so a resumed
replay simply re-reads and re-buffers them from `last_committed_seq` onward, the same recovery
model already used for any other interrupted replay.

## Decision 4: `embed_calls` increments at request-queue time inside window resolution, not at buffer time

**Chosen**: `resolve_embed_window`'s Phase 1 increments `stats.embed_calls` once per `(row,
vec_key)` request as it's queued for the window's single batch call — still inside
`resolve_embed_window`, never in the main buffering loop.

**Rejected**: incrementing `embed_calls` when a row is first buffered into `pending_window`, before
its window is resolved.

**Rationale**: this is what makes Decision 3's cancellation-safety free of an orphaned-count bug.
Since every embedding counter mutation lives inside `resolve_embed_window`, and a window is only
ever passed to that function once it's about to be fully resolved, there is no code path that
increments a counter for a row whose window later gets dropped. The counter's meaning also does
not change (FR-002): `embed_calls` still means "one increment per `(row, vec_key)` request
attempted," matching its pre-#486 semantics — the difference is purely how many physical
`RecomputeEmbedFn` invocations later back that same count (many, pre-#486; one per window, now).

## Consequences

- `RecomputeEmbedFn`'s `Fn` signature changes from `Fn(&str) -> Result<Vec<f32>, Error>` to
  `Fn(&[&str]) -> Result<Vec<Vec<f32>>, Error>`, mirroring `Embedder::embed_batch`'s own
  all-or-nothing shape (FR-001). The type's *name*, and every production call site's name
  (`ReplayOptions.recompute_embed_fn`, `EmbedderContext::recompute_fn_via_handle`,
  `Db::open_or_rebuild`'s `build_sync_recompute_fn`, `zero_vector_embed_fn`), stay unchanged —
  only bodies and inner closure shapes change, which kept the ~100+ test call sites that only ever
  call `zero_vector_embed_fn(dim)` compiling with zero edits.
- `EmbeddingCache` gains `get_or_compute_batch`, deduplicating a window's cache-miss texts
  (first-occurrence order) before issuing one `compute_batch` call — this is where FR-006's
  within-window dedup guarantee is satisfied; `replay.rs` itself stays cache-agnostic and submits a
  window's texts as-is, possibly with duplicates.
- A whole-window batch-call failure (`Err`, or a returned `Vec` whose length doesn't match the
  request) degrades every request in that window through the *same* per-request fallback path a
  single failed text hit pre-#486 (FR-004) — there is no separate "batch failure" branch, only a
  different (batched) source feeding the existing per-request `Option<Vec<f32>>` resolution,
  including `is_vector_only_set`'s independent per-row skip-vs-zero-fill decision (FR-003).
  **Accepted tradeoff, raised in review**: this is a deliberate, spec-mandated widening of
  per-failure blast radius, not an oversight — pre-#486 a transient embedder failure (a dropped
  connection, a rate limit, a timeout) degraded only the one row being recomputed at that moment;
  post-#486 the same transient failure zero-fills/skips every row queued in the current window (up
  to `embed_window_size`, default 64, max 256), since `Embedder::embed_batch`'s all-or-nothing
  contract gives `resolve_embed_window` no finer-grained signal to act on. This is exactly the cost
  FR-004/Edge Cases accepted in exchange for SC-001's round-trip reduction, and recompute's
  explicitly self-healing design (a zero-filled vector is a stale cache entry, not corrupted data —
  see ADR-0526) means a batch-failure window is repaired by any later rebuild, not permanently
  lost. An operator replaying against an embedder with a higher transient-failure rate can trade
  some of SC-001's round-trip reduction back for tighter failure isolation by lowering
  `LCG_REPLAY_EMBED_WINDOW_SIZE` (down to `1`, which reproduces the pre-#486 per-row isolation
  exactly) — this is the intended lever, not a new one.
- New knob: `LCG_REPLAY_EMBED_WINDOW_SIZE` / `ReplayOptions.embed_window_size` (default 64, valid
  range 1–256), validated by `resolve_embed_window_size` mirroring `resolve_batch_size`'s exact
  shape. At the default, the #217 fixture's 4,106 distinct texts collapse to at most 65 round-trips
  before `EmbeddingCache` dedup is even considered — comfortably clearing SC-001's 90% reduction
  bar.
- No IPC/MCP protocol surface change — this is an internal restructuring of an existing
  synchronous callback's shape and calling convention, with no new `knowledge_*` dispatch method
  and no wire-visible change to any existing one.
