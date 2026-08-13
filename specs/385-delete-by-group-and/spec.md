# Feature Specification: Attribute delete_by_group and rebind_pointers WAL mutations to the groups they actually modify

**Feature Branch**: `fabrik/issue-385`
**Created**: 2026-08-13
**Status**: Specified
**Input**: User description: "`knowledge_delete_by_group` and `knowledge_rebind_pointers` write their mutations to the default group's WAL stream rather than to the streams of the groups they actually mutate. This breaks the invariant #378 exists to establish — that every mutation in group G's WAL belongs to G, and that a group's stream is independently replayable."

## Background

[#378](../378-multi-stream-wal-one/spec.md) established that a `liminis-context-graph` instance keeps one WAL directory **per `group_id`**, each independently replayable, on the premise that every write handler names exactly one group at its flush site (`ADR-0378` FR-004). That premise held for ordinary writers, but two handlers are multi-group **by design** and were routed around it rather than through it:

- **`knowledge_delete_by_group`** ([#361](../../docs/adr/0361-group-scoped-purge.md)) deletes a purged group's own data *and*, in the same transaction, force-rebinds pointers on a different, non-purged "owning" group's `RelatesToNode_` rows.
- **`knowledge_rebind_pointers`** ([#369](../../docs/adr/0369-resolvable-cross-group-pointers.md)) is invoked *for* a source group being re-resolved, but the mutations it produces land on the *owning* groups' edges — a different group (or groups) than the one named in the call.

Both call sites currently flush their `drain_mutations()` output through `wal_exec::wal_flush_ungrouped(&state, DEFAULT_GROUP_ID, …)`, with an inline comment citing `ADR-0378`'s FR-004 ("per-operation attribution can't name one group here"). That citation is accurate as far as it goes — FR-004 explicitly declined to design general mutation-level attribution — but it was never meant to license routing *every* multi-group operation's mutations to the default group forever; it names four call sites as an accepted, documented limitation. This issue narrows two of those four back to correct behavior, because unlike the other two (see Out of Scope), these two mutate specific, identifiable groups that are not the default group, and a stream that doesn't contain its own writes stops being independently replayable — the entire point of #378.

### Reproduced against `main` @ `d5a3e14`

Three groups (`A`, `B`, `C`) were written purely through the assertion API, with `C` acting as a layer graph holding five cross-group edges into `A` and `B`. Then `knowledge_delete_by_group(["A"])` followed by `knowledge_rebind_pointers(source_group_id: "A")` were called.

Afterwards `<wal_root>/liminis/` exists — despite nothing ever having been written to the `liminis` group — and contains 18 mutations belonging to other groups:

```
seq 0  MATCH (rn:RelatesToNode_) WHERE rn.group_id IN $gids DETACH DELETE rn
seq 1  MATCH (e:Entity)          WHERE e.group_id  IN $gids DETACH DELETE e
seq 2  MATCH (ep:Episodic)       WHERE ep.group_id IN $gids DETACH DELETE ep
seq 3  MATCH (rn:RelatesToNode_ {uuid: $uuid}) SET rn.attributes = $attributes
...    (15 more: C's pointer re-binding — attribute writes and hop MERGE/DELETE)
```

Sequences 0–2 are group **A**'s purge. Sequences 3–17 are group **C**'s pointer re-binding. Neither belongs to `liminis`. Meanwhile `A/` and `C/` are unchanged by either operation — the mutations that should describe their state live somewhere else entirely.

### Why this matters

- **`A/` no longer describes A's state.** A's own stream contains its creation mutations but not its deletion. Replaying `A/` in isolation — the per-stream refresh #361 and #378 were built for — resurrects entities that were purged.
- **`C/` no longer describes C's bindings.** The layer group's `binding_state` transitions live in another group's stream. Replaying `C/` alone reproduces stale bindings; the corrections are absent.
- **`liminis/` becomes a shared dumping ground** carrying other groups' mutations — precisely the single-shared-directory condition #378 was filed to eliminate, reintroduced through a side door.
- **A group can be mutated by a stream it does not own.** Replaying `liminis/` executes `WHERE group_id IN $gids DETACH DELETE` against A, and attribute writes against C. Any consumer hydrating `liminis` inherits deletions belonging to groups it may not even mount.

For the downstream mesh topology (orac/zen), per-stream replay is *the* operation. A stream that does not contain its own deletions and re-bindings is not self-contained, and the guarantee the multi-stream model advertises does not hold.

### Suggested direction (non-binding — Research/Plan owns the actual approach)

The mutations are separable: each one already carries, or can be attributed to, the group whose data it touches. A purge of `A` that re-binds `C`'s pointers should produce two flushes — the `DETACH DELETE`s to `A/`, the pointer attribute writes and hop repairs to `C/`. That is narrower than the general mutation-level attribution FR-004 rejected: it is needed only in the two handlers that knowingly span groups, not threaded through `Conn::executed_mutations` → `drain_mutations` → `wal_flush_*` for every writer. One natural shape is a scoped drain-and-flush per affected group at the points where the group is already known — `group_purge` already iterates `group_ids`, and already calls a per-group forced-rebind pass. This is a hint for downstream stages, not a requirement of this spec.

Whatever shape is chosen, the invariant to restore is: **a mutation is written to the WAL stream of the group whose data it changes, and to no other.**

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Purging a group with a forced cross-group rebind routes each mutation to its own group's stream (Priority: P1)

An operator purges group `A` via `knowledge_delete_by_group(["A"])`. `A`'s own entities, episodes, and edges are deleted. In the same call, pointers on group `C`'s edges that referenced `A` are force-rebound (per #361). Today, all of this — A's deletions and C's rebind writes — lands in a `liminis/` directory that neither group owns. After this fix, A's deletions land in `A/` and C's rebind writes land in `C/`; no `liminis/` directory is created.

**Why this priority**: This is the reproduction that motivated the issue, and the case where a single call already spans two known groups in one transaction — the sharpest test of the fix.

**Independent Test**: Reproduce the harness scenario (three groups A/B/C, C holding cross-group edges into A and B, `knowledge_delete_by_group(["A"])`) and inspect the resulting WAL directory tree directly.

**Acceptance Scenarios**:

1. **Given** groups A, B, C where C holds cross-group edges into A, **When** `knowledge_delete_by_group(["A"], confirm: true)` is called, **Then** `A/`'s stream contains exactly the `DETACH DELETE` mutations for A's purge, `C/`'s stream contains the forced-rebind pointer mutations touching C's `RelatesToNode_` rows, and no directory for the default group (`liminis/`) exists on disk.
2. **Given** the same purge has completed, **When** `A/`'s stream alone is replayed against a fresh database, **Then** A's purged entities, episodes, and edges are absent — not resurrected — because the deletions are present in A's own stream.
3. **Given** the same purge, **When** `B/`'s stream is inspected, **Then** it is unchanged (B was neither purged nor rebound by this call).

---

### User Story 2 - A standalone rebind_pointers call routes to the owning group, not the source group or the default group (Priority: P1)

An operator (or an automated process, independent of any purge) calls `knowledge_rebind_pointers(source_group_id: "A")` directly to re-resolve pointers after `A`'s state has changed. The pointers being fixed live on edges owned by group `C`. Today, the resulting mutations land in the default group's stream. After this fix, they land in `C/` — the stream of the group whose edges were actually written.

**Why this priority**: `knowledge_rebind_pointers` is independently invokable, not only reachable as part of a purge's forced rebind, so it needs its own coverage distinct from User Story 1.

**Independent Test**: Call `knowledge_rebind_pointers` directly against a fixture with unbound pointers on edges owned by a known group, and inspect WAL directory contents afterward.

**Acceptance Scenarios**:

1. **Given** group C owns an edge with an unbound pointer whose `source_group_id` is "A", **When** `knowledge_rebind_pointers(source_group_id: "A")` is called, **Then** the resulting attribute-write and hop `MERGE`/`DELETE` mutations appear in `C/`'s stream.
2. **Given** the same call has completed, **When** `A/`'s stream is inspected, **Then** it is unchanged — `rebind_pointers` does not write to the source group's own stream merely because the source group was named in the call.
3. **Given** pointers owned by multiple distinct groups all resolving against the same `source_group_id`, **When** `knowledge_rebind_pointers` is called once, **Then** each owning group's mutations land only in that group's own stream, with no cross-contamination between owning groups and none reaching the default group's stream.

---

### Edge Cases

- `knowledge_delete_by_group(["A", "B"])` purges two groups in one call, where A's forced rebind touches C and B's forced rebind touches D: each group's deletions and each foreign owning group's rebind mutations land in their own respective streams, not merged into one "operation-wide" stream.
- A purge's forced rebind touches a group that is *also* in the same call's purge set (e.g., purging `["A", "B"]` where A owns edges pointing into B): mutations attributable to B land in B's own stream, not the default group's — even though B is itself a target of the same operation.
- `knowledge_rebind_pointers(source_group_id)` resolves to zero unbound pointers: no mutations are produced, so no group's WAL stream — including the default group's — gains a new directory or entry as a side effect of the call.
- `knowledge_delete_by_group` called with `dry_run: true`: no WAL mutations of any kind are flushed to any stream; existing dry-run behavior is unaffected by this fix.
- A group that has never been mutated by any operation continues to have no on-disk WAL directory after this fix lands. The default group is not special-cased — it only gains a directory when a mutation is legitimately attributed to it, same as any other group.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A mutation MUST be flushed to the WAL stream of the `group_id` whose data it modifies. No handler may route another group's mutations to the default group.
- **FR-002**: `knowledge_delete_by_group` MUST flush each purged group's deletions to that group's own stream, and any forced re-bind writes to the owning group's stream.
- **FR-003**: `knowledge_rebind_pointers` MUST flush each pointer's attribute and hop mutations to the stream of the group that **owns** the edge (the `RelatesToNode_`'s `group_id`), not the source group being re-resolved and not the default group.
- **FR-004**: A group never written to MUST NOT acquire a WAL directory as a side effect of another group's operation. In the reproduction, `liminis/` must not exist.
- **FR-005**: Replaying a single group's stream in isolation MUST reproduce that group's state including deletions — a purge followed by a replay of that group's own stream MUST NOT resurrect purged data.
- **FR-006**: `ADR-0378`'s FR-004 rationale MUST be corrected to record that per-operation attribution is insufficient for handlers that span groups by design, and to name `knowledge_delete_by_group` and `knowledge_rebind_pointers` as the two handlers this issue fixes. The other two FR-004-exempted call sites (`backfill.rs`/`canonicalize.rs`'s database-wide maintenance passes, and `handle_query_cypher`'s arbitrary-Cypher escape hatch) remain under the original documented-limitation rationale, since they are out of scope for this issue.

### Key Entities

- **WAL stream / group directory**: the per-group subdirectory under `<wal_root>` holding that group's JSONL mutation log, established by #378 (`<wal_root>/<group_id>/`).
- **Mutation**: a single Cypher write (`DETACH DELETE`, `SET`, `MERGE`) captured via `drain_mutations`, attributable to exactly one group's data.
- **Owning group**: for a `RelatesToNode_` edge, the `group_id` recorded on that edge row itself — distinct from the *source group* a pointer on that edge currently references.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After `delete_by_group(["A"])` on a database whose only groups are `A`, `B`, `C`, no `liminis/` directory exists and `A/` contains the deletion mutations.
- **SC-002**: After `rebind_pointers(source_group_id: "A")`, the pointer mutations appear in `C/` (the edge-owning group), and `A/` is byte-identical to its state before the call.
- **SC-003**: Purging group A and then replaying only `A/` yields a graph in which A's purged entities are absent.
- **SC-004**: No operation on one group causes any other group's WAL directory to receive mutations.
- **SC-005**: The existing multi-stream isolation properties are unchanged — cross-group edges still land in the owning group's stream, and per-group positions remain independent.

## Assumptions

- This fix's scope is limited to the two handlers explicitly named: `knowledge_delete_by_group` and `knowledge_rebind_pointers`. `ADR-0378`'s other two FR-004-exempted call sites are not touched by this issue and keep routing to the default group under the existing documented-limitation rationale (see Out of Scope).
- [#383](https://github.com/verveguy/liminis-context-graph/issues/383) (`applied_seq` never advances for `wal_flush_ungrouped`-routed writes) is a related but independent defect on the same flush path. This issue does not fix it, even where the two touch overlapping code — see the issue body's own note that "neither fixes the other."
- The exact mechanism for splitting a single `drain_mutations()` call's output per owning group (e.g., a scoped drain-and-flush per group at the points where the group is already known, as sketched in Background) is a design decision for Research/Plan, not fixed by this spec.
- The multi-stream isolation properties established by #378 (per-group `global_seq`, independent `WalPosition` rows, independent replay) are assumed correct and unchanged by this issue — this issue only corrects which stream two specific handlers' mutations are routed to.

## Out of Scope

- Fixing #383 (`applied_seq` tracking for `wal_flush_ungrouped`-routed writes).
- Changing routing behavior for `backfill.rs`, `canonicalize.rs`, or `handle_query_cypher` — the other three `ADR-0378` FR-004-exempted call sites — which keep routing to the default group as a documented limitation.
- Introducing general mutation-level attribution across every writer (`Conn::executed_mutations` → `drain_mutations` → `wal_exec::wal_flush_*`). FR-004's original rejection of that broader design stands; this issue only narrows the two call sites that need per-mutation-group splitting.

## Source References

- `crates/core/src/handlers.rs` — `handle_delete_by_group`, `handle_rebind_pointers`
- `crates/core/src/group_purge.rs` — `purge_groups`, the forced-rebind pass
- `crates/core/src/cross_group.rs` — `rebind_pointers`/`rebind_pointers_impl`
- `docs/adr/0378-multi-stream-wal-per-group-directory.md` — FR-004 rationale to be corrected (FR-006)
- `docs/adr/0361-group-scoped-purge.md` — introduces the forced-rebind pass this issue fixes the routing for
- `docs/adr/0369-resolvable-cross-group-pointers.md` — introduces `rebind_pointers`
- `specs/378-multi-stream-wal-one/spec.md` — the per-group WAL stream invariant this issue restores
- Related: [#383](https://github.com/verveguy/liminis-context-graph/issues/383) (`applied_seq` tracking, an independent defect on the same flush path)
