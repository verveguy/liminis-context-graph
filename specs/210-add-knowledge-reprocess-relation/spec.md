# Feature Specification: Add `knowledge_reprocess_relation_types` — Fact-Based LLM Relation Classification

**Feature Branch**: `fabrik/issue-210`
**Created**: 2026-07-24
**Status**: Draft
**Input**: User description: "Add `knowledge_reprocess_relation_types` — mirror of `reprocess_entity_types` — that asks the configured extraction LLM to pick exactly one declared relation type (or honestly abstain to `UNCLASSIFIED`) for each in-scope edge, based on the edge's `fact` and the ontology's relation-type menu."

## Background

Relation typing today has no fact-based classifier, which makes typed traversals unreliable. The two existing relation-typing tools both fall short of that job:

- `knowledge_canonicalize_relations` maps each edge's **existing raw predicate** (its `name`, plus a secondary check on `relation_type`) onto the ontology by lexical / alias / keyword / embedding match. Only its fallback promoter reads the `fact` text at all, and that promoter force-assigns every residual edge to its single nearest type with no abstention (e.g. "*X is affiliated with Y*" incorrectly lands on `HOLDS` because it's the closest lexical match, not because it's semantically correct). This approach topped out at ~79% correct classification on a real-world corpus.
- `knowledge_backfill_relation_types` doesn't classify at all — it mints fact-prefix pseudo-types (e.g. deriving a type from the first four words of the fact string) for edges with no `relation_type`. This produces syntactically-present but semantically meaningless types, tracked as a known gap in #205.

Entities already have a real fact-based classifier: `knowledge_reprocess_entity_types` (established in #30, extended with scope control in #177) sends each entity's name/summary to the configured extraction LLM and asks it to pick a declared ontology type. Relations have no equivalent tool. This gap was reported in #204, where a reporter-built prototype doing genuine fact-based LLM classification (reading the edge's `fact` text, offering the ontology's declared relation types with descriptions, and asking the LLM to pick one or abstain) reached **98%** correct classification (6,177 of 6,303 edges) with the remaining 126 honestly marked `UNCLASSIFIED` rather than force-assigned.

Edges are stored in the `RelatesToNode_` node table (`crates/core/src/schema.rs:44-56`), which carries `fact` (the natural-language sentence justifying the edge) and `relation_type` (the typed-traversal label). The ontology's `RelationTypeDef` (`crates/core/src/canonicalize.rs:488-505`, defined in `crates/core/src/ontology.rs`) carries a `name` and an optional `description` per declared relation type — exactly the menu a classifier needs.

This feature adds `knowledge_reprocess_relation_types`, the relation-side twin of `reprocess_entity_types`: for each in-scope edge, classify using the edge's `fact` against the ontology's declared relation types (name + description), writing either a declared type or the honest `UNCLASSIFIED` sentinel — never a nearest-match guess and never a fact-prefix pseudo-type.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Classify Untyped Edges With `scope=untyped` (Priority: P1)

A user has edges with no `relation_type` at all — either freshly ingested before any typing pass ran, or left untouched by earlier tools. The user calls `knowledge_reprocess_relation_types {scope: "untyped"}`. The service reads each untyped edge's `fact`, asks the LLM to pick one of the ontology's declared relation types (or abstain), and writes the verdict: a declared type name, or `UNCLASSIFIED` if the LLM honestly cannot map the fact to any declared type.

**Why this priority**: This is the primary use case and directly subsumes #205's use case — instead of `knowledge_backfill_relation_types` minting a meaningless fact-prefix pseudo-type for every untyped edge, this tool makes an honest, fact-grounded classification attempt first.

**Independent Test**: Seed a graph with 20 edges with `relation_type` NULL or empty string, backed by facts that clearly imply declared ontology relation types (e.g., "Alice authored the report" → `AUTHORED`), and 5 edges with facts that don't correspond to any declared type. Configure an ontology declaring `AUTHORED`, `AFFILIATED_WITH`. Call `knowledge_reprocess_relation_types {scope: "untyped"}`. Assert the 20 edges receive the correct declared type and the 5 ambiguous edges are set to `UNCLASSIFIED`; no edge is left NULL/empty and no fact-prefix pseudo-type appears anywhere.

**Acceptance Scenarios**:

1. **Given** an edge with `relation_type` NULL and `fact` "Brett is affiliated with Acme Corp", **When** `knowledge_reprocess_relation_types {scope: "untyped"}` runs against an ontology declaring `AFFILIATED_WITH`, **Then** the edge's `relation_type` becomes `AFFILIATED_WITH`.
2. **Given** an edge with `relation_type` NULL and a `fact` that does not correspond to any declared relation type, **When** `scope=untyped` runs, **Then** the edge's `relation_type` is set to `UNCLASSIFIED` (not left NULL, not force-assigned to the nearest type).
3. **Given** an edge that already has a non-empty `relation_type` (of any kind — ontology-declared, `UNCLASSIFIED`, or a fact-prefix pseudo-type from backfill), **When** `scope=untyped` runs, **Then** that edge is NOT a candidate and is left unchanged.

---

### User Story 2 — Fix Off-Ontology and Mis-Typed Edges With `scope=off_ontology` (Priority: P1)

A user has edges whose `relation_type` is populated but not a declared type in the current ontology — e.g., `UNCLASSIFIED` sentinels from a prior `canonicalize_relations` run, fact-prefix pseudo-types minted by `backfill_relation_types`, or raw predicate text that was never mapped. The user calls `knowledge_reprocess_relation_types {scope: "off_ontology"}`. The service treats every edge whose current `relation_type` is not in the declared ontology (including NULL/empty and `UNCLASSIFIED`) as a candidate, reclassifies each from its `fact`, and writes a declared type or honest `UNCLASSIFIED`.

**Why this priority**: This is the tool's main correctness lever — genuinely reclassifying the edges that `canonicalize_relations`'s lexical fallback got wrong (the "*affiliated with*" → `HOLDS` example) and the edges `backfill_relation_types` polluted with pseudo-types, using an approach that already demonstrated 98% accuracy in the #204 prototype.

**Independent Test**: Seed a graph with: (a) 10 edges with `relation_type = ''` (untyped); (b) 10 edges with `relation_type` set to a fact-prefix pseudo-type (e.g., `"Brett attended the"`); (c) 5 edges with `relation_type = 'UNCLASSIFIED'`; (d) 5 edges with a correct, ontology-declared `relation_type`. Call `knowledge_reprocess_relation_types {scope: "off_ontology"}`. Assert groups (a), (b), and (c) are all reclassified (to a declared type or `UNCLASSIFIED`); group (d) is untouched.

**Acceptance Scenarios**:

1. **Given** an edge with `relation_type = "HOLDS"` where `HOLDS` is not declared in the current ontology, **When** `scope=off_ontology` runs, **Then** the edge is reclassified from its `fact`.
2. **Given** an edge with `relation_type = "UNCLASSIFIED"` (from a prior pass), **When** `scope=off_ontology` runs, **Then** the edge is a candidate and is reclassified; if the LLM again cannot map it, it remains `UNCLASSIFIED` (no write, no-op).
3. **Given** an edge with `relation_type` already equal to a declared ontology type name, **When** `scope=off_ontology` runs, **Then** that edge is NOT reclassified and is left unchanged.
4. **Given** `scope=off_ontology` and no ontology relation types configured, **When** called, **Then** the service returns a structured error and no edges are modified.

---

### User Story 3 — Full-Graph Reclassification With `scope=all` (Priority: P2)

A user wants to re-run classification over every edge in a group, regardless of current `relation_type` — for example after a significant ontology change. They call `knowledge_reprocess_relation_types {scope: "all"}`. Every edge in the group is classified from its `fact`; edges whose LLM-assigned type matches their current `relation_type` are left unchanged (no-op, no WAL entry).

**Why this priority**: P2 — this is the most expensive scope (every edge is classified) and is appropriate for deliberate, occasional graph-wide rationalization rather than routine use.

**Independent Test**: Seed a graph with a mix of correctly-typed, off-ontology, and untyped edges. Run `scope=all`. Assert every edge is fed to the LLM; correctly-typed edges are unchanged; all others are corrected to a declared type or `UNCLASSIFIED`.

**Acceptance Scenarios**:

1. **Given** an edge already correctly typed `AUTHORED`, **When** `scope=all` runs and the LLM re-confirms `AUTHORED`, **Then** the edge's `relation_type` is unchanged and no WAL entry is written for it.
2. **Given** `scope=all` and no ontology relation types configured, **When** called, **Then** the service returns a structured error; no edges are modified.

---

### User Story 4 — Dry-Run Preview Before Applying (Priority: P2)

A user wants to see what would change before committing. They call `knowledge_reprocess_relation_types {scope: "off_ontology", dry_run: true}`. The service runs classification and returns a plan — per-edge old/new type plus an aggregate per-type breakdown — but writes nothing to the graph.

**Why this priority**: P2 — a quality-of-life feature for careful operators to assess classification quality (including how many edges land on `UNCLASSIFIED`) before writing.

**Acceptance Scenarios**:

1. **Given** `dry_run: true` with any scope, **When** called, **Then** the graph state is identical before and after the call — no `relation_type` mutations, no WAL entries written.
2. **Given** `dry_run: true` with `scope=untyped`, **When** the response is returned, **Then** it includes `would_reclassify_count`, a `plan` array of per-edge `{edge_id, fact, old_type, new_type}` entries, and a per-type breakdown (a count of edges per assigned `new_type`, including a count for `UNCLASSIFIED`).
3. **Given** no edges would change under the given scope, **When** `dry_run: true` is called, **Then** `plan: []`, `would_reclassify_count: 0`, and an empty breakdown.

---

### Edge Cases

- **Any scope with no ontology relation types configured.** Unlike `reprocess_entity_types` (whose `untyped` scope works without an ontology via open-ended classification), relation classification always requires a declared menu of relation types to choose from — the tool's entire purpose is picking from a declared menu or abstaining, so there is no open-ended fallback. All three scopes (`untyped`, `off_ontology`, `all`) return a structured error (`{success: false, error: "..."}`) when the ontology declares no relation types.
- **LLM abstains (returns no assignment) for an edge.** The edge's `relation_type` is set to the literal string `UNCLASSIFIED` — this is a real write (WAL-durable, counted in `reclassified_count`), not a skip. This differs deliberately from `reprocess_entity_types`, where entity classification abstention leaves the entity unchanged. Never force-assign the nearest lexical/embedding match.
- **Edge's current `relation_type` already equals the LLM's verdict** (e.g., already `UNCLASSIFIED` and the LLM abstains again, or already correctly typed under `scope=all`). No write, no WAL entry, counted in `unchanged_count` not `reclassified_count` — this is what makes re-runs idempotent.
- **`scope=untyped` candidate definition.** An edge is untyped when `relation_type IS NULL OR relation_type = ''` — the same predicate `backfill_relation_types` already uses (`crates/core/src/backfill.rs`), so this tool's `untyped` scope directly supersedes backfill's candidate set with real classification instead of pseudo-typing.
- **`scope=off_ontology` candidate definition.** An edge qualifies when its `relation_type` is untyped (per above) OR is non-empty but not a member of the ontology's declared relation-type names — this naturally covers `UNCLASSIFIED` sentinels, fact-prefix pseudo-types from backfill, and any unmapped raw predicate, without needing special-case handling for any of them.
- **Concurrent ingestion.** The method acquires the writer lock for its write phases, same as `reprocess_entity_types`; concurrent `add_episode` or other write operations queue behind it.
- **Large edge counts.** Classification and writes are batched (mirroring `reprocess_entity_types`'s `REPROCESS_BATCH_SIZE` phase-split) so large graphs do not exhaust memory and partial progress is WAL-durable if the LLM fails mid-run.
- **LLM unavailable / call fails mid-run.** Edges already reclassified before the failure keep their new `relation_type` (WAL-durable); the call returns a structured error; remaining unclassified edges are untouched and can be retried.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A new `Extractor` trait method `classify_relations` MUST accept a batch of `(fact, current_type)` pairs and an `allowed_types: &[(name, description)]` menu (the declared ontology relation types), and return a `Vec<String>` of the same length — one verdict per input edge. An empty string in the result means the LLM abstained for that edge. This MUST be implemented for `AnthropicExtractor` and `LlmRouter`, mirroring the existing `classify_entities` method's structure.
- **FR-002**: `knowledge_reprocess_relation_types` MUST require a configured ontology with at least one declared relation type for **all** scope values (`untyped`, `off_ontology`, `all`) — there is no open-ended/unconstrained classification mode for relations. If no relation types are declared, the call returns a structured error `{success: false, error: "..."}` and modifies nothing.
- **FR-003**: The method MUST accept an optional `scope` field with valid values `"untyped"` (default), `"off_ontology"`, `"all"`. An unrecognized value MUST return a structured error.
- **FR-004**: `scope=untyped` MUST select edges where `relation_type IS NULL OR relation_type = ''` (matching `backfill_relation_types`'s existing candidate predicate).
- **FR-005**: `scope=off_ontology` MUST select every `untyped` candidate (per FR-004) plus every edge whose `relation_type` is non-empty but not a member of the ontology's declared relation-type names (this includes prior `UNCLASSIFIED` sentinels and fact-prefix pseudo-types, with no special-casing required).
- **FR-006**: `scope=all` MUST select every edge in the named `group_id`, regardless of current `relation_type`.
- **FR-007**: For each candidate edge, the LLM MUST be asked to classify using only the edge's `fact` text and the ontology's declared relation types (name + description) as the allowed menu. The LLM MUST NOT be permitted to invent a type outside that menu.
- **FR-008**: If the LLM abstains (empty string per FR-001) for an edge, the edge's `relation_type` MUST be written as the literal string `UNCLASSIFIED`. This is a real, WAL-durable write, not a skip — never force-assign the abstained edge to the nearest declared type.
- **FR-009**: If the computed verdict for an edge (a declared type name or `UNCLASSIFIED`) is identical to the edge's current `relation_type`, no write MUST occur and no WAL entry MUST be created for that edge (idempotency).
- **FR-010**: `fact`, `name`, `episodes`, embeddings, and all other `RelatesToNode_` fields MUST NOT be modified by this operation. Only `relation_type` changes.
- **FR-011**: The method MUST accept an optional `dry_run` field (bool, default `false`). When `true`, no mutations are written to the graph and no WAL entries are created.
- **FR-012**: The `dry_run: true` response MUST include `would_reclassify_count: int`, a `plan` array of `{edge_id: string, fact: string, old_type: string | null, new_type: string}` entries for every edge that would change, and a per-type breakdown object mapping each assigned `new_type` (including `"UNCLASSIFIED"`) to its count among the planned changes.
- **FR-013**: The non-dry-run success response MUST include at minimum `{success: true, reclassified_count: int, unchanged_count: int, group_id: string}`.
- **FR-014**: Processing MUST be batched (read-lock page candidates → batch LLM calls over `fact` → batched write-lock `SET relation_type` + `wal_flush`), mirroring `reprocess_entity_types`'s phase-split, to avoid OOM on large graphs and to keep partial progress WAL-durable.
- **FR-015**: `knowledge_reprocess_relation_types` MUST be registered in the MCP tool registry (`crates/service/src/mcp/tools.rs`) as `Scope::Write`, with the registry's total and per-scope count assertions updated accordingly.
- **FR-016**: `knowledge_reprocess_relation_types` MUST be added to `is_streaming_method` so long runs emit MCP progress notifications when a `_progress_token` is supplied, mirroring `knowledge_canonicalize_relations` and `knowledge_backfill_relation_types`.
- **FR-017**: The `group_id` parameter (string, optional, default `"liminis"`) MUST scope candidate selection, mirroring the existing `reprocess_entity_types` signature.
- **FR-018**: The method MUST acquire the writer lock for its write phases, ensuring concurrent `add_episode` or other mutating operations are serialized against it.
- **FR-019**: Re-running `knowledge_reprocess_relation_types` with the same scope on a graph where every candidate edge already carries its correct verdict (declared type or `UNCLASSIFIED`) MUST be idempotent: `reclassified_count: 0`, no WAL entries written, no `relation_type` changes.

### Key Entities

- **`scope`** (new parameter): Controls which edges are candidates. `"untyped"` — `relation_type` NULL/empty; `"off_ontology"` — untyped edges plus edges whose `relation_type` isn't a declared ontology type; `"all"` — every edge in the group.
- **`dry_run`** (new parameter): When true, runs classification but writes nothing; returns a plan for review.
- **`classify_relations`** (new `Extractor` trait method): Takes `(fact, current_type)` pairs plus the declared `allowed_types` menu; returns one verdict per edge (empty string = abstain).
- **`RelatesToNode_.relation_type`**: The only field mutated by this operation. Set to a declared ontology relation-type name or the literal `UNCLASSIFIED` sentinel — never a fact-prefix pseudo-type and never a force-assigned nearest match.
- **`plan`** (dry-run response field): Array of `{edge_id, fact, old_type, new_type}` entries describing what would change, plus an aggregate per-type breakdown.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a seeded corpus with a mix of untyped, off-ontology (including `UNCLASSIFIED` and fact-prefix pseudo-typed), and correctly-typed edges, `knowledge_reprocess_relation_types {scope: "off_ontology"}` classifies every off-ontology and untyped edge to either a declared ontology relation type or `UNCLASSIFIED` — no fact-prefix pseudo-type remains anywhere in the graph after the run, and correctly-typed edges are unchanged.
- **SC-002**: Given `scope=off_ontology` or `scope=all` with no ontology relation types configured, the response is `{success: false, error: "..."}` and no edges are modified.
- **SC-003**: Given `dry_run: true`, the response includes a `plan` array and a per-type breakdown (including an `UNCLASSIFIED` count), and a subsequent graph query confirms every edge's `relation_type` is unchanged.
- **SC-004**: Running the same scope twice on a graph where the first run already classified every candidate edge produces `reclassified_count: 0` on the second run (idempotent) — no WAL entries written on the second run.
- **SC-005**: Progress notifications are emitted during a run when the caller supplies a `_progress_token`, consistent with other long-running MCP methods.
- **SC-006**: All new and existing tests pass under `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release`.

## Assumptions

- **A1.** `classify_relations` always requires a non-empty `allowed_types` menu — there is no open-ended/unconstrained classification mode for relations (unlike `classify_entities`, which supports `allowed_types: None` for free-form entity typing). This is a deliberate divergence from the entity-side tool, driven by the fact that a relation-type menu with descriptions is central to how the classifier is prompted.
- **A2.** `scope=untyped`'s candidate predicate (`relation_type IS NULL OR = ''`) exactly matches `backfill_relation_types`'s existing WHERE-guard predicate, so this tool is a drop-in, higher-quality replacement for backfill's use case on that same edge population.
- **A3.** `UNCLASSIFIED` is treated as "off-ontology" (not a declared type), so it is picked up automatically by `scope=off_ontology`'s general rule with no special-casing — an edge can cycle through repeated `off_ontology` runs (e.g., after ontology growth) and be reclassified once new types become available.
- **A4.** The LLM's abstention write (`UNCLASSIFIED`) is a genuine mutation for accounting purposes — it counts toward `reclassified_count` when it changes the edge's current value, and is WAL-durable like any other write. This is unlike `reprocess_entity_types`, where abstention leaves the entity's labels untouched.
- **A5.** Only `RelatesToNode_.relation_type` is modified. `fact`, `name`, `episodes`, embeddings, and all other edge properties are untouched.
- **A6.** `group_id` scoping and the writer-lock/batching discipline are inherited directly from `reprocess_entity_types`'s established pattern (`crates/core/src/handlers.rs`, phase-split with `REPROCESS_BATCH_SIZE`).
- **A7.** The classification batch size reuses (or is configured alongside) the existing `REPROCESS_BATCH_SIZE` constant used by `reprocess_entity_types` — no new batch-size configuration is introduced.

## Out of Scope

- Changing `knowledge_canonicalize_relations`'s lexical/embedding matching logic — this feature adds a new, separate tool rather than modifying that one.
- Removing or deprecating `knowledge_backfill_relation_types` — tracked separately as #205, which is `blockedBy` this issue.
- Local/non-LLM extractor implementations of `classify_relations` (only `AnthropicExtractor` and `LlmRouter` are required here).
- New UI changes in liminis-app (wiring a "Reclassify Relations" button is a liminis-app concern).
- LLM cost tracking or per-call cost reporting.
- Multi-group reprocessing in a single call (call per group if needed).
- Confidence scoring or partial/multi-label relation assignment — the classifier picks exactly one declared type or abstains.

## Source References

- `crates/core/src/schema.rs:44-56` — `RelatesToNode_` table definition; `fact` (input to classification) and `relation_type` (the field this feature writes).
- `crates/core/src/canonicalize.rs:488-505` — `RelationTypeDef` (`name` + `description`), the ontology's declared relation-type menu.
- `crates/core/src/ontology.rs` — `Ontology::relation_type_names()` (existing helper for off-ontology membership testing, already used analogously for entities).
- `crates/core/src/handlers.rs` — `handle_reprocess_entity_types` (`~L1930-2188`): the phase-split pattern (read-lock page → batch LLM → batched write-lock + `wal_flush`) this feature mirrors for relations.
- `crates/core/src/corrections.rs` — `ReprocessScope`, `list_entities_for_scope`, `is_off_ontology`: the entity-side scope-filtering pattern this feature's relation-side equivalent should follow.
- `crates/core/src/backfill.rs` — `derive_relation_type`, the existing `relation_type IS NULL OR = ''` candidate predicate this feature's `untyped` scope reuses, and the fact-prefix pseudo-typing behavior this feature supersedes for that edge population.
- `crates/core/src/extractor.rs` — `Extractor::classify_entities` (`~L50-54`): the trait method signature and batching convention `classify_relations` should mirror.
- `crates/service/src/mcp/tools.rs` — `ToolSpec` registry (entity twin at `~L432-455`); `is_streaming_method` (`~L614`); scope-bucket count assertions (`~L637-643`) to update.
- `crates/core/tests/ipc_parity.rs` — Tier 1a/1b/1c parity tests; existing `reprocess_entity_types` scope/dry_run test block (`~L2385` onward) to mirror for relations.
- Issue #30 — established `reprocess_entity_types`.
- Issue #177 — extended `reprocess_entity_types` with `scope`/`dry_run`; this feature's direct structural template.
- Issue #204 — reported the classification gap and prototype behind this feature.
- Issue #205 — `backfill_relation_types`' fact-prefix pseudo-typing gap, subsumed by this feature's `untyped` scope; blocked by this issue.
- Issue #163 — established `canonicalize_relations`.
