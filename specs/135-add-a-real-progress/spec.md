# Feature Specification: WAL Replay Real-Time Progress — Add `files_total` to `ReplayProgress`

**Feature Branch**: `fabrik/issue-135`
**Created**: 2026-06-16
**Status**: Draft
**Input**: User description: "Add a real progress bar for WAL replay/rebuild (files N/M, %, ETA) — backend already streams progress, needs files_total + UI render"

## Background

WAL replay (`knowledge_rebuild_from_wal` / `rebuild_from_workspace_wal`) can run a long time on real workspaces — a full rebuild observed at ~40+ min over 43,821 WAL files / ~2.5 M mutations. During that entire window the UI shows only a spinner ("Rebuilding…") with no sense of progress, percentage, or ETA. Users cannot tell whether the operation is working, stuck, or how long to wait.

The plumbing for progress reporting is already half-built:

- **Backend** produces progress via `ReplayProgress { files_processed, mutations_replayed, message }` (replay.rs), called once per file and once per 1 000 mutations. These events are streamed over IPC when `_progress_token` is set (`handle_rebuild_from_wal`, handlers.rs).
- **App main** already forwards these to the renderer: `contextGraph:rebuildFromWal` → `win.webContents.send('contextGraph:walRebuildProgress', progress)`.
- **App renderer** ignores them: `WalRecoveryToast` shows only a spinner and a final completion message.

The single missing piece in this repo is a **denominator**: `ReplayProgress` carries `files_processed` (numerator) but no `files_total`, so clients cannot compute a percentage. The `.jsonl` file list is already collected into a `Vec<PathBuf>` at replay start (before the processing loop), so `files.len()` is immediately available — the change is additive and low-risk.

The app-side rendering (subscribing to the already-forwarded events, drawing the bar, computing ETA) is a separate concern and is **not in scope for this repository**. The intent is to spawn a child issue in the liminis app repo (mirroring the pattern from #128 → verveguy/liminis#848) once the backend field lands.

The reliability-work in #128, #130, and #133 made WAL replay both more correct and slower (mutations now execute rather than failing fast). A 40-minute silent spinner is the new normal for a full rebuild, making this a user-facing regression in perceived quality.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Client can compute a meaningful percentage during replay (Priority: P1)

When a user triggers a "Rebuild from WAL" operation, the app main process receives progress events that include both numerator (`files_processed`) and denominator (`files_total`). This enables the app renderer to display "File 3,842 / 43,821 (8.8%)" rather than an indeterminate spinner.

**Why this priority**: This is the entire scope of the backend half of the feature. Without `files_total`, no percentage is computable and the UI improvement is blocked.

**Independent Test**: Start a replay against a WAL directory containing N `.jsonl` files. Capture every progress event emitted via `progress_fn`. Assert that every event carries `files_total == N`. Assert that `files_processed` advances from 1 to N across events. Assert `files_processed / files_total * 100` produces a monotonically increasing percentage from ~0% to 100%.

**Acceptance Scenarios**:

1. **Given** a WAL directory with N `.jsonl` files and a `progress_fn` registered, **When** replay starts, **Then** the very first progress event has `files_total == N` and `files_processed == 1`.
2. **Given** a WAL directory with N files, **When** replay completes without cancellation, **Then** the last progress event has `files_processed == N` and `files_total == N` (i.e. 100%).
3. **Given** a WAL directory with 0 `.jsonl` files, **When** replay runs, **Then** no progress events are emitted (current behavior — no files to iterate over) and the return value is a zero-stats `ReplayStats`.
4. **Given** replay is cancelled mid-run (cancel_fn fires), **When** the last progress event is inspected, **Then** `files_processed < files_total` and both fields are present and consistent.
5. **Given** the IPC handler `handle_rebuild_from_wal` streams progress events, **When** a progress event is read on the app side, **Then** the JSON payload includes `"files_total"` alongside the existing `"files_processed_so_far"` key.

---

### User Story 2 — Live data-quality counters visible during replay (Priority: P2, optional)

An operator performing a long recovery can see running `failed_lines` and `legacy_skipped_lines` counts increment in real time, rather than discovering data-quality issues only in the final summary. This lets them abort early if the failure rate looks wrong.

**Why this priority**: The issue author describes this as optional ("Optionally also surface…"). It is valuable but not blocking the core percentage display.

**Independent Test**: Start a replay against a WAL directory whose files contain a mix of valid mutations and invalid lines. Assert that the `ReplayProgress` events delivered during replay carry incrementing `failed_lines_so_far` and `legacy_skipped_lines_so_far` values that match the per-file running totals. Assert the final event's counters equal `ReplayStats::failed_lines` and `ReplayStats::legacy_skipped_lines`.

**Acceptance Scenarios**:

1. **Given** a WAL file containing 5 bad lines before the next file, **When** the per-file progress event fires after processing that file, **Then** `failed_lines_so_far` in the event is ≥ 5.
2. **Given** a clean WAL (no failures), **When** progress events are received, **Then** `failed_lines_so_far` is 0 in every event.
3. **Given** the IPC JSON, **When** a progress event is received, **Then** `failed_lines_so_far` and `legacy_skipped_lines_so_far` are present as numeric fields.

---

### Edge Cases

- **WAL dir contains no `.jsonl` files**: `files_total` is never relevant (loop does not execute); no progress events are emitted — this is unchanged behavior.
- **`replay()` (no-options variant) used by `recover_rebuild_from_workspace_wal`**: This path calls `replay()` (not `replay_opts`), so it has no `progress_fn` and no progress events — adding the field to `ReplayProgress` does not affect this path; it remains an explicit TODO (per the existing comment at handlers.rs:1880).
- **Multiple callers that construct `ReplayProgress` directly** (tests, handlers): Adding a new field requires updating all construction sites. Tests in `liminis-graph-core/tests/wal_replay.rs` and `handlers.rs` must be updated.
- **ETA computation**: Not in scope for this repo. The backend emits timestamps implicitly via event ordering; the app computes ETA from observed rate.
- **`from_seq` filter skips early files**: `files_processed` counts all iterated files (including those skipped due to `from_seq`), and `files_total` is the total JSONL file count — these semantics are consistent with the existing behavior and should not change.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `ReplayProgress` MUST gain a `files_total: u64` field representing the total number of `.jsonl` files in the WAL directory, computed once before the processing loop begins.
- **FR-002**: Every `ReplayProgress` event emitted by `replay_opts` via `progress_fn` MUST carry the correct `files_total` value (the same value in every event for a given replay run).
- **FR-003**: The `build_progress_fn` helper in `handlers.rs` MUST include `files_total` in the JSON object it sends over IPC, using the key `"files_total"` (matching the field name and consistent with the existing `"files_processed_so_far"` / `"mutations_replayed_so_far"` naming convention).
- **FR-004**: All existing construction sites of `ReplayProgress` (in `replay.rs`, `handlers.rs`, and any test files) MUST be updated to supply `files_total`. In `replay.rs` this is `files.len() as u64`; in test helpers that construct `ReplayProgress` directly it is whatever value the test supplies.
- **FR-005** *(P2 — optional)*: `ReplayProgress` MAY gain `failed_lines_so_far: u64` and `legacy_skipped_lines_so_far: u64` fields, populated from the running `stats` counters at the time each progress event fires. If added, they MUST be included in the IPC JSON under keys `"failed_lines_so_far"` and `"legacy_skipped_lines_so_far"`.
- **FR-006**: Pre-commit gates MUST pass: `cargo fmt --all && cargo test && cargo clippy --all-targets -- -D warnings`.

### Key Entities

- **`ReplayProgress`** (`liminis-graph-core/src/replay.rs`): Progress snapshot passed to the `progress_fn` callback. Currently: `files_processed: u64`, `mutations_replayed: u64`, `message: String`. After this change, also: `files_total: u64` (mandatory), plus optionally `failed_lines_so_far: u64` and `legacy_skipped_lines_so_far: u64` (FR-005).
- **`build_progress_fn`** (`liminis-graph-core/src/handlers.rs`): Converts a `ReplayProgress` reference into a JSON `Value` sent over the IPC progress channel. Requires updating to include the new field(s).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Every `ReplayProgress` event emitted during a replay of N files carries `files_total == N` (verified by test).
- **SC-002**: The IPC JSON payload for each `contextGraph:walRebuildProgress` event includes a numeric `"files_total"` field equal to the WAL file count.
- **SC-003**: All existing tests continue to pass; no new `ReplayProgress` construction site is left with a missing or incorrect `files_total`.
- **SC-004**: All pre-commit gates are green (`cargo fmt --all`, `cargo test`, `cargo clippy --all-targets -- -D warnings`).
- **SC-005** *(if FR-005 implemented)*: Running `failed_lines_so_far` in progress events matches the cumulative `ReplayStats::failed_lines` at replay completion.

## Assumptions

- **A1.** The `.jsonl` file list is already collected into a `Vec<PathBuf>` (sorted) before the processing loop in `replay_opts`. `files.len() as u64` is available without any additional I/O.
- **A2.** The app-side rendering (ETA, progress bar component, subscription to `contextGraph:walRebuildProgress`) is out of scope for this repository and will be addressed in a child issue against the liminis app repo.
- **A3.** `recover_rebuild_from_workspace_wal` (the `rebuild_from_workspace_wal` recovery path) calls `replay()` with no progress fn and is explicitly not wired for streaming — adding the field to `ReplayProgress` does not affect that path, and connecting it is a follow-up.
- **A4.** The liminis app already forwards `contextGraph:walRebuildProgress` events to the renderer (`context-graph-handlers.ts`). Adding `files_total` to the backend payload is sufficient for the app child issue to proceed.
- **A5.** `files_processed` semantics are unchanged: it counts iterated files (including those partially skipped by `from_seq`). `files_total` is the full `.jsonl` file count in the WAL dir.

## Out of Scope

- **App-side rendering**: subscribing to `contextGraph:walRebuildProgress`, displaying a progress bar, and computing ETA in the liminis renderer (`WalRecoveryToast`) — these belong in a child issue against the liminis app repo.
- **`recover_rebuild_from_workspace_wal` progress**: This path calls `replay()` (no options) and has no IPC channel for streaming; wiring it is a separate TODO (see handlers.rs comment at line ~1880).
- **Replay performance / throughput**: unchanged.
- **Cancellation UX**: a `cancel_fn` already exists; this issue is display-only.
- **ETA computation on the backend**: the app computes ETA from event timestamps; the backend only provides the raw counts.

## Source References

- `liminis-graph-core/src/replay.rs` — `ReplayProgress` struct, `replay_opts`, progress callback fire sites (lines ~97–101, ~159–172, ~318).
- `liminis-graph-core/src/handlers.rs` — `build_progress_fn` (line ~2009), `handle_rebuild_from_wal` (line ~1194), `recover_rebuild_from_workspace_wal` (line ~1850).
- `liminis-graph-core/tests/wal_replay.rs` — test construction sites of `ReplayProgress`.
- #128, #130, #133 — WAL replay fidelity fixes (the reason a full replay now runs long and progress feedback matters).
- verveguy/liminis#848 — prior cross-repo UI child issue pattern to replicate.
