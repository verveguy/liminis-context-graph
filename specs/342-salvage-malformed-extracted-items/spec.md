# Feature Specification: Salvage malformed extracted items instead of failing the whole chunk

**Feature Branch**: `fabrik/issue-342`
**Created**: 2026-08-04
**Status**: Draft
**Input**: User description: "Work issue for community report #340. `knowledge_process_chunk` fails an entire chunk with `-32000 JSON error: missing field \`name\`` when the extraction LLM emits a single entity or edge without a `name`. A client that treats a chunk error as fatal then loses the whole document — #340 reports a ~40-chunk document lost in full because chunk 13 produced one field-less item."

## Background

`knowledge_process_chunk` deserializes the extraction LLM's entity/edge response as a single
`Vec<T>`. If any one element in that list is missing a required field, the entire vector fails to
deserialize, and the whole chunk — not just the bad item — is rejected with a hard `-32000` error.

Community report #340 hit this in the field: a ~40-chunk document was lost in full because chunk
13's extraction response contained one entity or edge without a `name`. The client treated the
chunk-level error as fatal and abandoned the whole ingest rather than skipping one chunk.

This is confirmed present on `main` at 0.12.0 (originally reported against 0.11.0).

**Root cause.** All four extraction-response parse sites have this same one-shot-`Vec` shape:

| Site | Path | Payload |
|---|---|---|
| `crates/core/src/extractor.rs:948-956` | Anthropic entities | `EntityPayload { entities: Vec<ExtractedEntity> }` |
| `crates/core/src/extractor.rs:984-992` | Anthropic edges | `EdgePayload { edges: Vec<ExtractedEdge> }` |
| `crates/core/src/extractor.rs:1991-2000` | OAI entities | same shape |
| `crates/core/src/extractor.rs:2017-2025` | OAI edges | same shape |

`ExtractedEntity.name` and `ExtractedEdge`'s `source_name`, `target_name`, and `fact` are bare
`String` with no `#[serde(default)]`, so a missing key is a hard deserialization error. The resulting `ParseError` is not salvaged or
retried — `extractor.rs:375-385` (entities) and `:549` (edges) call `emit_extraction_failure(...)`
and then `return Err(e)`, which surfaces to the caller as `-32000`.

**This behaviour is already inconsistent, which is the strongest argument that it's a defect.**
`crates/core/src/episode.rs:254` already drops empty-name entities without complaint:

```rust
extraction.entities.retain(|e| !e.name.trim().is_empty());
```

So today, two payloads carrying the same amount of usable information produce opposite outcomes:

- `{"name": "", "entity_type": "Person"}` → item silently dropped, **chunk succeeds**
- `{"entity_type": "Person"}` (key absent) → **entire document ingest dies**

The tolerant behaviour is already the intended one; the missing-key case just never reaches it,
because deserialization fails before `episode.rs` runs.

**Direct precedent.** Issue #314 added `deserialize_summary_or_default` to
`ExtractedEntity.summary` for exactly this reason, and the comment at
`crates/core/src/types.rs:109-112` states the principle: *"losing an entire chunk's entities over
one missing string field is a disproportionate trade."* That fix defaulted a value. `name` cannot
be defaulted — a nameless entity has no identity and no downstream meaning — so the equivalent
remedy here is to **drop the item**, matching what `episode.rs:254` already does for the
empty-string case.

This ships as a 0.12.1 patch release: it costs users data on a released version, and the fix is
narrow enough not to wait for 0.13.0.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - One malformed item no longer sinks the chunk (Priority: P1)

An operator is ingesting a multi-chunk document. The extraction LLM returns a well-formed response
for every chunk except one, where it emits a single entity (or edge) missing its `name`. Today,
that one item kills the entire chunk, the client treats the chunk failure as fatal, and the whole
document ingest is lost. After this change, the malformed item is dropped, every other item in
that chunk's response is processed normally, and the document ingest completes.

**Why this priority**: This is the reported production impact (#340) — data loss on a scale (a
whole document) wildly disproportionate to the trigger (one field on one item).

**Independent Test**: Feed a stubbed extraction response containing N well-formed items and 1
item missing `name` into the entity (or edge) parse path; assert the chunk succeeds, N items are
persisted, and the drop counter reads 1.

**Acceptance Scenarios**:

1. **Given** an Anthropic extraction response with N valid entities and 1 entity missing `name`,
   **When** `knowledge_process_chunk` processes it, **Then** the chunk succeeds, the N valid
   entities are persisted, and the result reports 1 dropped entity.
2. **Given** the same shape of response but for edges instead of entities, **When** the chunk is
   processed, **Then** the chunk succeeds, the N valid edges are persisted, and the result reports
   1 dropped edge.
3. **Given** the same two scenarios above but produced via the OAI-compatible extraction path
   instead of Anthropic, **When** the chunk is processed, **Then** the outcome is identical.
4. **Given** a multi-chunk document where exactly one chunk's extraction response contains a
   malformed item and every other chunk is well-formed, **When** the full document is ingested,
   **Then** the ingest completes for all chunks (this is the regression test for #340's actual
   reported impact).

---

### User Story 2 - Structurally broken responses still fail loudly (Priority: P1)

A response that is not valid JSON at all, or that is missing the `entities`/`edges` key entirely,
is a different failure class from "one item has a bad field" — it means the model didn't follow
the response contract at all, and there is nothing to salvage. This must keep failing exactly as
it does today.

**Why this priority**: Without this boundary, per-item tolerance could silently widen into
accepting garbage responses, masking a real extraction failure as an empty success.

**Independent Test**: Feed a non-JSON body and a JSON body with no `entities`/`edges` key into the
parse path; assert both still return the existing hard error, unchanged from current behavior.

**Acceptance Scenarios**:

1. **Given** an extraction response body that is not valid JSON, **When** it is parsed, **Then**
   the chunk fails with the same error behavior as today.
2. **Given** an extraction response body that is valid JSON but has no `entities` (or `edges`)
   key, **When** it is parsed, **Then** the chunk fails with the same error behavior as today.

---

### Edge Cases

- **Missing vs. empty vs. null `name`**: a `name` field that is absent, present-but-`null`, and
  present-but-`""` currently produce three different outcomes (hard failure, hard failure, and
  silent drop, respectively — see Background). All three MUST reach the same outcome: item
  dropped, drop counted, chunk succeeds. This includes the empty-string case that
  `episode.rs:218` already handles silently today — it must now also be counted (see FR-007).
- **Edge with missing `source_name`/`target_name`**: same bare-`String`, no-default shape as
  `name`, so the same per-item drop logic applies. Post-#281 the tool schema constrains these
  fields to an enum of already-extracted entity names, so a well-behaved model shouldn't emit a
  missing value here — but the fix must not assume the model always behaves. Research should
  check whether the existing `edges_dropped_unresolvable` path already covers this once
  deserialization stops failing first, rather than assuming it needs a separate code path.
- **Every item in a response is malformed**: MUST still be a success (empty extraction) with the
  full item count reflected in the drop counter, not an error — see FR-004 for the rationale.
- **Interaction with strict-mode reclassification** (#310/#312): an item dropped for being
  malformed MUST NOT also be counted as reclassified by the strict-mode entity-type filter in
  `episode.rs`. The two counters track disjoint outcomes for disjoint items.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A malformed item (missing `name`, or any other required-field violation) in an
  extraction response MUST be dropped, and the remaining well-formed items in that response MUST
  be processed normally. The chunk MUST NOT fail because of one malformed item.
- **FR-002**: FR-001 applies to all four parse sites identified in Background: Anthropic entities,
  Anthropic edges, OAI-compatible entities, OAI-compatible edges. A fix covering only the
  Anthropic path is incomplete and does not satisfy this spec.
- **FR-003**: The count of dropped items MUST be surfaced in the `knowledge_process_chunk` result.
  **Decision**: use two separate counters, `entities_dropped_malformed` and
  `edges_dropped_malformed`, rather than one combined `items_dropped_malformed`. Rationale: drops
  happen at distinct parse sites with distinct result structs (entities vs. edges), and the
  existing convention is already per-type (`edges_dropped_unresolvable`) — a combined counter
  would hide which category degraded when only one side of a chunk's extraction misbehaved.
- **FR-004**: A response in which *every* item is malformed MUST be distinguishable from a
  genuinely empty extraction, but MUST NOT be treated as an error. **Decision**: it is an empty
  success with a drop count equal to the number of items submitted. Rationale: the response itself
  parsed successfully (valid JSON, `entities`/`edges` key present) — only its individual elements
  were malformed — and FR-001 already establishes that item-level defects reduce the result set
  rather than failing the chunk; a response of all-bad items is not qualitatively different from a
  response of some-bad items, just quantitatively so. The non-zero drop count (FR-003) is what
  distinguishes this from a response where the model genuinely found nothing to extract.
- **FR-005**: A response that is not valid JSON at all, or that is missing the `entities`/`edges`
  key entirely, MUST keep failing exactly as it does today. This spec narrows per-item tolerance
  only; it does not make the parser accept structurally broken responses. (See User Story 2.)
- **FR-006**: The existing `structured_output.{clean,recovered,malformed}` telemetry MUST stay
  coherent — a response salvaged per-item is explicitly not `clean`. **Decision**: this dimension
  (item-level salvage) is orthogonal to the existing `recovered` classification, which already
  means something specific — a whole-body defensive re-parse (e.g. extracting JSON from a
  markdown-fenced response), tracked via the existing `defensive_parse` flag. Conflating "some
  items were dropped" with "the whole body needed defensive re-parsing" would blur two different
  signals about model behavior into one bucket. The classification MUST therefore distinguish
  "one or more items dropped" as its own outcome, distinct from `clean`, `recovered`, and
  `malformed`. The exact enum value name and precedence rule (e.g. how to classify a response that
  is both defensively re-parsed *and* has dropped items) is an implementation decision for the
  Plan stage to make and record in the ADR, per the issue's own instruction to "classify it and
  say so in the ADR" — but the outcome MUST NOT be reported as `clean`, and MUST be visibly
  different from both `recovered` and `malformed` so the two signals aren't conflated.
- **FR-007**: The drop counters in FR-003 MUST count every item dropped for being malformed,
  regardless of which layer performs the drop. Today, `episode.rs:218` already silently drops
  empty-name entities post-parse without incrementing any counter; per the Edge Cases section,
  that silent case must now also be counted so that missing/`null`/empty-string `name` all produce
  the same observable outcome, including in telemetry. This may mean the counting responsibility
  moves to (or is duplicated at) the parse-time salvage layer — that mechanical decision belongs
  to Plan/Research, not this spec.

### Key Entities

- **`entities_dropped_malformed` / `edges_dropped_malformed`**: new counters on the
  `knowledge_process_chunk` result, following the shape of the existing
  `edges_dropped_unresolvable` counter, reporting how many entities/edges were dropped in that
  chunk for failing required-field validation during extraction-response parsing.
- **`structured_output` telemetry outcome**: existing three-valued classification
  (`clean`/`recovered`/`malformed`) emitted per extraction call; gains a fourth distinguishable
  outcome for "parsed successfully but one or more items were dropped" per FR-006.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A chunk whose extraction response contains one name-less entity among N valid ones
  returns success with the N valid entities persisted and `entities_dropped_malformed` = 1.
- **SC-002**: Same as SC-001 for a name-less edge, with `edges_dropped_malformed` = 1.
- **SC-003**: SC-001 and SC-002 both hold on the OAI-compatible extraction path as well as the
  Anthropic path.
- **SC-004**: A multi-chunk document ingest completes when exactly one chunk's extraction contains
  a malformed item — the regression test for #340's actual reported impact.
- **SC-005**: Structurally invalid responses (not valid JSON, or missing the `entities`/`edges`
  key) still fail exactly as before, proving the added tolerance did not widen into accepting
  garbage.

## Assumptions

- Per-item salvage is preferable to an extraction retry. A retry costs a second API call, and the
  report notes the failure is deterministic for the given chunk content, so a retry would likely
  reproduce it rather than resolve it. If Research finds evidence that retries are non-deterministic
  in practice (e.g. temperature > 0 on the extraction call), it should say so and this assumption
  should be revisited.
- No change to the extraction prompts or tool schemas is needed. The model emitting an occasional
  malformed item is expected, tolerable behavior, not a prompt defect to be eliminated by
  instructing the model differently.
- The three "Decision" points in FR-003, FR-004, and FR-006 are settled at the product/requirements
  level by this spec, per the issue's explicit request that the Specify stage make and justify
  them. The Plan stage still owns the concrete implementation shape (e.g. exact enum variant names,
  precedence rules between simultaneous telemetry signals) within the constraints stated here.

## Out of Scope

- Retrying the extraction call when malformed items are detected (see Assumptions).
- Any change to the extraction prompts or tool/function-calling schemas sent to the model.
- Any change to whole-response structural failure handling (FR-005) — a non-JSON or
  key-missing response is unaffected by this work.
- Any change to the unrelated `SendOutcome::MalformedBody` / `ChatFailure::Malformed` HTTP
  transport-layer error handling (different failure class: transport/HTTP body issues, not
  per-item extraction payload issues).

## Source References

- `crates/core/src/extractor.rs:948-956`, `:984-992`, `:1991-2000`, `:2017-2025` — the four parse
  sites.
- `crates/core/src/extractor.rs:375-385`, `:549` — where `ParseError` currently surfaces as a hard
  chunk failure.
- `crates/core/src/episode.rs:254` — existing silent empty-name drop (the precedent-tolerant
  behavior this spec extends and makes consistent).
- `crates/core/src/types.rs:95-118` — `ExtractedEntity`, and the `deserialize_summary_or_default`
  precedent from #314.
- Community report: #340 (close and move to Shipped on the public triage board when this ships).
- Related: #314 (`summary` default-value precedent), #310/#312 (strict-mode reclassify-not-drop),
  #281 (edge endpoint enum constraining `source_name`/`target_name`), #306 (raw-body capture on
  extraction failure).
