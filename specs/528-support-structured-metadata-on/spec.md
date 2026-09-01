# Feature Specification: Structured Attributes on Episodes

**Feature Branch**: `fabrik/issue-528`
**Created**: 2026-09-01
**Status**: Draft
**Input**: User description: "Support structured metadata on episodes: add Episodic.attributes and a knowledge_process_chunk parameter"

## Background

Internal work issue for community report #525 (filed from the orac project, GES/orac#70 / PR #73).

A document's **structured metadata** and the **facts extracted from its prose** cannot be co-located on, or joined to, one node. `knowledge_process_chunk` anchors extracted entities on the episode; `knowledge_assert_entity` is the only call taking an `attributes` map and it creates a *separate* Entity node. Today the two halves are unified only by a shared identity string (orac reuses one `base_name` for both `assert_entity`'s `name` and `process_chunk`'s `source_file`) — a naming convention, not a graph relationship.

**Verified against current `main`**, all three of the report's claims hold:

- `knowledge_assert_relationship` resolves both endpoints as entities within `group_id` — Entity→Entity only, no episode endpoint.
- `knowledge_process_chunk` has no `attributes`/`metadata` parameter.
- `knowledge_assert_entity`'s attributes land on `Entity.attributes` (`schema.rs:37`), disjoint from the episode.

And `Episodic` has no `attributes` column at all, so this needs a schema change.

**Why attributes-on-the-episode is the right shape.** The graph already defines:

```
CREATE REL TABLE MENTIONS (FROM Episodic TO Entity, ...)
```

`MENTIONS` runs Episodic → Entity, so the traversal half of the acceptance criterion exists today: every entity extracted from a chunk's prose is already one hop from that chunk's episode. Putting structured attributes on the episode node itself (zero hops) and leaving extracted facts reachable via `episode -[:MENTIONS]-> Entity` (one hop) satisfies the requirement without inventing a new join, a new edge type, or a new identity concept. Two alternative shapes were considered and rejected — see Out of Scope.

**Why this rides the 0.14.0 release.** 0.14.0 already forces a one-way storage migration (an lbug upgrade bumping storage version, see `CHANGELOG.md`'s Unreleased section) plus the `Entity.lookup_key` backfill and ART index build. An additive column costs far less riding that same migration event than forcing users through a second one later.

Closes the engine-side half of #525.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Ingest a chunk with structured metadata, retrieve both halves from one node (Priority: P1)

An integrator (e.g., orac) ingests a document chunk via `knowledge_process_chunk` and wants to attach structured metadata (e.g., originating system, ingestion batch, custom tags) directly to the resulting episode node — so that, later, both the structured metadata and the facts extracted from the chunk's prose are reachable from that one episode node, without relying on a shared naming convention across two disjoint nodes.

**Why this priority**: This is the entire content of the issue — the schema change and read-path changes exist only to serve this scenario.

**Independent Test**: Call `knowledge_process_chunk` with an `attributes` object, then read back the resulting episode (e.g., via `knowledge_get_episodes`) and confirm both the attributes and the `MENTIONS`-linked entities are retrievable from it.

**Acceptance Scenarios**:

1. **Given** a document chunk and a JSON object of structured attributes, **When** it is ingested via `knowledge_process_chunk` with `attributes` supplied, **Then** the resulting episode node stores those attributes and, when queried, returns both the attributes and (via one `MENTIONS` hop) the entities extracted from the chunk's body.
2. **Given** a `knowledge_process_chunk` call that omits `attributes`, **When** processed, **Then** behavior is identical to before this feature: entity extraction, episode creation, and response shape are unchanged, aside from the new column's empty-default value being stored.
3. **Given** a pre-existing workspace created before this change, **When** the service opens it, **Then** the `Episodic.attributes` column is added automatically (non-fatal on failure) with no manual operator step, and existing episodes continue to serve reads with the new field present (empty) rather than causing errors.

---

### Edge Cases

- **Non-object `attributes`** (a string, number, array, or `null`): treated the same as an absent parameter, per the existing convention `knowledge_assert_entity`/`knowledge_assert_relationship` already use for their own `attributes` parameter (non-object input defaults to an empty object). This spec assumes episodes follow the same convention.
- **Omitted vs. empty-object `attributes`**: under that same existing convention, both cases store the same empty-object value — the two are indistinguishable on read. This spec assumes episodes follow suit rather than introducing a new "attributes was never set" sentinel.
- **Compaction / WAL dump-replay**: an episode's `attributes` must survive being dumped and replayed during WAL compaction, the same as every other `Episodic` column — this is a correctness requirement (no silent data loss), not new behavior to design.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST add an `attributes` column to the `Episodic` node table via an additive migration (probe-then-`ALTER TABLE`, non-fatal on failure), following the pattern established for `Entity.summary_embedding` (issue #470) and `Entity.lookup_key` (issue #221).
- **FR-002**: A pre-existing database MUST open successfully after upgrade, automatically gain the `Episodic.attributes` column, and continue serving reads, with no manual operator step required.
- **FR-003**: `knowledge_process_chunk` MUST accept an optional `attributes` parameter (a JSON object), written through to the resulting episode's `attributes` column.
- **FR-004**: When `attributes` is omitted from a `knowledge_process_chunk` call, behavior MUST be identical to the feature's absence in every other respect — no other request or response field changes, and entity extraction is unaffected.
- **FR-005**: A non-object value supplied for `attributes` (string, number, array, `null`) MUST be treated the same as an absent parameter, consistent with the existing convention used by `knowledge_assert_entity` and `knowledge_assert_relationship`.
- **FR-006**: `knowledge_get_episodes` MUST return each episode's `attributes`.
- **FR-007**: Reading an episode (e.g., via `knowledge_get_episodes`) MUST continue to allow reaching the entities extracted from that episode via the existing `MENTIONS` edge, in the same call pattern available today — this feature must not regress that traversal.
- **FR-008**: The WAL dump/replay (compaction) path MUST preserve `attributes` — an episode dumped and replayed retains the same attributes it had before compaction.
- **FR-009**: The schema-parity divergence between `Episodic` (with `attributes`) and graphiti's `kuzu_driver.py` (without it) MUST be recorded in a numbered ADR, following the pattern of ADR-0470 and ADR-0221.

### Key Entities *(if the feature involves data)*

- **Episodic.attributes**: A JSON object, serialized as a string (matching the existing `Entity.attributes` and `RelatesToNode_.attributes` columns), holding caller-supplied structured metadata for a document/chunk — independent of, and co-located with, the facts extracted from that chunk's prose text (which remain reachable via the existing `MENTIONS` edge to `Entity`).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A document ingested via `knowledge_process_chunk` with `attributes` supplied, then queried from the episode node, reaches both its structured attributes and the entities extracted from its body in a single traversal (one `MENTIONS` hop).
- **SC-002**: `knowledge_process_chunk` calls omitting `attributes` behave exactly as before this feature shipped, aside from the new column's empty-default value being stored.
- **SC-003**: 100% of pre-existing workspaces open, migrate, and serve episode reads with no manual operator step.
- **SC-004**: The schema-parity divergence is recorded in a numbered ADR, following the ADR-0470 / ADR-0221 pattern.

## Assumptions

- `attributes` follows the exact convention already established by `knowledge_assert_entity`/`knowledge_assert_relationship`: a JSON object serialized to a string column, defaulting to an empty object when absent or non-object.
- Episode creation semantics (one new `Episodic` row per `knowledge_process_chunk` call) are unchanged; this feature does not introduce upsert/dedup behavior keyed on `attributes`.
- This column rides the storage migration already forced by the in-flight lbug upgrade recorded in `CHANGELOG.md`'s Unreleased section — the exact lbug version numbers cited in the originating community report predate a later bump recorded there; the underlying rationale (one migration event, not two) holds regardless of the exact intermediate version.

## Out of Scope

- Generalizing `knowledge_assert_relationship` to accept an episode endpoint (an alternative shape considered and rejected — it would invent a join that `MENTIONS` already provides).
- Binding an episode to an already-asserted `Entity` (another alternative shape considered and rejected, for the same reason).
- Any change to orac's current shared-key workaround, which stays correct until this ships and can be swapped independently — no coordinated release needed.
- Surfacing episode `attributes` through entity-provenance read paths (e.g., the `source_descriptions` returned alongside entities/relationships) — those paths return only a `source_description` string today and are not being widened to full episode objects by this issue.

## Source References

- Community report: #525 (GES/orac#70, orac PR #73)
- ADR-0470 (`docs/adr/0470-entity-summary-embedding.md`) — precedent for a schema-parity-divergence ADR
- ADR-0221 (`docs/adr/0221-secondary-art-index-for-entity-name-lookup.md`) — precedent for additive migration + ADR
- `crates/core/src/schema.rs` — `Entity.attributes` (existing precedent column) and the `Episodic` table DDL (currently lacking `attributes`)
- `crates/core/src/schema.rs`'s `migrate()` — the probe-then-`ALTER` pattern to follow
