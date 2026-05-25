# ADR-001: WAL Drain-and-Flush Pattern for Production Write Handlers

**Status**: Accepted
**Date**: 2026-05-25

## Context

`WalWriter::log_mutation` and `with_chunk` were defined but never called from any production
write handler (issue #74). Every `Cypher` mutation committed to lbug was silently missing
from the application WAL, making `knowledge_rebuild_from_wal` non-functional.

The Python `LadybugDriver.execute_query()` logs every mutation after successful execution.
The Rust binary needed equivalent behavior without changing the existing `WalWriter` API.

## Decision

**Cypher recording on `Conn`, with caller-side WAL flush.**

`Conn` gains an `executed_cyphers: RefCell<Vec<String>>` field. After each successful
`raw_query()` or `cypher_query()` call, the Cypher string is appended to this buffer.
Callers drain the buffer with `conn.drain_cyphers()` after their write operations complete
and pass the result to one of two helpers in `wal_exec.rs`:

- **`wal_flush_chunk(wal, cyphers)`** — wraps all cyphers in a single `with_chunk` call.
  Use for episode processing (Phase C of `add_episode`) where all mutations for one
  chunk should land in the WAL atomically as a unit.
- **`wal_flush_ungrouped(wal, cyphers)`** — calls `with_chunk(|w| w.log_mutation(...))` per
  cypher. Use for delete handlers, corrections, and `handle_query_cypher` where mutations
  are independent and do not need chunk-level atomicity.

## Why not add `WalWriter` directly to `Conn`

Adding `Arc<Mutex<Option<WalWriter>>>` to `Conn` creates a deadlock: `with_chunk` holds
the mutex while invoking its closure, and any nested `raw_query` that tries to lock the same
mutex again deadlocks. The `RefCell<Vec<String>>` side channel avoids this — mutations are
buffered in the `Conn`, then the *caller* (outside `with_chunk`) flushes to WAL.

## Why `RefCell` not `Mutex`

`Conn` is used only inside `spawn_blocking` (single-threaded sync context). `Conn` is already
`!Send` due to `lbug::Connection`'s lifetime bound. `RefCell` provides interior mutability
without synchronization overhead.

## WAL failures are non-fatal

Both helpers swallow WAL errors: the DB write already committed before the WAL call.
Errors are logged to stderr. The WAL is a recovery artifact, not a write gate — this matches
Python's behavior.

## Invariant for future write handlers

**Every new write handler that executes Cypher mutations MUST call `conn.drain_cyphers()`
after the writes succeed and pass the result to `wal_flush_chunk` or `wal_flush_ungrouped`.**

Failing to drain means mutations are silently missing from the WAL. The integration test
`wal_population.rs` detects this for `add_episode`; extend it for new handlers.

Use `wal_flush_chunk` when the handler processes a logical chunk of content (currently only
`add_episode` / `process_chunk` via `episode::add_episode`). Use `wal_flush_ungrouped` for
everything else.

## Consequences

- The WAL directory is now populated during normal ingestion, unblocking
  `knowledge_rebuild_from_wal` (issue #74 root cause fixed).
- WAL params field is always `{}` because lbug does not expose parameterized queries;
  all values are interpolated into the Cypher string. Replay handles this correctly
  (empty params = no substitution in `WalReplayer::interpolate_params`).
- Schema operations (`init_schema`, `build_indices_and_constraints`) and read queries
  that go through `raw_query` will also record, but the recorded cyphers are discarded
  (no `drain_cyphers()` called) — harmless.
- Replay (`WalReplayer`) goes through `raw_query`, so replayed mutations are also
  recorded. They are discarded because no caller calls `drain_cyphers()` on the replay
  `Conn` — no infinite replay loop risk.
