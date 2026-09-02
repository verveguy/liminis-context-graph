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

**Revised fix direction (human review of PR #534, 2026-09-01):** the original report proposed
adding both `edges` and `facts` as aliases of the same list, on the theory that this is
non-breaking. That plan was superseded during review: this issue's fix is shipping in the same
release (0.14.0) as a one-way storage-format migration (lbug 0.17.0 → 0.20.1, storage v41 →
v47), which already breaks in-place upgrades in one direction. Since that release is already the
cheapest available point to make a breaking change, a permanent dual-key alias would spend that
breaking window without using it — the inconsistency would become permanent, because the next
opportunity to remove `facts` for free does not arrive until 1.0. The fix is instead a clean
rename: `knowledge_find_relationships` and `knowledge_list_relationships` now return `edges`
only, matching `knowledge_get_edges_by_group`, which was already correct and needs no change.
This is a **breaking change** for any client currently reading `facts` from either renamed
method, documented in the CHANGELOG's `[Unreleased]` section.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Client reads relationships via any affected method and gets the same key (Priority: P1)

A client (internal or external, e.g. zen's catalog/graph reader, orac/GES) calls any of
`knowledge_find_relationships`, `knowledge_list_relationships`, or `knowledge_get_edges_by_group`
and can reliably read the resulting edge list from the same top-level key, regardless of which
method it called.

**Why this priority**: This is the entire defect being fixed. Without it, a client that shares
parsing code across these methods (or that is refactored to switch which method it calls)
silently loses data instead of erroring.

**Independent Test**: Call each of the three affected RPCs against a graph with at least one
relationship in the target group, and assert that the response contains an `edges` key holding
the full relationship list with length equal to `count`, and does **not** contain a `facts` key.

**Acceptance Scenarios**:

1. **Given** a graph with N relationships in a group, **When** a client calls
   `knowledge_find_relationships` for that group, **Then** the response contains an `edges` key
   holding an N-element list, `count` equals N, and no `facts` key is present.
2. **Given** a graph with N relationships in a group, **When** a client calls
   `knowledge_list_relationships` for that group, **Then** the response contains an `edges` key
   holding an N-element list, `count` equals N, and no `facts` key is present.
3. **Given** a graph with N relationships in a group, **When** a client calls
   `knowledge_get_edges_by_group` for that group, **Then** the response contains an `edges` key
   holding an N-element list and `count` equals N (unchanged behavior — this method was already
   correct).
4. **Given** a group with zero relationships, **When** a client calls any of the three
   affected RPCs, **Then** the response contains `edges: []` and `count` equals 0 — a genuinely
   empty result is distinguishable only by an explicit `[]`, never by the key's absence.

---

### Edge Cases

- A client that reads `facts` from `knowledge_find_relationships` or `knowledge_list_relationships`
  today will, after this change, get an empty list on every call instead of an error — this is a
  deliberate breaking change (see Background), not a regression to silently avoid. The CHANGELOG
  entry is the mitigation: it names both methods and the old → new key so integrators can update
  before upgrading.
- A client that already parses `edges` from `knowledge_find_relationships` or
  `knowledge_list_relationships` today (i.e., was already hitting the bug this issue exists to
  fix) starts receiving the real, non-empty list once this change ships — this direction of
  change was, and remains, the entire point.
- `knowledge_get_edges_by_group` gains no new key. A dual-key alias would have added a `facts`
  key to a method with no callers reading it, for symmetry alone; the rename approach avoids
  that new-surface-for-no-reason cost entirely.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_find_relationships` MUST return the relationship list under the
  `edges` key. It MUST NOT include a `facts` key.
- **FR-002**: `knowledge_list_relationships` MUST return the relationship list under the
  `edges` key. It MUST NOT include a `facts` key.
- **FR-003**: `knowledge_get_edges_by_group` MUST continue to return the relationship list
  under the `edges` key, unchanged from its current behavior. It MUST NOT gain a `facts` key.
- **FR-004**: The `count` field on all three responses MUST continue to reflect the number of
  relationship objects (i.e., match `edges.len()`).
- **FR-005**: No existing top-level key on any of the three responses other than the `facts` →
  `edges` rename on the two affected methods (`edges`, `count`, or any other field already
  present) may be removed or renamed by this change.
- **FR-006**: The MCP tool schema/description (`crates/service/src/mcp/tools.rs`) for each of
  the three affected tools MUST document the actual response shape (`edges`, not `facts`), so a
  client author reading the tool's schema sees the current key rather than a stale one.
- **FR-007**: This change applies only to the three methods named in FR-001–FR-003.
  `knowledge_get_entity_neighbors` is explicitly excluded (see Out of Scope).
- **FR-008**: The CHANGELOG's `[Unreleased]` section MUST include a prominent breaking-change
  entry naming both renamed methods and the old (`facts`) → new (`edges`) key, since this is the
  only migration notice available to downstream integrators (e.g., orac/GES).

### Key Entities

- **Relationship / edge object**: The JSON representation of a graph edge (fact) as already
  produced by the existing handlers — its internal shape is unchanged by this issue; only the
  top-level response envelope's collection key changes for two of the three methods.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A client that parses `edges` retrieves the complete, correct relationship list
  from all three affected RPCs — verified by an automated test per RPC that asserts `edges` is
  present with length equal to `count`, and that `facts` is absent (so a partial revert that
  reintroduces `facts` is caught, not just a missing-`edges` regression).
- **SC-002**: No existing automated test (IPC parity tests or otherwise) that asserts on the
  `edges` key or other existing fields of any of the three methods regresses. Tests that
  previously asserted the now-removed `facts` key are updated to assert `edges` instead — this
  is an intentional, tracked update, not a regression.
- **SC-003**: The MCP tool schema text for each of the three affected tools accurately describes
  the current response shape (`edges` only), so a client author reading only the schema (not the
  handler source) sees the post-rename contract.
- **SC-004**: The CHANGELOG's `[Unreleased]` section names both renamed methods and the old →
  new key in a clearly marked breaking-change entry.

## Assumptions

- The fix changes the response-envelope key on two of the three in-scope methods (rename, not
  alias) — no change to the shape of individual relationship/edge objects, no change to request
  parameters, and no change to which data is queried.
- This is a breaking change, accepted deliberately because it ships in the same release (0.14.0)
  as an already-breaking one-way storage migration — see Background. There is no dual-key
  transition period and no deprecation timeline for `facts`; it is removed outright.
- `knowledge_get_edges_by_group`'s existing `edges` key already matches the canonical name and
  requires no change; only `knowledge_find_relationships` and `knowledge_list_relationships`
  (whose existing key was `facts`) are renamed.
- Known integrators of the renamed methods' responses (e.g., orac/GES) are notified via the
  CHANGELOG rather than via a compatibility window, since no compatibility window is being
  offered.

## Out of Scope

- `knowledge_get_entity_neighbors`: it returns `edges` today, but as one field of a distinct
  subgraph-traversal response shape (`nodes`, `center_uuid`, `node_count`, `edge_count`,
  `edges`), not a flat relationship-list response. It is unaffected by this issue's rename and
  was not reported as exhibiting the silent-empty failure mode.
- Any other IPC/MCP method not named above, even if it happens to return edge-shaped data
  under a differently-named key.
- Changing or standardizing the shape of individual relationship/edge objects themselves.
- A dual-key transition period or deprecation timeline for `facts` — superseded by the rename
  decision in Background; `facts` is removed outright, not deprecated.
- Client-side changes (e.g., to the zen catalog/graph reader, orac/GES) — this issue covers only
  the server-side response shape; the CHANGELOG entry is the coordination mechanism for
  downstream updates.

## Source References

- `crates/core/src/handlers.rs`: `handle_find_relationships`, `handle_list_relationships`
  (both renamed `facts` → `edges`), `handle_get_edges_by_group` (unchanged, already `edges`).
- `crates/service/src/mcp/tools.rs`: MCP tool schema entries for `knowledge_find_relationships`,
  `knowledge_list_relationships`, `knowledge_get_edges_by_group`.
- `crates/core/tests/ipc_parity.rs`, `crates/core/tests/ipc_response_shapes.rs`: parity and
  conformance test coverage for all three methods, including negative assertions that `facts`
  is absent.
- `docs/adr/0020-ipc-collection-envelope-contract.md`: naming-convention and audit-table entries
  for all three methods, recording the `facts` → `edges` rename decision.
- `CHANGELOG.md`: `[Unreleased]` → `### Changed` breaking-change entry.
