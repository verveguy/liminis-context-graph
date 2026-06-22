# Feature Specification: Relation Canonicalization — Map Free-Text Relation Names to Controlled `relation_type` (Two-Layer; Drop Co-occurrence Noise)

**Feature Branch**: `fabrik/issue-163`
**Created**: 2026-06-22
**Status**: Draft
**Input**: User description: "Add a relation-canonicalization pass: map the free-text relation `name` vocabulary onto a controlled canonical set (a `relation_type`), while preserving the free-text `fact`, and drop the co-occurrence noise class."

## Background

A production graph has **32,316 distinct relation `name`s across 76,935 edges**. Three-way decomposition from Track-1 analysis:

- **~42%** map to ~21 canonical relations via simple lexical rules (~65% reachable by adding ~10 more types to the ontology).
- **~26% are pure noise** — labels of the form `BRETT → RAJI` (co-occurrence junk, ~20K edges), produced by an earlier extraction path that emitted source→target entity pairs instead of real predicates.
- **~32% residual** — 12,304 labels, 8,644 singletons; specific predicates whose nuance lives in the edge `fact` sentence and that don't map cleanly onto any canonical type.

Issue #94 added the `relation_type` column and established the SCREAMING_SNAKE_CASE normalization contract. However, #94's scope was forward-only: new edges now carry a proper `relation_type`, but the ~76K existing edges were not backfilled. The `relation_type` values on those edges are either null or echo the noisy free-text `name` verbatim.

**Why this matters:**

- Structural queries ("find all `AUTHORED` relationships") fail or return garbage on the existing corpus because no consistent `relation_type` token is present.
- The `BRETT → RAJI` co-occurrence edges are extraction artifacts with no semantic content; they inflate the edge count, pollute FTS/embedding indexes, and distort graph traversal results. Co-occurrence information is recoverable from shared `episode_uuids` if ever needed, so deletion is safe.
- The ontology work (#83, merged) declared canonical relation-type vocabularies — `AUTHORED`, `AFFILIATED_WITH`, etc. — that can now serve as the mapping target for this backfill pass.

The fix is a **two-layer architecture**: canonical `relation_type` (controlled vocabulary, query-friendly) + free-text `fact` (rich, search-friendly). Both coexist on every edge. No information is lost.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Dry-Run Coverage Report (Priority: P1)

An operator with a large existing graph calls a `dry_run` pass and receives a breakdown showing what percentage of edges would be canonicalized, how many are noise (and would be deleted), and how many remain residual — with zero mutations applied.

**Why this priority**: The pass may take minutes on a large graph, and operators need to see coverage projections before committing. A dry-run report also validates that the canonical set in the ontology is correctly wired to the mapping logic.

**Independent Test**: With a test graph containing edges in each category (mapped, noise, residual), call the pass in `dry_run: true` mode. Assert: the response includes `mapped_count`, `noise_count`, `residual_count`, and `total_edges`; that mapped + noise + residual = total; that the graph is unchanged after the call; and that no WAL entries were written.

**Acceptance Scenarios**:

1. **Given** an existing graph with 100 edges (50 mappable, 20 noise, 30 residual), **When** `knowledge_canonicalize_relations` is called with `dry_run: true`, **Then** the response reports `{mapped_count: 50, noise_count: 20, residual_count: 30, total_edges: 100, dry_run: true}` and the graph is unchanged.
2. **Given** a dry run completes, **When** the graph is queried for edge `relation_type` values, **Then** all `relation_type` values are identical to what they were before the call.
3. **Given** `dry_run: true` is called, **When** the WAL file is inspected, **Then** no new mutation lines were added by the pass.

---

### User Story 2 — Lexical Canonicalization of Mappable Edges (Priority: P1)

An operator runs the pass in non-dry-run mode. Edges whose `name` (or `relation_type`) matches a canonical ontology type via stem/keyword rules get their `relation_type` set to the canonical form. The `fact` is untouched.

**Why this priority**: This is the primary value: ~65% of edges become structurally queryable by canonical predicate (lexical-only baseline ~42%; ~65% reachable with extended canonical set). Lexical rules are deterministic and fast — no embedding inference required.

**Independent Test**: Ingest 10 edges with varied `name` values that lexically map to `AUTHORED` (e.g. `WROTE`, `authored by`, `AUTHORING`, `is the author of`). Run the canonicalization pass. Assert each edge now has `relation_type = "AUTHORED"` and an unchanged `fact`.

**Acceptance Scenarios**:

1. **Given** an edge with `name = "WROTE"` and no `relation_type`, **When** the pass runs, **Then** `relation_type` is set to `AUTHORED` (or whichever canonical type the lexical rule maps it to) and `fact` is unchanged.
2. **Given** an edge with `name = "AFFILIATED_WITH"` (already canonical), **When** the pass runs, **Then** `relation_type` is set to `AFFILIATED_WITH` (idempotent) and no mutation is logged for it.
3. **Given** an edge whose `relation_type` is already correctly set (from post-#94 ingestion), **When** the pass runs, **Then** the edge is unchanged.
4. **Given** the pass runs, **When** the WAL is inspected, **Then** each mapped edge has a corresponding mutation entry that captures the new `relation_type` value.

---

### User Story 3 — Noise Class Deletion (Priority: P1)

The pass detects co-occurrence edges (`BRETT → RAJI` pattern: entity-name arrows as the `name` field) and deletes them from the graph. The count is reported.

**Why this priority**: ~20K noise edges inflate the edge count, pollute FTS indexes, and have no semantic content. Co-occurrence information is recoverable from shared `episode_uuids`, so deletion is safe and clean.

**Independent Test**: Create 5 edges with `name` values matching the `X → Y` arrow pattern. Run the pass. Assert: the 5 edges no longer exist in the graph; the response `noise_count` is 5; WAL delete mutations capture each deletion.

**Acceptance Scenarios**:

1. **Given** an edge with `name = "BRETT → RAJI"`, **When** the pass runs, **Then** the edge is deleted from the graph and the response `noise_count` includes it.
2. **Given** the pass runs, **When** the WAL is inspected, **Then** each deleted noise edge has a corresponding WAL delete mutation.
3. **Given** the pass runs with `dry_run: true`, **When** the response is inspected, **Then** `noise_count` is populated but no edges are deleted.

---

### User Story 4 — Residual Edges Marked `UNCLASSIFIED` (Priority: P1)

Edges that don't match any lexical rule (and fall below the embedding threshold) are marked with `relation_type = "UNCLASSIFIED"`. This is honest (no fake `RELATED_TO`), and lets a future embedding/LLM pass target exactly those edges by querying for that sentinel value. The free-text `fact` is preserved regardless.

**Why this priority**: Without a sentinel, residual edges are indistinguishable from un-processed edges. `UNCLASSIFIED` makes the pass's output complete and auditable, and provides a clean query target for future improvement passes.

**Independent Test**: Create 5 edges with highly specific `name` values that match no lexical rule. Run the pass with embedding disabled. Assert: all 5 edges now have `relation_type = "UNCLASSIFIED"` and their `fact` is unchanged.

**Acceptance Scenarios**:

1. **Given** an edge with `name = "IS_THE_THIRD_COUSIN_TWICE_REMOVED_OF"` that matches no lexical rule and falls below the embedding threshold, **When** the pass runs, **Then** `relation_type` is set to `UNCLASSIFIED`.
2. **Given** residual edges are marked `UNCLASSIFIED`, **When** the graph is queried for `relation_type = "UNCLASSIFIED"`, **Then** exactly those edges are returned — forming a well-defined work queue for future improvement.
3. **Given** the pass runs, **When** the WAL is inspected, **Then** each edge marked `UNCLASSIFIED` has a corresponding WAL update mutation.

---

### User Story 5 — Embedding Fallback for Ambiguous Residual Edges (Priority: P2)

For edges that don't match any lexical rule, the pass uses the edge's `fact` sentence as the embedding input and computes cosine similarity against each canonical type's description gloss. If similarity exceeds a confidence threshold, `relation_type` is set to the matched canonical type (not `UNCLASSIFIED`). Below threshold, the edge is marked `UNCLASSIFIED`.

**Why this priority**: The embedding fallback converts some fraction of the residual into the canonicalized set. It requires the embedder service and is slower; thus it's a second pass after the lexical sweep, and the threshold must be tunable. P2 because lexical pass delivers most of the value; embedding fallback is incremental.

**Independent Test**: Create 5 edges whose `name` values are too specific for lexical rules but whose `fact` sentence clearly expresses a canonical relationship (e.g. fact = "Alice oversaw Bob's annual performance review" → `MANAGES`). Run the pass with embedding enabled. Assert: the 5 edges have `relation_type` set to the matched canonical type (not `UNCLASSIFIED`), and the response `embedding_fallback_promoted` count is 5.

**Acceptance Scenarios**:

1. **Given** an edge whose `fact` embeds close to the `MANAGES` canonical gloss (above threshold), **When** the pass runs, **Then** `relation_type` is set to `MANAGES` (not `UNCLASSIFIED`).
2. **Given** an edge whose `fact` doesn't embed close to any canonical gloss (below threshold), **When** the pass runs, **Then** `relation_type` is set to `UNCLASSIFIED`.
3. **Given** the embedder service is unavailable, **When** the pass runs, **Then** the lexical pass completes, all unmatched edges are marked `UNCLASSIFIED`, and the response includes a warning that embedding fallback was skipped (not a hard failure).
4. **Given** `embedding_threshold` is set above the default, **When** the pass runs, **Then** fewer edges are promoted from residual to mapped (stricter matching); more are marked `UNCLASSIFIED`.

---

### User Story 6 — WAL Round-Trip Fidelity (Priority: P1)

A WAL replay after the canonicalization pass reproduces the same `relation_type` values (and deletions) as the live graph.

**Why this priority**: WAL is the canonical backup. If the canonicalized `relation_type` values or noise deletions live only in the live DB and not in the WAL, a `knowledge_rebuild_from_wal` would silently revert every edge to its pre-canonicalization state.

**Independent Test**: Run canonicalization pass on a test graph. Capture all edge `relation_type` values and edge UUIDs. Wipe the DB. Run `knowledge_rebuild_from_wal`. Re-capture edge UUIDs and `relation_type` values. Assert they are identical (deleted noise edges absent, canonical types preserved, `UNCLASSIFIED` markers present).

**Acceptance Scenarios**:

1. **Given** the canonicalization pass ran and wrote WAL entries, **When** `knowledge_rebuild_from_wal` replays those entries against an empty DB, **Then** each edge's `relation_type` matches its post-pass value and noise edges remain absent.
2. **Given** the WAL is inspected after the pass, **Then** each mutation (update or delete) corresponds to exactly one edge and records the canonical `relation_type`, `UNCLASSIFIED`, or deletion.

---

### Edge Cases

- **Edge already has a correct `relation_type`** (set by post-#94 ingestion): The pass must be idempotent — no mutation emitted if the existing value already matches what the rules would assign.
- **`name` is empty or `NULL`**: Skip the lexical step; run the embedding fallback on the `fact` directly. If no `fact` either, mark `UNCLASSIFIED`.
- **`fact` is also empty**: No signal for the embedding fallback; edge is marked `UNCLASSIFIED`.
- **Noise pattern matches ambiguously** (e.g. `ALICE → BOB` where Alice and Bob are also common words): False positive risk. The noise regex MUST require that both sides be capitalized tokens separated by `→` or `->` literal; bare lowercase words should not match.
- **Edge already marked `UNCLASSIFIED`**: Idempotent — no mutation emitted on re-run.
- **Ontology has no relation types loaded**: The pass MUST fail fast with a clear error, not silently process 0 edges.
- **Very large graphs (>1M edges)**: The pass MUST support progress streaming via `_progress_token` and use batched WAL writes rather than holding a write lock for the entire duration.
- **Concurrent ingestion during the pass**: The exact locking model is left to Research/Plan; this edge case MUST be addressed there.
- **Embedding fallback with no ontology description glosses**: If canonical types have no `description` field in the ontology YAML, there's nothing to embed against. The embedding fallback is skipped (not an error) and a warning is included in the response.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A new IPC method `knowledge_canonicalize_relations` MUST accept at minimum `dry_run: bool` and `embedding_threshold: Optional<f32>` parameters.
- **FR-002**: The pass MUST implement a lexical-first mapping: for each edge, the `name` and `relation_type` fields are matched against each canonical type's name and any configured aliases/keywords using stem/keyword rules; on a match, `relation_type` is set to the canonical form.
- **FR-003**: The lexical mapping rules MUST be driven from the workspace ontology's `relation_types`; the ontology YAML schema MUST be extended to support optional `aliases` or `keywords` per relation type.
- **FR-004**: The pass MUST detect co-occurrence noise edges: any edge whose `name` or `relation_type` matches the pattern `<CAPITALIZED_TOKEN(s)> → <CAPITALIZED_TOKEN(s)>` (arrow-separated entity-name pair, with `→` or `->` literal) MUST be classified as noise.
- **FR-005**: Noise edges MUST be **deleted** from the graph. A WAL delete mutation MUST be written for each deleted edge.
- **FR-006**: For edges not matched by lexical rules, the pass MUST optionally run an embedding fallback: embed the edge's `fact` sentence and compute cosine similarity against each canonical type's description gloss. If similarity ≥ `embedding_threshold`, set `relation_type` to the matched canonical type.
- **FR-007**: The pass MUST use the embedder already in use by the service for the embedding fallback; it MUST NOT introduce a new embedding dependency.
- **FR-008**: Edges that are neither matched by lexical rules nor promoted by the embedding fallback MUST have `relation_type` set to `"UNCLASSIFIED"`.
- **FR-009**: In `dry_run: true` mode, the pass MUST return a coverage report (`mapped_count`, `noise_count`, `residual_count`, `total_edges`, `mapped_pct`, `noise_pct`, `residual_pct`, `embedding_fallback_promoted`) and make zero mutations to the graph or WAL.
- **FR-010**: Every mutation applied by the pass (edge update or delete) MUST be written to the application WAL, so that `knowledge_rebuild_from_wal` reproduces the post-pass state.
- **FR-011**: The pass MUST be idempotent: re-running on an already-canonicalized graph produces the same result and emits no new mutations.
- **FR-012**: The pass MUST support `_progress_token` streaming for large graphs (consistent with existing admin methods).
- **FR-013**: If the workspace ontology has no `relation_types` defined, the pass MUST return a clear error (not silently process 0 edges).
- **FR-014**: The `fact` field on every edge MUST be untouched by the pass.

### Key Entities

- **Canonical set**: The set of SCREAMING_SNAKE_CASE relation types declared in the workspace ontology's `relation_types` list — this is the target vocabulary for canonicalization.
- **Lexical rule**: A stem/keyword match linking a free-text `name` token (or alias pattern) to a canonical type. Stored as optional `aliases`/`keywords` fields on each ontology `relation_type`.
- **Noise edge**: An edge whose `name`/`relation_type` matches the co-occurrence pattern `X → Y` (capitalized entity-pair arrow notation). Deleted by the pass.
- **Residual edge**: An edge that matched neither the lexical rules nor the embedding threshold. Marked `UNCLASSIFIED`.
- **Coverage report**: The response shape: `{mapped_count, noise_count, residual_count, total_edges, mapped_pct, noise_pct, residual_pct, embedding_fallback_promoted, dry_run}`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After the pass runs (non-dry-run, with extended canonical set of ~31 types), the fraction of edges with a non-noise, non-`UNCLASSIFIED` canonical `relation_type` MUST reach ≥ **65%** of total pre-pass edges.
- **SC-002**: The `noise_count` in the response matches the number of `X → Y` pattern edges actually deleted; WAL reflects a delete mutation for each.
- **SC-003**: WAL round-trip: ingest, run pass, dump edge UUIDs + `relation_type` values, wipe DB, replay WAL, dump again — sets are bit-identical (deleted edges absent, canonical types and `UNCLASSIFIED` values preserved).
- **SC-004**: `dry_run: true` call on a test graph leaves zero WAL entries and zero graph mutations; the coverage report is accurate.
- **SC-005**: The `fact` field on every surviving edge is unchanged after the pass.
- **SC-006**: Idempotency: running the pass twice produces no second-round mutations (WAL line count delta = 0 on the second run).
- **SC-007**: All existing tests pass; new tests cover lexical mapping, noise detection and deletion, `UNCLASSIFIED` marking, embedding fallback (mocked), dry-run, WAL round-trip, and idempotency.

## Assumptions

- **A1**: The `relation_type` column was added by #94 and is present in the schema. This issue only populates it on existing edges — no schema migration needed.
- **A2**: The workspace ontology is the canonical source for the mapping target set. If no ontology is present, the pass fails fast (FR-013).
- **A3**: The embedder already running in the service (ORT-based, CoreML on Apple Silicon) is used for the embedding fallback. No new embedding dependency is introduced.
- **A4**: The canonical set + aliases that cover ~65% of edges (per Track-1 analysis) will be committed to the workspace ontology before or alongside the code change; this spec doesn't constrain the specific type list.
- **A5**: The default `embedding_threshold` is a tunable float (suggested: 0.7) that can be overridden per call.
- **A6**: The pass processes all edges in the workspace (no per-group or per-episode filtering in v1).
- **A7**: Concurrent ingestion during the pass is handled via an appropriate locking strategy; the exact locking model is left to Research/Plan.

## Out of Scope

- Defining the specific canonical relation types or their alias lists (that is ontology configuration, not code — Track-1's `relation_map.csv` informs it but is separate).
- Filtering the pass to a subset of edges (by group, episode, date range). Can be a follow-up.
- Auto-running canonicalization on every new ingest (this is a one-time backfill + on-demand admin pass).
- Changing the edge extraction LLM prompt (that's #94 / existing behavior).
- Backfilling `fact` sentences on edges that have none (separate concern).

## Source References

- `crates/core/src/ontology.rs` — `normalize_relation_type`, `RelationTypeRaw`, `Ontology` — extend with `keywords`/`aliases` fields and lexical mapping logic
- `crates/core/src/handlers.rs` — `knowledge_rebuild_from_wal`, `knowledge_recover` — models for the new IPC method pattern (dry_run, progress streaming, WAL writing)
- `crates/core/src/schema.rs` — edge schema with `relation_type` column (from #94)
- `crates/core/src/types.rs` — `RelatesToEdge` struct with `relation_type: Option<String>`
- **#94** (merged): added `relation_type` to new edges; this issue backfills existing edges
- **#83** (merged): ontology support, canonical `relation_types` declaration
- Track-1 `relation_map.csv` + canonical rule set (21 relations, ~42% coverage) — user-held; needs to be committed to ontology YAML alongside this feature
