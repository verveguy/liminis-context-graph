# Feature Specification: Record ingest time separately from reference_time

**Feature Branch**: `fabrik/issue-673`
**Created**: 2026-10-03
**Status**: Specified
**Input**: User description: "Record ingest time separately from reference_time — add an ingest timestamp to episodes, entities and edges, set from the service clock, so event time (`reference_time`) and knowledge time are both kept."

## Background

`reference_time` is the caller-supplied *event* time: when the content happened. Today the service writes it into **both** `created_at` and `valid_at` for episodes, and into `created_at` for entities and edges (`crates/core/src/episode.rs` around lines 989, 1387 and 1416–1421 at v0.16.4). When a caller supplies a real event time, the graph keeps no record of *when it learned* the content.

Liminis is about to do exactly that:

- The indexing queue will send per-chunk times taken from document content, not file mtime.
- Conversation consolidations will ingest months-old discussions dated to when they happened.

After that, three things become impossible: answering "what did the graph learn this week?", debugging ingest order, and applying any retention or cleanup policy based on *knowledge age* rather than *event age*. A backlog ingested today is old in the world but new to the graph, and both facts matter.

The fix is two separately stored clocks:

- **Event time** is `reference_time`, defaulting to now. It is what `valid_at` and existing time-ordered reads use today.
- **Ingest time** is when this service wrote the record. It is always the service clock and is never caller-supplied.

Requested by Liminis's memory work (verveguy/liminis PR #1173: `ideas/conversation-consolidation.md` → "Time", and `ideas/memory-ontology-layers.md`). The broader temporal work — time-aware ranking, supersession of contradicted facts, decay — is a separate track and out of scope.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - See when the graph learned an episode (Priority: P1)

A Liminis operator or the indexing queue ingests a chunk whose `reference_time` is months in the past. Reading the episode back shows both when the content happened and when the graph ingested it.

**Why this priority**: This is the core need; every other story builds on the ingest time being stored and returned.

**Independent Test**: Process a chunk with `reference_time` = 2026-08-04 on 2026-10-03, then read the episode back.

**Acceptance Scenarios**:

1. **Given** a chunk with `reference_time` 2026-08-04 processed on 2026-10-03, **When** the episode is read via `knowledge_get_episodes`, **Then** event time (`valid_at`) is 2026-08-04 and the ingest time is 2026-10-03.
2. **Given** a chunk processed with no `reference_time`, **When** the episode is read, **Then** event time defaults to now as today and the ingest time is the service's write time.
3. **Given** a caller attempts to pass an ingest time in a request, **When** the record is written, **Then** the stored ingest time is still the service clock.

---

### User Story 2 - Ingest time on entities and edges, including direct assertions (Priority: P1)

Entities and relationships extracted from an episode, and those written directly via `knowledge_assert_entity` / `knowledge_assert_relationship`, each carry the time the service wrote them.

**Why this priority**: "What did the graph learn this week?" and knowledge-age retention are asked about facts and entities, not just episodes.

**Independent Test**: Process a backdated chunk that yields entities and edges; assert an entity and a relationship directly; read each record back.

**Acceptance Scenarios**:

1. **Given** a backdated chunk that yields entities and edges, **When** they are read through the entity and relationship reads, **Then** each reports an ingest time equal to the write time, while `created_at` / `valid_at` are unchanged from today's values.
2. **Given** a direct `knowledge_assert_entity` or `knowledge_assert_relationship` call (with or without `valid_at`), **When** the result is read, **Then** the ingest time is the service clock at the call.

---

### User Story 3 - Rebuild preserves ingest time (Priority: P1)

An operator rebuilds the graph from the WAL. Every record keeps its original ingest time rather than being restamped with the rebuild time.

**Why this priority**: A rebuild that restamps everything would silently destroy the signal this feature exists to provide.

**Independent Test**: Ingest records, capture ingest times, run `knowledge_rebuild_from_wal`, compare.

**Acceptance Scenarios**:

1. **Given** a graph with ingested records, **When** `knowledge_rebuild_from_wal` runs, **Then** every episode, entity and edge has the same ingest time as before.
2. **Given** WAL-tail recovery (`knowledge_recover`) or crash-recovery replay, **When** records are re-applied, **Then** their original ingest times are retained.

---

### User Story 4 - Existing graphs migrate (Priority: P2)

An operator upgrades a service whose graph predates this feature. Existing records receive a backfilled ingest time, and the docs state where it came from.

**Why this priority**: Without it, pre-existing records read as having no ingest time. It is lower priority only because new data is correct without it.

**Independent Test**: Open a graph created before this feature and read back records.

**Acceptance Scenarios**:

1. **Given** a graph written before this feature, **When** the upgraded service opens it, **Then** every episode, entity and edge has an ingest time, derived from the best available signal.
2. **Given** a backfilled record, **When** its provenance is looked up in the docs, **Then** the docs state the signal used and the precedence between signals.

---

### User Story 5 - Reads and projection expose ingest time (Priority: P2)

Clients can see and select the ingest time on every read surface that returns episodes, entities or relationships.

**Why this priority**: Stored but unreadable data has no value; the existing `fields` projection must keep working.

**Independent Test**: Call each read surface with and without `fields`.

**Acceptance Scenarios**:

1. **Given** any read that returns episodes, entities or relationships, **When** called without `fields`, **Then** each item includes the ingest time.
2. **Given** `knowledge_get_episodes` or `knowledge_list_entities` with `fields` containing the ingest-time key, **When** called, **Then** it is returned; **and when** `fields` omits it, it is not returned.

---

### Edge Cases

- **Re-observed records**: an entity or edge that already exists and is matched or updated by a later episode keeps its original ingest time (the time the service first wrote it).
- **Merged entities**: after `knowledge_merge_entities`, the surviving entity keeps its own ingest time; it is not restamped.
- **Other writers**: `knowledge_add_cross_group_edge`, `knowledge_apply_corrections` and the reprocess/canonicalize/backfill operations that create or rewrite records stamp ingest time on newly created records only; rewriting an existing record in place does not change it.
- **Caller-supplied value**: any ingest time in request parameters is ignored; the service clock is always used.
- **No `reference_time` supplied**: ingest time and event time are close but are still stored as separate values.
- **Clock format**: ingest time uses the same timestamp format and timezone convention as `created_at`.
- **Legacy WAL entries** that lack an ingest time replay without failure and receive the backfill rule from FR-005.
- **Index/dump/checkpoint paths**: dump compaction and checkpoint restore carry ingest time through unchanged.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The service MUST store an ingest timestamp (named `ingested_at`) on episodes, entities and edges, set from the service clock at write time. This includes records written by direct assertions (`knowledge_assert_entity` / `knowledge_assert_relationship`).
- **FR-002**: Ingest time MUST never be caller-supplied; a value passed in a request MUST NOT influence the stored value.
- **FR-003**: Existing semantics MUST be unchanged. `created_at` / `valid_at` behave exactly as today for every existing caller and read path, and nothing that sorts or filters on them (including episode ordering and paging cursors) changes behaviour.
- **FR-004**: Ingest time MUST be returned on the read surfaces that return these records: `knowledge_get_episodes`; entity reads (`knowledge_find_entities`, `knowledge_resolve_entity`, `knowledge_list_entities`, `knowledge_get_nodes_by_group`, `knowledge_get_entities_by_source`, `knowledge_get_entity_neighbors`); and relationship reads (`knowledge_find_relationships`, `knowledge_list_relationships`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`). The addition MUST be additive to existing response shapes.
- **FR-005**: The existing `fields` projection on `knowledge_get_episodes` and `knowledge_list_entities` MUST accept the ingest-time key and honour it like any other field (included only when requested; absent when `fields` omits it).
- **FR-006**: WAL replay MUST preserve the original ingest time of every record. `knowledge_rebuild_from_wal`, WAL-tail recovery, startup recovery and checkpoint/dump restore MUST NOT restamp records with the replay time.
- **FR-007**: On upgrade, existing records that lack an ingest time MUST be backfilled from the best available signal, in this order: the timestamp of the WAL entry that created the record; else `created_at`. Records written before this feature are therefore *approximate* — `created_at` was the event time for them. The backfill MUST be idempotent and MUST NOT run again once a record has an ingest time.
- **FR-008**: A record's ingest time MUST be set once, when the service first writes it, and not changed by later matches, updates or merges of that record (see Edge Cases).
- **FR-009**: The docs MUST define the two clocks in the API docs (`docs/ipc-mcp-reference.md`): `reference_time` (event time: what it feeds, its default) and ingest time (service clock, never caller-supplied, first-write semantics). They MUST also state which signal the migration backfill uses and the precedence between signals.
- **FR-010**: The new field MUST be added without breaking the schema parity requirement with graphiti's Kuzu driver, and without breaking the IPC protocol consumed by the Liminis Electron app (additive keys only).

### Key Entities

- **Episode**: a unit of ingested content; carries event time (`valid_at`, plus today's `created_at`) and now ingest time.
- **Entity**: a node extracted or asserted; carries `created_at` (today) and now ingest time.
- **Edge / Relationship**: a fact between entities; carries `created_at`, `valid_at`, `invalid_at` (today) and now ingest time.
- **WAL entry**: carries its own append timestamp (`ts`); also the source of truth for replay, so ingest time must survive it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Process a chunk with `reference_time` = 2026-08-04 on 2026-10-03. The episode reports event time 2026-08-04 and ingest time 2026-10-03.
- **SC-002**: A `knowledge_rebuild_from_wal` replay leaves every ingest time unchanged (episodes, entities and edges compared before and after).
- **SC-003**: No existing test that asserts on `created_at` / `valid_at` changes.
- **SC-004**: After upgrading a graph that predates this feature, 100% of episodes, entities and edges have a non-empty ingest time.
- **SC-005**: Direct assertions report an ingest time equal to the service clock at call time, independent of any `valid_at` supplied.

## Assumptions

- The key is named `ingested_at` (the issue's suggested name), formatted like `created_at`.
- Ingest time means *first write by this service*. "When the record was last modified" is a different clock and is not added.
- "Best available signal" for the backfill is WAL entry timestamp first, `created_at` second. Where the WAL has been compacted or dumped past a record, `created_at` is used; the docs flag this as approximate.
- Only episodes, entities and edges get the field. Other node/rel tables (passages, communities, MENTIONS, etc.) are untouched.
- Read surfaces that already return all columns of these records (including the Cypher escape hatch) expose the new column without special handling.

## Out of Scope

- Time-aware ranking, supersession of contradicted facts, and decay (separate track).
- Retention/cleanup policies that consume ingest time; this issue only records it.
- Filtering or sorting reads by ingest time (new query parameters).
- Changing what `created_at` / `valid_at` mean or which value they hold.
- A "last updated" clock.

## Source References

- `crates/core/src/episode.rs` (~lines 989, 1387, 1416–1421): where `reference_time` is written into `created_at` / `valid_at`.
- `crates/core/src/schema.rs`, `crates/core/src/wal.rs`, `crates/core/src/read_page.rs`: schema, WAL entry `ts`, read projection field lists.
- `docs/ipc-mcp-reference.md`: API docs ("Bulk reads", "Direct assertion").
- ADR-0379 (direct assertion conventions); ADR-0667 (read-path paging and projection).
- verveguy/liminis PR #1173.
