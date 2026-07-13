# ADR-0030: Batched Write-Lock Acquisition for Long-Running Passes

**Status**: Accepted  
**Date**: 2026-06-22  
**Context**: Issue #163 — Relation canonicalization pass

## Context

All prior write handlers in this service hold a single `write_lock.write()` guard for the
entire duration of their DB mutations. This is safe and simple for handlers that process tens
or hundreds of edges (e.g., `knowledge_merge_entities`, `knowledge_apply_corrections`).

However, the relation canonicalization pass (`knowledge_canonicalize_relations`) may process
76,000+ edges in a single call. Holding the write lock for the full duration would block all
concurrent ingestion (`knowledge_add_episode`, `knowledge_process_chunk`) for potentially
minutes — an unacceptable production impact.

## Decision

For passes that process more than ~1,000 mutations, the write phase (Phase D) MUST acquire
and release the write lock in batches rather than holding it for the full pass duration.

**Chosen batch size: 250 edges per lock acquisition.**

Each batch follows the standard drain-and-flush pattern per ADR-0015:

```
_write_guard = write_lock.write().await
  for edge in batch:
    conn.exec_params(SET or DETACH DELETE)
  wal_flush_ungrouped(conn.drain_mutations())
drop(_write_guard)          ← lock released before next batch
```

This means ~300 lock acquisitions for a 76K-edge corpus, with brief unlock windows between
each batch. Concurrent ingestion can interleave during those windows.

## Atomicity guarantee

Each batch of 250 mutations is WAL-flushed before the lock is released. If the service crashes
mid-pass, WAL replay reproduces all mutations from completed batches. The pass is idempotent
(second run emits zero new mutations), so re-running after recovery is safe.

There is **no cross-batch atomicity**: the graph may be in a partially-canonicalized state
during the pass and after a crash. This is acceptable because the pass is monotonic (each
mutation moves an edge from "unclassified" to "classified") and idempotent.

## When to apply this pattern

Future long-running admin passes SHOULD follow this pattern when the expected mutation count
exceeds approximately 1,000 rows. Smaller passes can continue to use the simpler
single-lock-hold pattern from prior handlers.

The threshold is intentionally fuzzy. The tradeoff: simplicity (single lock) vs. ingestion
availability (batched locks). Choose batched when the pass could noticeably delay an
in-progress `add_episode` call.

## Alternatives considered

- **Single write lock for entire pass**: Rejected — too long for 76K+ edges.
- **No write lock, eventual consistency**: Rejected — mutations would interleave with
  concurrent ingestion in unpredictable ways, potentially writing over in-flight episode data.
- **Batch size of 1000**: Considered but 250 was chosen to keep individual lock windows
  short (~250 `exec_params` calls, ~milliseconds per batch).
