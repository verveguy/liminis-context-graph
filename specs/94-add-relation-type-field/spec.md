# Feature Specification: Add `relation_type` Field to Edges — Separate Normalized SCREAMING_SNAKE_CASE Predicate Alongside `fact` Paraphrase

**Feature Branch**: `fabrik/issue-94`
**Created**: 2026-05-25
**Status**: Draft
**Input**: Quality validation 2026-05-25 against demo-notebook after merging liminis-graph#92 (prompts port). SCREAMING_SNAKE_CASE share of relation labels went **1% → 0%** despite being a stated success criterion of #92. Root cause: #92 implemented the `fact` paraphrase half of graphiti's edge contract but not the separate normalized `relation_type` predicate half. The edge schema lacks the field, and the LLM output isn't asked for or stored as one.

## Background

Graphiti's edge extraction (`graphiti_core/prompts/extract_edges.py:edge`) instructs the LLM to produce **two pieces** for every relationship:

1. A **`fact`**: paraphrase of the source sentence describing the relationship in natural language.
2. A **`relation_type`**: a normalized SCREAMING_SNAKE_CASE predicate (e.g. `AUTHORED`, `WORKS_AT`, `LIVES_IN`), drawn from the supplied `FACT_TYPES` vocabulary when an ontology declares one, or derived from the predicate of the source sentence when not.

These serve different purposes:

- `fact` is the human-readable / search-friendly statement.
- `relation_type` is the queryable index: "find all `AUTHORED` edges" is only possible if a consistent token exists across all such edges.

liminis-graph#92's prompts-port implementation landed paraphrase-style facts (a real quality win — `"Children of Time won the Arthur C. Clarke Award"` is a useful claim) but the edge schema in lbug + the IPC contract + the WAL JSONL format have no `relation_type` field. Live evidence from the post-port snapshot: 108 edges, 0% match the `^[A-Z][A-Z0-9_]*$` SCREAMING_SNAKE_CASE pattern, because every value going into the `fact` field is prose.

The 1% baseline figure came from a small number of pre-port labels that happened to be all-uppercase by accident (`"Won"`, `"Set"`). Even those are gone now that the LLM is producing proper paraphrases. Without a separate field, there is structurally nowhere for SCREAMING_SNAKE_CASE labels to live.

**Why this matters:**

- Queryability is the point of a knowledge graph. Without normalized predicates, queries like "show me everyone who authored a book" or "show me all employment relationships" require fuzzy text matching across hundreds of paraphrase variants — exactly the problem graphiti's two-piece edge design solves.
- The ontology work (liminis-graph#83) declares relation-type vocabularies — `AUTHORED`, `AFFILIATED_WITH`, etc. — that today have nowhere to land on actual edges. Ontology constraint enforcement (FR-006 of #83) is currently unverifiable for relation types because the relation type isn't stored.
- This is the closing piece of the prompts-port quality story. Without it, the comparison harness reports a regression on the SCREAMING_SNAKE_CASE metric and the cutover-plan Stage 3 quality bar against graphiti is incomplete.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Every Edge Has a Normalized SCREAMING_SNAKE_CASE Predicate (Priority: P1)

After this issue lands, every edge produced by `do_extract_edges` MUST have a `relation_type` field populated with a SCREAMING_SNAKE_CASE token (e.g. `AUTHORED`, `LIVES_IN`, `IS_FOLLOWED_BY`). The `fact` field continues to hold the natural-language paraphrase.

**Why this priority**: This is the core feature — without it, none of the quality / queryability story works.

**Independent Test**: Ingest 10 chunks asserting varied authorship phrasings ("Alice wrote X", "Bob authored Y", "Carol penned Z"). Query the resulting edges. Assert: each edge has a non-empty `relation_type` field, all 10 edges share the same `relation_type` value (e.g. `AUTHORED`), and that value matches `^[A-Z][A-Z0-9_]*$`.

**Acceptance Scenarios**:

1. **Given** a chunk producing entity relationships, **When** ingested, **Then** each resulting edge has both a `fact` (paraphrase) and a `relation_type` (SCREAMING_SNAKE_CASE) field populated.
2. **Given** multiple sentences expressing the same logical relationship in different phrasings, **When** extracted, **Then** the resulting edges share a single `relation_type` value (e.g. all `WROTE` / `AUTHORED`).
3. **Given** a relationship whose predicate doesn't map cleanly to a verb (e.g. "Alice is Bob's neighbor"), **When** extracted, **Then** the extractor derives a sensible SCREAMING_SNAKE_CASE label (e.g. `NEIGHBOR_OF`) rather than emitting prose.
4. **Given** the LLM returns a relation_type with mixed case or whitespace ("Authored By", "lives in"), **When** post-processed, **Then** it's normalized to `AUTHORED_BY` / `LIVES_IN` deterministically.

---

### User Story 2 — Ontology Relation-Types Drive the `relation_type` Field (Priority: P1)

When an ontology declares relation types (e.g. `{AUTHORED, AFFILIATED_WITH, CITES}`), edges MUST use those values for `relation_type` whenever the relationship matches, in preference to the LLM's free-form derivation.

**Why this priority**: liminis-graph#83 merged ontology relation-type vocabularies that currently have nowhere to land on actual edges. This story gives them the landing field and activates #83's constraint enforcement for relation types.

**Independent Test**: Configure a workspace ontology with `{AUTHORED, AFFILIATED_WITH}`. Ingest "Alice wrote a paper while at Acme." Assert two edges: `Alice --AUTHORED--> paper` and `Alice --AFFILIATED_WITH--> Acme`. Assert the `relation_type` values are exactly those strings, not derivatives like `WROTE` or `WORKS_AT`.

**Acceptance Scenarios**:

1. **Given** an ontology with declared relation types, **When** ingestion runs, **Then** matching edges use the declared types in `relation_type`.
2. **Given** strict-mode ontology, **When** an extracted edge doesn't match any declared relation type, **Then** the edge is dropped (per FR-006 of #83) — which is already covered by ontology validation; this just adds `relation_type` as the validation target.
3. **Given** open-mode ontology, **When** an extracted edge doesn't match any declared relation type, **Then** the edge is kept and `relation_type` is the LLM-derived SCREAMING_SNAKE_CASE label.

---

### User Story 3 — `relation_type` Is Surfaced Through the Existing IPC Surface (Priority: P2)

Existing IPC methods that return edge data — `knowledge_list_relationships`, `knowledge_find_relationships`, `knowledge_get_entity_neighbors`, etc. — MUST surface the `relation_type` field in their responses so callers can use it.

**Why this priority**: Surfacing the field is the minimum contract; filtering by it can be a follow-up. Without surfacing it, no caller can benefit from the new predicate.

**Independent Test**: After ingestion, call `knowledge_list_relationships`. Assert every edge object in the response includes a non-null `relation_type` field alongside `fact`.

**Acceptance Scenarios**:

1. **Given** any IPC method returning edges, **When** called, **Then** every edge in the response includes a `relation_type` field (alongside `fact`).
2. **Given** legacy edges ingested before this issue (with no `relation_type` in lbug), **When** an IPC method returns them, **Then** the response includes `relation_type: null` or empty string rather than failing.

---

### User Story 4 — WAL Entries Capture `relation_type` for Replay Correctness (Priority: P1)

The application WAL (populated since liminis-graph#74) MUST include `relation_type` in the JSONL line for every edge-creation mutation. Replay against a fresh DB MUST reproduce identical edges with the same `relation_type` values.

**Why this priority**: WAL is the canonical backup. If `relation_type` lives only in the live DB and not the WAL, a `knowledge_rebuild_from_wal` would silently drop the field on every replayed edge — corrupting the recovery story.

**Independent Test**: Ingest several chunks, dump the WAL JSONL, wipe the DB, replay the WAL via `knowledge_rebuild_from_wal`, then query edges. Assert `relation_type` values are bit-identical between original and post-replay edges.

**Acceptance Scenarios**:

1. **Given** an ingestion that produces edges with `relation_type`, **When** inspecting the WAL JSONL, **Then** each edge-mutation line contains the `relation_type` value.
2. **Given** a populated WAL replayed against an empty DB, **When** edges are queried after replay, **Then** every edge has the same `relation_type` as it had pre-rebuild.

---

### Edge Cases

- **LLM returns `relation_type: ""` or omits the field.** Treat as unknown; in open-mode ontology, derive from the `fact` text (last-resort fallback). In strict mode, drop the edge.
- **LLM returns the same predicate in different cases across episodes** (e.g. `Authored` vs `AUTHORED` vs `authored_by`). Normalizer collapses all to `AUTHORED` (or `AUTHORED_BY` if the predicate is genuinely different).
- **Legacy edge query after schema migration.** Edges ingested before this issue have no `relation_type`. IPC response includes `relation_type: null` or empty string; queries don't filter them in unless explicitly asked.
- **Migration via WAL replay.** A WAL pre-this-issue contains no `relation_type`. Replaying it produces edges with `relation_type: null` — i.e. the same gap; expected.
- **Ontology declares a relation type that doesn't match anything the LLM ever produces.** Inert; no enforcement applies if no edges match. The ontology is a vocabulary, not a quota.
- **A single fact has two predicates conjunctively** ("Alice both wrote and edited the book"). The extractor's job is one relationship per edge; produce two edges with `WROTE` and `EDITED`. Existing per-edge structure already handles this.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The lbug schema MUST add a `relation_type` STRING property to the canonical edge representation in `liminis-graph-core/src/schema.rs`.
- **FR-002**: Schema migration: existing workspaces with edges lacking `relation_type` MUST not break. Reads return `relation_type: null` (or empty string) for legacy edges; new writes populate it.
- **FR-003**: The Anthropic edge-extraction prompt MUST be updated to ask the LLM for `relation_type` in addition to the existing `fact` (graphiti's `extract_edges` prompt already does this — verify the Rust port's `extract_edges.txt` and update if not present).
- **FR-004**: The Rust edge parser MUST extract both `fact` and `relation_type` from the LLM's JSON response.
- **FR-005**: A post-process normalizer MUST convert any LLM output to canonical SCREAMING_SNAKE_CASE: uppercase, replace non-alphanumeric with `_`, collapse runs of `_`, trim leading/trailing `_`. Log at debug level when normalization changes the input.
- **FR-006**: When an ontology is loaded and the normalized `relation_type` matches one of the declared relation types (case-insensitive after normalization), use the ontology's canonical form. When strict-mode and no match, drop the edge.
- **FR-007**: The IPC contract (response shapes for `knowledge_list_relationships`, `knowledge_find_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source`, etc.) MUST include `relation_type` in every edge object.
- **FR-008**: The WAL JSONL writer MUST include `relation_type` in every edge-create mutation line.
- **FR-009**: `knowledge_rebuild_from_wal` MUST round-trip `relation_type` faithfully.
- **FR-010**: New tests MUST cover: every edge has a populated `relation_type` after ingestion, varied phrasings collapse to the same `relation_type`, ontology values are honored, WAL round-trip preserves the value.

### Key Entities

- **`relation_type`**: A normalized SCREAMING_SNAKE_CASE string token representing the semantic predicate of a relationship edge (e.g. `AUTHORED`, `LIVES_IN`, `AFFILIATED_WITH`). Distinct from `fact`, which is a natural-language paraphrase.
- **Normalizer**: A deterministic post-processing function that converts any LLM-returned predicate string to canonical SCREAMING_SNAKE_CASE.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After this issue lands, re-running the quality comparison harness against a fresh demo-notebook ingestion produces **SCREAMING_SNAKE_CASE share = 100%** of `relation_type` values (the comparison harness must be updated to check `relation_type`, not the `fact` field — see SC-005).
- **SC-002**: A test ingesting 10 chunks with varied phrasings of the same authorship relationship produces 10 edges with identical `relation_type` (e.g. all `AUTHORED` or all `WROTE`).
- **SC-003**: With an ontology declaring `{AUTHORED, CITES, AFFILIATED_WITH}`, edges that match those relationships use exactly those `relation_type` values; in strict mode, non-matching edges are dropped.
- **SC-004**: WAL round-trip: ingest, dump WAL, wipe DB, replay WAL, dump edges — `relation_type` values are bit-identical between the original and the post-replay edges.
- **SC-005**: The comparison harness at `~/dev/liminis-project/demo-notebook/.liminis/quality-baselines/compare.py` is updated to check `relation_type` (not the first-word-of-`fact` proxy) for the SCREAMING_SNAKE_CASE metric. After this issue lands, the metric reports 100%.
- **SC-006**: New tests pass; existing tests pass unchanged.

## Assumptions

- **A1**: Graphiti's `extract_edges.txt` already has the `relation_type` field in its response-schema specification (verified empirically against graphiti's prompts; just needs adoption in the Rust port).
- **A2**: lbug supports adding a property to an existing node type without destructive schema migration. If not, this issue may need a coordinated schema-bump release.
- **A3**: The normalizer's deterministic SCREAMING_SNAKE_CASE rules are sufficient for the vast majority of inputs. Edge cases (special characters, unicode) get logged but don't fail extraction.
- **A4**: Ontology canonical-form substitution is a simple string match after normalization; no synonym resolution or embedding-based matching in v1.
- **A5**: Adding the field is an additive change to the IPC contract — existing clients that don't read `relation_type` continue to work; clients that need it can read the new field.

## Out of Scope

- Backfilling `relation_type` on existing edges in workspaces ingested before this lands. That's a separate "re-derive `relation_type` from the existing `fact`" admin tool.
- IPC filter parameter on `relation_type` (filter edges by relation type). Useful but additive; can be a follow-up.
- Renaming the lbug edge node or restructuring the schema beyond adding the `relation_type` column.
- Changing the LLM model or prompt model parameters.
- A `LCG_RELATION_TYPE_BACKFILL` admin IPC command.

## Source References

- **liminis-graph#92 (merged)**: prompts port that delivered the `fact` paraphrase quality win but didn't deliver the `relation_type` half. This issue closes the gap.
- **liminis-graph#83 (merged)**: ontology support, including relation-type vocabulary. Today its declared relation types have nowhere to land on actual edges; this issue gives them the landing field.
- **Live evidence**: quality comparison 2026-05-25 against demo-notebook (`.liminis/quality-baselines/`) — SCREAMING_SNAKE_CASE share 1% → 0% after #92's merge, confirming the gap.
- **Graphiti source**: `graphiti_core/prompts/extract_edges.py:edge` — already produces `relation_type` in its output schema. The Rust port adopted the system prompt content but didn't carry over the response-schema/parser side.
- **Cutover plan**: `ideas/cutover-plan.md` Stage 3 requires extraction quality to match graphiti's; without `relation_type` it doesn't.
- `liminis-graph-core/src/schema.rs` — edge schema (add `relation_type` field here)
- `liminis-graph-core/src/prompts/extract_edges.txt` — edge extraction prompt (update response schema)
- `liminis-graph-core/src/extractor.rs` — LLM response parser (extract `relation_type`)
- `liminis-graph-core/src/handlers.rs` + Python-side `service_protocol.py` — IPC contract
- `liminis-graph-core/tests/ipc_parity.rs` — IPC parity tests (update for new field)
