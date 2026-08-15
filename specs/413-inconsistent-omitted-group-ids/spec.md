# Feature Specification: Consistent Omitted-`group_ids` Semantics Across MCP Read Tools

**Feature Branch**: `fabrik/issue-413`
**Created**: 2026-08-15
**Status**: Specified
**Input**: User description: "On a multi-group graph (0.13.0 per-group WAL root, ADR-0378), the MCP
read tools disagree on what an omitted `group_ids` argument means. Passage search treats it as 'all
groups'; entity and relationship search treat it as an (empty) default group and return nothing. A
reader that follows the documented 'query them all together with no filter' contract therefore sees
an empty graph for entities/relationships, with no error to indicate why."

## Background

Since 0.13.0's move to a per-group WAL root (ADR-0378), a graph routinely holds several groups at
once, and the documented pattern for a read-only consumer that wants "everything, no filter" is to
omit `group_ids` entirely. `knowledge_search_passages` honors that contract. `knowledge_find_entities`
and `knowledge_find_relationships` do not: omitting `group_ids` silently resolves to a single
implicit default group rather than "no filter," and on a graph whose groups don't happen to include
that default, the call returns zero results with no error — indistinguishable from a genuinely empty
graph.

Reproduced against lcg `0.13.0` (`liminis-context-graph --mcp-stdio --connect <sock> --scope=read`),
two hydrated groups (`psetadrs`, `a2htest`; 29 entities / 40 relationships total), query
`"Write-Ahead Log"`, `num_results=10`:

| tool | `group_ids` omitted | `group_ids: ["psetadrs","a2htest"]` |
|---|---|---|
| `knowledge_search_passages` | 10 | 10 |
| `knowledge_find_entities` | **0** | 10 |
| `knowledge_find_relationships` | **0** | 10 |

The tool schema itself currently hedges rather than documenting this: the shared `group_ids`
description reads *"Omit for all groups (or the default group, depending on the tool)."* — which
tells a caller a divergence exists but not which behavior applies to which tool, so the only way to
find out today is by hitting exactly this failure.

A lightweight check against the current `crates/core/src/handlers.rs` confirms the shape of the
divergence: `knowledge_find_entities` and `knowledge_find_relationships` fall back to a single
hard-coded default group ID when `group_ids` is absent, while `knowledge_search_passages`,
`knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, and
`knowledge_get_entities_by_source` already fall back to "no filter, all groups" when it is absent.
This narrows the issue's requested audit of `knowledge_list_entities` / `knowledge_list_relationships`
to a confirmation-plus-regression-test task (FR-003) rather than a further behavior change — Research
should still verify this precisely against the code and add the coverage, since it hasn't yet been
exercised by a dedicated test.

Downstream, this breaks the co-query contract used by zen's `streams.conf` / `commands/zen-sync.md`
and by orac (#41/#42): any Claude Code session going through zen's `.mcp.json` today either gets a
silently empty entity/relationship result set, or must already know about — and hand-code around — a
divergence the tool schema doesn't actually spell out.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Reader queries entities/relationships across every group with no filter (Priority: P1)

A downstream reader (e.g. a zen co-query, or any MCP client following the "omit `group_ids` for
everything" pattern that already works for passage search) calls `knowledge_find_entities` or
`knowledge_find_relationships` with `group_ids` omitted and gets results scoped across every group
present in the graph — not an empty result silently scoped to an implicit default group.

**Why this priority**: This is the exact defect reported. Without it, entity/relationship co-query is
broken for every multi-group graph that doesn't happen to contain the implicit default group, with no
error surfaced to explain why.

**Independent Test**: Run the reproduction above end-to-end on the two-group fixture: call
`knowledge_find_entities` and `knowledge_find_relationships` for `"Write-Ahead Log"` with `group_ids`
omitted, and confirm the result count and content match the equivalent call with
`group_ids: ["psetadrs","a2htest"]` explicitly supplied.

**Acceptance Scenarios**:

1. **Given** a graph with two or more populated groups and none named after the current implicit
   default group, **When** `knowledge_find_entities` is called with `group_ids` omitted, **Then** it
   returns results drawn from every group, matching an explicit call listing all group IDs.
2. **Given** the same graph, **When** `knowledge_find_relationships` is called with `group_ids`
   omitted, **Then** it returns results drawn from every group, matching an explicit call listing all
   group IDs.
3. **Given** a graph with zero groups (empty graph), **When** either tool is called with `group_ids`
   omitted, **Then** it returns an empty result set with `count: 0` — not an error — indistinguishable
   in shape from any other zero-match query.

---

### User Story 2 - Caller reads the tool schema and knows exactly what omission means, per tool (Priority: P2)

An MCP client author reading the tool schema for any of `knowledge_find_entities`,
`knowledge_find_relationships`, `knowledge_search_passages`, `knowledge_list_entities`, or
`knowledge_list_relationships` sees a precise statement of what an omitted `group_ids` resolves to for
that specific tool, with no "depending on the tool" hedge, and does not need to run a probing query to
find out.

**Why this priority**: R3/R4's discoverability goal. Once the behavior is unified (User Story 1), the
schema hedge becomes actively misleading — it implies a divergence that no longer exists for these
tools — and must be corrected regardless, so a caller isn't left second-guessing behavior that is now
consistent.

**Independent Test**: Read the `input_schema` returned for each affected tool (e.g. via
`tools/list` over MCP-stdio) and confirm each `group_ids` description states its own omitted-value
behavior without deferring to "depending on the tool."

**Acceptance Scenarios**:

1. **Given** the MCP tool registry, **When** a client requests the schema for `knowledge_find_entities`,
   `knowledge_find_relationships`, `knowledge_search_passages`, `knowledge_list_entities`, or
   `knowledge_list_relationships`, **Then** the `group_ids` field's description states plainly that
   omitting it searches/lists across all groups, with no hedge language.
2. **Given** the MCP tool registry, **When** a client requests the schema for
   `knowledge_get_nodes_by_group` or `knowledge_get_edges_by_group`, **Then** the schema continues to
   mark `group_ids` as required and non-empty, unchanged from today.

---

### Edge Cases

- A graph with zero groups: omitted `group_ids` on any affected tool returns an empty result set
  (`count: 0`), not an error (Acceptance Scenario 1.3).
- A single string passed for `group_ids` instead of an array (a form already accepted by some of these
  tools) continues to be treated as a one-element filter — this issue changes only the *omitted* case,
  not how a supplied value of either shape is interpreted.
- `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`: `group_ids` remains required and
  non-empty; explicit by design, untouched by this issue (issue Scope: Out).
- Tools that take a singular `group_id` (not the plural, array-shaped `group_ids`) — e.g.
  `knowledge_get_episodes`, which defaults to a named default group — are a different parameter
  shape entirely and are out of scope for this issue (see Out of Scope).
- An explicit `group_ids` call naming a group that doesn't exist in the graph: unaffected by this
  issue; continues to return an empty result for that group, as today.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When `group_ids` is omitted (absent or `null`), `knowledge_find_entities` MUST return
  results scoped across every group present in the graph, matching the result set of an explicit call
  listing every group ID — not a result scoped to an implicit single default group.
- **FR-002**: When `group_ids` is omitted (absent or `null`), `knowledge_find_relationships` MUST
  return results scoped across every group present in the graph, matching FR-001's guarantee.
- **FR-003**: `knowledge_list_entities` and `knowledge_list_relationships` MUST continue to resolve an
  omitted `group_ids` to "all groups" (their current behavior per this spec's Background); this MUST
  be covered by a regression test asserting the omitted-vs-explicit-all-groups result parity, since no
  such test currently exists.
- **FR-004**: Every affected tool's schema description for `group_ids`
  (`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`,
  `knowledge_list_entities`, `knowledge_list_relationships`) MUST state precisely, per tool, what an
  omitted value resolves to. The shared hedge text "(or the default group, depending on the tool)"
  MUST NOT appear on any of these five tools' schemas once this issue ships.
- **FR-005**: `knowledge_get_nodes_by_group` and `knowledge_get_edges_by_group` MUST remain unchanged:
  `group_ids` stays required and non-empty, with no omitted-value behavior introduced.
- **FR-006**: Behavior for an explicitly supplied `group_ids` (populated array, single string, or
  empty array) MUST be unchanged by this fix for every affected tool — only the omitted case changes.
- **FR-007**: The fix MUST NOT change behavior for any tool that takes a singular `group_id` parameter
  rather than the plural `group_ids` array (e.g. `knowledge_get_episodes`) — those are out of scope
  (see Out of Scope).

### Key Entities

- **`group_ids` parameter**: The optional, array-shaped filter accepted by several MCP read tools,
  distinct from the singular `group_id` parameter used elsewhere. This issue is about unifying what
  its *omission* means.
- **Group**: A named partition of the graph (e.g. `psetadrs`, `a2htest`), each with its own WAL root
  per ADR-0378. A graph may hold zero, one, or many groups at once.
- **MCP tool schema registry** (`crates/service/src/mcp/tools.rs`): The hand-maintained source of
  truth for each tool's `input_schema`, including the `group_ids` field's description text that FR-004
  requires be made tool-specific.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On the two-group repro fixture (Background), `knowledge_find_entities` and
  `knowledge_find_relationships` with `group_ids` omitted return the same count and entity/relationship
  set as the equivalent explicit `group_ids: ["psetadrs","a2htest"]` call (10 and 10 respectively for
  the reproduction query, matching `knowledge_search_passages`'s existing omitted-call result).
- **SC-002**: The `group_ids` schema description for each of `knowledge_find_entities`,
  `knowledge_find_relationships`, `knowledge_search_passages`, `knowledge_list_entities`, and
  `knowledge_list_relationships` states its own omitted-value behavior with no "depending on the tool"
  hedge; `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group` keep their existing
  required-and-non-empty schema, unchanged.
- **SC-003**: A regression test (e.g. under `crates/service/tests/` or `crates/core/tests/`) exists
  asserting omitted-`group_ids` vs. explicit-all-groups result parity, on a fixture with two or more
  groups, for `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_list_entities`,
  and `knowledge_list_relationships`.
- **SC-004**: Existing coverage for `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`
  (required, non-empty `group_ids`) continues to pass unmodified.

## Assumptions

- Per the issue's own stated preference (R2) and this spec's resolution of its two Open Questions: an
  omitted `group_ids` means **all groups**, not an explicit error and not an implicit default-group
  fallback, for every tool in scope. The "default group" behavior `knowledge_find_entities` /
  `knowledge_find_relationships` exhibit today is treated as an artifact of the pre-per-group
  singleton path (predating ADR-0378), not an intentional 0.13.x concept worth preserving behind a
  discovery mechanism.
- Because the resolved default is "all groups" rather than a retained "default group" concept, the
  issue's R4 (a discovery path enumerating existing group IDs) is not required as new work: an
  existing mechanism already serves that purpose — `knowledge_status`'s per-group WAL breakdown
  (`wal_groups`, from #378 FR-007) already enumerates every group with WAL activity, giving a caller
  that wants an explicit filter a way to build one.
- A lightweight check of the current codebase (this spec's Background) indicates
  `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, and
  `knowledge_get_entities_by_source` already resolve an omitted `group_ids` to "all groups," consistent
  with `knowledge_search_passages`; only `knowledge_find_entities` and `knowledge_find_relationships`
  diverge. Research should verify this precisely (it is based on a quick read, not exhaustive testing)
  before treating FR-003 as test-only rather than a behavior change.
- The singular `group_id` parameter used by tools like `knowledge_get_episodes` (defaulting to a named
  default group) is a distinct, unrelated concept from the plural `group_ids` array this issue
  addresses, and is not affected.

## Out of Scope

- `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group` — `group_ids` is required and
  non-empty by design; this issue does not change that (issue's own Scope: Out).
- Any tool taking a singular `group_id` parameter (e.g. `knowledge_get_episodes`,
  `knowledge_add_episode`), including its own default-group behavior.
- A new "enumerate all group IDs" tool or field — `knowledge_status`'s existing `wal_groups` breakdown
  is deemed sufficient per Assumptions; introducing a dedicated discovery tool is not required by this
  issue.
- Changes to downstream consumers (zen `streams.conf`, `commands/zen-sync.md`, orac) that currently
  work around this divergence — this issue ships the lcg-side fix only; removing any downstream
  workaround is a separate, follow-up concern for those repos.
- `knowledge_get_entity_neighbors` and `knowledge_get_entities_by_source` — per Assumptions, a
  lightweight check indicates these already behave correctly (omitted → all groups); they are covered
  only to the extent Research's verification pass confirms no change is needed.

## Source References

- Reproduction: two-group fixture (`psetadrs`, `a2htest`), lcg `0.13.0`,
  `liminis-context-graph --mcp-stdio --connect <sock> --scope=read`, query `"Write-Ahead Log"`,
  `num_results=10` (Background table).
- `crates/core/src/handlers.rs`: `knowledge_find_entities` / `knowledge_find_relationships` handlers
  (omitted-`group_ids` default) vs. `knowledge_search_passages` / `knowledge_list_entities` /
  `knowledge_list_relationships` / `knowledge_get_entity_neighbors` / `knowledge_get_entities_by_source`
  handlers (existing "all groups" default).
- `crates/service/src/mcp/tools.rs`: `group_ids_prop()`, the shared schema description carrying the
  "(or the default group, depending on the tool)" hedge FR-004 removes.
- ADR-0378 — per-group WAL root / per-group `knowledge_status` breakdown (source of the `wal_groups`
  field Assumptions relies on for discoverability).
- Downstream co-query contract: zen `streams.conf`, `commands/zen-sync.md`; consumers orac #41/#42.
