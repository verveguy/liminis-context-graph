# ADR-0003: `ArcSwap<Db>` for Live Database Replacement in `clear_all`

**Date**: 2026-05-22
**Status**: Accepted

## Context

`knowledge_clear_all` must delete the on-disk database files and reinitialize a fresh empty database while the service remains alive and continues accepting requests. After reinitialization, all subsequent calls must use the new database, not the deleted one.

`AppState` previously held `Arc<Db>`. `Arc<T>` cannot be mutated in place — once created, the pointer is immutable — so `clear_all` had no way to replace the live `Db` without one of:

1. Wrapping the `Arc<Db>` in a `Mutex<Arc<Db>>` so the inner value can be swapped.
2. Using an atomic reference-counted swap type (`ArcSwap`) that permits lock-free reads.
3. Exiting and restarting the process, shifting the reconnection burden to the caller.

## Decision

`AppState.db` is changed from `Arc<Db>` to `arc_swap::ArcSwap<Db>`.

**Read path** (all handlers except `clear_all`): call `state.db.load_full()` to get a snapshot `Arc<Db>`. `load_full` is a lock-free atomic operation — no contention with concurrent readers or with the `store` in `clear_all`.

**`clear_all` write path**:

1. Acquire the existing `write_lock.write().await` guard (ADR-0002 serialization).
2. In `spawn_blocking`: delete DB files on disk and the WAL directory.
3. In a second `spawn_blocking`: call `Db::open` and `conn.init_schema` to produce a fresh `Db`.
4. Call `state.db.store(Arc::new(new_db))` — atomically installs the new `Arc<Db>` pointer.
5. Drop the write guard.

If step 3 fails, the service returns an error with a recovery hint and does **not** call `store`. The old (now-deleted) `Arc<Db>` remains in the `ArcSwap`; subsequent DB queries will fail until the service is restarted. The error message instructs the caller to restart.

### Why not `Mutex<Arc<Db>>`?

`Mutex<Arc<Db>>` serializes every `db.connect()` call across all handlers — including the hot search path. Every read would block on mutex acquisition, conflicting with ADR-0002's explicit goal of keeping search reads off the write serialization path. `ArcSwap` reads are lock-free; only the `store` in `clear_all` has any synchronization cost.

### Why not process restart?

A process restart would require the Unix socket to be re-created and the caller to reconnect. This changes the protocol contract and shifts complexity to callers. `ArcSwap` keeps the service alive and transparent to all callers except during the brief `clear_all` window.

## Consequences

- **All handlers** must call `state.db.load_full()` (not `Arc::clone(&state.db)`) to obtain a database handle. A future handler that reads `state.db` directly (e.g., `let db = &state.db`) will get an `ArcSwap<Db>`, not an `Arc<Db>` — the compiler will flag the type mismatch, making the constraint self-documenting.
- **`ArcSwap` crate** is added as a workspace dependency (`arc-swap = "1"`).
- **`clear_all` degraded-service window**: if reinitialization fails after files are deleted, the service is degraded until restarted. This matches the spec's FR-015 requirement (do not claim success if the service cannot serve subsequent requests).
- **No change to write serialization**: `clear_all` still acquires the `write_lock` write guard (ADR-0002). Concurrent reads may see a brief gap where the old `Arc<Db>` refers to deleted files if they bypass the lock (hot search handlers). This is a pre-existing architectural gap documented in ADR-0002; `clear_all` cannot close it without adding lock acquisition to the search path.
