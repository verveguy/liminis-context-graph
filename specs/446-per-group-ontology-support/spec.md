# Feature Specification: Per-Group Ontology Support

**Feature Branch**: `fabrik/issue-446`
**Created**: 2026-08-20
**Status**: Specified
**Input**: User description: "Per-group ontology support: distinct ontologies for co-resident group_ids in one workspace"

## Background

Workspace-scoped ontology landed in #83 — one `{workspace}/.lcg/ontology.yaml`, loaded once at
startup. But lcg happily holds **many co-resident groups in one workspace** (multi-group hydrate,
co-queryable through the knowledge-reader, kept disjoint by `group_id`), and those groups have
**different domains that want different ontologies**. A single workspace ontology can't serve them:
turning on `mode: strict` or a `Person`/`Organization` vocabulary for one group wrongly constrains
every other group in the same instance.

A downstream system (orac/tarial/zen) hydrates many independently-produced streams into **one** lcg
instance — each stream is its own `group_id` with its own subject domain:

1. **Producer side** — each channel wants its own ontology to guide/canonicalize extraction for
   *its* group only.
2. **A special `orac-catalog` group** whose ontology is `KnowledgeChannel`/`Topic`/`Team`
   (+ `COVERS`/`MAINTAINED_BY`) — which must absolutely **not** apply to the content groups
   co-resident beside it.

Today there's no way to express "this ontology is for group X" — so per-channel ontologies and a
catalog ontology can't coexist in a shared workspace.

Direct-assert (`knowledge_assert_entity` / `knowledge_assert_relationship`) already takes arbitrary
`labels` and doesn't require the ontology be declared, so this feature is specifically about
**extraction-guided groups + per-group canonicalization/validation**, not direct assertion.

This extends #83; the ontology file format is documented at
https://v3rv.com/liminis-context-graph/ontology

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Per-channel extraction ontology (Priority: P1)

A producer configures a `group_id`-specific ontology file so that extraction, canonicalization, and
strict validation for that group's content are guided by that group's own vocabulary, without
affecting any other co-resident group in the same workspace.

**Why this priority**: This is the core capability the issue exists to deliver — without it,
per-group ontologies simply cannot coexist in a shared workspace.

**Independent Test**: Configure a per-group ontology file for group A and a different one for group
B in the same workspace; extract content into each group; verify group A's extraction/validation
reflects only group A's ontology and group B's reflects only group B's, with no cross-group leakage.

**Acceptance Scenarios**:

1. **Given** a workspace with per-group ontology files for groups A and B, **When** content is
   extracted into group A, **Then** only group A's ontology (entity/relation types, `mode`) governs
   that extraction.
2. **Given** group A has `mode: strict` in its ontology and group B does not, **When** an entity
   type not declared in either ontology is extracted into group B, **Then** it is accepted (group B
   is not strict), and if the same undeclared type were extracted into group A, it would be rejected
   per group A's strict mode.

---

### User Story 2 - Fallback to workspace-wide ontology (Priority: P1)

A group with no per-group ontology file falls back to the existing workspace-wide `ontology.yaml`,
preserving today's behavior for workspaces that have not adopted per-group ontologies.

**Why this priority**: Backward compatibility is required — existing single-ontology workspaces must
continue to work unchanged.

**Independent Test**: In a workspace with only a workspace-wide `ontology.yaml` (no per-group
files), extract content into any group and confirm behavior is identical to pre-feature behavior.

**Acceptance Scenarios**:

1. **Given** a workspace with a workspace-wide `ontology.yaml` and no per-group ontology files,
   **When** content is extracted into any group, **Then** the workspace-wide ontology governs
   extraction, exactly as before this feature.
2. **Given** a workspace with per-group files for some groups but not others, **When** content is
   extracted into a group without its own file, **Then** the workspace-wide ontology governs that
   group's extraction.

---

### User Story 3 - Published ontology is documentation, not policy (Priority: P2)

A group's stream is published with the ontology that was used to extract it, so a consumer can see
what vocabulary produced the graph it received. The received ontology is provenance only — it must
never drive the consumer's own extraction, validation, canonicalization, or reprocessing for that
group.

**Why this priority**: Without this guarantee, hydrating a stream from an untrusted or independently
managed producer would let that producer's vocabulary reach into a workspace it does not own — the
same cross-group effect the rest of this feature exists to prevent, arriving through the hydrate
path instead of local configuration.

**Independent Test**: Publish a stream for group A with an ontology sidecar; hydrate it into a
different workspace that has its own (different or absent) ontology configuration for that group;
verify the consumer's extraction/validation/canonicalization behavior for that group is governed
solely by the consumer's own local configuration, and that the received ontology is visible only as
informational metadata.

**Acceptance Scenarios**:

1. **Given** a published stream that includes an ontology sidecar, **When** a consumer hydrates that
   stream, **Then** the consumer can inspect the producer's ontology as documentation, but it is not
   applied to the consumer's own extraction, `mode: strict` validation, canonicalization, or
   reprocessing.
2. **Given** a published stream whose ontology sidecar is missing, **When** a consumer hydrates that
   stream, **Then** replay succeeds normally and only the documentation available to the consumer is
   degraded — nothing about correctness or replay is affected.

---

### Edge Cases

- A `group_id` contains characters that are unsafe as a filesystem path component — the per-group
  ontology file path must not break or collide.
- A workspace has neither a per-group ontology file nor a workspace-wide `ontology.yaml` for a given
  group — extraction proceeds with no ontology, as it does today for ontology-less workspaces.
- `canonicalize_relations` and `backfill_relation_types` needed to be group-scoped before per-group
  ontology resolution could apply to them without risking one group's vocabulary being applied to
  another group's edges — #447 delivered that group-scoping (merged prior to this feature's
  implementation). `canonicalize_relations` now resolves and applies the target group's own
  ontology; `backfill_relation_types` has no ontology dependency to scope at all (see FR-006).
- A consumer already has its own operative ontology for a group and hydrates a stream published by a
  different producer for that same group — the received ontology must not overwrite or merge into
  the consumer's local configuration; adopting it requires a deliberate, separate action by the
  consumer operator.
- Replay of a hydrated stream requires no ontology at all (extraction already happened at the
  producer) — this remains unaffected by this feature.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST resolve an ontology per `group_id`, using a per-group ontology file
  when one exists for that group.
- **FR-002**: When no per-group ontology file exists for a `group_id`, the system MUST fall back to
  the workspace-wide `ontology.yaml`, preserving current single-ontology behavior.
- **FR-003**: Per-group ontology files MUST be stored at
  `{workspace}/.lcg/ontology/<group_id>.yaml`, one file per group.
- **FR-004**: A `group_id` used as a per-group ontology filename component MUST be encoded using the
  same bijective percent-encoding scheme already applied to WAL group directory names
  (`wal_group::encode_group_dir_name`), so that arbitrary `group_id` values remain safe, collision-free
  path components.
- **FR-005**: The per-group resolved ontology MUST govern, for that group only: extraction guidance,
  `mode` (including strict validation), canonicalization, and reprocessing (`reprocess_entity_types`,
  `reprocess_relation_types`).
- **FR-006**: `canonicalize_relations` and `backfill_relation_types` MUST select candidates and apply
  vocabulary only within the target group's own resolved ontology. This feature MUST NOT apply
  per-group ontology enforcement to those two operations in a way that leaves them mutating another
  group's edges under a different group's vocabulary. Delivered: `canonicalize_relations` resolves
  and applies the target group's own ontology (building on #447's group-scoping); `backfill_relation_types`
  derives pseudo relation types from edge fact text and has no ontology dependency at all, so there
  is no vocabulary-selection step for this FR to scope there.
- **FR-007**: When a group's stream is published, the ontology used to guide extraction for that
  group MUST be included in the stream's dot-namespace as an informational item, alongside the
  existing `.wal-generation.json` and `.wal-bounds.json` artifacts.
- **FR-008**: A consumer hydrating a published stream MUST treat any received ontology strictly as
  documentation/provenance. It MUST NOT be applied to the consumer's own extraction, `mode: strict`
  validation, canonicalization, or reprocessing for that group.
- **FR-009**: Absence of the published ontology informational item on a hydrated stream MUST NOT
  block replay or affect correctness — it only degrades the documentation available to the consumer.
- **FR-010**: Replay of a hydrated WAL stream MUST continue to require no ontology, unaffected by
  this feature.
- **FR-011**: Direct-assert operations (`knowledge_assert_entity`, `knowledge_assert_relationship`)
  MUST remain unaffected by per-group ontology resolution — they continue to accept arbitrary
  `labels` without requiring a declared ontology.

### Key Entities *(if applicable)*

- **Per-group ontology file**: `{workspace}/.lcg/ontology/<group_id>.yaml` — the operative ontology
  for one `group_id`, encoded per FR-004.
- **Workspace-wide ontology file**: `{workspace}/.lcg/ontology.yaml` — the existing fallback
  ontology used when a group has no per-group file.
- **Published ontology sidecar**: the informational, documentation-only copy of a group's ontology
  included in its published stream's dot-namespace (FR-007).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Two co-resident groups in one workspace (e.g., a content channel and the
  `orac-catalog` group) can each enforce `mode: strict` with distinct entity/relation vocabularies,
  with zero observed cross-group leakage in extraction or validation results.
- **SC-002**: A workspace with no per-group ontology files behaves identically to pre-feature
  behavior — verified by existing workspace-ontology tests continuing to pass unchanged.
- **SC-003**: A consumer hydrating a stream published with an ontology sidecar shows no change in
  its own extraction, validation, canonicalization, or reprocessing outcomes for that group,
  regardless of what the received ontology contains.
- **SC-004**: A stream published without an ontology sidecar still replays successfully with no
  correctness difference from one published with the sidecar.

## Assumptions

- **#447 was a prerequisite for FR-006, and has landed.** Before #447, `canonicalize_relations` and
  `backfill_relation_types` selected candidates database-wide with no `group_id` filter; per-group
  ontologies layered on top of that would have applied one group's vocabulary to another group's
  edges — strictly worse than prior behavior. #447 (group-scoping those two operations) merged
  ahead of this feature's implementation, so FR-006 is fully enforced for `canonicalize_relations`
  at delivery. The rest of this feature (FR-001 through FR-005, FR-007 through FR-011) never
  depended on #447.
- **Runtime hot-set of a group's ontology without restart (originally proposed Option 3) is
  deferred.** It is valuable for a long-lived multi-tenant worker adding channels on the fly, but is
  a substantially larger change (today's ontology is `Option<Arc<Ontology>>`, read once at startup
  and never mutated; hot replacement introduces concurrency, drift recomputation, and
  in-flight-extraction questions). It is out of scope for this issue and may be filed as a follow-up.
- **In-file multi-group scoping (originally proposed Option 2) was considered and rejected** in
  favor of per-group files, because the ontology drift sidecar (`.lcg/ontology-hash.json`) is
  workspace-scoped and single-valued, and one file carrying several groups' vocabularies would make
  a future per-group drift design harder to retrofit than separate per-group files would. Note that
  this feature, as delivered, keeps drift detection workspace-scoped for all groups regardless of
  ontology shape — per-group files make a future per-group drift design *possible*, but do not by
  themselves produce per-group drift hashes today (see ADR-0446, Decision 3).

## Out of Scope

- Runtime/hot per-group ontology replacement without a restart (originally proposed Option 3).
- In-file multi-group ontology scoping (originally proposed Option 2).
- Group-scoping `canonicalize_relations` and `backfill_relation_types` themselves — that work was
  tracked and delivered separately in #447 (merged ahead of this feature); this issue built FR-006
  enforcement on top of it but did not implement the group-scoping itself.
- Automatic adoption of a producer's published ontology into a consumer's own operative
  configuration — a consumer that wants to use a producer's vocabulary must copy it into its own
  configuration deliberately.

## Source References

- #83 — workspace-scoped ontology (the feature this issue extends)
- #447 — prerequisite: group-scope `canonicalize_relations` and `backfill_relation_types`
- https://v3rv.com/liminis-context-graph/ontology — ontology file format documentation
- `docs/operations.md` — publish contract for the stream dot-namespace (load-bearing / cache /
  local-only buckets; this feature adds an informational bucket per FR-007–FR-009)
