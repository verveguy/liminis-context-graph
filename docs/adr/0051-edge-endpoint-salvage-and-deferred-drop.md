# ADR-0051: Edge Endpoint Salvage and Deferred Drop Decision

**Status**: Accepted
**Date**: 2026-07-29
**Issue**: #281

## Context

`add_episode` (`crates/core/src/episode.rs`) resolves edge endpoints in two independent places:
a pre-lock pass (before Phase C's write-lock is acquired) and a lock-held pass inside Phase C's
`spawn_blocking` commit closure. Before this change, **both** passes could drop an edge for an
unresolvable endpoint, and they ran two genuinely separate resolution attempts:

- The pre-lock pass checked the batch's own entity list, then (added in #209/PR #218) issued its
  own `get_entity_by_name_ci` DB round-trip against a `missing_names` set to catch endpoints
  created in an earlier ingest batch. Anything that failed both checks was `retain`-dropped
  before the write lock was ever taken.
- Phase C's lock-held pass repeats essentially the same two checks (batch `name_to_uuid` map,
  then `get_entity_by_name_ci` fallback) for whichever edges survived the pre-lock filter, and
  drops anything still unresolved there too.

This meant an edge that Phase C's lock-held resolution *could* have rescued (e.g. an entity
persisted by a concurrent write between the pre-lock check and the lock-held check, or simply a
name variant the pre-lock pass's lookup missed) never got the chance — it was already gone.
More importantly, #281 identified that the pre-lock pass's *first* check (the batch's own entity
list) was frequently empty for reasons upstream of any DB lookup: the entity-extraction prompt
banned "abstract concepts" outright, so the edge-extraction pass would name an endpoint (e.g.
"climate change") that the entity pass was never allowed to produce in the first place. On an
unchunked 257KB fixture page, this discarded 97.8% of extracted edges — the pre-lock drop path
had no chance to help, because there was nothing in the batch *or* the persisted graph to resolve
against; the entity that should exist simply didn't.

Separately, FR-001 constrains the Anthropic tool-use call's `source_name`/`target_name` fields to
an `enum` of the batch's entity names, so a compliant model literally cannot name an off-list
endpoint. But the OpenAI-compatible path (ADR-0041) has no tool-use schema to constrain — it
coerces structured output via a JSON-shape instruction in the prompt, which a local model may or
may not honor. A schema-level fix alone cannot be the whole story; a post-hoc mechanism is
still required for endpoints that make it through despite the schema (FR-006), and that
mechanism needs to actually get a chance to run before anything is dropped.

## Decision

**Pre-lock edge validation becomes advisory only, and is narrowed to two things: filtering
self-referential edges, and salvaging (not dropping) an off-list endpoint.**

1. **Self-referential filtering stays pre-lock and stays a hard drop.** `source_name ==
   target_name` (case-insensitively) is always wrong regardless of DB state or timing — there is
   no lock-held check that could ever rescue it — so dropping it early costs nothing and saves a
   wasted salvage/resolution attempt.

2. **An off-list endpoint (present in an edge but absent from the batch's own entity list) is
   *salvaged*, not dropped, by cosine-matching its name embedding against the batch's own entity
   name embeddings.** This reuses `DEDUP_THRESHOLD` (0.85) and `db::cosine_similarity` — the same
   threshold and function already used for entity-vs-entity dedup — rather than introducing a
   second tunable. A match above threshold rewrites the edge's endpoint name in place to the
   matched entity's canonical name; anything that doesn't match is left untouched.

   Salvage matches **only** against the current batch's own entities, not the persisted graph.
   Persisted-entity resolution is already Phase C's job (see below); duplicating it here would
   just be a second, redundant DB round-trip for no additional recall, since anything Phase C's
   `get_entity_by_name_ci` fallback can resolve, it can resolve just as well without a pre-lock
   pass having tried first.

3. **The pre-lock `globally_resolved` DB lookup (the #209 addition) is removed entirely**, not
   kept alongside the new salvage step. It was solving the same problem — "is there an existing
   entity this endpoint could resolve to?" — that Phase C's lock-held `get_entity_by_name_ci`
   fallback already solves, just earlier and redundantly. Keeping both would mean two independent
   passes that could silently drift (e.g. one checks lowercase-trimmed names, the other doesn't;
   one runs before a concurrent write lands, one after) with no single source of truth for "was
   this edge actually resolvable." Removing it is a straightforward simplification, not a
   regression: nothing it could resolve becomes unresolvable, because Phase C performs the
   identical lookup itself, just later.

**Phase C (the lock-held commit) becomes the sole, authoritative point at which an edge endpoint
is finally resolved or the edge is dropped.** For each edge, it checks the batch's `name_to_uuid`
map (now including any name a salvage rewrite pointed at), then falls back to
`get_entity_by_name_ci` against the persisted graph exactly as before. An edge whose endpoints
both resolve is inserted; an edge where either side fails to resolve here is dropped and counted
in a new `edges_dropped_unresolvable` counter, which flows out of the `spawn_blocking` closure
onto `AddEpisodeResult` and into `knowledge_process_chunk`'s JSON result (FR-004) — surfacing drop
counts to a caller rather than leaving them observable only via `eprintln!`.

One consequence: `edges_extracted` on `AddEpisodeResult` changes meaning from "how many edges
survived the pre-lock filter" to "how many edges were actually inserted." This is the only way to
report `edges_dropped_unresolvable` accurately once the drop decision moves entirely to Phase C.
Existing integration tests already asserted on final-inserted-edge semantics for this field
(`crates/core/tests/edge_endpoint_resolution.rs`), so no test needed to change meaning to match —
only the new counter needed wiring.

**Why this is what makes the OpenAI-compatible path (FR-006) safe without a schema `enum`:**
salvage and deferred drop both live in `episode.rs`, operating on the `ExtractionResult` produced
by *either* `Extractor` implementation. Whether an off-list endpoint arrived because a local model
ignored its JSON-shape instruction, or because the Anthropic schema `enum` somehow didn't apply
(e.g. no ontology constraint reachable), the same salvage-then-defer-to-Phase-C path runs
identically. FR-001's `enum` reduces how often an off-list endpoint occurs on the Anthropic path;
this mechanism is what makes the outcome correct when one occurs anyway, on either path.

## Consequences

- Salvage threshold behavior is proven only against controlled `NameMapEmbedder` vectors in unit
  tests (`crates/core/tests/edge_endpoint_resolution.rs`), not real embedding output. If
  production salvage turns out too aggressive (cross-resolving genuinely distinct entities like
  "carbon dioxide" and "carbon monoxide") or too conservative (missing true synonyms), tuning
  `DEDUP_THRESHOLD` — or introducing a separate salvage-specific threshold — is a fast follow-up,
  not a blocker to this change.
- `edges_extracted`'s meaning change is a public IPC-field behavior change. Real-world values
  should only go up relative to the pre-#281 behavior (edges that used to be dropped pre-lock,
  wrongly, may now be salvaged or resolved at Phase C), never down — but any consumer reading this
  field as "edges seen before validation" rather than "edges inserted" should be aware it now
  means the latter unambiguously.
- Removing the pre-lock `globally_resolved` lookup shifts that DB round-trip's timing from
  "before the write lock" to "inside Phase C, under the write lock" — but Phase C already made
  that identical call for its own fallback path, so this doesn't add a new query, only moves an
  existing one later for the subset of edges that need it.
- A future contributor reading `episode.rs` and wondering why the pre-lock phase doesn't drop
  unresolvable edges anymore, or why the persisted-entity lookup moved, should find this ADR
  rather than needing to reconstruct the reasoning from the diff alone.

## Related

- #209 / PR #218: introduced the pre-lock `globally_resolved` persisted-entity lookup this ADR
  removes; the original defect (a batch-local edge endpoint that exists in the graph resolving
  incorrectly) is unaffected — Phase C's fallback still covers it, just as the sole resolver now.
- FR-001 (this issue, #281): the Anthropic tool-use schema `enum` constraint on
  `source_name`/`target_name`, which reduces how often salvage/deferred-drop needs to do work on
  the Anthropic path but does not replace it (see FR-006 above).
- FR-002 (this issue, #281): the entity-prompt concept-ban rewording, addressing the root cause
  of why an edge endpoint would be off-list in the first place for a large share of real-world
  documents.
- ADR-0041: Local/OpenAI-Compatible Extraction Adapter — the path with no tool-use schema to
  constrain, which is why this ADR's mechanism (not a schema `enum`) is what FR-006 relies on.
- ADR-0029: Name-First Entity Resolution in `add_episode` Phase B — the entity-side counterpart
  to this ADR's edge-side resolution; both use case-insensitive name matching as the first-choice
  resolution strategy before falling back to embedding similarity.
- `crates/core/src/episode.rs`: the pre-lock salvage step and Phase C's commit closure.
- `crates/core/tests/edge_endpoint_resolution.rs`: salvage, adversarial non-collapse, and
  drop-counting coverage.
