# Feature Specification: Structured Attributes on Episodes

**Feature Branch**: `fabrik/issue-528`
**Created**: 2026-09-01
**Status**: Specified
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

**Both episode-creation entry points are in scope, not just `knowledge_process_chunk`.** `knowledge_add_episode` shares the same underlying `add_episode()` code path as `knowledge_process_chunk`. Supporting `attributes` on only one of them would create the same kind of per-call asymmetry that produced issue #410 (`episode_uuids` populated on some read paths and silently empty on others) and issue #524 (`facts` vs. `edges`). Two calls that create the same node type accept the same metadata.

**Read-path exposure is the larger half of this issue, not an afterthought.** Structured metadata that can be written but not retrieved at the point of search is write-only storage — it does not deliver the originating community report's motivation, which is a single queryable node: *this document, with its structured attributes, from which the facts it states are reachable*. If a passage search returns hits whose attributes the caller then has to re-fetch episode-by-episode, the feature has not delivered its purpose. Every read path that returns episodes or passages is therefore decided explicitly (see Requirements), and any path that deliberately does not carry `attributes` states that in its own tool description rather than leaving callers to infer behavior from an absent field — the same discipline issue #410's tool-description registry test already enforces for `episode_uuids`.

**Why this rides the 0.14.0 release.** 0.14.0 already forces a one-way storage migration plus the `Entity.lookup_key` backfill and ART index build. An additive column costs far less riding that same migration event than forcing users through a second one later. This issue was blocked on issue #529, which has since merged: `lbug` is now pinned at `=0.20.1` (storage version 47) on `main`. The migration must be authored and tested against that pin, not against the earlier `0.19.1` figure cited in the originating community report.

Closes the engine-side half of #525.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Ingest a chunk with structured metadata, retrieve both halves from one node (Priority: P1)

An integrator (e.g., orac) ingests a document chunk via `knowledge_process_chunk` (or `knowledge_add_episode`) and wants to attach structured metadata (e.g., originating system, ingestion batch, custom tags) directly to the resulting episode node — so that, later, both the structured metadata and the facts extracted from the chunk's prose are reachable from that one episode node, without relying on a shared naming convention across two disjoint nodes.

**Why this priority**: This is the entire content of the issue — the schema change and read-path changes exist only to serve this scenario.

**Independent Test**: Call `knowledge_process_chunk` with an `attributes` object, then read back the resulting episode (e.g., via `knowledge_get_episodes` or `knowledge_search_passages`) and confirm both the attributes and the `MENTIONS`-linked entities are retrievable from it.

**Acceptance Scenarios**:

1. **Given** a document chunk and a JSON object of structured attributes, **When** it is ingested via `knowledge_process_chunk` with `attributes` supplied, **Then** the resulting episode node stores those attributes and, when queried, returns both the attributes and (via one `MENTIONS` hop) the entities extracted from the chunk's body.
2. **Given** a `knowledge_process_chunk` or `knowledge_add_episode` call that omits `attributes`, **When** processed, **Then** behavior is identical to before this feature: entity extraction, episode creation, and response shape are unchanged, aside from the new column's empty-default value being stored.
3. **Given** a pre-existing workspace created before this change, **When** the service opens it, **Then** the `Episodic.attributes` column is added automatically (non-fatal on failure) with no manual operator step, and existing episodes continue to serve reads with the new field present (empty) rather than causing errors.
4. **Given** an episode created via `knowledge_add_episode` with `attributes` supplied, **When** read back, **Then** it behaves identically to one created via `knowledge_process_chunk` with the same `attributes` — both entry points expose the field on the same terms.
5. **Given** an episode with `attributes` supplied, **When** a passage from that episode is returned by `knowledge_search_passages`, **Then** the result carries that episode's `attributes` directly, without requiring a separate per-episode fetch.

---

### Edge Cases

- **Non-object `attributes`** (a string, number, array, or `null`): treated the same as an absent parameter, per the existing convention `knowledge_assert_entity`/`knowledge_assert_relationship` already use for their own `attributes` parameter (non-object input defaults to an empty object). This spec assumes episodes follow the same convention.
- **Omitted vs. empty-object `attributes`**: under that same existing convention, both cases store the same empty-object value — the two are indistinguishable on read. This spec assumes episodes follow suit rather than introducing a new "attributes was never set" sentinel.
- **Compaction / WAL dump-replay**: an episode's `attributes` must survive being dumped and replayed during WAL compaction, the same as every other `Episodic` column — this is a correctness requirement (no silent data loss), not new behavior to design.
- **Read paths that surface only partial episode data** (e.g., entity/relationship provenance lookups that return a `source_description` string, not a full episode object): these deliberately do not gain `attributes` by this issue, but must say so in their own tool description rather than leaving the omission implicit.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST add an `attributes` column to the `Episodic` node table via an additive migration (probe-then-`ALTER TABLE`, non-fatal on failure), following the pattern established for `Entity.summary_embedding` (issue #470) and `Entity.lookup_key` (issue #221).
- **FR-002**: A pre-existing database MUST open successfully after upgrade, automatically gain the `Episodic.attributes` column, and continue serving reads, with no manual operator step required.
- **FR-003**: Both `knowledge_process_chunk` and `knowledge_add_episode` MUST accept an optional `attributes` parameter (a JSON object), written through to the resulting episode's `attributes` column, since both share the same underlying episode-creation path.
- **FR-004**: When `attributes` is omitted from a `knowledge_process_chunk` or `knowledge_add_episode` call, behavior MUST be identical to the feature's absence in every other respect — no other request or response field changes, and entity extraction is unaffected.
- **FR-005**: A non-object value supplied for `attributes` (string, number, array, `null`) MUST be treated the same as an absent parameter, consistent with the existing convention used by `knowledge_assert_entity` and `knowledge_assert_relationship`.
- **FR-006**: `knowledge_get_episodes` MUST return each episode's `attributes`.
- **FR-007**: Reading an episode (e.g., via `knowledge_get_episodes`) MUST continue to allow reaching the entities extracted from that episode via the existing `MENTIONS` edge, in the same call pattern available today — this feature must not regress that traversal.
- **FR-008**: The WAL dump/replay (compaction) path MUST preserve `attributes` — an episode dumped and replayed retains the same attributes it had before compaction.
- **FR-009**: The schema-parity divergence between `Episodic` (with `attributes`) and graphiti's `kuzu_driver.py` (without it) MUST be recorded in a numbered ADR, following the pattern of ADR-0470 and ADR-0221.
- **FR-010**: `knowledge_search_passages` MUST return each result's `attributes`, sourced from the episode the passage belongs to — a caller must not need a separate per-episode fetch to obtain them.
- **FR-011**: Every MCP tool whose response is derived from episode data MUST state, in its own tool description, whether `attributes` is included in that response — mirroring the precedent issue #410 set for `episode_uuids`. A read path that deliberately does not carry `attributes` (e.g., entity/relationship provenance lookups that surface only `source_description` strings) MUST say so explicitly in its tool description rather than leaving the omission to be inferred from an absent field.

### Key Entities *(if the feature involves data)*

- **Episodic.attributes**: A JSON object, serialized as a string (matching the existing `Entity.attributes` and `RelatesToNode_.attributes` columns), holding caller-supplied structured metadata for a document/chunk — independent of, and co-located with, the facts extracted from that chunk's prose text (which remain reachable via the existing `MENTIONS` edge to `Entity`).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A document ingested via `knowledge_process_chunk` with `attributes` supplied, then queried from the episode node, reaches both its structured attributes and the entities extracted from its body in a single traversal (one `MENTIONS` hop).
- **SC-002**: `knowledge_process_chunk` and `knowledge_add_episode` calls omitting `attributes` behave exactly as before this feature shipped, aside from the new column's empty-default value being stored.
- **SC-003**: 100% of pre-existing workspaces open, migrate, and serve episode reads with no manual operator step.
- **SC-004**: The schema-parity divergence is recorded in a numbered ADR, following the ADR-0470 / ADR-0221 pattern.
- **SC-005**: A passage returned by `knowledge_search_passages` carries the `attributes` of the episode it belongs to, with no additional call required to obtain them.
- **SC-006**: Every episode-derived MCP tool description accurately states whether its response includes `attributes` — verified the same way issue #410's tool-description registry test verifies `episode_uuids` claims.

## Assumptions

- `attributes` follows the exact convention already established by `knowledge_assert_entity`/`knowledge_assert_relationship`: a JSON object serialized to a string column, defaulting to an empty object when absent or non-object.
- Episode creation semantics (one new `Episodic` row per `knowledge_process_chunk`/`knowledge_add_episode` call) are unchanged; this feature does not introduce upsert/dedup behavior keyed on `attributes`.
- This column rides the storage migration already forced by the lbug upgrade landed via issue #529: `lbug` is pinned at `=0.20.1` (storage version 47) on `main`. The migration is authored and tested against that pin.

## Out of Scope

- Generalizing `knowledge_assert_relationship` to accept an episode endpoint (an alternative shape considered and rejected — it would invent a join that `MENTIONS` already provides).
- Binding an episode to an already-asserted `Entity` (another alternative shape considered and rejected, for the same reason).
- Any change to orac's current shared-key workaround, which stays correct until this ships and can be swapped independently — no coordinated release needed.
- Surfacing episode `attributes` through entity-provenance read paths (e.g., the `source_descriptions` returned alongside entities/relationships) — those paths return only a `source_description` string today and are not being widened to full episode objects by this issue (per FR-011, they must document this omission rather than leave it implicit).

## Source References

- Community report: #525 (GES/orac#70, orac PR #73)
- Precedent for read-path-per-tool documentation discipline: #410 (`episode_uuids`), #524 (`facts` vs. `edges`)
- ADR-0470 (`docs/adr/0470-entity-summary-embedding.md`) — precedent for a schema-parity-divergence ADR
- ADR-0221 (`docs/adr/0221-secondary-art-index-for-entity-name-lookup.md`) — precedent for additive migration + ADR
- `crates/core/src/schema.rs` — `Entity.attributes` (existing precedent column) and the `Episodic` table DDL (currently lacking `attributes`)
- `crates/core/src/schema.rs`'s `migrate()` — the probe-then-`ALTER` pattern to follow
- Issue #529 (merged) — the lbug `=0.20.1` / storage version 47 pin this migration rides
