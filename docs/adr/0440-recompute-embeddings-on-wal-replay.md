# ADR-0440: Recompute Embeddings on WAL Replay, With a Sync Bridge and a Two-Mechanism Identity Split

**Status**: Accepted
**Date**: 2026-08-19
**Issue**: #440
**Relates to**: ADR-0387 (WAL stream generation identity), ADR-0353 (persist and expose applied
WAL seq), ADR-0378 (multi-stream WAL per group directory), ADR-0045 (prepared-statement cache
scope), ADR-0047 (WAL replay transaction boundaries)

## Context

The WAL stores each embedding vector alongside the text it was computed from. Replay bound the
stored vector verbatim into the Cypher template it re-executed — the embedder was never
consulted. Measured on the #217 real-corpus capture, embedding vectors were 89.9% of WAL bytes
(66.9 MB of 74.4 MB), because each `f32` is serialized as a full JSON decimal literal (~21
bytes/float against a 4-byte raw value) — but the size was a symptom, not the defect. The defect
is that a graph rebuilt from WAL carried the vectors of whatever embedder was configured at
*capture* time, permanently, regardless of what embedder the querying process was actually
running — and nothing compared the two, so a model change silently degraded vector search with no
error and no warning.

Every embedding's source text is already co-located in the same WAL record
(`name_embedding`/`name`, `fact_embedding`/`fact`, `content_embedding`/`content`) — verified with
zero misses across the #217 capture's 4,126 vectors. Recomputing from that text on replay, instead
of binding the stored value, makes replay-time and query-time embedders the only two things that
must agree (normally the same process), rather than pinning every future consumer of the WAL to
the exact model that captured it. It is also self-healing: upgrade the model, rebuild, and search
stays coherent.

Three design problems had to be resolved to make this real rather than aspirational:

1. `Embedder::embed` is `async` (`OaiEmbedder` does a network/UDS round-trip), but
   `WalReplayer::replay_opts` and all four of its call sites are synchronous, and the four call
   sites differ in what tokio runtime context is available to them.
2. Detecting a mismatch (the "nothing compares the two" defect above) needs to answer two
   genuinely different questions — "what embedder did this WAL claim to be written under" and
   "what embedder actually produced the graph's currently-applied vectors" — which can diverge
   (e.g. a stale WAL directory that was never replayed into the live graph).
3. A cache that avoids re-embedding the same text is required by the spec (FR-003), but the
   thing being cached is itself something a persisted or shared cache could get catastrophically
   wrong if it forgot which model computed which entry.

## Decision

### 1. A synchronous callback, bridged per call site — not an async trait threaded through `replay.rs`

`ReplayOptions` gains `recompute_embed_fn: Option<RecomputeEmbedFn>` where
`RecomputeEmbedFn = Box<dyn Fn(&str) -> Result<Vec<f32>, Error> + Send>` — the same
injectable-closure shape `progress_fn`/`cancel_fn` already use in that struct. `replay.rs` itself
never touches tokio. Recompute happens **per row, before the row is pushed into a batch** — never
inside `flush_batch`'s open `BEGIN`/`COMMIT` transaction, so a slow or unreachable embedder cannot
hold an lbug transaction open across up to `batch_size` (default 64) network round-trips.

Each of the four replay call sites bridges its own `Arc<dyn Embedder>` into that sync shape using
whatever runtime context it already has, via two methods on a new `EmbedderContext` (embedder +
model name + shared cache, `crates/core/src/embedding_cache.rs`):

| Call site | Runtime context | Bridge |
|---|---|---|
| `handlers::handle_rebuild_from_wal` | Inside `tokio::task::spawn_blocking`, `AppState` in scope | `EmbedderContext::recompute_fn_via_handle` — `Handle::current().block_on(...)`, documented-safe from a `spawn_blocking` thread (not an async worker thread) |
| `recovery::run_full_recovery_sequence` | Inside `spawn_blocking` (called from `main.rs` and `handlers::handle_recover`) | Same `recompute_fn_via_handle` |
| `handlers::recover_rebuild_from_workspace_wal` (the `rebuild_from_workspace_wal` `knowledge_recover` strategy) | Inside `spawn_blocking`, `AppState` in scope | Same `recompute_fn_via_handle` — added after initial review found this fourth wipe-and-replay-from-WAL site had been missed (see Consequences) |
| `Db::open_or_rebuild` | Bare sync fn — its own test callers run with **no ambient runtime at all** | Builds and owns a dedicated single-threaded `tokio::runtime::Runtime` on demand |

**`Db::open_or_rebuild` has no caller inside the shipped `crates/service` binary today** — a
reviewer confirmed this by grep: it's called only from `crates/core`'s own tests
(`wal_replay.rs`, `name_index_coherence.rs`). This predates this issue by a wide margin (the
function has existed since the original `crates/core`/`crates/service` workspace split) and is
not a regression introduced here — `main.rs`'s actual startup path calls `Db::open` directly and
falls back to `recovery::run_full_recovery_sequence` on error, never `open_or_rebuild`. It is kept
wired into recompute anyway because it is `pub fn` library surface on `Db` (a legitimate call
site for an external consumer of the `lcg-core` crate, or a future internal one), and this
project's struct/signature-change convention is to update every call site consistently rather than
leave an unreached one stale — see "When adding or modifying a struct field" in this repo's
CLAUDE.md. Read "four replay call sites" in this ADR as "four places in the `lcg-core` library
that replay a WAL into a database," not as "four places the running service exercises" — three of
the four are.

`Db::open_or_rebuild`'s callers are why option (1), an ambient-runtime assumption inside
`replay.rs`, was never viable: `real_corpus_replay_perf.rs` and most of `replay.rs`'s own
`#[cfg(test)]` unit tests call `replay_opts` directly with zero runtime present, and
`Handle::current()` panics without one. Pushing the bridge to each caller means only the three
call sites that are always inside `spawn_blocking` pay for `Handle::current()`, and the one
bare-sync call site pays for (and owns) its own runtime instead of forcing that requirement onto
every caller, including tests that want zero tokio involvement.

An embed-call failure during replay (sidecar down, network error) falls back to the row's stored
vector and is counted (`ReplayStats::embeddings_recompute_failed`), never fatal — consistent with
recompute being explicitly self-healing rather than a hard precondition. A row with no co-located
source text falls back the same way, counted separately
(`embeddings_recompute_fallback`) so the two causes stay distinguishable.

### 2. Two separate identity-tracking mechanisms, not one

FR-005/FR-006 ("what did this WAL claim to be written under") reuses `wal_generation.rs`'s exact
sidecar-file pattern (ADR-0387) via a **new, separate** file,
`<wal_dir>/.wal-embedding-model.json`, minted once per WAL directory
(`wal_embedding_identity::ensure_model_identity`, gated the same way `ensure_generation` is — on
`WalWriter::global_seq() == 0`, checked by the caller after construction, not inside
`WalWriter::new` itself). A missing or corrupt record is always "unknown," never a mismatch — the
same non-negotiable invariant ADR-0387 established for `.wal-generation.json`.

FR-007 ("what actually produced the graph's currently-applied vectors") extends the existing
`WalPosition` table with `embedding_model STRING`/`embedding_dim INT64` columns, written by
`set_wal_position` alongside `applied_seq`/`generation` on every successful WAL-position advance —
a replay/rebuild, and (after the fix described in Consequences) every ordinary live write too —
the same extension shape ADR-0387 used to add `generation` to that table, and the same
"re-derived and persisted on every write" treatment `generation` already gets.

These answer different questions and can genuinely diverge — a WAL directory can claim one
identity while the graph's live vectors reflect a different (possibly older, un-replayed)
identity, or the reverse after a partial/failed rebuild. Collapsing them into one mechanism would
lose that distinction, exactly as ADR-0387/ADR-0353's own split between "what's on disk" and
"what's been applied" was preserved rather than merged for `generation`/`applied_seq`. A single
new sidecar file, not new fields bolted onto `.wal-generation.json`, keeps the "opaque reset
token" and "model identity" concerns independently readable/writable without coupling
`wal_generation.rs`'s carefully-reasoned concurrency/self-heal logic to unrelated content.

Both comparisons are exposed the same way: a `[WAL WARN]` log line at replay time
(`EmbedderContext::check_replay_mismatch`, called from all four replay call sites before
replaying) for FR-006, and new `knowledge_status` fields — `embedding_model`/`embedding_dim`/
`embedding_model_status` on the flat default-group `wal` object and mirrored per-group inside
`wal_groups` — for FR-007, following `wal.generation`/`generation_status`'s exact precedent
(`wal_generation_status`'s classification helper is the direct template for the new
`embedding_model_status`). Neither is ever a hard failure: a mismatch is inherently self-healing
via `knowledge_rebuild_from_wal`, so refusing outright (as ADR-0414 does for a *reset*, a
different and more dangerous condition) would contradict the spec's own framing of recompute as
the corrective mechanism.

### 3. The embedding cache is in-memory-only, keyed by `(model, dim, text)`

`EmbeddingCache` (`crates/core/src/embedding_cache.rs`) is a `Mutex<HashMap<[u8; 32], Vec<f32>>>`
keyed by `sha256(model || 0x00 || dim.to_le_bytes() || 0x00 || text)` — mixing the embedder
identity into the key, not text alone, per FR-003's literal "(source text, embedding model
identity)" spec. One instance lives on `AppState`, constructed once right after the embedder is
probed in `main.rs`, and is threaded into both the startup recovery call and `AppState::from_env`
so it stays warm across the startup-recovery → serving transition. It is never written to the WAL
or any other durable store (FR-004): losing it only means the next lookup recomputes at full
cost, never an incorrect result.

Mixing identity into the key was not optional even though, today, exactly one embedder identity
is ever live in a given process (so the mix is presently a constant prefix with zero observable
behavior change): a cache keyed by text alone is a trap for the moment it is shared or persisted
across more than one identity — vectors from one model would be served as another's with nothing
to detect it, reintroducing through the cache exactly the silent-degradation failure mode FR-008
exists to eliminate everywhere else. Fixing the key now, while it is a no-op, means any future
persistence work (FR-004 already permits discarding/reloading a cache) inherits a safe cache
rather than a hazard that has to be separately remembered.

## Rejected Alternatives

**Assume an ambient tokio runtime inside `replay.rs` and call `Handle::current().block_on(...)`
directly from the row loop.** Rejected because two of `WalReplayer`'s real callers —
`real_corpus_replay_perf.rs` and `replay.rs`'s own unit tests — run with no runtime at all;
`Handle::current()` would panic there. Keeping `replay.rs` fully decoupled from tokio, with the
bridge pushed to each caller, is what lets those callers keep working unchanged when recompute is
disabled (`recompute_embed_fn: None`), which is also how every pre-existing `ReplayOptions`
construction site in the codebase was updated — no behavior change for a caller that opts out.

**Fold FR-005/FR-006 and FR-007 into one mechanism** (e.g. a single "embedding identity" concept
stamped in one place and read for both purposes). Rejected for the same reason ADR-0387 and
ADR-0353 keep source-side and consumer-side WAL position tracking separate: "what the WAL claims"
and "what the graph currently contains" are different facts that can diverge, and a caller that
needs to distinguish "replay would mismatch" from "the live graph is already stale" loses that
ability the moment the two collapse into one value.

**A single-model-only cache with no identity mixed into the key**, on the reasoning that "one
embedder per process" makes it moot. Rejected per the argument in Decision (3) above — the
no-behavior-change property of mixing identity in now is exactly why there's no reason not to,
and the failure mode it forecloses (silent cross-model vector reuse) is the one class of bug this
whole issue exists to eliminate everywhere else in the system.

## Consequences

- **All four replay call sites recompute uniformly**, closing the risk (flagged at Research/Plan
  time) that wiring only the easiest call site — `handle_rebuild_from_wal`, which already had
  `AppState` in scope — would leave other from-scratch/rebuild paths silently still binding stale
  stored vectors. The risk materialized exactly as predicted: the initial implementation wired
  `handle_rebuild_from_wal`, `recovery::run_full_recovery_sequence`, and `Db::open_or_rebuild`, but
  missed `handlers::recover_rebuild_from_workspace_wal` — the handler for `knowledge_recover`'s
  `rebuild_from_workspace_wal` strategy, which wipes and replays the *entire* embedded DB from
  WAL, i.e. precisely the "graph rebuilt from WAL" scenario this issue exists to fix. Two
  independent reviewers (an automated deep-review pass and GitHub Copilot) caught it on the PR; it
  was wired in the same way as the other `AppState`-scoped call sites (`recompute_fn_via_handle`
  plus the FR-006 mismatch warning). All four now take an `EmbedderContext` (or, for
  `Db::open_or_rebuild`'s bare-sync test callers, `Option<EmbedderContext>` built the same way) —
  `None` preserves exactly today's stored-vector behavior for a caller that doesn't supply one
  (every existing test).
- **`set_wal_position` gained a fourth parameter** (`embedding_identity: Option<(&str, i64)>`),
  touching every call site across the crate (production and test) — mechanical, but a large
  surface; verified by a full workspace build and the lib test suite rather than by inspection
  alone.
- **The cache's real-world hit rate on a single-group, post-dedup WAL (the #217 fixture) is ~0.5%**
  (4,126 vectors → 4,106 distinct texts) — entity dedup during ingest means each `CREATE` in the
  WAL already carries a distinct name/fact/content, so replay-time caching has little left to hit
  on for that fixture specifically. The cache is expected to matter more for a multi-group
  workspace (the same entity name recurring across separate groups' `Entity` nodes) — not
  exercised by this fixture — and the FR-011 benchmark (`real_corpus_replay_perf.rs`) reports the
  hit rate explicitly, with this caveat documented inline, rather than implying the cache "solved"
  replay cost from a single-fixture measurement.
- **`EmbeddingCache` has no eviction, capacity bound, or TTL** — every distinct `(model, dim,
  text)` triple embedded over the process's lifetime accumulates permanently, reclaimed only by a
  restart (flagged by `handarbeit-pruefer` on the PR). Accepted as-is: FR-004 requires the cache
  be safe to *discard*, not bounded, and its job is avoiding redundant embedder calls within a
  single rebuild/recovery, not serving as a long-lived store — a capacity bound (e.g. LRU) is a
  reasonable follow-up if a deployment's distinct-text pool makes this a real concern, not a
  correctness gap in this issue's scope.
- **Recompute is not batched.** Each recognized embedding param triggers one `embed()` call; the
  `/v1/embeddings` OpenAI-compatible endpoint the embedder speaks accepts an array of inputs, so
  batching remains available as a follow-up if the measured cost warrants it — out of scope here,
  where the deliverable is the correctness fix plus its measurement, not an optimization of the
  measurement's result.
- **No vectors are dropped from newly-written WAL files** — recompute is proven correct
  (FR-010/SC-001, the #217 fixture's built-in validation oracle: every stored vector recomputed
  from its co-located text and compared via cosine similarity, threshold 0.999 on the per-kind
  mean) while the stored vectors are still present to validate against. Stripping vectors from the
  WAL, and their on-disk encoding if any survive, are deliberately sequenced follow-up issues (see
  the spec's Out of Scope).
- **`WalPosition.embedding_model`/`embedding_dim` are re-derived and re-stamped on every
  successful WAL-position advance, not only after a replay/rebuild.** The initial cut of this work
  wired `embedding_identity` only into the three replay call sites, leaving every ordinary live
  write (`add_episode`, `wal_exec::advance_wal_position`'s ~19 other callers) passing `None` —
  which meant a group that had only ever been live-ingested, never explicitly rebuilt, reported
  `embedding_model_status: "unknown"` forever, including across a model change and restart with
  zero intervening rebuild: exactly the User Story 2 Acceptance Scenario 2 case, just for the
  group type that gap didn't cover. Two independent reviewers (an automated deep-review pass and
  GitHub Copilot) flagged this on the PR, and it was closed by threading `state`'s running
  `(embedding_model, embedder.dim())` through `advance_wal_position` and `add_episode`'s direct
  `set_wal_position` call, mirroring exactly how `generation` is already re-derived and
  re-persisted on every write rather than only after a replay. This is deliberately still a
  best-effort marker, not a full-graph audit: a write stamps the identity of the embedder that
  *ran* it, not a claim that every vector currently in the group was computed under that identity
  — a group that changes embedders mid-life without an intervening full rebuild can still carry
  stale, un-recomputed vectors even while the status reads `"match"` (the status reflects the most
  recent write's embedder; a delete/correction/relabel write stamps the running identity the same
  way a content-embedding write does, even though it touched no vector itself). This is the same
  approximation `generation` already makes for every mutation regardless of content, not a new
  category of imprecision introduced here — see the Rejected Alternatives entry above on why
  FR-005/FR-006 and FR-007 stay two separate mechanisms; this fix doesn't blur that boundary, it
  only changes how often FR-007's own mechanism is refreshed. `docs/operations.md`'s
  `embedding_model_status` section carries the operator-facing version of this caveat.
- **A replay/rebuild only persists the running embedder's identity if no recompute attempt
  actually failed during it.** All four replay call sites originally derived the identity to
  persist from `EmbedderContext::identity()` alone — the *configured* embedder — with no check of
  the replay's own outcome. If recompute failed for some or all rows during that specific rebuild
  (e.g. the embedder sidecar was transiently unreachable), the stored, un-recomputed vector stayed
  bound, yet `WalPosition.embedding_model`/`embedding_dim` — and therefore
  `embedding_model_status` — still reported `"match"`: the exact silent-divergence case FR-006/
  FR-008 exist to close, occurring in the one family of code paths (explicit rebuilds) whose whole
  purpose is to fix stale vectors. `handarbeit-pruefer` flagged this on the PR, initially framing
  it as "recompute failed OR the row had no co-located text to recompute from." The latter half
  turned out to be over-broad: `crates/core/tests/fixtures/wal/python_produced.jsonl` (used by
  `test_recovery_rebuild_from_workspace_wal_recomputes_embeddings`) contains legitimate `SET`-only
  mutations that update `content_embedding`/`fact_embedding` without re-supplying the source text
  — normal, ongoing WAL shape (FR-002), not a defect — and gating on that too made the fixed test
  fail, since a rebuild that recomputed cleanly everywhere it could still reported `"unknown"`
  because of these structurally-textless rows. Fixed by `ReplayStats::
  embeddings_recompute_had_no_failures()` — true when `embeddings_recompute_failed == 0`,
  deliberately independent of `embeddings_recompute_fallback` — gating the identity write at every
  call site: `Db::open_or_rebuild` and `handle_rebuild_from_wal`'s two replay paths filter the
  single group's identity on the replay's own stats; `recover_rebuild_from_workspace_wal` and
  `run_full_recovery_sequence` (which each replay several groups per call) carry a per-group flag
  alongside `(seq, generation)` so one group's failure doesn't suppress another group's
  legitimately-confirmed identity. A gated write persists `None`, which reads as `"unknown"`
  rather than falsely `"match"` — an accurate, if less informative, signal, consistent with the
  "best-effort marker" framing above.
