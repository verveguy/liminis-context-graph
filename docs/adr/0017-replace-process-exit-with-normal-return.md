# ADR-0017: Replace `std::process::exit(0)` with Normal Return in async main

**Status:** Accepted  
**Date:** 2026-05-25  
**Relates to:** ADR-0003 (ArcSwap DB hot-swap), ADR-0011 (auto-heal index build), issue #90

## Context

The `liminis-context-graph` binary previously ended its async main function with
`std::process::exit(0)` after the graceful shutdown sequence:

1. Cancel token broadcast
2. Drain all connection `JoinSet` tasks (`join_set.join_next()`)
3. Abort and await rebuild job handles
4. `drop(state)` — drops the `ArcSwapOption<Db>` which holds one `Arc<Db>` clone

The intent was to guarantee an exit code of 0 without waiting for any remaining
background work. However, this created a race condition:

`handle_build_indices` (and the auto-heal path in `build_indices_once`) calls
`tokio::task::spawn_blocking(move || { ... db ... })`, where `db` is an
`Arc<Db>` clone moved into the blocking closure. The `await` on the returned
`JoinHandle` resolves when the blocking thread's *result* is ready, but the
`BlockingTask` struct that owns the captured `Arc<Db>` is dropped on the
blocking thread *after* the wakeup notification is sent to the awaiting async
task.

This creates a window:

1. `join_set.join_next()` returns — the connection handler task is done
2. `drop(state)` fires — the `ArcSwapOption<Db>` clone's refcount decrements,
   but the blocking thread still holds its `Arc<Db>` clone, so refcount > 0
3. `std::process::exit(0)` terminates the process — the blocking thread's
   `BlockingTask` is never dropped, the `Arc<Db>` refcount never reaches zero,
   and the LadybugDB WAL checkpoint never fires

The LadybugDB hash-index overflow page written by `knowledge_build_indices` is
left in a partially-committed state on disk. On subsequent re-open, lbug detects
the inconsistency and throws an `InternalException` (surfaced as a `Lbug` error
in Rust). In Release-mode lbug builds the `DASSERT` that exposes this is a
no-op, so the inconsistency silently exists rather than producing an immediate
error — future reads or writes may produce incorrect results.

This failure was observed as a consistent integration test failure in
`sigterm_produces_clean_exit_and_no_wal_corruption` on macOS arm64 (where lbug
is built from source via `LBUG_BUILD_FROM_SOURCE=1`). The same race exists on
all platforms; the macOS source build simply made it visible by enabling the
`DASSERT` check.

## Decision

Replace `std::process::exit(0)` at the end of async main with `Ok(())` (normal
return).

When the `#[tokio::main]`-generated `Runtime::block_on` completes, the tokio
runtime is dropped. Runtime drop waits for all blocking pool threads (spawned by
`spawn_blocking`) to finish their `BlockingTask` cleanup, including dropping any
captured `Arc<Db>` clones. The LadybugDB WAL checkpoint therefore fires
deterministically before the process exits.

Signal handler tasks (`tokio::spawn` for SIGTERM and Ctrl+C) are aborted on
runtime drop. This is safe: they carry no cleanup obligation and are designed to
fire once then exit. A second SIGTERM during the drain window is not caught, but
a process that is already shutting down and will shortly exit naturally does not
need to handle it.

Do not re-introduce `std::process::exit` before the runtime has been given a
chance to drain the blocking pool. If an explicit exit is ever needed (e.g., to
force exit on a hung task), use `Runtime::shutdown_timeout` or a background
watchdog task that calls `exit` only after a bounded wait — not an immediate
`exit(0)` that bypasses the drain.

## Rationale

**Why not a sleep or busy-wait?**  
FR-004 from the original spec prohibits unconditional sleeps in the production
shutdown path. More importantly, a sleep is a workaround, not a fix: it reduces
the probability of the race but does not eliminate it. The tokio runtime drain
is a deterministic guarantee; a sleep is not.

**Why not drain spawn_blocking explicitly in the shutdown sequence?**  
There is no stable public API in tokio 1.x to enumerate or wait for blocking
pool threads directly. The simplest correct solution is to let the runtime drop
handle it, which is exactly what `return Ok(())` achieves.

**Why not fix the handlers to not hold Arc<Db> across spawn_blocking?**  
The `spawn_blocking(move || { ... db ... })` pattern is correct and idiomatic:
the blocking thread needs the database handle for the duration of the operation.
The defect was the shutdown path racing against that cleanup, not the handler
pattern itself.

## Consequences

- The LadybugDB WAL checkpoint is guaranteed to fire before process exit,
  eliminating the hash-index partially-committed state that caused the
  integration test failure.
- Both `handle_build_indices` and the auto-heal path in `build_indices_once`
  (ADR-0011) are covered by this fix without any change to those handlers.
- If a `spawn_blocking` task hangs indefinitely, the process will not exit. This
  is mitigated by: (a) `LCG_SHUTDOWN_TIMEOUT_MS` applies to the async join_set
  drain so blocking work is near-complete before the runtime drops; (b) lbug
  C++ operations are synchronous and do not deadlock under normal conditions.
  If indefinite-hang protection is needed in future, add `shutdown_timeout` to
  the runtime builder.
- Future contributors must not add `std::process::exit` calls after the
  connection drain without also ensuring all `Arc<Db>` clones are explicitly
  released first.
