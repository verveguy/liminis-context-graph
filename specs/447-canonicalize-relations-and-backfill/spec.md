# Feature Specification: Group-Scope `canonicalize_relations` and `backfill_relation_types`

**Feature Branch**: `fabrik/issue-447`
**Created**: 2026-08-20
**Status**: Draft
**Input**: User description: "`knowledge_canonicalize_relations` and `knowledge_backfill_relation_types`
select candidates database-wide with no `group_id` filter and flush their mutations to the default
group's WAL stream. On a multi-group workspace both behaviours are wrong, and they are the two
standing exceptions to ADR-0371's rule that a write in group G touches only G's data."

## Background

Both operations document the limitation in code today (`crates/core/src/canonicalize.rs:468`,
`crates/core/src/backfill.rs:207`): they select `RelatesToNode_` candidates database-wide with no
`group_id` filter, and route their mutations through the default group's writer. Neither handler
accepts a `group_id` parameter at all. By contrast, the other two ontology-driven maintenance
operations — `knowledge_reprocess_entity_types` (`handlers.rs:3536`) and
`knowledge_reprocess_relation_types` (`handlers.rs:3917`) — both take one, so the four
ontology-driven maintenance operations are split down the middle today.

Two distinct defects follow, each with a precedent this repo has already treated as serious:

1. **Cross-group mutation.** One invocation rewrites relation types on every group's edges using a
   single workspace vocabulary. This is the failure class of #368 (a merge destroying another
   group's edge) and #406 (an unscoped delete sweeping every group), both fixed as data-integrity
   issues. A consumer that hydrates many independently-produced streams into one workspace — the
   deployment shape 0.13.0 exists to serve — has no way to canonicalize its own group without
   touching everyone else's.
2. **Wrong stream attribution.** Mutations affecting group X are written to the default group's
   WAL. This is exactly #385, which fixed the same defect in `delete_by_group` and
   `rebind_pointers`; these two operations were left as a documented limitation. The consequence is
   worse after #378 than before it: a downstream consumer replaying the default stream receives
   another group's mutations, and the owning group's stream never records changes to its own data.

### The deployment invariant that sharpens this issue's severity

The deployment model this project targets is: **a consumer that hydrates a stream treats the
resulting group graph as read-only.** The sole exception is the stream's owner, which ingests its
own WAL to replay or rebuild its database.

Under that invariant, the exposure here is not "an operator might scope a call wrongly." Because
neither handler takes a `group_id` at all, there is no way to express "operate on only the group I
own" — so the *correct, intended* use of either operation is exactly what causes the damage:

> An owner runs `canonicalize_relations` (or `backfill_relation_types`) against the one group it
> legitimately owns, and rewrites relation types across every co-resident received group as a side
> effect — groups it is explicitly not permitted to write to, whose owners are elsewhere.

The blast radius scales with how many streams a workspace hydrates — precisely the deployment shape
0.13.0 was built for. It is also invisible at the point of harm: the affected groups are read-only
to this node, so nothing else it does would ever surface the difference. The owner of the corrupted
group finds out, if ever, only on its own next comparison.

This invariant is why an omitted `group_id` cannot be treated as a permissive convenience (see
FR-005): under it, there is no legitimate caller for whom "operate on every group in this
workspace" is the correct request, because no node is permitted to write to more than the group(s)
it owns.

Whether to go further and refuse an operation against a `group_id` this node does not own is a
separate, larger question — it requires representing ownership, which lcg does not do today (the
same gap that surfaced around generation minting in #431). That is not needed to fix this issue:
scoping the operation to a named group removes the collateral damage regardless of who owns what
(see Assumptions and Out of Scope).

### Relationship to #446

#446 asks for per-`group_id` ontologies. It cannot be built safely on top of these two operations as
they stand: a per-group vocabulary applied by a database-wide rewrite would apply group X's ontology
to group Y's edges — strictly worse than today, where a single vocabulary at least applies
uniformly. This issue is a prerequisite for that one, and stands on its own regardless of whether
per-group ontology ships.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Owner canonicalizes or backfills only the group it owns (Priority: P1)

An operator that owns group A and has also hydrated co-resident group B (received from another
node's stream) calls `knowledge_canonicalize_relations` with `group_id: "A"`. Only group A's
relation types are rewritten; group B's edges and WAL stream are untouched. The same holds for
`knowledge_backfill_relation_types`.

**Why this priority**: This is the exact defect reported (Background) — today this call rewrites
every co-resident group's edges as a side effect of legitimate, correct use, with no way to express
the intended scope at all.

**Independent Test**: On a fixture with two populated groups A and B, call
`knowledge_canonicalize_relations` (and separately `knowledge_backfill_relation_types`) with
`group_id: "A"`. Confirm group A's relation types change as expected, group B's edges are
byte-identical to their pre-call state, and B's WAL stream (`applied_seq` and contents) is
unchanged.

**Acceptance Scenarios**:

1. **Given** a workspace with groups A and B both populated with `RelatesToNode_` rows, **When**
   `knowledge_canonicalize_relations` is called with `group_id: "A"`, **Then** only A's relation
   types are rewritten and B's edges are unchanged.
2. **Given** the same workspace, **When** the call in scenario 1 completes, **Then** the resulting
   mutations appear in A's WAL stream and B's WAL stream (`applied_seq`, contents) is untouched.
3. **Given** the same workspace, **When** `knowledge_backfill_relation_types` is called with
   `group_id: "A"`, **Then** the same two guarantees (scenarios 1–2) hold for it.

---

### User Story 2 - An omitted `group_id` is rejected, not run database-wide (Priority: P1)

An operator calls `knowledge_canonicalize_relations` or `knowledge_backfill_relation_types` without
supplying `group_id`. The call returns an error and performs no candidate selection or WAL write,
rather than falling back to a database-wide rewrite or to the default group.

**Why this priority**: Per the deployment invariant (Background), there is no legitimate caller for
whom the unscoped, database-wide behavior is correct — every existing caller is, by definition,
either passing a scope it should have been passing already, or relying on behavior that has no
valid use case. Rejecting the call is what closes the exposure; a compatible-but-quiet default
(e.g. silently substituting the default group) would preserve a quieter version of the same
footgun.

**Independent Test**: Call each operation with `group_id` omitted and confirm the call returns an
error before touching any candidate rows or WAL stream, verified by comparing pre- and post-call
state on disk.

**Acceptance Scenarios**:

1. **Given** any workspace, **When** `knowledge_canonicalize_relations` is called with `group_id`
   omitted, **Then** the call returns an error and no edges in any group are modified.
2. **Given** any workspace, **When** `knowledge_backfill_relation_types` is called with `group_id`
   omitted, **Then** the call returns an error and no edges in any group are modified.

---

### Edge Cases

- A `group_id` naming a group with zero `RelatesToNode_` candidates: the call succeeds as a no-op —
  no rows rewritten, no WAL entry written for that group.
- A `group_id` naming a group that does not exist in the workspace at all: treated the same as the
  zero-candidate case above (a no-op), not an error — consistent with how other group-scoped reads
  in this codebase treat an unknown group ID.
- A `group_id` naming a group this node does not own (e.g. a hydrated, read-only group): this issue
  does not detect or refuse that case — see Background and Out of Scope. Scoping to a named group
  still eliminates the collateral damage to *other* co-resident groups even though it does not, by
  itself, prevent an owner from being asked to canonicalize a group it does not own.
- Empty string or otherwise malformed `group_id`: treated the same as omitted (User Story 2) —
  rejected with an error, not treated as a valid scope.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_canonicalize_relations` MUST accept a `group_id` parameter and restrict
  candidate selection to that group's `RelatesToNode_` rows only.
- **FR-002**: `knowledge_backfill_relation_types` MUST do the same.
- **FR-003**: Mutations MUST be attributed to the WAL stream of the group named by `group_id` —
  never the default group's writer — per ADR-0385's pattern. Because `group_id` is required
  (FR-005), a single call touches exactly one group's data and one group's WAL stream; no
  per-group flush-splitting is needed, unlike `delete_by_group`, which spans groups by design.
- **FR-004**: Neither operation may write to any group other than the one named by `group_id`.
- **FR-005**: `group_id` is a required parameter for both operations. Omitting it (absent, `null`,
  or empty) MUST return an error and MUST NOT run the operation database-wide, and MUST NOT fall
  back to the default group. This is a breaking change for any existing caller relying on the
  current unscoped behavior — per the deployment invariant (Background), that behavior has no
  legitimate use case to preserve, so there is no compatible fallback worth keeping.
- **FR-006**: Tests MUST demonstrate that canonicalizing group A leaves group B's edges
  byte-identical and B's WAL stream (`applied_seq`, contents) untouched, asserted on disk — the
  property that caught #385. Equivalent tests MUST cover `backfill_relation_types`.
- **FR-007**: The in-code "documented limitation" comments in `canonicalize.rs:468` and
  `backfill.rs:207` MUST be removed or rewritten, since they will no longer describe the behavior.

### Key Entities

- **`group_id` parameter**: A required, singular scope parameter both operations currently lack.
  Distinct from the plural `group_ids` array used by several MCP read tools (see #413) — this
  issue's parameter shape matches the existing singular `group_id` already accepted by
  `knowledge_reprocess_entity_types` and `knowledge_reprocess_relation_types`.
- **Group ownership**: Which node is entitled to write to a given group. Not represented anywhere
  in lcg today (Background, Assumptions); this issue does not add it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `canonicalize_relations` scoped to group A rewrites only A's relation types; group
  B's edges and B's `applied_seq`/stream contents are unchanged.
- **SC-002**: A's mutations appear in A's WAL stream, not the default group's.
- **SC-003**: The same two guarantees (SC-001, SC-002) hold for `backfill_relation_types`.
- **SC-004**: The in-code "documented limitation" comments in `canonicalize.rs` and `backfill.rs`
  are removed, because they no longer describe the behavior.
- **SC-005**: A call to either operation with `group_id` omitted returns an error and results in no
  candidate selection and no WAL write to any group, verified by a test comparing on-disk state
  before and after the call.

## Assumptions

- The deployment invariant stated in Background — a non-owner node treats a hydrated group's graph
  as read-only, and only the owning node writes to its own group — is treated as a given property of
  the deployment model this issue defends, not something this issue's code changes verify or
  enforce.
- Enforcing that a caller-supplied `group_id` actually belongs to a group this node owns is out of
  scope: it requires representing ownership, which lcg does not do anywhere today. Scoping the
  operation to a named group (FR-001–FR-005) removes the cross-group collateral damage regardless
  of who owns what, which is sufficient to close this issue even without ownership enforcement.
- Per #413's established rule ("reads default to all groups; writes and deletes require an explicit
  scope"), both operations here are writes, so requiring an explicit `group_id` is consistent with
  existing precedent, not a new policy invented for this issue.

## Out of Scope

- Representing group ownership, or refusing an operation against a `group_id` this node does not
  own. A larger, separate effort (Background); noted as related to the ownership gap that also
  surfaced in #431.
- Per-group ontology (#446) — this issue is a prerequisite for it but does not implement it.
- Deciding the release channel (patch release vs. normal 0.14.0 cycle) for this fix — see Open
  Questions.

## Open Questions

- [ ] **Is this urgent enough to ship as a patch release ahead of 0.14.0, or does it follow the
  normal release cycle?** No data loss has been reported to date. The exposure is real for any
  workspace that hydrates multiple groups and invokes either operation — which describes the
  orac/zen deployment — so the triage question is whether those deployments actually call
  `knowledge_canonicalize_relations` or `knowledge_backfill_relation_types` today. This does not
  block the technical requirements above, but should be resolved before this issue moves to
  Research so the implementation's target release is clear.

## Prior art

- **ADR-0371** — a write in group G touches only G's data.
- **ADR-0385** — per-group mutation attribution for multi-group writers; the pattern FR-003 follows.
- **ADR-0368** — group-scoping a database-wide query to stop one group's operation destroying
  another's data.

## Source References

- `crates/core/src/canonicalize.rs:468`, `crates/core/src/backfill.rs:207` — the documented
  limitation comments FR-007/SC-004 remove.
- `crates/core/src/handlers.rs:3536` (`knowledge_reprocess_entity_types`), `:3917`
  (`knowledge_reprocess_relation_types`) — the two sibling ontology-maintenance operations that
  already take a `group_id`, establishing the parameter shape this issue aligns onto.
- #368, #406 — cross-group mutation precedents (Background).
- #385 — the per-group WAL attribution fix this issue's FR-003 follows, previously left undone for
  these two operations.
- #413 — "reads default to all groups; writes and deletes require an explicit scope," the precedent
  Assumptions cites for FR-005.
- #431 — where the same group-ownership representation gap previously surfaced.
- #446 — per-group ontology, blocked on this issue (Background).
