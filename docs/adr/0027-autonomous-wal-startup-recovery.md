# ADR-0027: Autonomous WAL-Corruption Self-Recovery on Startup

**Status**: Accepted
**Date**: 2026-06-18
**Issue**: [#151](https://github.com/verveguy/liminis-context-graph/issues/151)

## Context

When lbug detects a torn WAL tail (corrupted `liminis.db.wal`), the engine previously entered `degraded` mode immediately and waited for an external caller to invoke `knowledge_recover`. In standalone deployments with no orchestrator, this meant a torn WAL caused a hard, permanent outage.

A hand-validated recovery session confirmed a 200× speedup from a checkpoint-resume approach (~107 s) over a full re-replay (~7 h) by using the last episode's WAL sequence number as a safe resume point.

See ADR-0009 (degraded mode) and ADR-0025 (index build lifecycle during bulk-load replay) for the foundational patterns this decision builds on.

## Decision

### 1. Autonomous startup recovery before entering degraded mode

When the binary detects a recoverable DB-open failure (`Corrupted wal file` and similar), it now attempts `run_full_recovery_sequence` via `spawn_blocking` **before** entering degraded mode:

```
Err(e) if is_recoverable →
  attempt recovery::run_full_recovery_sequence(...)
    Ok  → start healthy (no degraded state entered)
    Err → fall back to degraded mode as before
```

Degraded mode now means: **recovery itself failed** — not merely that a torn WAL was detected. This is a deliberate change to the degraded-mode semantics.

### 2. Four-step recovery sequence

`run_full_recovery_sequence` executes:

1. **Checkpoint-drop**: rename `{db}.wal` aside (to `{db}.wal.corrupt-{ts}`), reopen `liminis.db` at its last lbug checkpoint.
2. **Episode-cursor derivation**: determine `from_seq` — the inclusive WAL sequence number to resume replay from.
3. **Resume replay**: `schema::drop_fts_indexes` + `WalReplayer::replay_opts(from_seq)` — replays all application WAL mutations at `seq ≥ from_seq`.
4. **Index rebuild**: `conn.build_indices_and_constraints()` — rebuilds FTS and HNSW indexes.

If step 1 fails (main DB also unreadable), the sequence falls back to full `rebuild_from_workspace_wal` from `seq=0`.

### 3. Episode-cursor derivation over a persistent seq cursor

**Why episode-cursor**: the resume point is derivable from data already in the DB — no additional instrumentation or migration is required, and it works retroactively on databases that were checkpointed before this feature existed.

**Derivation algorithm**:
1. `MATCH (ep:Episodic) RETURN ep.uuid ORDER BY ep.created_at DESC LIMIT 1` — get the last episode's uuid.
2. Scan all `.lcg/wal/*.jsonl` files for lines containing that uuid in `params["uuid"]` or `params["ep"]`.
3. `from_seq = min(all matched seqs)` — scanning ALL files ensures the global minimum is found (the uuid may appear in multiple files as MENTIONS edges include `params["ep"]`).
4. Fallback: `from_seq = 0` when no episodes exist (`NoEpisodes`) or uuid is not found in any WAL file (`UuidNotFound`).

**Why not a persistent seq cursor**: a persistent cursor requires an additional write on every WAL replay and a migration for existing databases. The episode-cursor approach has equivalent correctness guarantees (episodes are ingested in WAL seq order, so the last episode's seq is a conservative lower bound) with no additional instrumentation.

### 4. Shared `recovery` module

`crates/core/src/recovery.rs` contains `run_full_recovery_sequence` and `derive_episode_cursor` as a shared library. Both the binary startup path (`main.rs`) and the new `knowledge_recover_full` IPC handler call into this module, avoiding duplication and enabling independent unit tests.

### 5. New `knowledge_recover_full` IPC command

A separate `knowledge_recover_full` IPC command (exempt from the degraded-mode guard) executes the full four-step sequence for external callers. It is idempotent: calling it on a healthy engine returns `recovery_needed: false` and `mutations_replayed: 0` without data mutation. Concurrent calls are serialized via the existing `write_lock.try_write()` pattern.

Response shape (FR-008):
```json
{
  "success": true,
  "recovery_needed": true,
  "episodes_before": N,
  "mutations_replayed": M,
  "episodes_after": K,
  "indexes_rebuilt": true
}
```

### 6. Pre-existing index gap fix

`recover_rebuild_from_workspace_wal` (the fallback strategy for `knowledge_recover`) did not call `build_indices_and_constraints` after replay. Fixed inline: HNSW indexes were never created by `init_schema` alone, leaving the graph unsearchable after that recovery path.

## Consequences

- **Operators in standalone deployments** get zero-intervention recovery from torn WAL events. The engine comes up healthy without any external call.
- **Callers with orchestrators** can use `knowledge_recover_full` as a single idempotent call rather than orchestrating multiple `knowledge_recover` strategy calls.
- **Degraded mode** now specifically means the recovery attempt itself failed — a stronger signal than before.
- **Resume replay** re-applies mutations at `seq ≥ from_seq`, including those already committed by lbug's internal checkpoint. This relies on the empirically confirmed idempotency of the existing replay path (A3 in the spec).
- **Large WAL scan at startup**: scanning all `.jsonl` files for the last episode uuid adds startup latency proportional to total WAL size. For 200×-speedup recovery scenarios, this overhead is negligible.

## References

- ADR-0009 — Degraded mode and socket-first startup (constraint)
- ADR-0025 — Drop FTS before replay, rebuild all after (constraint on step 3)
- Issue #151 — feature spec and hand-validated recovery session
