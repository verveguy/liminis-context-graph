# Feature Specification: `applied_seq` never advances for `wal_flush_ungrouped` writes

**Feature Branch**: `fabrik/issue-383`
**Created**: 2026-08-13
**Status**: Draft
**Input**: User description: "`applied_seq` never advances for writes made through `wal_exec::wal_flush_ungrouped`, which is every write path except episode ingest. With #379's assertion API and #378's per-group positions both landed, that gap now has consequences it did not have when it was knowingly deferred."

## Background

`episode.rs` advances `applied_seq` after `wal_exec::wal_flush_chunk` (the episode-ingest path). `wal_exec::wal_flush_ungrouped` — the helper used by every other write path — never has. Its doc comment still claims three callers ("delete handlers, corrections, and `handle_query_cypher`"), but its actual callers today are far broader: at least `handle_assert_entity`, `handle_assert_relationship`, `handle_add_cross_group_edge`, `handle_rebind_pointers`, `handle_merge_entities`, `handle_apply_corrections`, `handle_delete_by_group`, `handle_delete_by_source`, `handle_delete_episode`, `handle_delete_chunk_episode`, `handle_clear_all`, `handle_query_cypher`, plus `backfill.rs`, `canonicalize.rs`, and `reprocess_relations.rs`.

**Reproduced** against `main` at `27377a1` (pre-#378), driving a fresh instance over MCP stdio with no LLM in the loop — six entities and eight relationships written via `knowledge_assert_entity`, `knowledge_assert_relationship`, and `knowledge_add_cross_group_edge` across three groups:

```
35 WAL lines written
knowledge_status → "wal": { "file_count": 1, "applied_seq": 0, "max_seq": 34 }
```

The database is exactly current with its own WAL and reports itself 34 positions behind. Not lagging — never starting. (Harness: a standalone MCP-stdio driver with a stub embedder; no extraction, fully deterministic. Available on request, or reconstructible from `crates/service/tests/common/mod.rs`'s `spawn_stub_embedder` plus the `--mcp-stdio` launch pattern in `crates/service/tests/mcp_stdio.rs`.)

### This is a documented deferral whose stated condition has now been met

[ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md) recorded this deliberately, and gated it:

> **`wal_flush_ungrouped` paths (delete/corrections/raw-cypher) are not wired up.** `applied_seq` can lag further behind reality after those operations than after an ingest chunk. This is the explicitly-safe "trailing" direction FR-003 already accepts, not a gap — extending it is unnecessary surface area for this issue and **can be added later if the lag proves to matter in practice**.

That reasoning was sound when `wal_flush_ungrouped` meant occasional maintenance operations against a graph whose bulk arrived through ingest. Two changes since have invalidated the premise:

1. **#379 (the direct assertion API) made `wal_flush_ungrouped` a primary write path**, not an occasional one. An agent-authored or layer graph is built entirely from `knowledge_assert_entity` / `knowledge_assert_relationship` / `knowledge_add_cross_group_edge` — every one of which flushes ungrouped. Such a graph's `applied_seq` is not "trailing"; it is permanently `0` no matter how much is written.
2. **#378 (per-group WAL positions) makes the position load-bearing for correctness, not just observability.** Its FR-011 requires cross-group pointers into a group to be re-bindable after that group's stream advances, using *that group's own applied position* as the staleness signal. For a layer graph built entirely by assertion, that signal never moves — so re-binding would never trigger, for precisely the topology #369/#378 exist to serve.

The second point is why this is worth fixing rather than continuing to accept: it turns a cosmetic reporting inaccuracy into a silently-skipped correctness pass.

### Dependency on #378

As of this writing, #378 ("Multi-stream WAL: one WAL directory per group") is not yet merged to `main` — it has an open, mergeable, CI-green PR (#382). This spec is written against the shape #378 establishes: `get_applied_seq`/`set_applied_seq` keyed by `group_id`, and `wal_flush_ungrouped` already taking a `group_id` parameter. Direct inspection of #378's branch confirms `wal_flush_ungrouped` there still returns `()` and still never advances `applied_seq` — so this issue's fix is genuinely additive on top of #378, not redundant with it, regardless of which of the two lands first. If #378 has not merged into `main` by the time Research begins on this issue, Research should treat that as a sequencing dependency to flag rather than reimplement per-group position infrastructure from scratch.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - `knowledge_status` reflects assertion-only writes (Priority: P1)

An operator or agent builds a graph entirely through the assert API (`knowledge_assert_entity`, `knowledge_assert_relationship`, `knowledge_add_cross_group_edge`) — the pattern #379 exists to serve, with no episode ingest involved at all. They call `knowledge_status` and expect `wal.applied_seq` for that group to reflect what has actually been written, not report `0` regardless of how much content exists.

**Why this priority**: This is the exact reproduction in the issue: 35 WAL lines written, `applied_seq` permanently `0`. It is the direct, user-visible symptom the issue is filed against.

**Independent Test**: Write N entities/relationships through the assert API alone into one group, call `knowledge_status`, and confirm the reported `applied_seq` for that group is consistent with (not necessarily identical to, if some writes are filtered — see Edge Cases) that group's WAL `max_seq`, not `0`.

**Acceptance Scenarios**:

1. **Given** a fresh group with no prior writes, **When** N entities and relationships are written via `knowledge_assert_entity`/`knowledge_assert_relationship`/`knowledge_add_cross_group_edge` and `knowledge_status` is then called, **Then** the reported `applied_seq` for that group is consistent with the group's WAL `max_seq` — not `0`.
2. **Given** a group already at some non-zero `applied_seq`, **When** additional assert-API writes land in that group, **Then** `applied_seq` advances further, tracking the new writes.

---

### User Story 2 - Cross-group re-bind staleness detection fires for assertion-built graphs (Priority: P1)

A layer graph is built entirely through the assertion API and later receives a cross-group pointer from another group. When the target group's content changes further, #378's FR-011 re-bind staleness gate — which compares against the target group's own `applied_seq` — must actually detect the change and trigger re-resolution, rather than silently never firing because the target group's `applied_seq` never moved.

**Why this priority**: This is the correctness consequence that elevates the issue beyond cosmetic reporting — the motivating case for filing it at all.

**Independent Test**: Build a target group entirely via the assert API, create a cross-group pointer into it from a second group, then write further assert-API content into the target group. Confirm the re-bind staleness check (FR-011) observes the target group's advanced `applied_seq` and triggers re-resolution.

**Acceptance Scenarios**:

1. **Given** a target group built entirely through the assertion API with an existing cross-group pointer into it, **When** further content is asserted into the target group, **Then** the target group's `applied_seq` advances, and the FR-011 re-bind staleness check observes the advance and fires.

---

### User Story 3 - Per-group isolation (Priority: P1)

Multiple groups are written to independently. A write to one group's `applied_seq` must never move another group's reported position.

**Why this priority**: #378 keys `applied_seq` per group specifically so that one group's activity can't corrupt another's status reporting or re-bind signal. This fix must preserve that isolation, not just restore a single shared counter.

**Independent Test**: Write to group A only, then read `knowledge_status` for group B and confirm its `applied_seq` is byte-identical to its value before group A's write.

**Acceptance Scenarios**:

1. **Given** two groups, A and B, with established `applied_seq` values, **When** a write is flushed via `wal_exec::wal_flush_ungrouped` for group A only, **Then** group B's `applied_seq` is unchanged.

---

### User Story 4 - Crash safety is preserved (Priority: P2)

The existing safety property — `applied_seq` may trail reality but must never lead it — must continue to hold for the newly-wired-up path, exactly as it already does for `wal_flush_chunk`.

**Why this priority**: This is not new value being added; it is a non-regression requirement. Getting `wal_flush_ungrouped` to advance `applied_seq` at all is worthless (worse than the status quo, in fact) if it can advance past what was durably written.

**Independent Test**: Simulate a WAL write failure partway through a batch of mutations flushed via `wal_exec::wal_flush_ungrouped`, and confirm the resulting `applied_seq` reflects only what was actually, durably written — never more.

**Acceptance Scenarios**:

1. **Given** a batch of mutations flushed via `wal_exec::wal_flush_ungrouped` where a WAL write fails partway through, **When** `applied_seq` is subsequently read, **Then** it reflects only the mutations that were durably recorded before the failure, never advancing past that point.

---

### Edge Cases

- **Empty mutation batch**: a call to `wal_exec::wal_flush_ungrouped` with no mutations must not advance `applied_seq` at all (consistent with `wal_flush_chunk`'s existing behavior for an empty chunk).
- **Partial failure mid-batch**: `wal_flush_ungrouped` flushes one `with_chunk` per mutation (not one atomic chunk for the whole batch), so a failure partway through a batch leaves some mutations durably written and others not — the advanced position must reflect exactly the durably-written prefix, not the whole attempted batch.
- **Multi-group call sites that route through a single default group regardless of the mutations' actual group**: per ADR-0378 FR-004, four call sites (`backfill.rs`, `canonicalize.rs`, `handle_delete_by_group`'s forced rebind pass, and `handle_query_cypher`) flush through `DEFAULT_GROUP_ID`'s writer unconditionally, regardless of which group(s) the mutations logically touch. After this fix, those call sites will advance `DEFAULT_GROUP_ID`'s `applied_seq` specifically — this is inherited from #378's existing routing decision, not introduced by this issue, and is not a defect to fix here.
- **`handle_query_cypher`'s treatment**: see Open Questions below.
- **Doc-comment and ADR-text corrections** (FR-004, FR-005): these are documentation-only changes with no runtime edge cases of their own.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Writes flushed via `wal_exec::wal_flush_ungrouped` MUST advance the target group's persisted `applied_seq`, in the same way `wal_flush_chunk` already does.
- **FR-002**: The advance MUST be per-group, consistent with #378's per-group `WalPosition` rows — a write to group B MUST NOT advance group A's position.
- **FR-003**: The existing safety property MUST be preserved: `applied_seq` may trail reality but MUST NEVER lead it. A failed or partial WAL write MUST NOT advance the position past what was durably recorded.
- **FR-004**: `wal_flush_ungrouped`'s doc comment MUST be corrected to name its actual callers (or describe them accurately as a category), not the stale three-caller list.
- **FR-005**: [ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md)'s consequence entry recording the `wal_flush_ungrouped` deferral MUST be updated to record that its "if the lag proves to matter" condition was met, and why, so the history stays legible rather than looking like an unexplained reversal.

### Key Entities *(if the feature involves data)*

- **Per-group applied WAL position**: the existing `WalPosition` record (introduced by ADR-0353, made per-group by #378) recording the highest WAL `seq` whose mutations are committed for a given group. This issue extends which write paths keep it current; it does not change its storage shape or its `null`/`0`/integer contract.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After writing N entities/relationships through the assert API alone, `knowledge_status` reports an `applied_seq` consistent with the WAL's `max_seq` for that group — the reproduction in this spec's Background no longer shows `applied_seq: 0` against `max_seq: 34`.
- **SC-002**: A write to one group leaves every other group's `applied_seq` byte-identical.
- **SC-003**: #378's FR-011 re-bind staleness check fires for a layer graph built entirely through the assertion API — the case that motivated this issue.
- **SC-004**: No path advances `applied_seq` beyond the highest durably-written seq, including when a WAL write fails mid-flush.

## Assumptions

- #378 (per-group `WalPosition` rows, `group_id`-keyed `get_applied_seq`/`set_applied_seq`, and a `group_id`-parameterized `wal_flush_ungrouped`) is treated as a prerequisite this work builds on, per the Background's "Dependency on #378" note. It is not part of this issue's scope to implement.
- The four `DEFAULT_GROUP_ID`-routed call sites established by ADR-0378 FR-004 keep that routing; this issue only makes the position-advance itself work, it does not revisit which group each call site attributes its writes to.
- `wal_flush_ungrouped`'s current signature (returns `()`, swallows per-mutation write outcomes) will need to change to expose what was actually, durably written, the way `wal_flush_chunk` already returns `Option<u64>` — the exact shape is left to Research/Plan, not prescribed here.

## Out of Scope

- Implementing #378 itself (per-group `WalPosition` infrastructure) — a prerequisite, not part of this issue.
- Changing which group any of `wal_flush_ungrouped`'s call sites attribute their mutations to (ADR-0378 FR-004's `DEFAULT_GROUP_ID` routing for the four multi-group call sites) — reused as-is.
- Any change to `wal_flush_chunk`'s already-correct advancement behavior.
- Backfill/upgrade semantics for pre-existing databases — ADR-0353's existing backfill mechanism is unaffected by this issue.
