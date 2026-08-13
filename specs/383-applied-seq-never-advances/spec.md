# Feature Specification: `applied_seq` never advances for `wal_flush_ungrouped` writes

**Feature Branch**: `fabrik/issue-383`
**Created**: 2026-08-13
**Status**: Specified
**Input**: User description: "`applied_seq` never advances for writes made through `wal_exec::wal_flush_ungrouped`, which is every write path except episode ingest. With #379's assertion API and #378's per-group positions both landed, that gap now has consequences it did not have when it was knowingly deferred."

## Background

`episode.rs` advances `applied_seq` after `wal_exec::wal_flush_chunk` (the episode-ingest path). `wal_exec::wal_flush_ungrouped` — the helper used by every other write path — never has. Its doc comment still claims three callers ("delete handlers, corrections, and `handle_query_cypher`"), but its actual callers today are far broader: at least `handle_assert_entity`, `handle_assert_relationship`, `handle_add_cross_group_edge`, `handle_rebind_pointers`, `handle_merge_entities`, `handle_apply_corrections`, `handle_delete_by_group`, `handle_delete_by_source`, `handle_delete_episode`, `handle_delete_chunk_episode`, `handle_clear_all`, `handle_query_cypher`, plus `backfill.rs`, `canonicalize.rs`, and `reprocess_relations.rs`.

**Reproduced** against `main` at `d5a3e14` (post-#378, the current per-group architecture), driving three groups through the assertion API with no LLM in the loop:

```text
knowledge_status → "wal_groups": {
  "A": { "applied_seq": null, "max_seq": 4 },
  "C": { "applied_seq": null, "max_seq": 24 },
  ...
}
```

Real, non-zero `max_seq` values are recorded for every group that received writes; `applied_seq` is `null` for every one of them, regardless of how much was written. (Originally reproduced against `main` at `27377a1`, pre-#378, against the single shared position that existed then: 35 WAL lines written, `applied_seq: 0` against `max_seq: 34` — the same bug, in the architecture that predated per-group positions.) Harness: a standalone MCP-stdio driver with a stub embedder; no extraction, fully deterministic. Available on request, or reconstructible from `crates/service/tests/common/mod.rs`'s `spawn_stub_embedder` plus the `--mcp-stdio` launch pattern in `crates/service/tests/mcp_stdio.rs`.

### This is a documented deferral whose stated condition has now been met

[ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md) recorded this deliberately, and gated it:

> **`wal_flush_ungrouped` paths (delete/corrections/raw-cypher) are not wired up.** `applied_seq` can lag further behind reality after those operations than after an ingest chunk. This is the explicitly-safe "trailing" direction FR-003 already accepts, not a gap — extending it is unnecessary surface area for this issue and **can be added later if the lag proves to matter in practice**.

That reasoning was sound when `wal_flush_ungrouped` meant occasional maintenance operations against a graph whose bulk arrived through ingest. Two changes since have invalidated the premise:

1. **#379 (the direct assertion API) made `wal_flush_ungrouped` a primary write path**, not an occasional one. An agent-authored or layer graph is built entirely from `knowledge_assert_entity` / `knowledge_assert_relationship` / `knowledge_add_cross_group_edge` — every one of which flushes ungrouped. Such a graph's `applied_seq` is not "trailing"; it is permanently unset no matter how much is written.
2. **#378 (per-group WAL positions) made the position load-bearing for correctness, not just observability.** Its FR-011 requires cross-group pointers into a group to be re-bindable after that group's stream advances, using *that group's own applied position* as the staleness signal. For a layer graph built entirely by assertion, that signal never moves — so re-binding would never trigger, for precisely the topology #369/#378 exist to serve.

The second point is why this is worth fixing rather than continuing to accept: it turns a cosmetic reporting inaccuracy into a silently-skipped correctness pass.

### Current architecture (post-#378)

Issue `#378` ("Multi-stream WAL: one WAL directory per group") merged via PR #382; `main` is at `d5a3e14`. `Conn::get_applied_seq`/`set_applied_seq` are now keyed by `group_id` (one `WalPosition` row per group), and `wal_exec::wal_flush_ungrouped` already takes `state: &AppState, group_id: &str, mutations: Vec<(String, Value)>` — but, confirmed by direct inspection, still returns `()` and still never advances the position, for any group. `wal_exec::wal_flush_chunk` takes the seq it assigned and advances that group's persisted `applied_seq` directly, exactly as it did before #378 — just scoped per group now instead of to one singleton row. `knowledge_status` exposes this as a `wal_groups` map (one entry per group with WAL content) rather than the pre-#378 singleton `wal` object. This issue's fix targets that existing per-group model; it does not itself introduce per-group tracking.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - `knowledge_status` reflects assertion-only writes (Priority: P1)

An operator or agent builds a graph entirely through the assert API (`knowledge_assert_entity`, `knowledge_assert_relationship`, `knowledge_add_cross_group_edge`) — the pattern #379 exists to serve, with no episode ingest involved at all. They call `knowledge_status` and expect the `wal_groups` entry for that group to report an `applied_seq` reflecting what has actually been written, not `null` regardless of how much content exists.

**Why this priority**: This is the exact reproduction in this spec's Background: real `max_seq` values, `applied_seq: null` for every group. It is the direct, user-visible symptom the issue is filed against.

**Independent Test**: Write N entities/relationships through the assert API alone into one group, call `knowledge_status`, and confirm the reported `applied_seq` for that group's `wal_groups` entry is consistent with (not necessarily identical to, if some writes are filtered — see Edge Cases) that group's `max_seq`, not `null`.

**Acceptance Scenarios**:

1. **Given** a fresh group with no prior writes, **When** N entities and relationships are written via `knowledge_assert_entity`/`knowledge_assert_relationship`/`knowledge_add_cross_group_edge` and `knowledge_status` is then called, **Then** the reported `applied_seq` for that group is consistent with the group's `max_seq` — not `null`.
2. **Given** a group already at some known `applied_seq`, **When** additional assert-API writes land in that group, **Then** `applied_seq` advances further, tracking the new writes.

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
- **Multi-group call sites that route through a single default group regardless of the mutations' actual group**: per ADR-0378 FR-004, the database-wide passes in `backfill.rs` and `canonicalize.rs`, plus `handle_query_cypher`, flush through `DEFAULT_GROUP_ID`'s writer unconditionally, regardless of which group(s) the mutations logically touch. After this fix, those call sites will advance `DEFAULT_GROUP_ID`'s `applied_seq` specifically — this is inherited from #378's existing routing decision, not introduced by this issue, and is not a defect to fix here. (`handle_delete_by_group`'s forced rebind pass and `handle_rebind_pointers` were also on that list when this spec was written; issue #385 / ADR-0385 has since given them per-mutation owning-group attribution, so each of their buckets flushes to — and now advances — its own group's position rather than the default group's.)
- **`handle_query_cypher`'s treatment**: resolved — it advances `applied_seq` uniformly with every other `wal_flush_ungrouped` caller (FR-006). See Notes for the rationale.
- **Doc-comment and ADR-text corrections** (FR-004, FR-005): these are documentation-only changes with no runtime edge cases of their own.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Writes flushed via `wal_exec::wal_flush_ungrouped` MUST advance the target group's persisted `applied_seq`, in the same way `wal_flush_chunk` already does.
- **FR-002**: The advance MUST be per-group, using the existing `group_id`-keyed `get_applied_seq`/`set_applied_seq` and `wal_groups` status model #378 established — a write to group B MUST NOT advance group A's position.
- **FR-003**: The existing safety property MUST be preserved: `applied_seq` may trail reality but MUST NEVER lead it. A failed or partial WAL write MUST NOT advance the position past what was durably recorded.
- **FR-004**: `wal_flush_ungrouped`'s doc comment MUST be corrected to name its actual callers (or describe them accurately as a category), not the stale three-caller list.
- **FR-005**: [ADR-0353](../../docs/adr/0353-persist-and-expose-applied-wal-seq.md)'s consequence entry recording the `wal_flush_ungrouped` deferral MUST be updated to record that its "if the lag proves to matter" condition was met, and why, so the history stays legible rather than looking like an unexplained reversal.
- **FR-006**: `handle_query_cypher`'s mutations MUST advance `applied_seq` uniformly with every other `wal_flush_ungrouped` caller — no per-caller exception. `applied_seq` is a statement about WAL position, not about the trustworthiness of the content at that position; a deployment that wants to withhold raw-cypher writes has `Scope::Cypher` for that, not selective position accounting (see Notes).

### Key Entities *(if the feature involves data)*

- **Per-group applied WAL position**: the existing `WalPosition` record (introduced by ADR-0353, made per-group by #378) recording the highest WAL `seq` whose mutations are committed for a given group. This issue extends which write paths keep it current; it does not change its storage shape or its `null`/`0`/integer contract.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After writing N entities/relationships through the assert API alone, `knowledge_status`'s `wal_groups` entry for that group reports an `applied_seq` consistent with the group's `max_seq` — the reproduction in this spec's Background no longer shows `applied_seq: null` for a group with a non-zero `max_seq`.
- **SC-002**: A write to one group leaves every other group's `applied_seq` byte-identical.
- **SC-003**: #378's FR-011 re-bind staleness check fires for a layer graph built entirely through the assertion API — the case that motivated this issue.
- **SC-004**: No path advances `applied_seq` beyond the highest durably-written seq, including when a WAL write fails mid-flush.

## Assumptions

- The `DEFAULT_GROUP_ID`-routed call sites established by ADR-0378 FR-004 keep that routing; this issue only makes the position-advance itself work, it does not revisit which group each call site attributes its writes to. (Where #385 has since replaced that routing with per-mutation owning-group attribution, the advance follows the same per-group buckets.)
- `wal_flush_ungrouped`'s current signature (returns `()`, swallows per-mutation write outcomes) will need to change to expose what was actually, durably written, the way `wal_flush_chunk` already returns `Option<u64>` — the exact shape is left to Research/Plan, not prescribed here.

## Out of Scope

- Implementing #378 itself (per-group `WalPosition` infrastructure) — already merged, not part of this issue.
- Changing which group any of `wal_flush_ungrouped`'s call sites attribute their mutations to (ADR-0378 FR-004's `DEFAULT_GROUP_ID` routing for the multi-group call sites, as amended by #385/ADR-0385) — reused as-is.
- Any change to `wal_flush_chunk`'s already-correct advancement behavior.
- Backfill/upgrade semantics for pre-existing databases — ADR-0353's existing backfill mechanism is unaffected by this issue.
- Changing `knowledge_query_cypher`'s scope gating or any other access-control mechanism — `Scope::Cypher` already exists and is the correct lever for withholding untrusted raw-cypher writes; this issue only makes its WAL position accounting consistent with every other caller.

## Notes

- **Decided (2026-08-13): `handle_query_cypher` advances `applied_seq` uniformly with every other `wal_flush_ungrouped` caller — no per-caller exception.** `applied_seq` answers "how far has this database been hydrated from its own WAL," not "how much of that WAL do we trust" — nothing else in the system distinguishes those two ideas, and carving out one caller would introduce a second, fuzzier concept every future reader has to learn. Excluding raw-cypher writes would not protect anything `knowledge_query_cypher`'s existing warning doesn't already cover — it bypasses the embedding/name-index invariants the structured tools maintain, a graph-content concern unrelated to WAL position. Excluding it would also create a hazard: raw-cypher statements are the WAL's least idempotent content (the native write path emits `CREATE`, not `MERGE` — ADR-0046), so an under-reported position would make them the ones most likely to be re-executed by a future incremental catch-up replay from `applied_seq + 1`, which fails on a duplicate primary key rather than being a safe no-op. The existing lever for withholding untrusted raw-cypher writes is `Scope::Cypher` — `knowledge_query_cypher`'s own dedicated permission bucket — not selective position accounting.
