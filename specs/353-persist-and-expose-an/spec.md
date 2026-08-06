# Feature Specification: Persist and expose an applied WAL sequence in knowledge_status

**Feature Branch**: `fabrik/issue-353`
**Created**: 2026-08-05
**Status**: Draft
**Input**: User description: "Work issue for community report #351. Persist the highest WAL `seq` whose mutations are committed in the current graph, and expose it via `knowledge_status` as `wal.applied_seq` alongside `wal.max_seq`, so a consumer can decide at boot — with one call and an integer comparison — whether its DB is already consistent with the WAL, needs an incremental resume, or needs a full rebuild."

## Background

Community report #351 describes the orac/zen deployment model: the distributed JSONL WAL (published to a git repo) is authoritative, and every node — read-only zen consumers and the ingesting master node alike — rebuilds its local LadybugDB from that WAL on boot. On startup, a node needs to answer cheaply and reliably: *does my local DB already reflect this exact WAL, or do I need to replay?* In normal operation the container restarts frequently with no WAL changes in between, so the common case must be a cheap no-op; divergent cases (failover advanced the WAL, a backup restored behind it, a corpus reset) must self-heal.

Today there is no reliable way to answer that question. `knowledge_status`'s `wal` object exposes only `exists` / `file_count` / `byte_size` — nothing that ties DB content to a WAL position. `knowledge_rebuild_from_wal` computes exactly the needed value (`last_committed_seq`) but discards it once the call returns. The downstream workaround — hashing WAL file contents to detect change — is unreliable: lcg writes compact `serde_json`, but the distributed copy is re-serialized by a Python publisher with spaced formatting, so the bytes differ while the semantics are identical, forcing a full rebuild on every boot even when nothing changed. All of this was verified against `main` at `1183160a`. #351's full motivation and consumer decision table are not restated here.

This issue persists the applied position durably (surviving restart, not living in `AppState` or the WAL files) and surfaces it in `knowledge_status` so a client can make the up-to-date / incremental / full decision from a single call and an integer comparison.

### Blocked by the seq-uniqueness bug

This work is `blockedBy` the `global_seq` re-derivation fix (#352). The whole design rests on WAL `seq` being unique and monotonic; a WAL populated after process start can otherwise receive duplicate seqs, which makes both `applied_seq` and `max_seq` ambiguous. That fix must land first.

### The gap #351 does not cover: upgrade semantics

Issue #351 specifies `applied_seq` as "integer, or `null` when the graph is empty / nothing applied." That conflates two different states, and the difference bites on the first boot after upgrade:

| State | Naive `applied_seq` | `entity_count` | Correct action |
|---|---|---|---|
| Fresh/cleared DB | `null` | `0` | nothing to do, or full rebuild if WAL non-empty |
| **Upgraded pre-existing DB** | `null` | **> 0** | **unknown position — full rebuild required** |

A DB populated before this feature existed has content but no recorded position. Reporting `null` makes that indistinguishable from "empty." Every existing deployment would hit this exactly once on upgrade, if the unknown state were allowed to exist at all — it does not need to. `lcg` can derive the position from graph content on first open using [ADR-0026](../../docs/adr/0026-episode-cursor-wal-resume.md)'s episode cursor, collapsing the "upgraded, unknown" row into the "known position" row (see FR-007). Picking a sentinel value and documenting it was considered and rejected: that pushes a correctness obligation onto every consumer, forever, to handle a state the service can resolve for itself once.

### Relationship to ADR-0026

[ADR-0026](../../docs/adr/0026-episode-cursor-wal-resume.md) already considered persisting a last-applied seq in a metadata row, for crash recovery, and rejected it in favor of the episode cursor — because the cursor is crash-proof and retroactive (works on databases that predate any cursor mechanism). This issue is not unaware of that decision; it diverges deliberately, because the two problems are different:

- ADR-0026 is crash recovery, where a one-off WAL scan on the recovery path is acceptable.
- This issue wants a cheap boot check: `applied_seq` itself is an O(1) persisted-row lookup, but
  `max_seq` must still observe WAL content written by other processes, so the goal is to make
  that scan cheap enough to run on every `knowledge_status` call, not to avoid it entirely.

The two are complementary, not competing: persist a cursor for the fast path (this issue), and derive one via the episode-cursor mechanism when the persisted record is absent (FR-007, reusing ADR-0026's mechanism rather than inventing a second one). The ADR written for this work must cite ADR-0026 and state this reasoning, not present the persisted cursor as a novel idea.

### Schema parity constraint

This needs new persistent DB state, and `schema.rs` currently has no metadata/singleton table — only `Entity`, `Episodic`, `RelatesToNode_`, `Community`, `Saga`, and the rel tables. This project's schema is required to track parity with graphiti's `kuzu_driver.py` (the canonical source of truth for node/rel tables). A metadata table that graphiti does not have is a **deliberate divergence** from that parity rule and must be recorded as such in the ADR, not introduced silently — confirm whether graphiti has an equivalent before inventing one.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Boot-time consistency check with one call (Priority: P1)

A consumer node (e.g. an orac/zen deployment) calls `knowledge_status` on startup and compares `wal.applied_seq` to `wal.max_seq` to decide, without any other call, whether its local DB is already consistent with the WAL, needs an incremental resume, or needs a full rebuild.

**Why this priority**: This is the entire point of the issue — replacing today's fragile byte-signature heuristic with a reliable, cheap, single-call check is the reason #351 was filed.

**Independent Test**: After a `knowledge_process_chunk` call, read `knowledge_status` and confirm `wal.applied_seq` equals the max `seq` of the WAL lines just written, and that this holds both immediately and after a service restart.

**Acceptance Scenarios**:

1. **Given** a DB whose applied position matches the WAL's current max seq, **When** a client calls `knowledge_status`, **Then** `wal.applied_seq == wal.max_seq` and the client can skip any rebuild.
2. **Given** a DB behind the WAL's current max seq (a forward extension), **When** a client calls `knowledge_status`, **Then** `wal.applied_seq < wal.max_seq`, correctly signaling an incremental resume is needed.
3. **Given** a DB whose position cannot be reconciled with the WAL as a forward extension, **When** a client calls `knowledge_status`, **Then** the reported values signal that a full rebuild is needed.

---

### User Story 2 - Safe first boot after upgrading an existing deployment (Priority: P1)

An operator upgrades a deployment that already has a populated graph from before this feature existed. On first boot after the upgrade, the service backfills a usable applied-position value from the existing graph content, rather than reporting `null` and leaving the deployment indistinguishable from an empty DB.

**Why this priority**: Every existing deployment hits this exactly once on upgrade. Reporting `null` for a populated graph is a correctness gap that would cause a client to either skip a needed rebuild or be unable to tell "empty" from "unknown."

**Independent Test**: Open a DB created before this feature (built on 0.12.1 or earlier) and confirm `knowledge_status` reports an integer `wal.applied_seq`, derived via the episode-cursor mechanism, rather than `null`.

**Acceptance Scenarios**:

1. **Given** a pre-existing populated DB with no persisted applied-seq record, **When** it is opened under the new version, **Then** `wal.applied_seq` is backfilled to a conservative integer position derived from the last `Episodic` node's location in the WAL, not reported as `null`.
2. **Given** a pre-existing populated DB whose last episode's uuid cannot be found in the WAL, **When** it is opened, **Then** `wal.applied_seq` is `null` and the documented consumer action for that state is a full rebuild.

---

### User Story 3 - A crash never overstates applied progress (Priority: P2)

A process is killed (e.g. `kill -9`) between a chunk's graph commit and the applied-position update. On restart, `wal.applied_seq` reflects a position at or behind what is actually committed in the graph — never ahead of it.

**Why this priority**: This is the safety property the rest of the feature depends on. An `applied_seq` that overstates progress would cause a client to skip mutations that were never actually applied, silently corrupting the derived DB — worse than the status quo the feature replaces.

**Independent Test**: Kill the process between a chunk commit and the position update (or simulate the ordering directly), restart, and confirm `applied_seq` is less than or equal to the seq actually reflected in the DB.

**Acceptance Scenarios**:

1. **Given** a crash between a chunk's commit and the applied-position write, **When** the service restarts, **Then** `wal.applied_seq` is less than or equal to the seq actually committed — trailing is acceptable (a resume redoes a little work), leading is not (it would skip committed-but-unrecorded mutations).

---

### Edge Cases

- **Fresh or cleared DB, no WAL content**: `applied_seq` reports `0` (a known position: nothing applied) and `max_seq` reports the WAL's state; no rebuild is implied unless the WAL is itself non-empty.
- **Pre-existing DB with no `Episodic` nodes at all**: `applied_seq` backfills to `0`, the same as a fresh DB — there is no episode to anchor a position derivation on, and no other content to lose track of, so "known, nothing applied" is the correct report, not `null`.
- **Pre-existing DB, backfill succeeds**: covered by User Story 2, scenario 1 — the common upgrade path.
- **Pre-existing DB, backfill fails** (populated DB whose last episode's uuid is not found in the WAL): `applied_seq` is `null`; this is the one case where `null` remains the correct report, and the documented action is a full rebuild (same fallback ADR-0026 already defines for its own recovery path).
- **`null` vs. `0` vs. integer in client languages other than Rust**: `null` must be documented as a distinct state from `0`, because a naive numeric comparison behaves differently across languages — `null` breaks arithmetic in Rust and Python, but JavaScript coerces `null < 5` to `true`, so a JS client can silently fall through a numeric comparison unless the meaning of each value is stated explicitly rather than left to be inferred from type.
- **Prerequisite bug**: this feature is not meaningful until the `global_seq` re-derivation fix (#352) lands, since duplicate seqs make both `applied_seq` and `max_seq` ambiguous.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Persist an applied WAL sequence as DB state that survives restart. Not in `AppState`, not in the WAL files.
- **FR-002**: `knowledge_process_chunk` MUST advance it to the max `seq` of the WAL lines written for that chunk, after those mutations commit.
- **FR-003**: **Crash-safety — `applied_seq` MUST NEVER exceed what is actually committed.** If an atomic update with the chunk commit is not achievable under the storage engine's transaction semantics, write it *after* the commit so a crash leaves it behind rather than ahead. Trailing is recoverable (a replay redoes a little work); leading silently skips committed-but-unrecorded mutations. The implementation must state which of the two mechanisms was used and why.
- **FR-004**: `knowledge_rebuild_from_wal` MUST set it to the replay's `last_committed_seq` on completion.
- **FR-005**: `knowledge_clear_all` and fresh DB init MUST reset it.
- **FR-006**: `knowledge_status` MUST expose `wal.applied_seq` and `wal.max_seq` in the existing nested `wal` object. Additive only — existing fields unchanged.
- **FR-007**: **Backfill the position on first open rather than reporting it as unknown.** When a DB has content but no persisted applied-seq record, derive one using the episode-cursor mechanism from [ADR-0026](../../docs/adr/0026-episode-cursor-wal-resume.md): read the last `Episodic` node, locate its uuid in the WAL, and take that line's `seq`. ADR-0026 documents this mechanism as explicitly *retroactive* — it works on databases that predate any cursor mechanism, which is exactly the upgrade case. The derived value is conservative (an episode boundary, so `<=` the true position), the same safe direction FR-003 requires.
- **FR-008**: Reserve `null` for genuine backfill failure only — a populated DB (at least one `Episodic` node) whose last episode's uuid is not found in the WAL. A DB with no `Episodic` nodes at all is not a backfill failure — there is nothing to derive a position from and nothing to lose track of, so it backfills to `0` directly, without a WAL scan. ADR-0026 already defines the `null` action (fall back to a full rebuild). Distinct values, distinct meanings: `null` = unknown, `0` = nothing applied, integer = known position. Document the rule explicitly rather than relying on the type: `null` breaks arithmetic in Rust and Python, but JavaScript coerces `null < 5` to `true`, so a JS consumer can fall through a numeric comparison unless the docs state the branch.

### Key Entities *(if applicable)*

- **Applied WAL Sequence record**: a singleton piece of persisted DB state recording the highest WAL `seq` whose mutations are committed in the current graph. `null` means unknown (backfill failed), `0` means nothing applied, a positive integer means a known applied position.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After `knowledge_process_chunk`, `wal.applied_seq` equals the max seq just written — both immediately and after a service restart (proving persistence, not memoisation).
- **SC-002**: After `knowledge_rebuild_from_wal {from_seq: 0, force_clear: true}` over a WAL whose max seq is `N`, `wal.applied_seq == N`.
- **SC-003**: After `knowledge_clear_all`, `wal.applied_seq` resets.
- **SC-004**: A `kill -9` between the graph commit and the position update leaves `applied_seq` less than or equal to the seq actually reflected in the DB, never greater.
- **SC-005**: Opening a DB created before this feature backfills a usable integer position via the episode cursor (FR-007) — not `null`, and not a value that would let a client skip a needed rebuild. Verified against a DB built on 0.12.1 or earlier.
- **SC-006**: `wal.max_seq` is derived from `scan_max_seq` over the WAL dir — the literal highest `seq` present (`scan_max_seq() - 1`, since `scan_max_seq` itself returns the next-assignable seq, one past the highest seq actually written).
- **SC-007**: With a populated graph whose last episode's uuid is absent from the WAL, the reported value is `null` and the documented action is a full rebuild (FR-008).

## Assumptions

- A singleton metadata record is sufficient; no per-group or per-table position is needed. #351 asks for one global position, and the consumer logic only compares one integer.
- `scan_max_seq` is cheap enough to call per `knowledge_status`. If it turns out to be O(WAL size) and status is polled frequently, that would argue for caching `max_seq` rather than rescanning — worth confirming during research/implementation.

## Out of Scope

- The `global_seq` re-derivation fix itself (#352) — this issue depends on it but does not implement it.
- Any change to ADR-0026's episode-cursor crash-recovery mechanism — this issue reuses it for backfill, it does not modify it.
- Picking a sentinel value to represent "unknown position on a populated DB" — considered and explicitly rejected in favor of backfilling a real value (see "The gap #351 does not cover" above).

## Notes

- Milestone **0.12.2**, by maintainer decision (2026-08-05). This adds API surface and new persistent schema, which would normally argue for a minor rather than a patch release; the call was made to ship it alongside the prerequisite fix (#352) in one patch release because the consuming deployment needs both together. The crash-safety (FR-003), upgrade-semantics (FR-007), and schema-parity constraints are unchanged by the retarget and still gate the work.
- Community report #351 should be closed and moved to Shipped when this ships — its primary ask is this feature, not the prerequisite bug fix.

## Source References

- #351 — the community report this issue implements, with motivation and consumer decision table
- #352 — the blocking `global_seq` re-derivation fix
- [ADR-0026](../../docs/adr/0026-episode-cursor-wal-resume.md) — episode-cursor WAL resume; the retroactive backfill mechanism this issue reuses, and the prior rejection of a persisted-cursor approach for crash recovery specifically
- [ADR-0009](../../docs/adr/0009-degraded-mode-startup-recovery.md) — degraded-mode startup & recovery
- [ADR-0025](../../docs/adr/0025-auto-heal-index-build.md) — auto-heal index build
- `crates/core/src/handlers.rs:209-211` — the `wal` status object
- `crates/core/src/replay.rs:155` — `last_committed_seq`
- `crates/core/src/schema.rs` — node/rel table definitions; schema-parity constraint
