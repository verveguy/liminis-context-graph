# ADR-0526: Vectors Are a Local Cache — Stop Writing Them to the WAL, Ignore Them on Replay

**Status**: Accepted
**Date**: 2026-09-01
**Issue**: #526
**Relates to**: ADR-0440 (recompute embeddings on WAL replay, with a sync bridge and a
two-mechanism identity split — this ADR completes it and removes the parts its own model makes
dead), ADR-0470 (Entity.summary_embedding), ADR-0387/ADR-0353 (the sidecar-file and
single-row-position patterns this issue's removed mechanism mirrored)

## Context

ADR-0440 made WAL replay recompute embedding vectors from co-located source text instead of
binding whatever value the WAL happened to store — but it kept the writer emitting vectors into
the WAL, and kept a fallback that bound the stored vector verbatim when co-located text was
absent. Measured on the #217 real-corpus capture (16 files, 12,482 records, 74.4 MB): embedding
vectors were 89.9% of WAL bytes (66.9 MB), because a 3,072-byte raw 768-dim vector costs 5.3× as a
JSON decimal literal, and a `name_embedding` is 958× the 16-byte name it came from.

Once replay unconditionally recomputes every vector it can, and `AppState.embedder` is
`Arc<dyn Embedder>` — not an `Option`, fatal-at-startup if unreachable — the only remaining
argument for keeping vectors in the log evaporates: every consumer already has an embedder (it
needs one to embed its own queries), so requiring one to hydrate a WAL costs nothing that wasn't
already required. ADR-0440's stored-vector fallback exists precisely for a state
("no embedder available") that cannot arise in that world — the same shape as issue #432's removal
of machinery built for a case with no instance.

This issue makes the model unconditional: the writer stops emitting vector params entirely, and
the fallback that made "no source text" survivable is removed. That removal is only safe if every
WAL record kind that carries a vector today is confirmed to carry its source text co-located in
the same record — the one open risk ADR-0440 itself flagged and this issue had to resolve before
removing the fallback that covered it.

## Decision

### 1. Strip vector params in `WalWriter::log_mutation`, the one choke point every WAL write shares

`Conn::exec_params` binds `(cypher, params)` to lbug and records the *same* tuple for WAL logging
in one call — the DB-bind and the WAL-log value are literally one object, so stripping must happen
strictly downstream of the DB write, never inside `exec_params`/`insert_*` (which would strip the
value lbug itself binds). `wal_exec.rs`'s `wal_flush_chunk`/`wal_flush_ungrouped` looked like the
natural site, but `dump.rs` calls `WalWriter::log_mutation` directly, bypassing `wal_exec.rs`
entirely — stripping only there would leave dump/compaction-authored WAL lines still carrying
vectors. `log_mutation` is the one point every WAL write, live or dumped, passes through, so a
fixed key list (`name_embedding`, `fact_embedding`, `content_embedding`, `summary_embedding` —
`wal::VECTOR_PARAM_KEYS`) is removed from `params` there, unconditionally, before the line is ever
serialized.

### 2. Replay recognizes a vector-bearing row by its Cypher template, not by what's in `params`

Before this issue, `recompute_row_embeddings` decided whether a row carried a recognized vector by
checking whether `params` still had the key. That signal disappears the moment the writer stops
emitting it. Relevance now comes from the row's own **Cypher template**: `cypher.contains
("$name_embedding")` (and so on for the other three keys, `replay::EMBEDDING_TEXT_PAIRS`). This
happens to work identically for a fresh (stripped) WAL and an older (unstripped, pre-#526) one,
since the template text — the schema's own `CREATE`/`SET` clause — is unchanged either way; only
whether the JSON params object *also* happens to carry a stale value differs, and that stale value
is now always ignored, never bound (FR-002/FR-003): a recognized row is either freshly recomputed
or explicitly zero-filled, and the stored value (if any) is simply never read at all.

### 3. Textless rows split into skip-row vs. zero-fill, decided structurally, not by record kind

FR-005 required every vector-bearing record kind to be confirmed as co-locating its text. Two
concrete kinds don't, on inspection:

- `backfill_summary_embeddings.rs`'s `SET n.summary_embedding = $emb` (no `summary` text in that
  record — it lives in an earlier, separate WAL record for the same entity).
- The pre-existing Python/graphiti-driver `SET`-only `content_embedding`/`fact_embedding` updates
  already sitting in the wild, pinned by `tests/fixtures/wal/python_produced.jsonl` and called out
  in ADR-0440's own Consequences as "normal, ongoing WAL shape."

These two shapes need opposite treatment when text is unavailable (missing, or a recompute
attempt failed). A **vector-only `SET`** — a mutation that exists for no purpose other than
writing that one column — is **skipped entirely**: never executed at all. Binding a placeholder
here would *overwrite* a real vector the entity's own `CREATE` record already computed for that
column, actively degrading it; skipping preserves whatever is already there. Any **other**
record — most commonly a `CREATE`, which must still create the entity/edge/episode it represents
— gets a **same-dimension zero vector** instead: dropping the row would silently lose the whole
node, failing SC-002's exact-count-parity requirement.

The two are distinguished by a new `is_vector_only_set(cypher, vec_key)` helper, not by a fixed
enumeration of known record kinds: it counts assignments in the Cypher template's `SET` clause
(a `CREATE`-form record has no `SET` keyword to find at all; a `MERGE ... SET` template that also
sets other real properties has more than one assignment). This generalizes to any future
single-purpose vector `SET` without needing a matching update to a hardcoded kind list.

`backfill_summary_embeddings.rs` is the one gap this issue can actually close, since (unlike the
external Python-driver shape) lcg controls this call site: its param is renamed `emb` →
`summary_embedding` and a co-located `summary` text param is added, so it recomputes normally
going forward and never reaches the skip path.

### 4. One shape is genuinely unrecoverable, and stays that way by explicit decision

`python_produced.jsonl`'s `RelatesToNode_` `CREATE` line inlines `fact_embedding: [0.1, 0.2, 0.3,
0.4]` as a raw Cypher literal, with `"params":{}` — no `$fact_embedding` placeholder at all. This
is an external, non-lcg-authored write shape (lcg's own writer always binds vectors as params via
`exec_params`, never inlines a literal), so there was never a param for FR-001's stripping to
touch, and there is no placeholder for FR-002's cypher-template relevance check to recognize. It
replays exactly as written — out of scope for recompute by construction, not silently dropped or
degraded — and `test_literal_inlined_fact_embedding_replays_unchanged`
(`crates/core/tests/wal_replay.rs`) pins this as a documented decision rather than an accident.

### 5. The embedder becomes a mandatory argument, not an `Option`, throughout the replay API

`ReplayOptions.recompute_embed_fn`/`Db::open_or_rebuild`'s `embedder` parameter drop their
`Option` wrapper (`ReplayOptions::new(recompute_embed_fn, recompute_embed_dim)` is now the
constructor). A `None` had no safe interpretation left: once the writer never supplies a fallback
value, a caller with no embedder has no way to satisfy a `CREATE` template's vector placeholder at
all. Every real production call site (`db.rs`, `recovery.rs`, `handlers.rs`, `main.rs`) already
built a real `EmbedderContext` unconditionally before this issue, so this is a test-only-callers
change in practice — but a large mechanical one, touching every `.replay(&conn)`/`ReplayOptions`
construction site across `crates/core/tests/*.rs` (50+ sites across 12+ files, dominated by
`wal_replay.rs` alone). `replay::zero_vector_embed_fn(dim)` — a trivial fixed-dimension zero
vector, ignoring input text — is exported for a test that needs *some* schema-valid embed fn and
doesn't itself assert on embedding fidelity.

### 6. FR-003's removal is mechanical once (5) lands; FR-004 is untouched by design

Two model-identity mechanisms coexisted before this issue: a WAL-side sidecar
(`wal_embedding_identity.rs`'s `.wal-embedding-model.json`, "what did this stream claim to be
written under," diagnostic-only) and a DB-side stamp (`WalPositionRecord.embedding_model`/
`embedding_dim`, "what actually produced the graph's live vectors," compared at query time via
`embedding_model_status`). Once no stored vector is ever bound, the WAL-side stamp has nothing
left to govern — its only purpose was warning about a mismatch that could affect which vector got
bound, and that possibility is gone. `EmbedderContext::check_replay_mismatch`, both copies of
`warn_on_embedding_model_mismatch` (`db.rs`, `recovery.rs`), the three `check_replay_mismatch`
call sites in `handlers.rs`, the entire `wal_embedding_identity.rs` module, and its mint call site
in `app_state.rs` are all removed. The DB-side stamp (FR-004) is untouched — no code change — since
it answers an entirely different, still-necessary question: whether the *database's own* vectors
match the *currently running* embedder, independent of anything WAL-related. It is now the sole
surviving model-identity mechanism (User Story 3).

## Rejected Alternatives

**Keep `recompute_embed_fn` as `Option<RecomputeEmbedFn>`, redefining `None` to mean "skip
recompute, leave whatever's in the DB."** Considered as a smaller mechanical change (avoiding the
50+-site test-suite update), but rejected: it reintroduces exactly the ambiguous partial-behavior
surface the spec's "unconditional, no flag, no mode" framing exists to close, and gives a fresh
WAL's `CREATE` row no safe value to bind for its vector placeholder at all once the writer never
supplies one. Every production call site already builds a real embedder unconditionally, so the
mandatory signature costs those call sites nothing and removes a surface that could otherwise be
misused by a future caller.

**Zero-fill every textless vector-bearing row uniformly, without the vector-only-`SET` vs.
`CREATE` split.** Rejected because it would actively degrade the two known real vector-only-`SET`
shapes: overwriting a `RelatesToNode_`/`Episodic` node's already-correct, CREATE-time-recomputed
vector with a placeholder zero vector on every replay of
`python_produced.jsonl`-shaped WAL content — the opposite of "same graph as before" (FR-006).

**Skip every textless vector-bearing row uniformly, without the split.** Rejected in the other
direction: skipping a `CREATE`-type row with a textless vector (an embedder hiccup during a real
replay, however rare) would silently drop the whole entity/edge/episode from the rebuilt graph,
failing SC-002's exact-count-parity requirement outright.

**An exact-key-name allowlist keyed off the backfill's original `emb` parameter name, instead of
fixing the backfill's WAL shape.** Rejected: FR-001's stripping is driven by the four canonical
vector *column* names (`wal::VECTOR_PARAM_KEYS`), which the backfill's original `emb` parameter
name didn't match at all — it was silently exempt from stripping already, an accident, not a
decision. Renaming the backfill's own parameter to `summary_embedding` (and adding the co-located
`summary` text) both fixes the accidental-exemption and closes the one FR-005 gap this issue could
actually close, in one change.

## Consequences

- **A freshly written WAL contains no embedding vector params (SC-001).** Measured by replaying
  the #217 capture's original, vector-bearing shape into a fresh DB and re-dumping it through the
  live write path (`crates/core/tests/wal_vector_stripping.rs`): 74,333,455 → 4,954,222 bytes, a
  **93.3%** reduction — exceeding the 89.9% ADR-0440 originally measured (the fixture's
  MENTIONS-edge dump path also skips some null-uuid rows, slightly changing the byte mix, but the
  dominant effect is the vector strip).
- **An older, vector-bearing WAL still replays to the same graph (SC-002/FR-006).** The #217
  capture's existing `real_corpus_e2e.rs` tests (built for ADR-0440, unchanged by this issue)
  replay the same fixture end to end through `knowledge_rebuild_from_wal`'s now-mandatory-recompute
  path and confirm identical `entity_count`/`relationship_count`/`episode_count`, golden
  entity/relationship query results, and multi-hop traversal — passing in ~167s.
- **SC-005's live-embedder-dependent cold/warm-cache measurement degrades to a documented `[SKIP]`
  in an environment with no reachable embedder sidecar** (`real_corpus_replay_perf.rs`'s
  `measure_cold_vs_warm_cache_replay_over_real_corpus_wal`) — this was a known risk at Plan time,
  not a defect: the mechanism itself (a `CountingEmbedder` wrapping a live embedder, replaying the
  same fixture twice through one shared `EmbeddingCache`) is implemented and exercised by its own
  skip path, but the live-embedder figures it reports are unverified in an environment without one
  reachable. The embedder-independent throughput baseline still runs unconditionally and reports
  238.9 mutations/s over the full 12,482-record capture.
- **`ReplayStats::embeddings_recompute_fallback` is renamed to
  `embeddings_recompute_skipped_no_text`**, and a new `embeddings_skip_rows` counter tracks rows
  dropped entirely by the vector-only-`SET` skip path — both visible on every replay/rebuild
  result, so a zero-fill or skip decision is a checked, reportable outcome rather than a silent one.
- **Test-suite blast radius was larger than initially estimated**: not ~30 `ReplayOptions`
  literals in `wal_replay.rs` alone, but 50+ `.replay()`/`ReplayOptions` construction sites across
  12+ files, once the bare `.replay(&conn)` convenience wrapper (used broadly outside
  `wal_replay.rs`) was counted. The mechanical pass (adding `zero_vector_embed_fn(dim)` + `dim`,
  or a small `test_embedder_ctx()` helper where an `EmbedderContext` is needed) was done as one
  focused pass ahead of any assertion rewrites, so compiler errors drove completeness; three
  call sites needed a non-default dimension (32, matching that test's own schema) rather than the
  otherwise-uniform 4 used throughout this crate's test suite.
