# Feature Specification: Correct the documented meaning of edge `episode_uuids`

**Feature Branch**: `fabrik/issue-410`
**Created**: 2026-08-23
**Status**: Specified
**Input**: User description: "edge episode_uuids is entity co-occurrence, not provenance — correct the public documentation"

## Background

The `episode_uuids` field returned on relationship edges (`RelatesToEdge`) does not mean what its
name and current documentation imply. It is populated by `enrich_edge_from_entity_ep_info`
(`crates/core/src/handlers.rs`) from `Conn::get_episode_info_for_entities`
(`crates/core/src/db.rs`), whose query is:

```cypher
MATCH (ep:Episodic)-[:MENTIONS]->(n:Entity) WHERE n.uuid IN $uuids
```

Run against both the edge's source and target entity UUIDs, this returns **every episode that
mentions either endpoint entity**, deduplicated — regardless of whether that episode had anything
to do with asserting the relationship itself. For a well-connected entity this can be most of the
corpus, not the handful of episodes that actually produced the fact.

This either-endpoint behavior is a deliberate, already-accepted design decision (ADR-0012,
"Edge-to-Episode Associations via Either-Endpoint Entity Traversal", from issue #32) and the
function's own doc comment states the semantics accurately: *"either-endpoint semantics: any
episode that mentions the source OR target entity is attributed to the edge."* But that honesty
does not reach any surface a consumer actually reads. `liminis-context-graph` is a public repository
with tagged releases; the field ships on the MCP-over-stdio transport (`--mcp-stdio`), which is
documented for use by arbitrary MCP clients with no app in between. A consumer reading
`episode_uuids` on an edge — via the MCP tool description, the JSON response itself, or the IPC
reference — will reasonably conclude it identifies the evidence for that specific assertion. It
does not, and the population of clients that could act on this misunderstanding is not enumerable.

This is a **documentation-only** correction: no schema change, no behavior change, no change to
any value returned. It states the actual, already-shipped semantics everywhere a consumer can
currently read a description of the field, so the code and the published surface agree.

A related but distinct defect — the substrate-level `RelatesToNode_.episodes` column being never
written, so there is no true per-edge episode provenance at all — is tracked separately as #404
and is explicitly out of scope here (see Out of Scope).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - MCP client reads a tool description before calling it (Priority: P1)

An MCP client (Claude Code, Claude Desktop, or any other MCP-speaking agent) calls `tools/list`
against the `--mcp-stdio` transport before deciding how to use a tool that returns edges (e.g.
`knowledge_find_relationships`, `knowledge_list_relationships`, `knowledge_get_edges_by_group`,
`knowledge_get_edges_by_uuids`, `knowledge_get_entity_neighbors`). The returned tool description
must not claim or imply that `episode_uuids` identifies the episodes that support or assert the
relationship.

**Why this priority**: This is the most direct, most automatable path by which a consumer forms an
incorrect belief about the field — a client can and reasonably will act on the tool description
alone, without reading any source code.

**Independent Test**: Run `--mcp-stdio` locally, send a `tools/list` request, and read the
`description` field of every tool whose output includes an edge object with `episode_uuids`.
Confirm each description states either-endpoint mention co-occurrence, not per-edge support or
provenance.

**Acceptance Scenarios**:

1. **Given** the MCP tool registry in `crates/service/src/mcp/tools.rs`, **When** a client reads
   the `description` of `knowledge_list_relationships` (which currently reads "...with episode
   provenance attached"), **Then** the description no longer uses "provenance" for edges and
   instead states that `episode_uuids` lists episodes mentioning either endpoint entity.
2. **Given** the same registry, **When** a client reads the `description` of any other tool that
   returns edges with `episode_uuids` (`knowledge_find_relationships`,
   `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`,
   `knowledge_get_entity_neighbors`), **Then** the description explicitly states the
   either-endpoint co-occurrence semantics for that field, even where today it says nothing about
   the field at all.
3. **Given** a tool that returns edges, **When** its description is corrected, **Then** the
   `input_schema` and every other field of its `ToolSpec` are unchanged.

---

### User Story 2 - Developer reads source to understand the response shape (Priority: P2)

A developer working in this repository (or a downstream integrator embedding `lcg-core` directly)
reads the doc comments on `RelatesToEdge` and on `enrich_edge_from_entity_ep_info` /
`get_episode_info_for_entities` to understand what a returned edge's `episode_uuids` field
contains, without needing to trace the Cypher query themselves.

**Why this priority**: This is the ground truth other documentation is corrected to match (FR-003
depends on this being accurate first), and it is what a Research/Plan/Implement stage working a
future edge-related issue will read.

**Independent Test**: Read the doc comment on the `episode_uuids` field of the `RelatesToEdge`
struct in `crates/core/src/types.rs` and confirm it states either-endpoint mention co-occurrence.
`enrich_edge_from_entity_ep_info`'s existing comment already does this correctly and is left as-is
or referenced.

**Acceptance Scenarios**:

1. **Given** `crates/core/src/types.rs`, **When** a reader looks at the `episode_uuids` field on
   `RelatesToEdge`, **Then** an adjacent doc comment states the either-endpoint semantics — this
   field currently has no doc comment at all.
2. **Given** `crates/core/src/handlers.rs`, **When** a reader looks at
   `enrich_edge_from_entity_ep_info`, **Then** its doc comment (already accurate) is unchanged or
   only clarified, not weakened.

---

### User Story 3 - Reader of the published reference docs (Priority: P3)

A reader of `README.md` or `docs/ipc-mcp-reference.md` — the two public, in-repo documentation
surfaces for the wire protocol — looks for what an edge's `episode_uuids` field means and finds an
accurate statement, wherever such a statement exists.

**Why this priority**: Lower priority than P1/P2 because research below found that neither
`README.md` nor `docs/ipc-mcp-reference.md` currently documents `episode_uuids` (or any other
edge response field) at field-level granularity at all — see Assumptions. There is no existing
inaccurate sentence to fix in either file today.

**Independent Test**: Search `README.md` and `docs/ipc-mcp-reference.md` for `episode_uuids`. If
either file is extended during Plan/Implement to describe edge response fields, confirm the
description matches the either-endpoint semantics.

**Acceptance Scenarios**:

1. **Given** `docs/ipc-mcp-reference.md`'s stated policy that it is a method index and defers
   field-level response detail to the `handlers.rs` source (see its own text: "this page is the
   method index, not a copy of each handler's parameter parsing"), **When** this issue is
   implemented, **Then** the source-of-truth doc comments from User Story 2 satisfy this
   requirement without requiring new field-level content in this file, unless a later stage
   decides to add a short field note here for discoverability.
2. **Given** `README.md` contains no field-level edge response documentation today, **When** this
   issue is implemented, **Then** no misleading text about `episode_uuids` exists in `README.md`
   (trivially true today, and must remain true).

---

### Edge Cases

- A future tool is added that returns edges with `episode_uuids` after this issue closes: not
  covered by this issue's acceptance criteria, but the corrected `ToolSpec` descriptions in this
  change serve as the template new tools should copy.
- `EntityRow.episode_uuids` (the entity-scoped field, populated by the same underlying query but
  keyed directly on the entity itself) is **not** affected by this issue: for an entity, "episodes
  that mention this entity" **is** accurate provenance. Only the edge-scoped field is misleading.
  `knowledge_list_entities`'s "with episode provenance attached" wording is correct as-is and must
  not be changed.
- `knowledge_get_episodes`, `knowledge_add_episode`, `knowledge_delete_episode`, and other
  episode-centric tools do not return `episode_uuids` on an edge and are out of scope.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every `ToolSpec` in `crates/service/src/mcp/tools.rs` whose tool returns edges
  carrying `episode_uuids` (`knowledge_find_relationships`, `knowledge_list_relationships`,
  `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`,
  `knowledge_get_entity_neighbors`) MUST describe the field as episodes mentioning either endpoint
  entity (source or target), not as support or provenance for the relationship itself. This
  includes tools whose description currently says nothing about the field at all — silence is not
  acceptable where the field is present in the response and its meaning is non-obvious.
- **FR-002**: Wherever `README.md` or `docs/ipc-mcp-reference.md` documents edge response shapes
  at field level, the same either-endpoint correction MUST apply. Per the research captured in
  User Story 3 and the Assumptions below, neither file currently does so; this requirement is
  satisfied by leaving both files free of misleading `episode_uuids` text (trivially true today)
  and by NOT introducing new misleading text. Adding new field-level content to either file for
  discoverability is permitted but not required by this issue.
- **FR-003**: Public-facing doc comments on the response-construction path MUST match the actual
  semantics:
  - `RelatesToEdge.episode_uuids` in `crates/core/src/types.rs` MUST gain a doc comment stating
    either-endpoint mention co-occurrence (it currently has none).
  - `enrich_edge_from_entity_ep_info` and `get_episode_info_for_entities` in
    `crates/core/src/handlers.rs` / `crates/core/src/db.rs` already state this accurately and MUST
    remain accurate (edit only if clarification is needed; do not weaken).
- **FR-004**: This issue does NOT require renaming the field. A future rename (e.g. to
  `mentioning_episode_uuids`) MAY be proposed as a follow-up issue, and if pursued MUST follow the
  additive-first wire-compatibility rule (emit both old and new field, deprecate the old) rather
  than a breaking rename. No rename work is performed as part of this issue.
- **FR-005**: No change to any returned value, response key, response shape, JSON-RPC method
  signature, or MCP tool `input_schema` is permitted. This is a description/doc-comment-only
  change.

### Key Entities *(if the feature involves data)*

- **`RelatesToEdge`** (`crates/core/src/types.rs`): the relationship-edge response struct carrying
  the `episode_uuids` field whose documented meaning this issue corrects. No fields are added,
  removed, or renamed.
- **`ToolSpec`** (`crates/service/src/mcp/tools.rs`): the hand-maintained MCP tool registry entry
  whose `description` string is corrected for each edge-returning tool. `name`, `scope`, and
  `input_schema` are unchanged.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A consumer reading only `tools/list` output (no source access) learns, for every
  tool that returns edges with `episode_uuids`, that the field is either-endpoint mention
  co-occurrence, not per-edge support or provenance.
- **SC-002**: `README.md` and `docs/ipc-mcp-reference.md` contain no text describing edge
  `episode_uuids` as relationship support, evidence, or provenance, whether that text is
  pre-existing or newly added.
- **SC-003**: No returned value, response key, or episode count changes for any method, verified
  by the existing IPC response-shape and parity test suites
  (`crates/core/tests/ipc_response_shapes.rs`, `crates/core/tests/ipc_parity.rs`) passing unchanged.
- **SC-004**: `crates/service/src/mcp/tools.rs`'s registry-count and per-scope bucket-size test
  assertions are unchanged (descriptions only; no entries added, removed, or moved between scopes).

## Assumptions

- `docs/ipc-mcp-reference.md` explicitly documents itself as a method-level index that defers
  request/response field detail to the `handlers.rs` dispatch source
  ("this page is the method index, not a copy of each handler's parameter parsing" — verified in
  the current file). Neither it nor `README.md` currently mentions `episode_uuids` at all. FR-002
  is therefore satisfied primarily by FR-001 (tool descriptions) and FR-003 (source doc comments)
  being accurate; Research/Plan may still choose to add a short discoverability note to
  `docs/ipc-mcp-reference.md` but is not required to.
- `EntityRow.episode_uuids` and its "episode provenance attached" description
  (`knowledge_list_entities`) are accurate as-is and are explicitly out of scope — only the
  edge-scoped field and its consuming tools are corrected.
- The set of tools returning edges with `episode_uuids` is: `knowledge_find_relationships`,
  `knowledge_list_relationships`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`,
  `knowledge_get_entity_neighbors`. Research should re-confirm this list against
  `crates/service/src/mcp/tools.rs` at implementation time, since line numbers in this spec reflect
  the state of the branch at specification time and may drift.
- ADR-0012 already documents and accepts the either-endpoint design decision at the architecture
  level; this issue does not revisit or reference updating that ADR, since ADR-0012 is itself
  accurate — the gap is only in consumer-facing surfaces.

## Out of Scope

- Writing `RelatesToNode_.episodes` (tracked separately as #404, targeted at 0.15.0) — that is a
  substrate/schema change requiring a WAL-dump payload change and is blocked on an unresolved
  design question (how edges created with no episode at all, e.g. via `assert_relationship` or
  `add_cross_group_edge`, should be handled).
- Any change to the values currently returned by any method.
- Renaming the `episode_uuids` field (see FR-004 — permitted as a future follow-up, not performed
  here).
- Revisiting or overturning the either-endpoint design decision recorded in ADR-0012.

## Source References *(optional)*

- `crates/core/src/handlers.rs` — `enrich_edge_from_entity_ep_info` (currently accurate doc
  comment to preserve)
- `crates/core/src/db.rs` — `get_episode_info_for_entities`, the `MENTIONS` query
- `crates/core/src/types.rs` — `RelatesToEdge` struct (field lacking a doc comment today)
- `crates/service/src/mcp/tools.rs` — the `ToolSpec` registry; `knowledge_list_relationships`
  currently reads "with episode provenance attached" and is the one existing misleading sentence
  found during specification
- `docs/adr/0012-edge-episode-via-entity-traversal.md` — the accepted design decision this issue's
  documentation must agree with
- `docs/ipc-mcp-reference.md` — states its own scope as a method index, not field-level detail
- #404 — the substrate defect (`RelatesToNode_.episodes` never written), deferred to 0.15.0
