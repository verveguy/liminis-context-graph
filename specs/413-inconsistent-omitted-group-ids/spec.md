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

### Root cause (confirmed)

Two helper functions in `crates/core/src/handlers.rs` give **opposite** meanings to an absent
`group_ids`:

- `extract_group_ids` (`handlers.rs:4341`) — an absent/non-array value resolves to
  `vec![DEFAULT_GROUP_ID.to_string()]` (i.e. `["liminis"]`), not "no filter."
- `extract_optional_group_ids` (`handlers.rs:4356`) — an absent, `null`, or `false` value resolves to
  `None`, which its own doc comment states means "all groups." Its doc comment further notes this is
  deliberate for *deletion* methods specifically ("absent = all groups, not the default `liminis`
  group") — but the same helper backs several read handlers too, which is how the inconsistency
  reported here arose.

Every one of the eleven call sites resolves to one side or the other:

- **Absent → `["liminis"]` (the reported defect)**: `handle_find_entities` (714),
  `handle_find_relationships` (760), `handle_get_nodes_by_group` (861), `handle_get_edges_by_group`
  (878).
- **Absent → all groups (already correct)**: `handle_search_passages` (997), `handle_list_entities`
  (1048), `handle_list_relationships` (1087), `handle_get_entity_neighbors` (1127),
  `handle_get_entities_by_source` (1193), `handle_delete_by_source` (1234),
  `handle_delete_chunk_episode` (1287).

Two corrections to this issue's originally reported scope follow directly from that full split:

1. **`knowledge_list_entities` / `knowledge_list_relationships` need no audit.** They already call
   `extract_optional_group_ids` (1048, 1087) and already resolve an omitted `group_ids` to all
   groups. No behavior change is needed here — only regression coverage confirming it (FR-005).
2. **`knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group` are in scope, not out.** The
   original report assumed these "correctly require an explicit `group_ids` (by design)." They do
   not: both call `extract_group_ids` (861, 878) and silently default to `["liminis"]` when it is
   omitted — the identical defect `knowledge_find_entities` / `knowledge_find_relationships` exhibit,
   not an enforced requirement. (Their MCP schema does mark `group_ids` as `"required"`, but the
   handler itself never enforces that — an omitted value is silently accepted and mis-scoped rather
   than rejected.) This issue resolves that by aligning these two, like `knowledge_find_entities` /
   `knowledge_find_relationships`, onto the same all-groups default the other seven call sites already
   use — four handlers move, not two; this is aligning a minority onto the majority's existing
   behavior, not inventing a new default.

The tool schema itself currently hedges rather than documenting any of this precisely: the shared
`group_ids` description reads *"Omit for all groups (or the default group, depending on the tool)."*
— which tells a caller a divergence exists but not which behavior applies to which tool, so the only
way to find out today is by hitting exactly this failure.

Downstream, this breaks the co-query contract used by zen's `streams.conf` / `commands/zen-sync.md`
and by orac (#41/#42): any Claude Code session going through zen's `.mcp.json` today either gets a
silently empty entity/relationship result set, or must already know about — and hand-code around — a
divergence the tool schema doesn't actually spell out.

### Relationship to #406 (read this issue's boundary against it)

`handle_delete_by_source` and `handle_delete_chunk_episode` sit on the all-groups side of the same
split (1234, 1287) — which is what lets an unscoped delete sweep every group instead of just the
intended one, a data-loss risk tracked separately as #406. Reads sit on the default-group side, which
is what makes an unscoped search/list return nothing. The two issues are opposite-direction fixes for
the same underlying inconsistency, and together they imply a principle worth stating even though
neither issue alone requires it: **reads default to all groups; writes and deletes require an
explicit scope.** This issue (#413) fixes the read half for the four handlers named above. It does
**not** touch `handle_delete_by_source` or `handle_delete_chunk_episode` — their current
all-groups-on-omission behavior is #406's concern, not this issue's (see Out of Scope).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Reader queries or lists entities and relationships across every group with no filter (Priority: P1)

A downstream reader (e.g. a zen co-query, or any MCP client following the "omit `group_ids` for
everything" pattern that already works for passage search) calls `knowledge_find_entities`,
`knowledge_find_relationships`, `knowledge_get_nodes_by_group`, or `knowledge_get_edges_by_group` with
`group_ids` omitted and gets results scoped across every group present in the graph — not an empty
result silently scoped to an implicit default group.

**Why this priority**: This is the exact defect reported, now confirmed to affect four handlers
rather than two (Background). Without it, entity/relationship co-query and by-group listing are both
broken for every multi-group graph that doesn't happen to contain the implicit default group, with no
error surfaced to explain why.

**Independent Test**: Run the reproduction above end-to-end on the two-group fixture: call each of
the four affected tools with `group_ids` omitted, and confirm the result count and content match the
equivalent call with `group_ids: ["psetadrs","a2htest"]` explicitly supplied (10/10 for the
`"Write-Ahead Log"` search query; 29 nodes / 40 edges for the by-group listing tools, per Background's
fixture counts).

**Acceptance Scenarios**:

1. **Given** a graph with two or more populated groups and none named after the current implicit
   default group, **When** `knowledge_find_entities` is called with `group_ids` omitted, **Then** it
   returns results drawn from every group, matching an explicit call listing all group IDs.
2. **Given** the same graph, **When** `knowledge_find_relationships` is called with `group_ids`
   omitted, **Then** it returns results drawn from every group, matching an explicit call listing all
   group IDs.
3. **Given** the same graph, **When** `knowledge_get_nodes_by_group` is called with `group_ids`
   omitted, **Then** it returns nodes drawn from every group, matching an explicit call listing all
   group IDs.
4. **Given** the same graph, **When** `knowledge_get_edges_by_group` is called with `group_ids`
   omitted, **Then** it returns edges drawn from every group, matching an explicit call listing all
   group IDs.
5. **Given** a graph with zero groups (empty graph), **When** any of the four tools above is called
   with `group_ids` omitted, **Then** it returns an empty result set with `count: 0` — not an error —
   indistinguishable in shape from any other zero-match query.

---

### User Story 2 - Caller reads the tool schema and knows exactly what omission means, per tool (Priority: P2)

An MCP client author reading the tool schema for any of `knowledge_find_entities`,
`knowledge_find_relationships`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`,
`knowledge_search_passages`, `knowledge_list_entities`, or `knowledge_list_relationships` sees a
precise statement of what an omitted `group_ids` resolves to for that specific tool, with no
"depending on the tool" hedge, and does not need to run a probing query to find out.

**Why this priority**: R3/R4's discoverability goal. Once the behavior is unified (User Story 1), the
schema hedge becomes actively misleading — it implies a divergence that no longer exists for these
tools — and must be corrected regardless, so a caller isn't left second-guessing behavior that is now
consistent.

**Independent Test**: Read the `input_schema` returned for each affected tool (e.g. via
`tools/list` over MCP-stdio) and confirm each `group_ids` description states its own omitted-value
behavior without deferring to "depending on the tool," and that `knowledge_get_nodes_by_group` /
`knowledge_get_edges_by_group` no longer mark `group_ids` as `"required"`.

**Acceptance Scenarios**:

1. **Given** the MCP tool registry, **When** a client requests the schema for any of
   `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_nodes_by_group`,
   `knowledge_get_edges_by_group`, `knowledge_search_passages`, `knowledge_list_entities`, or
   `knowledge_list_relationships`, **Then** the `group_ids` field's description states plainly that
   omitting it searches/lists across all groups, with no hedge language, and (for the two by-group
   tools specifically) the field is no longer listed as `"required"`.
2. **Given** the MCP tool registry, **When** a client requests the schema for
   `knowledge_delete_by_group` — the one tool in this area that genuinely requires an explicit,
   non-empty `group_ids` and errors otherwise — **Then** the schema continues to mark `group_ids` as
   required and non-empty, unchanged from today.

---

### Edge Cases

- A graph with zero groups: omitted `group_ids` on any affected tool returns an empty result set
  (`count: 0`), not an error (Acceptance Scenario 1.5).
- A single string passed for `group_ids` instead of an array (a form already accepted by some of these
  tools) continues to be treated as a one-element filter — this issue changes only the *omitted* case,
  not how a supplied value of either shape is interpreted.
- An explicit empty array (`group_ids: []`), as distinct from an omitted or `null` value, is not
  "omitted" and therefore MUST NOT be treated as "all groups" for the four fixed tools
  (`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_nodes_by_group`,
  `knowledge_get_edges_by_group`): it resolves to zero rows, exactly as it did before this issue
  shipped (FR-009).
- `knowledge_delete_by_group`: `group_ids` remains required and non-empty, validated explicitly with
  an error when absent or empty (`handlers.rs:1340`, doc comment at 1325-1327 explains why: "a
  destructive admin op must never silently default to purging everything"). This tool is genuinely
  out of scope, unaffected by this issue (see Out of Scope) — it must not be confused with
  `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`, which are in scope (User Story 1).
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
- **FR-003**: When `group_ids` is omitted (absent or `null`), `knowledge_get_nodes_by_group` MUST
  return nodes scoped across every group present in the graph, matching FR-001's guarantee, instead of
  silently defaulting to a single implicit group.
- **FR-004**: When `group_ids` is omitted (absent or `null`), `knowledge_get_edges_by_group` MUST
  return edges scoped across every group present in the graph, matching FR-003's guarantee.
- **FR-005**: `knowledge_list_entities` and `knowledge_list_relationships` MUST continue to resolve an
  omitted `group_ids` to "all groups" (confirmed current behavior — `handlers.rs:1048` and `:1087`
  respectively); this MUST be covered by a regression test asserting the omitted-vs-explicit-all-groups
  result parity, since no such test currently exists.
- **FR-006**: Every affected tool's schema description for `group_ids` — `knowledge_find_entities`,
  `knowledge_find_relationships`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`,
  `knowledge_search_passages`, `knowledge_list_entities`, `knowledge_list_relationships` — MUST state
  precisely, per tool, what an omitted value resolves to. The shared hedge text "(or the default
  group, depending on the tool)" MUST NOT appear on any of these seven tools' schemas once this issue
  ships.
- **FR-007**: `knowledge_get_nodes_by_group` and `knowledge_get_edges_by_group`'s schemas MUST drop
  their current `"required": ["group_ids"]` constraint, since `group_ids` becomes optional for these
  two tools once FR-003/FR-004 land — the schema must reflect the new optional,
  omit-for-all-groups contract rather than continuing to advertise a requirement the handler has never
  actually enforced.
- **FR-008**: `knowledge_delete_by_group` MUST remain unchanged: `group_ids` stays required and
  non-empty, validated explicitly with an error when absent or empty, with no omitted-value fallback
  introduced. This is the one tool in this area that is genuinely required-by-design (Edge Cases).
- **FR-009**: Behavior for an explicitly supplied `group_ids` (populated array, single string, or
  empty array) MUST be unchanged by this fix for every affected tool — only the omitted case changes.
- **FR-010**: The fix MUST NOT change behavior for any tool that takes a singular `group_id` parameter
  rather than the plural `group_ids` array (e.g. `knowledge_get_episodes`) — those are out of scope
  (see Out of Scope).
- **FR-011**: The fix MUST NOT change `handle_delete_by_source` or `handle_delete_chunk_episode`'s
  current omitted-`group_ids` behavior (today: resolves to all groups). Their behavior is out of scope
  for this issue and tracked separately in #406 (see "Relationship to #406" in Background).

### Key Entities

- **`group_ids` parameter**: The optional, array-shaped filter accepted by several MCP read tools,
  distinct from the singular `group_id` parameter used elsewhere. This issue is about unifying what
  its *omission* means.
- **Omitted-`group_ids` resolution helpers**: Two functions in `crates/core/src/handlers.rs` that
  currently give opposite meanings to an absent `group_ids` — `extract_group_ids` (absent → the single
  default group) and `extract_optional_group_ids` (absent → `None`, meaning "all groups"). This issue
  eliminates that split for the four read-tool call sites still on the default-group side (Background).
- **Group**: A named partition of the graph (e.g. `psetadrs`, `a2htest`), each with its own WAL root
  per ADR-0378. A graph may hold zero, one, or many groups at once.
- **MCP tool schema registry** (`crates/service/src/mcp/tools.rs`): The hand-maintained source of
  truth for each tool's `input_schema`, including the `group_ids` field's description text and
  `required` list that FR-006/FR-007 require be corrected.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On the two-group repro fixture (Background), `knowledge_find_entities` and
  `knowledge_find_relationships` with `group_ids` omitted return the same count and entity/relationship
  set as the equivalent explicit `group_ids: ["psetadrs","a2htest"]` call (10 and 10 respectively for
  the reproduction query, matching `knowledge_search_passages`'s existing omitted-call result).
  `knowledge_get_nodes_by_group` and `knowledge_get_edges_by_group` with `group_ids` omitted likewise
  return the same 29 nodes / 40 edges as the equivalent explicit all-groups call.
- **SC-002**: The `group_ids` schema description for each of `knowledge_find_entities`,
  `knowledge_find_relationships`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`,
  `knowledge_search_passages`, `knowledge_list_entities`, and `knowledge_list_relationships` states its
  own omitted-value behavior with no "depending on the tool" hedge, and the two by-group tools' schemas
  no longer mark `group_ids` as `"required"`. `knowledge_delete_by_group` keeps its existing
  required-and-non-empty schema, unchanged.
- **SC-003**: A regression test (e.g. under `crates/service/tests/` or `crates/core/tests/`) exists
  asserting omitted-`group_ids` vs. explicit-all-groups result parity, on a fixture with two or more
  groups, for `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_nodes_by_group`,
  `knowledge_get_edges_by_group`, `knowledge_list_entities`, and `knowledge_list_relationships`.
- **SC-004**: Existing coverage for `knowledge_delete_by_group` (required, non-empty `group_ids`,
  explicit error on omission) continues to pass unmodified — the one tool in this area that keeps
  requiring an explicit scope.

## Assumptions

- Per the issue's own stated preference (R2): an omitted `group_ids` means **all groups**, not an
  explicit error and not an implicit default-group fallback, for every tool in scope — now confirmed
  to be four handlers (`knowledge_find_entities`, `knowledge_find_relationships`,
  `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`), not two. The "default group"
  behavior these four exhibit today is treated as an artifact of the pre-per-group singleton path
  (predating ADR-0378), not an intentional 0.13.x concept worth preserving behind a discovery
  mechanism.
- Because the resolved default is "all groups" rather than a retained "default group" concept, the
  issue's R4 (a discovery path enumerating existing group IDs) is not required as new work: an
  existing mechanism already serves that purpose — `knowledge_status`'s per-group WAL breakdown
  (`wal_groups`, from #378 FR-007) already enumerates every group with WAL activity, giving a caller
  that wants an explicit filter a way to build one.
- Confirmed by direct code inspection (Background: full eleven-call-site split): `knowledge_list_entities`
  (`handlers.rs:1048`), `knowledge_list_relationships` (`:1087`), `knowledge_get_entity_neighbors`
  (`:1127`), and `knowledge_get_entities_by_source` (`:1193`) already resolve an omitted `group_ids` to
  "all groups," consistent with `knowledge_search_passages`. Only `knowledge_find_entities`,
  `knowledge_find_relationships`, `knowledge_get_nodes_by_group`, and `knowledge_get_edges_by_group`
  diverge (FR-001–FR-004).
- The singular `group_id` parameter used by tools like `knowledge_get_episodes` (defaulting to a named
  default group) is a distinct, unrelated concept from the plural `group_ids` array this issue
  addresses, and is not affected.
- `handle_delete_by_source` and `handle_delete_chunk_episode` already resolve an omitted `group_ids` to
  "all groups" today, which for a delete-adjacent operation is a data-loss risk, not a convenience —
  the opposite-direction defect tracked as #406. This issue deliberately does not touch either handler
  (FR-011); reads defaulting to all groups and writes/deletes requiring an explicit scope is the
  intended end state across both issues (Background, "Relationship to #406").

## Out of Scope

- `knowledge_delete_by_group` — `group_ids` is required and non-empty by design, validated explicitly
  with an error on omission; this issue does not change that (FR-008, Edge Cases).
- Any tool taking a singular `group_id` parameter (e.g. `knowledge_get_episodes`,
  `knowledge_add_episode`), including its own default-group behavior (FR-010).
- A new "enumerate all group IDs" tool or field — `knowledge_status`'s existing `wal_groups` breakdown
  is deemed sufficient per Assumptions; introducing a dedicated discovery tool is not required by this
  issue.
- Changes to downstream consumers (zen `streams.conf`, `commands/zen-sync.md`, orac) that currently
  work around this divergence — this issue ships the lcg-side fix only; removing any downstream
  workaround is a separate, follow-up concern for those repos.
- `knowledge_get_entity_neighbors` and `knowledge_get_entities_by_source` — confirmed via code
  inspection (`handlers.rs:1127`, `:1193`) to already behave correctly (omitted → all groups); no
  change needed.
- `handle_delete_by_source` and `handle_delete_chunk_episode` — currently default an omitted
  `group_ids` to "all groups," which for a delete is a data-loss risk distinct from this issue's
  read-side defect; tracked separately in #406, not addressed here (FR-011).

## Source References

- Reproduction: two-group fixture (`psetadrs`, `a2htest`), lcg `0.13.0`,
  `liminis-context-graph --mcp-stdio --connect <sock> --scope=read`, query `"Write-Ahead Log"`,
  `num_results=10` (Background table).
- `crates/core/src/handlers.rs`: `extract_group_ids` (`:4341`) and `extract_optional_group_ids`
  (`:4356`), and the full eleven-call-site split between them (Background) — `handle_find_entities`
  (714), `handle_find_relationships` (760), `handle_get_nodes_by_group` (861),
  `handle_get_edges_by_group` (878) on the default-group side; `handle_search_passages` (997),
  `handle_list_entities` (1048), `handle_list_relationships` (1087),
  `handle_get_entity_neighbors` (1127), `handle_get_entities_by_source` (1193),
  `handle_delete_by_source` (1234), `handle_delete_chunk_episode` (1287) on the all-groups side;
  `handle_delete_by_group` (1334, explicit-required validation at 1340) as the one genuinely
  required-by-design tool.
- `crates/service/src/mcp/tools.rs`: `group_ids_prop()`, the shared schema description carrying the
  "(or the default group, depending on the tool)" hedge FR-006 removes, and the
  `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group` `"required": ["group_ids"]` entries
  FR-007 removes.
- ADR-0378 — per-group WAL root / per-group `knowledge_status` breakdown (source of the `wal_groups`
  field Assumptions relies on for discoverability).
- Downstream co-query contract: zen `streams.conf`, `commands/zen-sync.md`; consumers orac #41/#42.
- #406 — the opposite-direction defect (delete handlers defaulting an omitted `group_ids` to all
  groups); see "Relationship to #406" in Background.
