# Feature Specification: Report dropped-edge detail from `knowledge_process_chunk`

**Feature Branch**: `fabrik/issue-411`
**Created**: 2026-08-15
**Status**: Specified
**Input**: User description: "`knowledge_process_chunk` returns `edges_dropped_unresolvable` as a bare count of edges that were extracted but not written because an endpoint resolved to no entity. Also return the details of those dropped edges so a consumer can tell the user what was missed and why."

## Background

`knowledge_process_chunk` extracts entities and relationships from a text chunk and writes them
to the graph. When an edge's source or target endpoint cannot be resolved to any entity — neither
in the current extraction batch nor in the persisted graph — the edge is dropped rather than
written, and the drop is currently surfaced only as an increment to the
`edges_dropped_unresolvable` count on the result. The edge's own content (which two names it
connected, what relation, what fact it stated) is never persisted anywhere and is not returned, so
once the count is reported the specific dropped fact is unrecoverable.

Orac/tarial ingests a knowledge document through this tool and comments the outcome back on the
submitter's GitHub issue. Today that comment can only say something like *"1 relationship dropped
as unresolvable"* — the submitter has no way to learn which fact was lost or why, so they can't
re-scope or split the source document to fix it. The count alone does not make the drop
actionable; the whole point of reporting it is so a human can do something about it. A downstream
consumer today can only guess at the missing content indirectly (e.g. by finding entities that
ended up with no relationships in the graph after the fact) — inference, not fact.

This spec covers exposing the per-edge detail behind the existing `edges_dropped_unresolvable`
count. It does not cover any other drop/reclassification category on the result (see Out of
Scope).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Consumer sees which fact was dropped and why (Priority: P1)

A pipeline (e.g. orac/tarial) calls `knowledge_process_chunk` to ingest a chunk of a submitted
document. The call succeeds, entities and most edges are written, but one edge is dropped because
one of its endpoints never resolved to an entity. The pipeline reads the per-dropped-edge detail
from the result and renders a concrete, human-readable warning naming the specific fact that was
lost and which endpoint caused the drop, instead of a bare count.

**Why this priority**: This is the entire purpose of the issue — turning an opaque count into an
actionable report. Without this, the feature delivers no value.

**Independent Test**: Ingest a chunk engineered to produce exactly one unresolvable edge (e.g. a
fact naming an endpoint the entity-extraction pass would never itself extract) and inspect the
JSON-RPC result. Confirm the result carries a structured entry describing that specific edge,
alongside the existing count.

**Acceptance Scenarios**:

1. **Given** a chunk that produces one edge whose target endpoint never resolves to an entity,
   **When** `knowledge_process_chunk` completes, **Then** the result's `edges_dropped_unresolvable`
   count is `1` and the result also carries a list of dropped-edge detail with exactly one entry
   describing that edge (its extracted source name, target name, relation type, and fact).
2. **Given** that same dropped edge, **When** the caller inspects the entry, **Then** it can tell
   which endpoint (source, target, or both) failed to resolve, without needing to cross-reference
   the persisted graph to infer it.
3. **Given** a chunk where every edge resolves successfully, **When** `knowledge_process_chunk`
   completes, **Then** the dropped-edge detail list is present and empty, and
   `edges_dropped_unresolvable` is `0`.
4. **Given** a chunk that produces multiple unresolvable edges, **When**
   `knowledge_process_chunk` completes, **Then** the detail list contains one entry per dropped
   edge, and its length equals `edges_dropped_unresolvable`.

---

### Edge Cases

- An edge where **both** endpoints fail to resolve: the detail entry must indicate both endpoints
  failed, not just one.
- An edge whose `relation_type` was never set by extraction (the field is optional on an extracted
  edge): the detail entry carries that absence through (e.g. `null`), rather than inventing a
  value or omitting the entry.
- A chunk containing multiple unresolvable edges that share the same source or target name (e.g.
  two facts both naming an endpoint that never resolves): each dropped edge gets its own entry;
  entries are not deduplicated by endpoint name.
- A chunk large enough to produce many dropped edges in one call: every dropped edge gets an
  entry — the list is not truncated or capped (see Assumptions).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_process_chunk`'s result MUST include a new field, `dropped_edges`, that
  is a list with exactly one entry per edge counted in `edges_dropped_unresolvable` from that same
  call.
- **FR-002**: Each entry in `dropped_edges` MUST include the edge's extracted `source_name`,
  `target_name`, `relation_type`, and `fact`, exactly as extracted (the same values that would
  have been written had the edge resolved) — not a truncated, summarized, or re-derived form.
- **FR-003**: Each entry in `dropped_edges` MUST indicate which endpoint(s) failed to resolve
  (source only, target only, or both), so a consumer does not have to infer this by
  cross-referencing the persisted graph.
- **FR-004**: The existing `edges_dropped_unresolvable` count field MUST remain on the result,
  unchanged in meaning, for backward compatibility with existing consumers that read only the
  count.
- **FR-005**: When no edges are dropped as unresolvable, `dropped_edges` MUST still be present on
  the result, as an empty list — not omitted — so a consumer can rely on the field always being
  present and iterable.
- **FR-006**: `dropped_edges` entries MUST appear in the same order the edges were extracted, so
  results are reproducible and diffable across otherwise-identical runs.
- **FR-007**: The `knowledge_process_chunk` tool's published description (the MCP tool registry
  entry consumers see) MUST be updated to mention `dropped_edges` alongside the existing
  description of `edges_dropped_unresolvable`, so the documented contract matches the actual
  result shape.

### Key Entities *(if the feature involves data)*

- **Dropped edge entry**: A record describing one edge that was extracted but not written because
  an endpoint failed to resolve to an entity. Carries the edge's extracted `source_name`,
  `target_name`, `relation_type`, `fact`, and which endpoint(s) failed to resolve.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: For any `knowledge_process_chunk` call, the number of entries in `dropped_edges`
  equals the value of `edges_dropped_unresolvable` from that same result, in every case.
- **SC-002**: A consumer reading a single `dropped_edges` entry can state, without querying the
  graph or making any other follow-up call, which fact was lost, what two endpoint names it
  connected, and which of those endpoints could not be resolved.
- **SC-003**: An existing consumer that reads only `edges_dropped_unresolvable` and ignores
  `dropped_edges` continues to work unchanged (the count's value and meaning are unaffected by
  this change).

## Assumptions

- Scope is limited to `edges_dropped_unresolvable` / the edges it already counts. Edges dropped
  for other reasons (`edges_dropped_malformed`, counted when a required field like `source_name`
  or `fact` is missing or blank at parse time) are explicitly out of scope for this issue — see
  Out of Scope. The issue's own text frames extending similar treatment to dropped entities as a
  separate, optional follow-up, not part of this work.
- No cap or truncation is applied to `dropped_edges` — the list can be as long as
  `edges_dropped_unresolvable`. This is considered safe because the list is bounded by the number
  of edges extracted from a single chunk, which is itself bounded by the chunking strategy
  upstream of this call (chunks are sized to keep a single extraction batch small); this is not a
  new unbounded-growth path introduced by this change.
- `relation_type` in a `dropped_edges` entry may be absent/`null`, mirroring the fact that
  `relation_type` is already optional on an extracted edge before resolution is attempted.

## Out of Scope

- Extending per-item detail reporting to `edges_dropped_malformed` (edges dropped at parse time
  for a missing/blank required field). Those items can be missing exactly the fields this feature
  reports (e.g. a dropped-for-missing-`source_name` edge has no `source_name` to report), so the
  same detail shape does not directly apply; that is a separate follow-up if wanted.
- Extending per-item detail reporting to any dropped/reclassified entity category
  (`entities_dropped_malformed`, `entities_reclassified_unclassified`,
  `edges_reclassified_unclassified`). The issue raises this only as a "would be welcome" aside for
  a future issue, not a requirement here.
- Any change to *whether* an edge is dropped, salvaged, or resolved (that logic is unchanged;
  see ADR-0051 and ADR-0283 for how endpoint resolution currently works). This issue is purely
  about reporting detail on drops that already occur.
- Any change to how orac/tarial renders its ingestion comment. That consumer-side rendering is out
  of scope for this repository's issue; this spec only covers making the detail available on the
  IPC result for such a consumer to use.

## Source References *(optional)*

- `crates/core/src/episode.rs` — Phase C commit closure, the sole point where an edge endpoint is
  finally resolved or the edge is dropped and `edges_dropped_unresolvable` is incremented.
- `crates/core/src/handlers.rs` — `handle_knowledge_process_chunk`, which builds the JSON result
  returned to the caller.
- `crates/service/src/mcp/tools.rs` — the `knowledge_process_chunk` `ToolSpec` entry, whose
  description text documents `edges_dropped_unresolvable` today (FR-007).
- `crates/core/tests/edge_endpoint_resolution.rs` — existing coverage of drop-counting behavior
  for unresolvable edges.
- ADR-0051 (`docs/adr/0051-edge-endpoint-salvage-and-deferred-drop.md`) — the endpoint
  salvage/deferred-drop decision that produces the drops this issue reports on.
- Issue #281 — introduced `edges_dropped_unresolvable` as a count; this issue extends it with
  per-edge detail.
