# Feature Specification: Resolve Edge Endpoints Against the Global Entity Table

**Feature Branch**: `fabrik/issue-209`
**Created**: 2026-07-24
**Status**: Draft
**Input**: User description: "Edges to already-existing entities silently dropped — resolve edge endpoints against the global entity table"

## Background

During ingest, edges whose endpoint was created in an earlier batch are silently dropped:

```
liminis-context-graph: dropping edge with unresolvable endpoint: 'Apple' → 'Creator Studio' (src_in_list=false, dst_in_list=true)
```

In this example the destination ('Creator Studio') is in the current chunk's extracted entity list, but the source ('Apple', a recurring hub entity that was created in an earlier batch) is not — so the edge is dropped even though 'Apple' already exists in the graph. This was reported in #202.

Root cause: endpoint resolution during ingest is **batch-local**. The validation step that decides whether an edge is kept builds its set of "known" entity names purely from the entities the LLM extracted from the *current* chunk — it never consults the persisted `Entity` table. Entities themselves *do* resolve globally elsewhere in the ingest pipeline (via a group-scoped, case-insensitive name lookup), but only after edge validation has already run and already dropped the edge.

There are two separate places in the ingest pipeline where this batch-local assumption causes a drop, not one: the edge validation predicate itself, and a second, independent endpoint→UUID mapping step used later in the same ingest flow to actually persist the edge. Both currently derive their notion of "known entities" only from the current batch's extraction result, and both must be fixed together — a fix that only covers one still drops edges at the other.

The machinery needed to fix this already exists in the codebase: a group-scoped, case-insensitive exact-name entity lookup against the persisted `Entity` table, with a deterministic (earliest-created) result when more than one entity shares a name within a group. This issue is about routing edge-endpoint resolution through that existing lookup as a fallback when an endpoint isn't in the current batch, rather than building new resolution machinery.

**Dependency note**: at the time this issue was filed, it was blocked on a separate in-flight fix (tracked as #203 → landed as #208) that also touches the same ingest code. That work has since merged to `main`, including changes to the surrounding Phase B dedup/resolution logic. The Research stage should re-locate the current validation and endpoint-mapping code against `main` as it exists now rather than relying on any file/line references quoted in prior discussion of this bug, since those have likely shifted.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Edge to a previously-ingested entity is preserved across batches (Priority: P1)

An operator ingests one document that creates an entity (e.g. 'Apple'), and later ingests a second, separate document whose extracted edges reference that same entity by name as an edge endpoint (e.g. `Apple → Creator Studio`). The edge is persisted in the graph rather than silently dropped.

**Why this priority**: this is the exact reported failure (#202) — recurring "hub" entities that show up across many ingested documents currently lose their edges to anything extracted in a later batch, which silently degrades graph connectivity over time with no error surfaced to the operator.

**Independent Test**: Ingest a document that creates entity 'Apple' (no edges required in this step). Ingest a second, independent document whose extraction produces an edge from 'Apple' to some other entity 'X' that is in this second batch. Query the graph and assert the `Apple → X` edge exists.

**Acceptance Scenarios**:

1. **Given** an entity 'Apple' was created by a prior ingest batch, **When** a later ingest batch extracts an edge `Apple → X` where 'X' is created in that same later batch, **Then** the edge is persisted and queryable.
2. **Given** the same setup, **When** the edge is persisted, **Then** it correctly links to the *existing* 'Apple' entity (no duplicate 'Apple' entity is created as a side effect of edge resolution).

---

### User Story 2 - Group isolation is preserved during cross-batch resolution (Priority: P1)

Two different `group_id`s (tenants/workspaces) each have an entity with the same name (e.g. 'Apple' exists independently in both group A and group B). An edge extracted in group A referencing 'Apple' as an endpoint must resolve only to group A's 'Apple', never to group B's, even when group A's 'Apple' isn't in the current batch.

**Why this priority**: the fix introduces a new fallback lookup path; if that lookup isn't correctly scoped, it creates a cross-tenant data leak (an edge in one group silently linking to another group's entity) — an unacceptable regression that must be verified alongside the primary fix.

**Independent Test**: Create entity 'Apple' independently under `group_id` A and `group_id` B. Ingest a batch under `group_id` A with an edge `Apple → X` where 'Apple' is absent from that batch. Assert the resulting edge's source resolves to group A's 'Apple' entity, and that no edge or entity relationship is created against group B's 'Apple'.

**Acceptance Scenarios**:

1. **Given** an entity with the same name exists under two different `group_id`s, **When** an edge in one group references that name as an endpoint not present in the current batch, **Then** the edge resolves only to the entity in the same `group_id`, never the other group's entity.

---

### Edge Cases

- **Endpoint missing from both the batch and the global table.** The endpoint cannot be resolved at all — the edge is still dropped and logged, exactly as it is today. This fix narrows *when* a drop happens; it does not remove the drop-and-log behavior for genuinely unresolvable endpoints.
- **Both endpoints of an edge are missing from the current batch.** Each endpoint (source and destination) is resolved independently via the same fallback; an edge is kept only if both ultimately resolve (locally or globally), and dropped (with logging identifying which side(s) failed) if either does not.
- **Multiple entities share the same name within a group.** The fallback lookup returns a single, deterministic match (the earliest-created entity with that name in that group) — this is the existing, already-established behavior for entity name resolution elsewhere in ingest, not a new matching policy introduced by this fix.
- **Endpoint name differs only by case from the persisted entity.** Resolution is case-insensitive, matching the existing case-insensitive entity lookup behavior.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When validating whether an edge's endpoints are resolvable, an endpoint name absent from the current batch's extracted entity set MUST be checked against the persisted `Entity` table (exact name match, case-insensitive) before the edge is rejected.
- **FR-002**: The same global fallback resolution MUST also apply at the later step that maps endpoint names to entity identifiers for persistence — an edge that survives validation must not be dropped at this second step solely because its endpoint isn't in the current batch's extraction result.
- **FR-003**: Global fallback resolution MUST be scoped to the edge's `group_id` — an endpoint MUST NOT resolve to an entity belonging to a different `group_id`, even if the name matches.
- **FR-004**: An edge endpoint that cannot be resolved either from the current batch or from the persisted `Entity` table (within the same `group_id`) MUST still be dropped, with the existing log message identifying the unresolved endpoint(s).
- **FR-005**: When more than one persisted entity in the same `group_id` shares the endpoint's name, resolution MUST deterministically pick the same single entity that the existing entity-resolution logic elsewhere in ingest would pick (earliest-created), rather than picking arbitrarily or failing.
- **FR-006**: This fix MUST NOT change entity deduplication behavior, and MUST NOT introduce fuzzy or embedding-based endpoint matching — only exact-name, case-insensitive matching is in scope.

### Key Entities

- **Entity**: A node in the persisted knowledge graph, scoped to a `group_id`, identified by name (with case-insensitive lookup support) and a stable identifier used as an edge endpoint reference.
- **Edge (relationship)**: A directed connection between two entities, extracted per ingest batch, referencing its endpoints by name at extraction time.
- **Batch (ingest chunk)**: One unit of extraction work (e.g. one document or chunk) that produces a set of entities and edges; today, edge endpoint resolution only considers entities extracted within the same batch.
- **`group_id`**: The tenant/workspace scope that both entities and edges belong to; resolution must never cross this boundary.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Ingesting a document that creates an entity, followed by ingesting a separate document whose edges reference that entity by name as an endpoint, results in those edges being persisted and queryable (currently they are silently dropped).
- **SC-002**: An entity name that exists identically under two different `group_id`s never causes an edge in one group to resolve against the other group's entity.
- **SC-003**: A new integration test exists that reproduces the cross-batch endpoint scenario, fails against the current (unfixed) code path, and passes after the fix.
- **SC-004**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass.

## Assumptions

- The existing group-scoped, case-insensitive, earliest-created-wins entity name lookup is the correct and sufficient mechanism to reuse for this fallback — this issue is about applying that existing mechanism at the two endpoint-resolution sites, not designing new matching logic.
- Exact-name matching is sufficient to resolve the reported case; fuzzy or embedding-based endpoint matching is a separate, out-of-scope concern.
- The dependency this issue was originally blocked on (tracked as #203, implemented and merged as #208) has landed on `main`; this spec assumes the fix is implemented against current `main`, not against the code as it existed when this issue was first filed.

## Out of Scope

- Fuzzy or embedding-based matching of edge endpoint names.
- Any change to entity deduplication behavior or thresholds.
- Changing how entities themselves are created or deduplicated — this issue is scoped to edge endpoint *resolution*, not entity resolution.

## Source References

- **liminis-context-graph#202**: original bug report this issue's engineering work closes out.
- **liminis-context-graph#203 / #208**: previously blocking dependency (index auto-heal fix), now merged to `main` — the exact code sites for this fix should be re-located against current `main`, not against line numbers referenced in earlier discussion of this bug.
