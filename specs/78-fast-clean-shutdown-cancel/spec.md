# Feature Specification: Fast Clean Shutdown — Cancel In-Flight LLM Work on SIGTERM Instead of Waiting Out the Inner Timeout

**Feature Branch**: `fabrik/issue-78`
**Created**: 2026-05-25
**Status**: Implemented
**Input**: User observation 2026-05-25, end-to-end testing of liminis-graph#71 — every Cmd+Q on an actively-ingesting workspace waits the full 30 s inner shutdown timeout because in-flight LLM extraction calls can't be interrupted by `JoinHandle::abort`. Clean shutdown ultimately succeeds, but UX is "app hangs for 30 s on quit, every time."

## Background

liminis-graph#71 (PR #72) installed `SIGTERM`/`SIGINT` handlers in `liminis-graph/src/main.rs` and a graceful shutdown sequence: stop accepting connections, drain in-flight per-connection async tasks under a bounded inner timeout (default 30 000 ms, env-overridable via `LCG_SHUTDOWN_TIMEOUT_MS`), then `drop(state)` so `Arc<Db>` drops and lbug's WAL checkpoint runs.

Verified working end-to-end 2026-05-25 evening: SIGTERM → "shutting_down" telemetry → 30 s drain → "aborting tasks" → "stopped" telemetry → exit 0 → next startup opens DB cleanly with no `"Corrupted wal file"`.

**However**: the 30 s inner timeout fires on every quit-during-ingestion. The LLM HTTP calls (`extractor.rs`, `embedder.rs`, `dedup_adapter.rs`) are actually *async* `reqwest` futures — but no cancellation was plumbed: the drain loop waited the full 30 s before `abort_all()` dropped the futures. Only the DB commit uses `spawn_blocking` (OS thread, non-interruptible). The per-chunk pipeline never checked the `state.shutdown` flag, so the plumbing was half-built.

Net behaviour: clean shutdown works, but always pays 30 s wall-clock when ingestion is in flight. Every Cmd+Q during indexing → 30 s "is it frozen?" wait.

Without cancellation, reducing user wait requires shortening the default timeout — which trades UX for certainty that in-flight chunks complete. Cancellation lets us have both.

> **Phase naming note**: This spec uses Phase A (concurrent HTTP), Phase B (async dedup), Phase C (candidate fetch), and Phase D (DB commit) to describe the four logical stages of `add_episode`. The code in `episode.rs` labels them Phase A, Phase B, and Phase C (combining spec Phase C/D into a single "Phase C"), which the ADR also adopts. Cancellation granularity is the same regardless of labeling: the DB commit runs to completion once started.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Quitting Mid-Ingestion Returns Within ~2 s (Priority: P1)

When the user quits the Liminis Electron app while a chunk is being extracted by Sonnet, the process should signal cancellation to the in-flight work, drop the unfinished chunk cleanly, and exit within ~2 s — not 30 s.

**Why this priority**: This is the dominant quit-during-ingestion experience; today it's a 30 s "is it frozen?" wait every time. Sonnet calls regularly exceed 30 s so the full timeout fires on almost every quit during ingestion.

**Independent Test**: Spawn liminis-graph against a tempdir workspace. Send a `knowledge_process_chunk` request that triggers a real Anthropic extraction call. Mid-extraction (≤ 5 s after the request), send `SIGTERM`. Assert the process exits with `code = 0` within 5 s of the SIGTERM, the lbug DB opens cleanly on next startup, and the in-flight chunk is either fully-committed-and-WAL-logged or fully-rolled-back-and-not-WAL-logged (no torn state).

**Acceptance Scenarios**:

1. **Given** a chunk is mid-extraction (async Sonnet HTTP call inside `add_episode`), **When** SIGTERM arrives, **Then** the HTTP request is dropped via `tokio::select!` within ~500 ms, `drop(state)` completes the WAL checkpoint, and the process exits cleanly.
2. **Given** a chunk is between extraction and DB commit (Phase D / code Phase C), **When** SIGTERM arrives, **Then** the Phase D commit completes (it's short, < 200 ms) and the chunk's mutations are durably written to lbug before exit. No torn writes.
3. **Given** no work is in flight, **When** SIGTERM arrives, **Then** shutdown completes in well under 1 s (no work to drain).

---

### User Story 2 — Default Inner Timeout Is Tuned Down (Priority: P2)

The current 30 s default exists to leave headroom under the liminis-app 60 s outer SIGTERM-to-SIGKILL budget. With cancellation in place, the realistic graceful-drain ceiling is short — Phase D commit time, ~200 ms, plus a safety margin.

**Why this priority**: Lower priority because it's purely a default tweak; the cancellation work in User Story 1 is what makes it meaningful.

**Acceptance Scenarios**:

1. **Given** the cancellation path from User Story 1 is in place, **When** `LCG_SHUTDOWN_TIMEOUT_MS` is unset, **Then** the inner timeout defaults to 5 000 ms (down from 30 000) — still 12× the worst-case Phase D commit, plus margin.
2. **Given** the inner timeout is exceeded (a misbehaving handler), **When** it fires, **Then** the existing best-effort path (R5 of #71) still runs: `abort_all` + `drop(state)` + exit 0. Same as today.

---

### User Story 3 — Shutdown Telemetry Distinguishes "Clean Drain" from "Cancelled Drain" (Priority: P3)

A user (or the liminis-app shutdown panel) should be able to tell from telemetry whether shutdown completed because work finished naturally or because cancellation cut it short. This is useful for ops visibility and for the eventual shutdown-progress UI.

**Acceptance Scenarios**:

1. **Given** all in-flight work completed naturally before the timeout, **When** the shutdown sequence emits the closing `state: "stopped"` event, **Then** it includes `detail: {"drained": N, "cancelled": 0}`.
2. **Given** the cancellation path interrupted in-flight work, **When** the closing `state: "stopped"` event fires, **Then** it includes `detail: {"drained": N, "cancelled": M}` with `M > 0`.

---

### Edge Cases

- **Cancellation arrives between phases of a single chunk.** e.g. Phase A done, Phase B not started. The pipeline reads the token at phase boundaries and bails cleanly with no DB writes attempted. Chunk is re-enqueued by liminis-app on next startup.
- **Cancellation arrives during the Anthropic HTTP call.** The async reqwest future is dropped via `tokio::select!` against the cancel token. No retry is attempted on the cancellation path (distinct from the network-error retry path, which still runs in non-shutdown contexts).
- **Cancellation arrives after Phase D commit has begun.** Phase D runs to completion (FR-003). The chunk lands durably. The next phase, if any, checks the token and bails.
- **Multiple concurrent chunks in flight.** Each per-chunk task holds its own child cancellation token derived from the parent on `AppState`. Trigger on the parent cascades to all children.
- **A connection's stream sender is mid-write when cancellation fires.** The IPC writer task finishes the current frame, then exits its loop. Client sees an `error` response (or a clean close, depending on framing) rather than a truncated payload.
- **`rebuild_from_wal` is in flight.** Out of scope per the parent #71 spec's Open Question. This issue does not change its behaviour; it remains "drain or abort with best-effort cleanup."
- **Inner timeout fires before cancellation completes.** Existing best-effort path still runs: `abort_all` + `drop(state)` + exit 0.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A `CancellationToken` MUST be threaded into every phase boundary of `add_episode`. Concretely: every long-running async call (`embed`, `extract`, `is_duplicate`) is wrapped in `tokio::select!` against the token; synchronous phase transitions check `token.is_cancelled()` before starting. The DB commit phase (Phase D / code Phase C) is exempt from mid-phase cancellation — the `select!` wraps `write_lock.write().await` only, so cancellation prevents *starting* the commit but not interrupting it once the write guard is held.
- **FR-002**: Long-running async HTTP calls MUST respect cancellation by dropping the in-flight `reqwest` future via `tokio::select! { r = http_call => r?, _ = token.cancelled() => return Err(Cancelled) }`. Because the extractor, embedder, and dedup adapter calls are already `async` (not in `spawn_blocking`), dropping the future aborts the TCP connection client-side within one round-trip window (~100–500 ms).
- **FR-003**: Phase D (the DB commit, "Phase C" in code) MUST be allowed to finish if it has started — it's short and produces durable state. Cancellation between phases is the granularity.
- **FR-004**: When a per-chunk pipeline is cancelled mid-flight, the chunk's partial state MUST NOT be left torn in the DB or the application WAL. Either commit fully before yielding to cancellation, or yield before any mutation lands.
- **FR-005**: The inner shutdown timeout default MUST be reduced to 5 000 ms once cancellation is reliable. `LCG_SHUTDOWN_TIMEOUT_MS` override semantics unchanged.
- **FR-006**: Cancellation-vs-clean-drain MUST be observable via the closing `state: "stopped"` telemetry event in the `detail` field as `{"drained": N, "cancelled": M}`.
- **FR-007**: Existing tests pass unchanged. New tests cover: cancellation mid-HTTP-call, cancellation between phases, no-cancellation normal completion.

### Key Entities

- **CancellationToken**: Parent token stored on `AppState`, triggered at the top of the shutdown sequence. All in-flight `add_episode` calls hold a reference to the same parent token via `Arc<AppState>`.
- **Phase D / Phase C**: The DB-commit phase of the per-chunk processing pipeline. It is short (~200 ms) and produces durable state — it must run to completion once started even if cancellation fires during it. Called "Phase D" in this spec and "Phase C" in the code/ADR.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Cmd+Q on an actively-ingesting workspace returns control to the user within 5 s (down from 30 s today). Measured: time from SIGTERM emission in liminis-app to process exit event.
- **SC-002**: lbug `db.wal` is checkpointed cleanly across every such quit — no `Corrupted wal file` on next startup. (Same SC as #71; preserved.)
- **SC-003**: Telemetry's closing `state: "stopped"` event includes a `detail` object with `drained` and `cancelled` counts on every shutdown.
- **SC-004**: Default `shutdown_timeout_ms` is 5 000 ms; `LCG_SHUTDOWN_TIMEOUT_MS` override unchanged.
- **SC-005**: Existing tests (`ipc_parity`, `degraded_startup`, auto-heal, etc.) pass unchanged. New tests under `liminis-graph-core/tests/cancel_shutdown.rs` cover the three User Story 1 acceptance scenarios.
- **SC-006**: No regression in the clean-drain path: if no work is in flight, shutdown completes in well under 1 s (verifiable in the new integration test).

## Assumptions

- **A1.** `tokio_util::sync::CancellationToken` is acceptable as a new dependency. It's the standard tool; the alternative (homegrown AtomicBool with polling) is uglier and slower.
- **A2.** `reqwest`'s in-flight request cancellation via `tokio::select!` is reliable. Verified by `tokio` docs.
- **A3.** Phase D commits do not exceed ~200 ms in the field today. We tolerate FR-003's "let Phase D finish" because Phase D is bounded. If batch sizes grow substantially in the future, this assumption needs revisiting.
- **A4.** The indexing queue in liminis-app re-enqueues cancelled chunks on next startup (verified). Cancelling mid-chunk does not lose data — it loses time on that specific extraction, which is repeated.
- **A5.** Replacing `state.shutdown: Arc<AtomicBool>` with `CancellationToken` as the single source of shutdown truth is preferable to coexistence. `is_cancelled()` is sync, so blocking contexts (e.g., `cancel_fn` in `WalReplayer`) work without change.
- **A6.** liminis-graph#74 (P0 WAL wiring) will land before users notice partial-write concerns. Until then, the app WAL is empty so "torn WAL writes" is structurally impossible.

## Out of Scope

- Replacing `spawn_blocking` with fully async handling — it's the right pattern for the DB commit; we just need to check the token before acquiring the write guard.
- Changing the liminis-app side. The 60 s outer SIGTERM-to-SIGKILL budget stays.
- Streaming-request cancellation semantics for `knowledge_rebuild_from_wal` — already noted as a separate Open Question in #71's spec.
- Persisting partial chunk state across restarts ("resume mid-chunk"). Cancellation = "don't bother finishing this chunk"; the indexing queue re-enqueues on next startup.

## Source References

- **liminis-graph#71 (PR #72), merged 2026-05-24:** installed the signal-handler infrastructure this fix builds on.
- **Python service equivalent:** `graphiti_service.py` shutdown is `await service.stop_server() + await service.cleanup()`. Python's async-everywhere model gave it cancellation for free at every `await`; the Rust port's `spawn_blocking` for the DB commit reintroduced the gap.
- **liminis-graph#74 (P0 WAL wiring):** While orthogonal, this fix makes the partial-write story simpler — once WAL writes are wired, "cancelled mid-chunk" means "WAL never saw a partial mutation" because nothing was committed.
- `liminis-graph/src/main.rs` — shutdown sequence, signal handlers
- `liminis-graph-core/src/extractor.rs`, `embedder.rs`, `dedup_adapter.rs`, `episode.rs` — in-scope files for cancellation threading
