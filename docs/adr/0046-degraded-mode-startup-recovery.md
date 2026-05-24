# ADR-0046: Degraded-Mode Startup and In-Process Recovery

**Status**: Accepted  
**Date**: 2026-05-24  
**Extends**: ADR-0042 (reader-writer split), ADR-0043 (ArcSwap hot-swap)

## Context

When lbug's internal `db.wal` file becomes corrupt (e.g. after a hard kill or power loss), `Db::open` returns an error and the previous startup sequence propagated it via `?`, exiting with code 1. The lifecycle supervisor treated this as a crash and restarted the binary — which immediately crashed again, creating a crash loop. The user had no way to invoke any IPC method to recover, because the binary never reached the point of binding its Unix socket.

This ADR documents three interrelated decisions made to address this situation:

1. Startup order flip (socket bind before DB open)
2. Error classification by Display-string matching
3. `ArcSwapOption<Db>` as the degraded state representation
4. Recovery serialization via the existing `write_lock`

## Decision 1: Bind socket before DB open

**Before:**
```rust
let db = Arc::new(Db::open(&db_path)?);  // exits on any lbug error
let listener = UnixListener::bind(&socket_path)?;
```

**After:**
```rust
let listener = UnixListener::bind(&socket_path)?;  // socket always bound
// ... attempt DB open, classify error
let state = AppState::from_env(sink, maybe_db, degraded_reason, db_path);
```

Binding the socket first means the binary can always serve IPC requests. When DB open fails recoverably, the process enters degraded mode instead of exiting. The `health_check`, `knowledge_status`, `knowledge_recover`, and `knowledge_close` methods remain available; all other methods return error code `-32001`.

## Decision 2: Error classification by Display-string matching

lbug errors do not expose a structured error hierarchy through its Rust bindings — all lbug failures surface as `lbug::Error` with a string message. Classification is therefore performed by substring match on the `Display` output:

```rust
let is_recoverable = msg.contains("Corrupted wal file")
    || msg.contains("Permission denied")
    || msg.contains("No such file or directory");
```

**Recoverable** errors (enter degraded mode):
- `"Corrupted wal file"` — lbug WAL corruption, the primary case
- `"Permission denied"` — DB file unreadable; user can fix permissions and recover
- `"No such file or directory"` — DB file missing; workspace WAL replay can rebuild

**Fatal** errors (propagate via `?`, process exits):
- Everything else — unknown lbug errors, OOM, port conflicts, etc.

The string-matching approach is fragile if lbug changes its error messages across versions, but it is the only option without changes to the lbug Rust bindings. This is documented as a known risk in the issue.

## Decision 3: `ArcSwapOption<Db>` for degraded state

ADR-0043 established `ArcSwap<Db>` to allow `clear_all` to atomically hot-swap the DB instance without a mutex. This ADR extends the same pattern to represent the degraded (no-DB) state:

```rust
// Before (ADR-0043):
pub db: ArcSwap<Db>

// After (ADR-0046):
pub db: ArcSwapOption<Db>   // = ArcSwapAny<Option<Arc<Db>>>
pub degraded_reason: Arc<Mutex<Option<String>>>
```

`ArcSwapOption<T>` is the first-class type alias from the `arc-swap` crate for `ArcSwapAny<Option<Arc<T>>>`. Its `load_full()` method returns `Option<Arc<Db>>` directly. `None` represents degraded state.

All handlers use a `load_db(&state)?` helper that extracts `Arc<Db>` or returns `Error::DbUnavailable(reason)`. A degraded-mode guard in `handle()` short-circuits this for all non-exempt methods before they reach their own `load_db` call.

The `degraded_reason` field holds the machine-readable reason string (`"lbug_wal_corrupt"`) set at startup and cleared to `None` after successful recovery. It is written only at startup and during recovery, so Mutex contention is negligible.

## Decision 4: Recovery serialization via `write_lock.try_write()`

ADR-0042 established `write_lock: Arc<RwLock<()>>` to serialize all mutating DB operations. `knowledge_recover` acquires this lock with `try_write()`:

```rust
let _write_guard = state
    .write_lock
    .try_write()
    .map_err(|_| Error::Ipc("Recovery already in progress".to_string()))?;
```

`try_write()` fails immediately (rather than blocking) if the write lock is held, which can happen if `knowledge_recover` is called concurrently or if another write is in flight. Concurrent callers receive a structured "recovery already in progress" error. No `AtomicBool` flag is needed — the write lock is the concurrency primitive.

## Consequences

**Positive:**
- Users can invoke `knowledge_recover` from any client without restarting the process.
- `health_check` and `knowledge_status` remain available in degraded mode, enabling the renderer to show actionable recovery UI.
- The `service_state` telemetry event gives operators observability into degraded/healthy transitions without polling.

**Negative:**
- The Display-string classification is brittle against lbug version changes.
- All 30+ handlers required a mechanical update from `state.db.load_full()` (which previously returned `Arc<Db>`) to `load_db(&state)?` (which returns `Arc<Db>` or `Error::DbUnavailable`). The degraded guard in `handle()` prevents most of them from being reached in degraded mode, but each site required updating for the type change.

## Alternatives Considered

**Process restart instead of in-process recovery**: The existing lifecycle supervisor already handles process restarts. We could exit 0 on a corrupt WAL and trust the supervisor to restart with a fresh WAL. Rejected: this requires the supervisor to detect the corruption reason and suppress the crash-loop counter, which is more complex and requires changes to the TypeScript layer.

**Structured lbug error types**: Changing lbug's Rust bindings to expose a structured error enum would be cleaner than Display-string matching. Out of scope for this issue; deferred as a future lbug improvement.
