# Feature Specification: Unify relationship/edge response keys across read RPCs

**Feature Branch**: `fabrik/issue-524`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "API inconsistency: knowledge_find_relationships returns `facts`, knowledge_get_edges_by_group returns `edges` (silent-empty on mismatch)"

## Background

Several read-only IPC/MCP methods return a list of relationship (edge) objects, but the
top-level JSON key holding that list is inconsistent across them:

- `knowledge_find_relationships` → `{"facts": [...], "count": N}`
- `knowledge_list_relationships` → `{"facts": [...], "count": N}`
- `knowledge_get_edges_by_group` → `{"edges": [...], "count": N}`

In every one of these handlers (`crates/core/src/handlers.rs`), the internal Rust variable
holding the result is literally named `edges` — the payload is the same shape, just labelled
differently in the outer JSON object depending on which method produced it.

This was reported because a downstream reader (zen's catalog/graph reader) parsed the `edges`
key on a `knowledge_find_relationships` response and silently got an empty list instead of an
error. That is the core danger here: a client that reads the wrong key does not fail loudly —
it gets `[]`/`None`, which looks exactly like "this group has no relationships." There is no
schema signal to catch the mistake, so it surfaces later as a plausible-but-wrong empty result,
which is the hardest kind of bug to track down.

During specification, a third affected method was found beyond the two named in the original
report: `knowledge_list_relationships` has the identical defect (`facts` key, same underlying
edge-list shape). It is included in scope below because it is the same bug, not a related one.

`knowledge_get_entity_neighbors` also returns an `"edges"` key, but as part of a richer subgraph
response (`nodes`, `center_uuid`, `node_count`, `edge_count` alongside `edges`) rather than a
flat edge-list response. It is explicitly out of scope — see "Out of Scope" below.

The fix direction was already decided in the originating issue report: add both `edges` and
`facts` keys (as aliases of the same list) to every affected response, rather than removing
either key. This is the non-breaking option — existing clients that read either key keep
working, and new/fixed clients can standardize on `edges` going forward.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Client reads relationships via any affected method and gets the same key (Priority: P1)

A client (internal or external, e.g. zen's catalog/graph reader) calls any of
`knowledge_find_relationships`, `knowledge_list_relationships`, or `knowledge_get_edges_by_group`
and can reliably read the resulting edge list from the same top-level key, regardless of which
method it called.

**Why this priority**: This is the entire defect being fixed. Without it, a client that shares
parsing code across these methods (or that is refactored to switch which method it calls)
silently loses data instead of erroring.

**Independent Test**: Call each of the three affected RPCs against a graph with at least one
relationship in the target group, and assert that both `edges` and `facts` are present in the
response, are equal, and both have length equal to `count`.

**Acceptance Scenarios**:

1. **Given** a graph with N relationships in a group, **When** a client calls
   `knowledge_find_relationships` for that group, **Then** the response contains both an
   `edges` key and a `facts` key, both holding the same N-element list, and `count` equals N.
2. **Given** a graph with N relationships in a group, **When** a client calls
   `knowledge_list_relationships` for that group, **Then** the response contains both an
   `edges` key and a `facts` key, both holding the same N-element list, and `count` equals N.
3. **Given** a graph with N relationships in a group, **When** a client calls
   `knowledge_get_edges_by_group` for that group, **Then** the response contains both an
   `edges` key and a `facts` key, both holding the same N-element list, and `count` equals N.
4. **Given** a group with zero relationships, **When** a client calls any of the three
   affected RPCs, **Then** the response contains both `edges: []` and `facts: []`, and
   `count` equals 0 — a genuinely empty result is distinguishable only by an explicit `[]`
   under both keys, never by a key's absence.

---

### Edge Cases

- A client that already parses the "wrong" key for a given method (e.g. reads `edges` from
  `knowledge_find_relationships` today) must continue to get `[]` today and a correct,
  non-empty list after this change — i.e., this change can only fix currently-broken parsing,
  never introduce a new break for a client that was already working.
- Very large relationship lists: adding a second key duplicates the list reference in the
  constructed JSON value before serialization. This is expected to be acceptable (see
  Assumptions) but should not be silently ignored if it turns out to matter at existing
  response-size limits.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_find_relationships` MUST include both an `edges` key and a `facts`
  key in its response, each holding the identical list of relationship objects.
- **FR-002**: `knowledge_list_relationships` MUST include both an `edges` key and a `facts`
  key in its response, each holding the identical list of relationship objects.
- **FR-003**: `knowledge_get_edges_by_group` MUST include both an `edges` key and a `facts`
  key in its response, each holding the identical list of relationship objects.
- **FR-004**: The `count` field on all three responses MUST continue to reflect the number of
  relationship objects (i.e., match the length of both `edges` and `facts`).
- **FR-005**: No existing top-level key on any of the three responses (`edges`, `facts`,
  `count`, or any other field already present) may be removed or renamed by this change.
- **FR-006**: The MCP tool schema/description (`crates/service/src/mcp/tools.rs`) for each of
  the three affected tools MUST document that the response includes both `edges` and `facts`
  as aliases for the same data, so the divergence (and its resolution) is discoverable by a
  client author reading the tool's schema rather than only the handler source.
- **FR-007**: This change applies only to the three methods named in FR-001–FR-003.
  `knowledge_get_entity_neighbors` is explicitly excluded (see Out of Scope).

### Key Entities

- **Relationship / edge object**: The JSON representation of a graph edge (fact) as already
  produced by the existing handlers — its internal shape is unchanged by this issue; only the
  top-level response envelope changes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A client that parses `edges` and a client that parses `facts` both retrieve the
  complete, correct relationship list from all three affected RPCs — verified by an automated
  test per RPC that asserts both keys are present and equal.
- **SC-002**: No existing automated test (IPC parity tests or otherwise) that asserts on the
  current single-key response shape of any of the three methods regresses — existing keys and
  values are unaffected, only a key is added.
- **SC-003**: The MCP tool schema text for each of the three affected tools mentions both
  `edges` and `facts`, so a client author reading only the schema (not the handler source)
  can discover the aliasing.

## Assumptions

- The fix is purely additive at the response-envelope level (add a second key pointing at the
  same list) — no change to the shape of individual relationship/edge objects, no change to
  request parameters, and no change to which data is queried.
- Duplicating the relationship list under two JSON keys in the same response is acceptable
  from a payload-size perspective for these methods; no pagination or size-limit change is
  in scope.
- `edges` is treated as the "canonical" name going forward (per the issue's stated preference
  and because the payload is structurally an edge list), with `facts` retained as a
  backward-compatible alias. No client-facing deprecation timeline for `facts` is defined by
  this issue.
- `knowledge_get_edges_by_group`'s existing `edges` key already matches the canonical name, so
  for that method this issue only adds the `facts` alias; for `knowledge_find_relationships`
  and `knowledge_list_relationships`, whose existing key is `facts`, this issue adds the
  `edges` alias.

## Out of Scope

- `knowledge_get_entity_neighbors`: it returns `edges` today, but as one field of a distinct
  subgraph-traversal response shape (`nodes`, `center_uuid`, `node_count`, `edge_count`,
  `edges`), not a flat relationship-list response. Adding a `facts` alias there is a
  reasonable follow-up but is a different response shape from the three methods in scope here
  and was not reported as exhibiting the silent-empty failure mode.
- Any other IPC/MCP method not named above, even if it happens to return edge-shaped data
  under a differently-named key.
- Changing or standardizing the shape of individual relationship/edge objects themselves.
- Deprecating or removing the `facts` key at any future date.
- Client-side changes (e.g., to the zen catalog/graph reader) — this issue covers only the
  server-side response shape.

## Source References

- `crates/core/src/handlers.rs`: `handle_find_relationships` (facts key), `handle_list_relationships`
  (facts key), `handle_get_edges_by_group` (edges key).
- `crates/service/src/mcp/tools.rs`: MCP tool schema entries for `knowledge_find_relationships`,
  `knowledge_list_relationships`, `knowledge_get_edges_by_group`.
- `crates/core/tests/ipc_parity.rs`: existing parity test coverage for `knowledge_get_edges_by_group`
  (`parity_get_edges_by_group_empty`), a useful pattern reference for new tests.
