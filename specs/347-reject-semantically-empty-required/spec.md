# Feature Specification: Reject semantically-empty required fields during item salvage

**Feature Branch**: `fabrik/issue-347`
**Created**: 2026-08-13
**Status**: Specified
**Input**: User description: "Follow-up to #342 / PR #344, from a CodeRabbit review finding deliberately left out of scope there. #342 made extraction-response parsing tolerant of items that fail to *deserialize* (a missing required field). It did not add validation of fields that deserialize successfully but are semantically empty — an empty or whitespace-only `String` deserializes fine and returns as a valid item. A blank `fact` on an edge has no validation at any layer and can be persisted; `crates/eval/src/runner.rs` consumes `outcome.result` directly, bypassing `episode.rs`'s empty-name `retain`, so eval scores are computed over a slightly different item set than production ingests."

## Background

#342 (shipped in 0.12.1, PR #344) fixed a data-loss bug reported in #340: `knowledge_process_chunk`
was rejecting an entire chunk when a single extracted entity or edge in the LLM's response failed
to *deserialize* (e.g. a missing `name` field), losing every other valid item in that chunk. The
fix, `salvage_items` in `crates/core/src/extractor.rs`, drops and counts only the individual items
that fail `serde_json::from_value::<T>`, keeping the rest.

That fix addresses structural validity (does the item deserialize at all) but not semantic
validity (does a successfully-deserialized field actually contain something useful). An empty or
whitespace-only `String` satisfies `serde` — the field is present and typed correctly — so it
passes through `salvage_items` as a valid item even though it carries no usable content:

```rust
match serde_json::from_value::<T>(value) {
    Ok(item) => items.push(item),
    Err(_) => dropped += 1,
}
```

This finding was raised during the #344 code review. It was correct about the mechanism but out of
scope for that PR: #342 was a 0.12.1 patch release fixing reported data loss, and adding semantic
field validation across both providers and both item types (entities and edges) is new logic on a
path that had just changed — the wrong addition to bundle into a data-loss patch. This issue is the
deferred follow-up, targeted at 0.13.0.

**Entities already have a partial answer; edges do not.** `episode.rs`'s existing `retain(|e|
!e.name.trim().is_empty())` (episode.rs:254) drops blank-name entities downstream of parsing, and
folds its count into `entities_dropped_malformed` (episode.rs:47) so that a missing, `null`, or
empty-string `name` all produce the same observable outcome at the `knowledge_process_chunk`
result (#342 FR-007). No entity-side gap remains today. Two real gaps remain:

1. **A blank `fact` on an edge has no validation at any layer and can be persisted.** A blank
   `source_name`/`target_name` at least lands in `edges_dropped_unresolvable`
   (episode.rs:27); `fact` has no equivalent check anywhere in the pipeline. An edge whose fact is
   `""` is not useful to any consumer and should not reach storage.
2. **The eval path bypasses the ingest path's filtering.** `crates/eval/src/runner.rs:319` consumes
   `outcome.result` directly (`.map(|outcome| outcome.result)`), never passing through
   `episode.rs`'s `retain`. The eval path therefore sees blank-name entities that the ingest path
   would silently drop, so eval scores are computed over a slightly different item set than
   production ingests would ever store. This is a correctness issue for eval scoring specifically,
   not a user-facing ingest defect (the ingest path already handles entity names correctly).

**A structural risk this issue must not (re-)introduce.** #342's design deliberately keeps two
counting layers disjoint: parse-time salvage (`entities_dropped_malformed` /
`edges_dropped_malformed`, from deserialize failure) and the downstream `retain` (folded into
`entities_dropped_malformed`, from semantic emptiness) never count the same item, because
parse-time salvage only removes items that failed to deserialize at all — an item with an
empty-string name deserializes fine and is never touched at that layer (episode.rs:243-252's
comment states this invariant explicitly). If semantic validation moves to parse time — as the
edge-level fix in this issue requires, and as extending it to catch entity names at the same layer
would also imply — that invariant no longer holds automatically and must be re-established
deliberately, or a single blank-name item gets counted twice.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Blank edge fact does not reach storage (Priority: P1)

An extraction LLM returns an edge whose `fact` field is present but empty or whitespace-only
(e.g. `"fact": ""` or `"fact": "   "`). Today this edge deserializes successfully and is persisted
as-is — a stored edge with no usable fact content, which is useless to any downstream consumer
(retrieval, summarization, graph traversal all depend on `fact` having content).

**Why this priority**: This is the primary data-quality gap the issue exists to close. An
unusable edge silently entering storage is worse than an edge being dropped and counted, since a
dropped-and-counted item is at least visible in the chunk result telemetry.

**Independent Test**: Feed a synthetic extraction response containing one edge with `fact: ""`
(or whitespace-only) alongside N valid edges through `knowledge_process_chunk`. Verify the chunk
succeeds, the blank-fact edge is absent from storage, and it is reflected in a drop counter.

**Acceptance Scenarios**:

1. **Given** an extraction response with an edge whose `fact` is `""`, **When** the chunk is
   processed, **Then** the chunk succeeds, that edge is not persisted, and it is counted in
   `edges_dropped_malformed`.
2. **Given** an extraction response with an edge whose `fact` is whitespace-only (e.g. `"   "`),
   **When** the chunk is processed, **Then** the same outcome as Scenario 1 applies.
3. **Given** an extraction response with an edge whose `fact` is a non-empty, non-whitespace
   string, **When** the chunk is processed, **Then** the edge is persisted normally (no
   regression for valid edges).

---

### User Story 2 - Blank edge endpoints are counted correctly (Priority: P2)

An extraction LLM returns an edge whose `source_name` or `target_name` is present but empty or
whitespace-only. This item deserializes fine today and is not caught by any explicit check for
blankness (as opposed to the existing "does this name resolve to a known entity" check that feeds
`edges_dropped_unresolvable`).

**Why this priority**: Related to User Story 1 but lower priority because a blank endpoint name is
more likely to already be caught incidentally by existing entity-resolution logic than a blank
`fact` is by anything. It still needs an explicit, deliberate check.

**Independent Test**: Feed a synthetic extraction response containing one edge with a blank
`source_name` or `target_name` alongside N valid edges through `knowledge_process_chunk`. Verify
the chunk succeeds, the edge is absent from storage, and it is reflected in
`edges_dropped_malformed`.

**Acceptance Scenarios**:

1. **Given** an extraction response with an edge whose `source_name` is `""` or whitespace-only,
   **When** the chunk is processed, **Then** the chunk succeeds, the edge is not persisted, and it
   is counted in `edges_dropped_malformed`.
2. **Given** the same for `target_name`, **When** the chunk is processed, **Then** the same
   outcome as Scenario 1 applies.

---

### User Story 3 - Eval scoring reflects what ingest would actually store (Priority: P2)

An eval corpus run contains chunks whose extraction responses include a blank-name entity or a
blank-fact/blank-endpoint edge. Because `crates/eval/src/runner.rs` reads `outcome.result`
directly rather than going through `episode.rs`'s filtering, the eval path today scores against a
superset of what production ingest would ever persist. This makes eval numbers not directly
comparable to what a real ingest run produces.

**Why this priority**: Correctness of the eval harness itself, not a user-facing defect — lower
urgency than the two storage-facing gaps above, but still required for eval results to be
trustworthy.

**Independent Test**: Run the eval harness over a corpus engineered to include at least one
blank-name entity and one blank-fact edge in some chunk's extraction response. Verify the item set
scored matches what `knowledge_process_chunk` would have persisted for the same input.

**Acceptance Scenarios**:

1. **Given** an eval corpus whose extraction response for some chunk contains a blank-name entity,
   **When** the eval run scores that chunk, **Then** the blank-name entity is excluded from
   scoring, matching what the ingest path would store.
2. **Given** the same corpus also contains a blank-fact edge, **When** the eval run scores that
   chunk, **Then** the blank-fact edge is likewise excluded from scoring.

---

### Edge Cases

- An edge with a blank `fact` *and* a blank `source_name`/`target_name` simultaneously — dropped
  once, counted once (not once per violated field).
- A `fact`, `source_name`, or `target_name` consisting entirely of non-space whitespace (tabs,
  newlines) — must be treated as blank, not merely a literal empty string check.
- An entity with a blank `name` continues to be counted exactly once end-to-end, even after this
  issue's changes move validation earlier in the pipeline (see FR-004) — this is the existing,
  already-correct behavior from #342 FR-007, and this issue must not regress it into a double
  count.
- A chunk where every edge in the response is dropped for blank fields — the chunk still succeeds
  (no edges persisted, all counted), consistent with #342's per-item (not per-chunk) failure model.
- Both the Anthropic and OAI-compatible extraction paths must exhibit identical behavior for all of
  the above, since the validation applies at all four parse call sites.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: An edge whose `fact` is empty or whitespace-only (per `str::trim().is_empty()`
  semantics, consistent with the existing entity-name check) MUST be dropped before it reaches
  storage and MUST be counted in `edges_dropped_malformed`.
- **FR-002**: An edge whose `source_name` or `target_name` is empty or whitespace-only MUST be
  dropped before it reaches storage and MUST be counted in `edges_dropped_malformed` — not
  `edges_dropped_unresolvable`. `edges_dropped_unresolvable` (episode.rs:27) reports a
  graph-state-dependent outcome (an edge whose endpoints did not resolve against the *current*
  graph, but might resolve later or elsewhere); a blank endpoint name can never resolve in any
  graph, at any time, so it is an invalid item, not an unresolved one. This also keeps FR-002
  consistent with FR-001, which routes a blank `fact` to `edges_dropped_malformed` on identical
  reasoning — see Assumptions.
- **FR-003**: The validation added for FR-001 and FR-002 MUST apply uniformly at all four
  extraction-response parse sites — Anthropic entities, Anthropic edges, OAI-compatible entities,
  OAI-compatible edges (`salvage_items` call sites, `crates/core/src/extractor.rs:1034`, `:1073`,
  `:2105`, `:2134`) — per #342 FR-002's precedent of uniform behavior across both providers.
- **FR-004**: No item MUST ever be counted more than once toward `entities_dropped_malformed` or
  `edges_dropped_malformed`. If blank-field validation is implemented at parse time (inside
  `salvage_items` or equivalent), any existing downstream check that would otherwise also catch
  the same condition — specifically `episode.rs`'s empty-name `retain` (episode.rs:254) — MUST be
  adjusted (e.g. reduced to a defensive no-op, or removed and its invariant re-verified by test)
  so the two layers remain disjoint by construction, re-establishing the invariant documented at
  episode.rs:243-252. Once this move happens, a blank-name entity MUST still be counted exactly
  once in `entities_dropped_malformed` — the counter itself does not change, only the layer that
  populates it (episode.rs:254's `retain` today; parse time after this issue) — preserving #342
  FR-007's guarantee that missing, `null`, and empty-string `name` all produce a single observable
  outcome at the `knowledge_process_chunk` result.
- **FR-005**: The eval path (`crates/eval/src/runner.rs`) MUST score the same filtered item set
  that the ingest path would persist for the same extraction response — i.e., blank-name entities
  and blank-fact/blank-endpoint edges MUST be excluded from eval scoring exactly as they would be
  dropped from storage. This is a direct consequence of FR-004's placement choice, not independent
  work: `runner.rs:319` consumes the extractor's output (`outcome.result`) directly, upstream of
  `episode.rs`'s `retain`, which is exactly why blank-name entities reach eval today. Placing the
  blank-field check at parse time (inside `salvage_items`, per FR-004) puts it upstream of *both*
  `episode.rs` and `runner.rs`, so eval and ingest see the same filtered set with no `runner.rs`
  change required. If an implementation instead keeps the check downstream in `episode.rs`, FR-005
  still holds, but only by duplicating the same logic in `runner.rs` — which is the outcome parse-
  time placement is meant to avoid.
- **FR-006**: `specs/342-salvage-malformed-extracted-items/spec.md:29` MUST be corrected: it
  states "`ExtractedEntity.name` and `ExtractedEdge.name` are bare `String`", but `ExtractedEdge`
  has no `name` field — it has `source_name`, `target_name`, and `fact` (all bare, non-defaulted
  `String`s; `crates/core/src/types.rs:151-167`). The correction must name the actual fields while
  preserving the sentence's substance (all three are bare non-defaulted `String`s, so the
  described failure mode is otherwise accurate).
- **FR-007**: The same spec file's Source References section MUST have "precedent tolerant
  behavior" corrected to "precedent-tolerant behavior".
- **FR-008**: The same line's citation of `crates/core/src/episode.rs:218` for the empty-name
  `retain` MUST be corrected to its current location (`episode.rs:254` as of this issue, or
  wherever the `retain` lands after FR-004's change — whichever is accurate once this issue's
  changes are implemented).

### Key Entities

- **`ExtractedEdge`** (`crates/core/src/types.rs:151-167`): `source_name: String`, `target_name:
  String`, `fact: String`, plus optional relation-type fields. This issue adds semantic
  (non-blank) validation to the three `String` fields.
- **`ExtractedEntity`** (`crates/core/src/types.rs:125-137`): `name: String`, already validated
  for blankness downstream (episode.rs:254); this issue changes *where* that validation happens
  (per FR-004) but not *whether* it happens.
- **Drop counters** (`crates/core/src/episode.rs`): `entities_dropped_malformed` (:47),
  `edges_dropped_malformed` (:56), `edges_dropped_unresolvable` (:27) — existing telemetry fields
  on the chunk-processing result that this issue's drops must be reflected in.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An extraction response containing an edge with `fact: ""` yields a chunk that
  succeeds, does not persist that edge, and reports it in a drop counter.
- **SC-002**: An extraction response containing an edge with a blank `source_name` or
  `target_name` yields the same outcome as SC-001, counted in `edges_dropped_malformed`.
- **SC-003**: An extraction response containing an entity with a blank `name` is counted exactly
  once in `entities_dropped_malformed`, never twice, after this issue's changes.
- **SC-004**: SC-001 through SC-003 hold identically on both the Anthropic and OAI-compatible
  extraction paths.
- **SC-005**: An eval run over a corpus containing blank-name entities or blank-field edges scores
  the same item set that a production ingest of the same corpus would store.

## Assumptions

- **FR-002's counter is settled as `edges_dropped_malformed`, not left to the implementer.**
  `edges_dropped_unresolvable` is a graph-state-dependent signal (episode.rs:27, per #281
  FR-004/FR-005) — an operator reads it as "my extraction is naming entities my graph does not yet
  know about," a condition that can change with more data or resolution tuning. A blank endpoint
  name can never resolve, in any graph, at any time — it is an invalid item, not an unresolved
  reference, and counting it as unresolvable would point operators at a remedy that cannot work.
  This is also the only choice compatible with FR-003/FR-004's placement of the check at parse
  time: resolution runs later, in `episode.rs`, so an edge rejected at parse time never reaches the
  code path that populates `edges_dropped_unresolvable` in the first place.
- **The concrete implementation shape (e.g. the `is_valid` predicate signature proposed in the
  issue, exact call-site wiring, whether `episode.rs`'s `retain` is deleted or kept as a defensive
  no-op) is a Plan/Implement-stage decision, not fixed by this spec.** The issue's suggested
  approach — threading a validity predicate through `salvage_items` so all four call sites share
  one mechanism — is a reasonable default and satisfies FR-001 through FR-005 in one pass (moving
  validation to parse time means both `episode.rs` and `runner.rs`, which both consume the same
  parsed result, automatically see the same filtered set — resolving FR-005 as a side effect
  rather than requiring separate `runner.rs` changes, per FR-005's own text). Research should
  confirm this before Plan commits to it.
- `str::trim().is_empty()` is the intended blankness test, matching the existing entity-name check
  at episode.rs:254 exactly (no new whitespace-detection semantics are introduced).
- This issue does not change entity-side *behavior* (blank names were already dropped and counted
  correctly before this issue); it only guards against a mechanism change (parse-time validation)
  regressing that existing correctness into a double count.
- FR-006 through FR-008 (spec-text corrections to the #342 spec file) are implementation work for
  this issue's PR, not something the Specify stage edits directly — `specs/342-.../spec.md` is a
  different feature's committed artifact and editing it outside this issue's own PR would create
  an unrelated, undated change to another issue's history.

## Out of Scope

- Any change to entity-side blank-name *behavior* — only the layer at which it's checked may move
  (FR-004); the observable outcome (one count in `entities_dropped_malformed`) must not change.
- Retrying extraction when a blank field is detected — out of scope for the same reason #342 ruled
  out retries: the failure is expected model behavior, not a transient error worth retrying.
- Any change to `entity_type` or `relation_type` validation — those are governed by strict-mode
  reclassification (#310/#312) and are unaffected by this issue.
- Any change to whole-response structural failure handling (a non-JSON or key-missing response) —
  unaffected, per #342 FR-005's precedent.

## Source References

- `crates/core/src/extractor.rs:973` — `salvage_items`, unchanged since #342, four call sites at
  `:1034`, `:1073`, `:2105`, `:2134`.
- `crates/core/src/episode.rs:254` — existing empty-name `retain`; disjointness invariant
  documented at `:243-252`.
- `crates/core/src/episode.rs:27` (`edges_dropped_unresolvable`), `:47`
  (`entities_dropped_malformed`), `:56` (`edges_dropped_malformed`) — the three existing counters.
- `crates/core/src/types.rs:151-167` — `ExtractedEdge` (`source_name`, `target_name`, `fact`, no
  `name` field).
- `crates/core/src/types.rs:125-137` — `ExtractedEntity` (`name` field).
- `crates/eval/src/runner.rs:319` — `.map(|outcome| outcome.result)`, the eval-path bypass FR-005
  addresses.
- `specs/342-salvage-malformed-extracted-items/spec.md:29`, `:239` — the two text errors FR-006
  through FR-008 correct.
- Related: #342 (the salvage mechanism this extends), #344 (the PR where this finding was raised
  and deferred), #340 (the original community report), #281 (edge endpoint enum), #310/#312
  (strict-mode reclassify-not-drop).
