# ADR-0005: Streaming IPC Progress Framing via `_progress_token`

**Date**: 2026-05-22
**Status**: Accepted

## Context

The WAL rebuild operation (`knowledge_rebuild_from_wal`) can take seconds to minutes depending on WAL log size. Without progress reporting, callers have no visibility into replay progress and must either poll `knowledge_rebuild_status` or treat the operation as a black box.

The existing IPC transport (`main.rs`) is a line-delimited JSON-RPC channel over a Unix domain socket. Each request produces a single JSON-RPC response. The transport has no native concept of streaming or intermediate events.

Two options were considered:

1. **Poll-only** — background job only; caller polls `knowledge_rebuild_status` every N ms.
2. **Optional streaming via sentinel param** — if caller sets `_progress_token` in params, dispatcher streams progress lines before the terminal response.

Option 1 is simpler but forces every caller to implement a polling loop. Option 2 lets Python/CLI callers opt into real-time progress with minimal protocol change.

## Decision

### `_progress_token` sentinel

If a JSON-RPC request includes a `"_progress_token"` key with a non-null value in its `params`, `main.rs` activates streaming mode for that request:

1. A `tokio::sync::mpsc::unbounded_channel::<Value>()` is created.
2. `handlers::dispatch()` is spawned as a separate `tokio::task`, receiving the `tx` end.
3. `main.rs` drains the `rx` end, writing each `Value` as a JSON line to the Unix socket immediately.
4. Once the dispatch task completes, the terminal JSON-RPC response is written.

Progress values written by the handler have the shape:
```json
{"type": "progress", "message": "replayed 1000 mutations in file ...", "mutations_replayed_so_far": 1500, "files_processed_so_far": 3}
```

The `_progress_token` value itself is opaque; the caller uses it to correlate progress events with the originating request (useful when multiple requests are in flight over the same channel, though the current server is single-request).

### Streaming vs. non-streaming dispatch

`handlers::dispatch()` receives `progress_tx: Option<UnboundedSender<Value>>`. Handlers that support streaming check this parameter:

- If `Some(tx)`: streaming path — hold the write lock, run `spawn_blocking` once with a `progress_fn` that sends to `tx`. Returns a terminal result.
- If `None` and `dry_run: true`: synchronous path — run `spawn_blocking` once without background job, return stats immediately.
- If `None` and `!dry_run`: background job path — spawn a task with `write_lock.write_owned()` (OwnedRwLockWriteGuard, which is `'static + Send`), return `{job_id, status: "running"}` immediately.

### WAL-after-DB ordering during rebuild

During normal writes, WAL is appended **before** lbug DB commit (Principle IV from the project constitution). During WAL rebuild/replay, the ordering is necessarily inverted: WAL files are the source of truth and lbug DB is the target. The rebuild operation acquires the same write lock used by `add_episode` to prevent concurrent mutations from racing with the replay cursor.

The `from_seq` parameter enables partial rebuilds: a caller can checkpoint the last replayed sequence number and resume without replaying already-applied mutations.

### `OwnedRwLockWriteGuard` in spawned tasks

Background `tokio::spawn` tasks cannot hold a `RwLockWriteGuard<'_, ()>` because the guard borrows from the `RwLock` and is therefore not `'static`. Instead, `write_lock.write_owned().await` is used, which returns an `OwnedRwLockWriteGuard<()>` that is `'static + Send` and can be moved into the spawned task.

## Consequences

- **Opt-in streaming**: callers that don't set `_progress_token` see no behavioral change. The background-job path for non-streaming non-dry-run calls is preserved.
- **Single write lock**: streaming and background-job paths both acquire the same `RwLock` write guard, ensuring rebuild and `add_episode` never overlap. The lock is held for the duration of the `spawn_blocking` replay, not across the full async task lifetime.
- **Dry-run is always synchronous**: `dry_run: true` without `_progress_token` returns a direct response with `mutations_replayed` and `dry_run: true`. No job is created, no lock is held.
- **Progress granularity**: `progress_fn` fires once per WAL file and once per 1 000 mutations, keeping stdout traffic low for large replays.
- **Transport compatibility**: progress lines and the terminal response share the same Unix socket. Callers must handle both structured (`{"type": "progress", ...}`) and terminal (`{"jsonrpc": "2.0", ...}`) JSON lines. Non-streaming callers see only the terminal response.

## References

- Issue #29 spec: `specs/29-tier-2-wal-admin/spec.md`
- ADR-0002: `docs/adr/0002-reader-writer-split.md` (RwLock design; `OwnedRwLockWriteGuard` pattern derived from it)
- WAL implementation: `crates/core/src/wal.rs`, `crates/core/src/replay.rs`
- Streaming main.rs path: `crates/service/src/main.rs`
