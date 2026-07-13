# ADR-0013: CancellationToken as the Single Shutdown Signal on AppState

**Date**: 2026-05-25
**Status**: Accepted
**Issue**: liminis-context-graph#78

## Context

The graceful shutdown sequence installed by liminis-context-graph#71 (PR #72) set
`AppState.shutdown: Arc<AtomicBool>` to `true` at shutdown start and depended
on a 30 s inner timeout to drain in-flight connection tasks. Because the LLM
HTTP calls in `add_episode` are `async` (reqwest futures), they could in
principle be cancelled by dropping the future. However, no cancellation was
actually plumbed: the drain loop waited the full 30 s timeout before
`abort_all()` dropped the futures. Every Cmd+Q during ingestion paid a 30 s
"is it frozen?" wait.

The `AtomicBool` was a polling-only mechanism; nothing checked it in the
hot path. Adding polling calls throughout `add_episode` would have been
both racy (TOCTOU at every check) and slow (no wakeup on change).

## Decision

Replace `AppState.shutdown: Arc<AtomicBool>` with
`AppState.cancel_token: tokio_util::sync::CancellationToken`.

At the top of the shutdown sequence in `main.rs`, call
`state.cancel_token.cancel()` before the drain loop. Inside `add_episode`,
wrap every long async call in `tokio::select!` against
`state.cancel_token.cancelled()`. When the token fires, the in-flight
reqwest future is dropped within one network round-trip window (~100 ms)
and `add_episode` returns `Err(Error::Cancelled)`.

The DB-commit phase (Phase C) is exempt: the `select!` wraps the
`write_lock.write().await` call, so cancellation prevents *starting* the
commit. Once the write guard is held, the `spawn_blocking` closure runs to
completion — Phase C is short (~200 ms) and produces durable state.

## Rationale

`tokio_util::sync::CancellationToken` was chosen over the `AtomicBool`
for three reasons:

1. **Async-friendly wakeup.** `token.cancelled()` returns a `Future` that
   resolves the instant the token is cancelled. A `tokio::select!` arm
   against it costs nothing when the token is not cancelled, and wakes
   immediately when it is — no polling interval required.

2. **Sync `is_cancelled()`.** `CancellationToken::is_cancelled()` is a
   synchronous `fn(&self) -> bool`, so it serves the same role as
   `AtomicBool::load()` in blocking contexts (e.g., the `cancel_fn`
   closure passed to `WalReplayer` inside `spawn_blocking`). No regression
   on that path.

3. **Composable child tokens.** Future work may need to cancel a subset of
   work without stopping everything (e.g., per-request cancellation).
   `token.child_token()` provides this for free. The `AtomicBool` approach
   would require per-task flags.

## Rejected Alternative: Polling AtomicBool

The `AtomicBool` approach would require `add_episode` to call
`state.shutdown.load(Ordering::Relaxed)` at every phase boundary and sleep
between checks in a polling loop. This is equivalent to what `select!`
does, but without the instant wakeup. It also introduces a TOCTOU window
between the poll and the next await — cancellation could arrive between
the check and the HTTP call, and would not be seen until the next poll.

## Known Gap: handle_rebuild_from_wal

The `knowledge_rebuild_from_wal` handler holds the write lock for long
periods via a `spawn_blocking` task. Because `spawn_blocking` runs on an
OS thread that cannot be interrupted, a rebuild in progress when SIGTERM
arrives will block `add_episode` callers waiting for `write_lock.write()`.
Those callers are caught by the `select!` on `write_lock.write().await` and
bail cleanly; the rebuild itself is out of scope for this change.

## Consequences

- **Fast shutdown**: Cmd+Q on an actively-ingesting workspace now exits
  within ~500 ms instead of 30 s. The default `LCG_SHUTDOWN_TIMEOUT_MS`
  is reduced from 30 000 ms to 5 000 ms (still 25× the worst-case Phase C
  commit time).
- **All new long-running async operations** in `add_episode` or any
  future async handler **must** check the token at phase boundaries via
  `tokio::select!` or `is_cancelled()`. This is a new project convention.
- **Telemetry**: The closing `state: "stopped"` event now includes
  `detail: {"drained": N, "cancelled": M}` so operations teams can
  distinguish clean drains from cancelled drains.
- **`handle_rebuild_from_wal`**: The `cancel_fn` closure used by
  `WalReplayer` now reads `token.is_cancelled()` instead of
  `shutdown.load(Ordering::Relaxed)`. Semantics are unchanged.
