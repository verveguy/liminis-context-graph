# Feature Specification: Close the 0.13.x documentation drift (breaking change, group_ids contract, assertion API, multi-graph entry points)

**Feature Branch**: `fabrik/issue-473`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "The docs site has drifted behind the 0.13.x line. Audited every user-visible entry in the CHANGELOG for 0.13.0 → 0.13.3 against `docs/`; most of it is covered well, but four gaps remain, and two of them are the kind that generate support questions."

## Background

Five releases (0.13.0–0.13.3) were dominated by multi-graph tenancy work — per-group WAL streams, cross-group pointers, group-scoped deletes, per-group ontologies — and the docs site mostly kept pace: `docs/operations.md` covers per-group WAL roots and generation tracking, `docs/ontology.md` covers per-group ontologies (#446), `docs/ipc-mcp-reference.md` covers cross-group edges and the required `group_id` on the canonicalize/backfill tools (#447), and `docs/configuration.md` covers the oversized-`chunk_text` advisory (#407).

An issue-by-issue audit of the CHANGELOG against `docs/` found four remaining gaps, verified against the current codebase and doc tree as of this spec:

1. **A shipped breaking change (#406) is undocumented.** 0.13.2 made `group_ids` required and non-empty on `knowledge_delete_chunk_episode` and `knowledge_delete_by_source`; a caller that previously omitted it now gets an error instead of a (previously cross-group-unsafe) delete. `docs/ipc-mcp-reference.md` mentions `group_ids` exactly once, for an unrelated tool (`knowledge_delete_by_group`, line 178). The only record of this break today is the CHANGELOG and the MCP tool's own description. One release later, #447's equivalent required-parameter change *was* documented in the reference, twice, in bold, with the issue number — #406 got the same treatment nowhere, despite being the more disruptive change (it breaks existing callers rather than tightening new ones).
2. **The omitted-vs-empty `group_ids` contract for read tools (#413) is unstated.** 0.13.2 also settled that an omitted `group_ids` on a read tool means "all groups," while an explicit `[]` is a zero-rows filter — two behaviors that look similar but are opposite in effect, verified in code by commit `1a5fcad` and covered by regression tests in `835c864`. This distinction appears nowhere in the docs.
3. **The direct-assertion API (`knowledge_assert_entity`, `knowledge_assert_relationship`, #379, shipped 0.13.0) has no prose documentation.** It appears in `docs/ipc-mcp-reference.md` only as two table entries (the tool-category table at line 28, the scope table at line 135) — no description of behavior, fields, or upsert semantics. This is not hypothetical: community report #465 (via the orac project) came from someone hitting undocumented retrieval behavior on this exact API, now tracked as #470. Verified in `crates/core/src/handlers.rs`: `handle_assert_entity` embeds only `name` (into `name_embedding`); `summary` and `attributes` are stored and, per #465/#470, lexically indexed but not embedded, so they are not semantically searchable. `handle_assert_relationship` embeds `fact` (into `fact_embedding`) — either the caller-supplied `fact` or, if omitted, one auto-derived as `"{source_name} {predicate} {target_name}"`.
4. **The entry points never mention multi-graph tenancy.** `docs/index.md` and `docs/getting-started.md` have zero occurrences of "group," "multi," or "tenant" (verified by direct grep), despite five releases of work in this area. `README.md` already carries this framing (PR #396) — including the explicit, load-bearing distinction "multi-graph, not multi-tenant" (no auth, no per-tenant isolation; the process boundary is the trust boundary) — but the docs site was never updated to match. `docs/telemetry.md` (372 lines) also has zero group-related mentions; worth a look while in this area to see whether telemetry events actually carry a `group_id` and should say so.

This issue closes those four gaps. It intentionally does not restate what's already covered well elsewhere — it links to it.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Discover that group_ids is now mandatory on the delete tools (Priority: P1)

A developer who has been calling `knowledge_delete_chunk_episode` or `knowledge_delete_by_source` without `group_ids` (the pre-0.13.2 pattern) starts seeing errors after upgrading. They go to `docs/ipc-mcp-reference.md` — not the CHANGELOG — to find out what changed and how to fix their call.

**Why this priority**: Explicitly the highest-priority gap in the source issue. This is an active breaking change with no discoverable remedy outside the CHANGELOG; a caller who doesn't read release notes has no path to understanding the new error.

**Independent Test**: Open `docs/ipc-mcp-reference.md` to the entries for `knowledge_delete_chunk_episode` and `knowledge_delete_by_source` with no other context. Confirm each entry states that `group_ids` is required and non-empty, names the release that introduced the requirement (0.13.2, #406), and states the remedy (pass the caller's own group; there is no default, because a silent default was the defect).

**Acceptance Scenarios**:

1. **Given** a reader on `docs/ipc-mcp-reference.md` who has never read the CHANGELOG, **When** they look up `knowledge_delete_chunk_episode`, **Then** they learn `group_ids` is required and non-empty, that this is a breaking change introduced in 0.13.2, and what to pass instead of omitting it.
2. **Given** the same reader looks up `knowledge_delete_by_source`, **When** they read its entry, **Then** they find the identical treatment (not a cross-reference that leaves them to infer the same rule applies).
3. **Given** a reader who already found the #447 (`knowledge_canonicalize_relations`/`knowledge_backfill_relation_types`) treatment of a required group parameter, **When** they compare it to the new #406 entries, **Then** the presentation (bold callout, issue number, plain statement of the requirement) matches in style.

---

### User Story 2 - Predict what an omitted vs. empty group_ids does on a read tool (Priority: P2)

A developer building a query integration needs to decide whether to omit `group_ids` (to query everything) or pass `[]` (which they might assume also means "everything," or might not be sure about at all). They need one authoritative statement of the contract, not tool-by-tool guesswork.

**Why this priority**: Named in the source issue as one of the two gaps "generate support questions" — a caller who gets this backwards either gets an unexpectedly broad result set or a silently empty one, and has no documentation to check against.

**Independent Test**: Search `docs/ipc-mcp-reference.md` for a single, central statement of the omitted-vs-`[]` contract for read tools. Confirm it exists exactly once (not duplicated per tool) and unambiguously states: omitted → all groups; explicit `[]` → zero rows.

**Acceptance Scenarios**:

1. **Given** a reader on `docs/ipc-mcp-reference.md`, **When** they look for what happens when `group_ids` is left out of a read call, **Then** they find a clear statement that it means "all groups."
2. **Given** the same reader, **When** they look for what an explicit `group_ids: []` does, **Then** they find a clear statement that it means "zero rows" (not "all groups").
3. **Given** a reader checking multiple read tools (e.g. `knowledge_find_entities`, `knowledge_get_nodes_by_group`), **When** they look for this contract, **Then** they are pointed to one central statement rather than finding it repeated (or missing) per tool.

---

### User Story 3 - Understand the direct-assertion API's behavior and retrieval characteristics (Priority: P2)

A developer building a catalogue-shaped or manually-curated graph wants to use `knowledge_assert_entity`/`knowledge_assert_relationship` instead of `knowledge_process_chunk`'s extraction pipeline. They need to know what fields exist, what an upsert does to an existing entity/relationship, how the two tools differ, and — critically — which fields will actually be findable by semantic search versus keyword search.

**Why this priority**: Not hypothetical — this exact gap already produced a community-reported issue (#465), now tracked for a fix at #470. Documenting today's behavior lets a caller design around it instead of discovering it by trial and error.

**Independent Test**: Read the new `knowledge_assert_entity` and `knowledge_assert_relationship` reference entries with no other context. Confirm a reader can answer: what fields does each tool accept; what happens to an existing entity/relationship on a repeat call (upsert semantics, including that an omitted `summary`/`attributes` clears the prior value rather than leaving it untouched); how do these tools differ from `knowledge_process_chunk`; and which specific field is embedded for semantic search on each tool.

**Acceptance Scenarios**:

1. **Given** a reader on the new `knowledge_assert_entity` entry, **When** they read it, **Then** they learn it accepts `name`, `entity_uuid` (optional strict lookup), `labels`, `summary`, `attributes`, and `group_id`; that only `name` is embedded for semantic search today; and that `summary` is stored and full-text searchable but not semantically searchable until #470 lands.
2. **Given** the same reader, **When** they read about repeat calls to the same entity, **Then** they learn it upserts by `(name, group_id)` (or by `entity_uuid` when supplied), and that omitting `summary` or `attributes` on an update clears the previously stored value rather than leaving it as-is.
3. **Given** a reader on the new `knowledge_assert_relationship` entry, **When** they read it, **Then** they learn it accepts `source_name`, `target_name`, `predicate`, `fact` (auto-derived from source/predicate/target if omitted), `attributes`, `relation_type`, `valid_at`, and `group_id`; that `fact` is the field embedded for semantic search; and that endpoint resolution is strictly scoped to the call's own `group_id` (pointing to `knowledge_add_cross_group_edge` for cross-group linking).
4. **Given** a reader trying to decide between the assertion API and `knowledge_process_chunk`, **When** they read either new entry, **Then** they find a plain statement of the difference (direct, caller-controlled upsert of a single entity/edge vs. LLM-driven extraction from unstructured text).

---

### User Story 4 - Learn the product is multi-graph before reaching the reference docs (Priority: P3)

A new reader arrives at the docs site (`docs/index.md` or `docs/getting-started.md`) with no prior context. Today they can read the entire entry point without learning that a single workspace can hold more than one graph — a concept that then appears, unexplained, throughout the rest of the site (WAL roots per group, group-scoped deletes, cross-group pointers).

**Why this priority**: Lower urgency than the three reference-accuracy gaps above — it's a discoverability/onboarding gap rather than an active source of errors — but still user-visible on every fresh visit to the site.

**Independent Test**: Read the first screen of `docs/index.md` and of `docs/getting-started.md`. Confirm each mentions, in plain language matching `README.md`'s framing, that one workspace can hold multiple independent graphs (`group_id`), before the reader reaches `docs/ipc-mcp-reference.md`.

**Acceptance Scenarios**:

1. **Given** a new reader on `docs/index.md`, **When** they read the first screen, **Then** they learn the product can hold several graphs in one workspace, each isolated by `group_id`.
2. **Given** the same reader on `docs/getting-started.md`, **When** they follow the getting-started flow, **Then** they encounter the same concept introduced consistently, not contradicted or reworded into a different vocabulary.
3. **Given** a reader who has just read `README.md`'s "multi-graph, not multi-tenant" framing, **When** they read the same concept on the docs site, **Then** the terminology matches — the docs site does not introduce "tenant" as a synonym for "graph," since README explicitly reserves "tenant" to mean something the product does not provide (auth, per-tenant isolation).
4. **Given** a reader on `docs/telemetry.md`, **When** they check whether telemetry events carry a group dimension, **Then** the doc states the answer either way (documents the `group_id` field if telemetry events carry one, or is deliberately left unchanged if they don't).

---

### Edge Cases

- A reader who reads *only* the new `docs/ipc-mcp-reference.md` entries for the two delete tools (no CHANGELOG, no PR, no ADR) must be able to name the exact required parameter and the exact remedy — not just know that "something changed."
- The omitted-vs-`[]` statement must be written once, centrally, and the two delete tools' entries and the read-tool entries must not restate it in conflicting or redundant language — cross-reference rather than duplicate.
- `knowledge_assert_relationship`'s embedded field (`fact`) differs from `knowledge_assert_entity`'s (`name`) — the two new reference entries must not imply a single shared embedding rule.
- The multi-graph framing added to `docs/index.md`/`docs/getting-started.md` must not use "multi-tenant" as a synonym for "multi-graph" — README explicitly distinguishes them (no auth/isolation guarantees), and copying the wrong word into the entry points would introduce a new, contradicting claim rather than fix a gap.
- If `docs/telemetry.md` events do not in fact carry a `group_id` field, the correct fix is confirming that in place (or making no change) — not inventing a field that doesn't exist.
- Regenerating `docs/llms-full.txt` must reflect every other change in this issue, not just be re-run as a no-op — a stale regeneration would pass the "script was run" bar while failing the "output matches source" one.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `docs/ipc-mcp-reference.md` MUST document, for both `knowledge_delete_chunk_episode` and `knowledge_delete_by_source`, that `group_ids` is a required, non-empty parameter; MUST mark this as a breaking change; MUST name the introducing release (0.13.2) and issue (#406); and MUST state the remedy (pass the caller's own group explicitly — there is no default to fall back to). Presentation must match the treatment #447 already received for the equivalent `group_id` requirement on `knowledge_canonicalize_relations`/`knowledge_backfill_relation_types`.
- **FR-002**: `docs/ipc-mcp-reference.md` MUST state, once and centrally, the omitted-vs-empty `group_ids` contract for read tools: an omitted `group_ids` means all groups; an explicit `group_ids: []` means zero rows (a filter matching nothing). Per-tool entries should reference this central statement rather than each restating it.
- **FR-003**: `docs/ipc-mcp-reference.md` MUST add real reference entries (not just table listings) for `knowledge_assert_entity` and `knowledge_assert_relationship`, covering: the fields each accepts; upsert/identity semantics (what matches an existing row, and what happens to fields omitted on an update — including that omitting `summary`/`attributes` clears the prior value); which single field is embedded for semantic search on each tool (`name` for `knowledge_assert_entity`, `fact` for `knowledge_assert_relationship`); that other stored fields (e.g. `summary`, `attributes`) are lexically/full-text indexed but not semantically searchable as of this writing; and how each tool differs in purpose from `knowledge_process_chunk`.
- **FR-004**: `docs/index.md` and `docs/getting-started.md` MUST introduce, within the first screen/section a new reader encounters, that one workspace can hold multiple independent graphs distinguished by `group_id`. Terminology MUST follow `README.md`'s existing framing (including its explicit "multi-graph, not multi-tenant" distinction) rather than introducing new vocabulary.
- **FR-005**: `docs/llms-full.txt` MUST be regenerated via `scripts/generate-docs-llms-full.sh` after all other documentation changes in this issue are made, so it reflects the final doc content and CI's "Verify llms-full.txt is up to date" check passes.
- **FR-006**: None of the above may restate material already covered by `docs/operations.md` (per-group WAL roots, generation tracking, hydration) or `docs/ontology.md` (per-group ontologies, path-escaping). Where relevant, link to those documents instead of duplicating their content.
- **FR-007**: While addressing FR-004, `docs/telemetry.md` MUST be checked for whether telemetry events carry a `group_id` dimension; if they do, that MUST be documented; if they don't, no fabricated field may be added.

### Key Entities

Not applicable — this is a documentation-only change with no new data entities. The subject matter concerns existing IPC/MCP tools (`knowledge_delete_chunk_episode`, `knowledge_delete_by_source`, `knowledge_assert_entity`, `knowledge_assert_relationship`, and the broader read-tool family) and existing doc files (`docs/ipc-mcp-reference.md`, `docs/index.md`, `docs/getting-started.md`, `docs/telemetry.md`, `docs/llms-full.txt`).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A reader who has only read `docs/`, never the CHANGELOG, can discover that `group_ids` is mandatory on both delete tools, and knows what to pass.
- **SC-002**: The same reader can predict what an omitted `group_ids` and an explicit `[]` each do on a read tool.
- **SC-003**: A reader arriving at the docs index learns within the first screen that one workspace can hold several graphs.
- **SC-004**: `knowledge_assert_entity`'s retrieval behavior is answerable from the docs alone — specifically, that a `summary` is full-text searchable but not semantically searchable until #470 lands.
- **SC-005**: CI's "Verify llms-full.txt is up to date" check passes, and the docs site builds.

## Assumptions

- "Multi-tenancy" in this issue's title is the colloquial term that motivated the audit, not the terminology to use in the docs themselves. `README.md` already draws a deliberate, load-bearing line between "multi-graph" (what the product provides — isolated graphs per `group_id`, no auth) and "multi-tenant" (what it explicitly does not provide — per-tenant auth/isolation as a security boundary). FR-004 follows README's actual wording, so "multi-graph" is the term that lands on `docs/index.md`/`docs/getting-started.md`, with the "not multi-tenant" caveat carried over if the entry points state any security/isolation expectation.
- The exact prose, heading placement, and section structure for each doc change are left to the Plan/Implement stages; this spec fixes *what* must become discoverable and *where* (which doc file, roughly which section), not exact wording.
- `docs/operations.md` and `docs/ontology.md` are assumed accurate and current as of this writing (per the issue's own audit) and are treated as link targets, not rewritten.
- This issue does not implement #470 (semantic search over `summary`); it documents the current (pre-#470) behavior. When #470 ships, the same text this issue adds will need a follow-up update — that update is out of scope here.

## Out of Scope

- Implementing #470 (summary embeddings / semantic search over `summary`).
- Restructuring the docs site or its navigation.
- The ADR corpus under `docs/adr/` — those are decision records, not user documentation, and are not touched by this issue.

## Source References

- #406 — the undocumented breaking change behind Gap 1 / FR-001 (`group_ids` required on the two delete tools).
- #413 — the omitted-vs-empty `group_ids` read-tool contract behind Gap 2 / FR-002.
- #447 — the model to copy for how a required group parameter is documented (style reference for FR-001).
- #379 — the direct-assertion API (`knowledge_assert_entity`/`knowledge_assert_relationship`) behind Gap 3 / FR-003.
- #465, #470 — the community report and follow-up work issue behind Gap 3's retrieval-behavior documentation.
- PR #396 — the README's multi-graph framing that FR-004 follows.
- ADR-0322 — docs-only PRs skip the Rust CI job by design; a SKIPPED `test` job on this issue's PR is expected, not a failure.
- `docs/ipc-mcp-reference.md`, `docs/index.md`, `docs/getting-started.md`, `docs/telemetry.md`, `docs/operations.md`, `docs/ontology.md`, `README.md`, `CHANGELOG.md` (0.13.0–0.13.3 entries), `scripts/generate-docs-llms-full.sh`, `crates/core/src/handlers.rs` (`handle_assert_entity`, `handle_assert_relationship`) — verified directly while writing this spec.
