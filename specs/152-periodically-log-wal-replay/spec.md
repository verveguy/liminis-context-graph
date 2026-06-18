# Feature Specification: Periodically Log WAL-Replay Progress to the Service Log

**Feature Branch**: `fabrik/issue-152`
**Created**: 2026-06-18
**Status**: Draft
**Input**: User description: "Periodically log WAL-replay progress to the service log so there is a durable record of how far a replay/recovery got."

## Background

WAL replay (`replay_opts` via `knowledge_rebuild_from_wal` / `rebuild_from_workspace_wal`) can run for
40+ minutes on large workspaces (observed: ~43,821 WAL files / ~2.5 M mutations). During that entire
window the service log is nearly silent on clean runs: `replay.rs` only writes to stderr on errors
(`[WAL SKIP]` / `[WAL WARN]`). A clean replay produces almost no log output.

Progress during the replay is delivered exclusively via the `progress_fn` callback → IPC
`type:"progress"` notification → UI progress bar. That channel is ephemeral: it exists only for the
duration of the IPC connection. When the connection is lost (app navigate-away, crash, disk-full
event, kill -9), all in-flight progress information is gone. After a restart there is no way to
reconstruct how far the replay got — during a recent disk-full interruption the only signal available
was live `lsof` on the open `.jsonl` file descriptor, which disappears after the process exits.

The data needed for a useful log line is already computed at the two `progress_fn` call sites in
`replay.rs` (lines ~176 and ~318): `files_processed`, `files_total`, `mutations_replayed`,
`failed_lines_so_far`, and the current file path (in `ReplayProgress.message`). Emitting a throttled
log line from those sites requires no struct changes and no new data collection.

This work pairs with liminis app #863 (UI loses progress on navigate-away) — a logged trail in the
service log is the durable backstop that remains after any client-side disruption.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Post-Crash Diagnosis (Priority: P1)

An operator restarts the service after a mid-replay crash (disk-full, OOM kill, power loss). They
`grep '[WAL PROGRESS]' service.log` and find the last checkpoint line, which tells them which file
was being processed and how many mutations had completed at roughly the time of the interruption.
They can then decide whether to re-run the replay from the beginning or accept partial state.

**Why this priority**: This is the core motivation from the issue. Without it, post-crash diagnosis
requires guesswork. The IPC/UI channel provides no help here — it is gone after restart.

**Independent Test**: Start a replay against a WAL directory containing N `.jsonl` files. Abort the
process mid-run (simulating a crash). Inspect stderr: at least one `[WAL PROGRESS]` line MUST be
present. That line's `files_processed` value MUST be less than N and consistent with which file was
being processed near the time of abort.

**Acceptance Scenarios**:

1. **Given** a WAL replay running for longer than the throttle interval, **When** the process is
   killed, **Then** the service log contains ≥ 1 `[WAL PROGRESS]` line whose `files_processed` and
   `mutations_replayed` values are consistent with actual replay progress up to that point.
2. **Given** the last `[WAL PROGRESS]` line in the log, **When** compared with the
   `wal_replay_complete` telemetry line (if one exists) or with re-inspection of the WAL directory,
   **Then** the progress values are accurate — not stale by more than one throttle interval.
3. **Given** a clean replay (no errors), **When** the replay completes, **Then** the service log
   contains at least one `[WAL PROGRESS]` line (unless the entire replay finishes in under one
   throttle interval — see Edge Cases).

---

### User Story 2 — Live Progress Monitoring (Priority: P2)

An operator tailing the service log (`tail -f`) during a long replay sees periodic `[WAL PROGRESS]`
lines appear at regular intervals, giving them a sense of pace and estimated time remaining without
needing the UI.

**Why this priority**: Valuable for ops workflows, but the UI already covers this case for connected
clients. The P1 case (crash diagnosis) has no other backstop.

**Independent Test**: Start a replay and tail stderr. Assert that `[WAL PROGRESS]` lines appear at
approximately the configured interval. Assert that successive lines show monotonically increasing
`files_processed` and `mutations_replayed` values.

**Acceptance Scenarios**:

1. **Given** a replay running for 2× the throttle interval, **When** stderr is captured, **Then**
   at least 2 `[WAL PROGRESS]` lines are present.
2. **Given** successive `[WAL PROGRESS]` lines, **When** compared, **Then** `files_processed` and
   `mutations_replayed` values are non-decreasing.
3. **Given** a replay on a fast SSD with thousands of files that completes in < 1 throttle interval,
   **When** the replay finishes, **Then** 0 or 1 `[WAL PROGRESS]` lines are present — the log is
   not flooded.

---

### Edge Cases

- **Short replay (finishes before first throttle window)**: 0 progress lines is acceptable. The
  existing `wal_replay_complete` telemetry event already covers the completion case.
- **WAL directory with 0 `.jsonl` files**: No progress events are emitted (loop does not execute) —
  unchanged behavior.
- **Throttle on a per-1000-mutation cadence**: The progress_fn is called once per 1000 mutations
  within a file. On a single very large WAL file the per-file trigger fires only once; the
  per-1000-mutation trigger fires many times. Throttling MUST apply to both call sites so that a
  single large file doesn't produce one log line per thousand mutations.
- **`replay()` (no-options variant)**: Called by the background workspace WAL recovery path (no IPC
  channel). Progress logging is **out of scope** for this path; it is not wired for options.
- **Concurrent replay runs**: Each call to `replay_opts` is independent; throttle state is local to
  the call (not global). Two concurrent replays each produce their own progress trail.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `replay_opts` MUST emit a progress log line to the service log (stderr) at regular
  intervals during replay, throttled to prevent flooding.
- **FR-002**: Each progress log line MUST include at minimum: `files_processed`, `files_total`,
  `mutations_replayed`, `failed_lines_so_far`, and an indication of the current file (basename or
  full path is acceptable).
- **FR-003**: Progress log lines MUST use a grep-able prefix consistent with the existing
  `[WAL WARN]` / `[WAL SKIP]` convention — specifically `[WAL PROGRESS]` — so operators can
  isolate them with a single grep.
- **FR-004**: Throttling MUST be applied so that a 43,000-file replay does not produce one log line
  per file or per 1000 mutations. The default throttle MUST prevent flooding while still producing a
  meaningful trail on a 40-minute replay (≥ 1 line per configured interval during active replay).
- **FR-005**: The throttle interval MUST be configurable via an `LCG_*`-prefixed environment
  variable (consistent with project naming: e.g., `LCG_REPLAY_LOG_INTERVAL_SECS` or
  `LCG_REPLAY_LOG_INTERVAL_FILES`). The default MUST be a sensible value that prevents flooding
  (e.g., 30 seconds or every N files where N is large enough to emit at most a few lines per minute
  on typical hardware).
- **FR-006**: Log emission MUST NOT require changes to the `ReplayProgress` struct — all required
  data is already present in the struct and in local variables at the call sites.
- **FR-007**: Existing `[WAL WARN]` / `[WAL SKIP]` error lines and the `wal_replay_complete`
  telemetry event MUST be preserved unchanged.
- **FR-008**: Log emission MUST NOT affect the return value of `replay_opts`, the behavior of the
  `progress_fn` callback, or the IPC progress notification channel.
- **FR-009**: Pre-commit gates MUST pass: `cargo fmt --all && cargo test && cargo clippy
  --all-targets -- -D warnings`.

### Key Entities

- **`ReplayProgress`** (`liminis-graph-core/src/replay.rs`): Progress snapshot passed to
  `progress_fn`. Already contains all required log fields: `files_processed`, `files_total`,
  `mutations_replayed`, `failed_lines_so_far`, `legacy_skipped_lines_so_far`, `message` (message
  includes the current file path, e.g., `"processing file /path/to/file.jsonl"`). **No struct
  change required.**
- **Throttle state**: New per-replay local state (time of last log emission, file count at last
  emission, or both) that gates whether a given `progress_fn` call site also writes to stderr. Scoped
  to a single `replay_opts` invocation; not shared across calls or stored globally.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A replay that runs longer than the configured throttle interval produces ≥ 1 line
  matching `[WAL PROGRESS]` in stderr output, verified by a test or integration check.
- **SC-002**: The last `[WAL PROGRESS]` line written before a simulated abort accurately reflects
  progress within one throttle interval of the abort point (i.e., `files_processed` is within the
  last throttle window's worth of files, not stale from the beginning of the replay).
- **SC-003**: A replay of 1,000 small files that completes in under 1 second produces 0 or 1
  `[WAL PROGRESS]` lines — demonstrating the anti-flood throttle works on fast runs.
- **SC-004**: All existing tests continue to pass; no regression in `[WAL WARN]` / `[WAL SKIP]`
  emission or `wal_replay_complete` telemetry.
- **SC-005**: Pre-commit gates are green: `cargo fmt --all`, `cargo test`, `cargo clippy
  --all-targets -- -D warnings`.

## Assumptions

- **A1**: The service log is stderr, captured by `graphiti_service.py`. Both `eprintln!` and the
  `StderrSink` telemetry path write to stderr. The implementation may use either mechanism; the spec
  does not prescribe which.
- **A2**: `ReplayProgress.message` already encodes the current file path at both call sites
  (`"processing file <path>"` and `"replayed N mutations in file <path>"`). Extracting the file
  from the existing `message` field or from a local `file_path` variable are both acceptable.
- **A3**: Rate (files/sec, mutations/sec) is a desirable but optional addition. Including elapsed
  time or rate in the log line requires tracking `Instant::now()` at replay start — the implementer
  decides whether to include it.
- **A4**: The throttle is per-`replay_opts` invocation, not global. If two replays run concurrently
  (unlikely but possible), each has independent throttle state and produces its own log trail.
- **A5**: The `replay()` (no-options) variant used by `recover_rebuild_from_workspace_wal` is
  explicitly out of scope; wiring it for progress logging is a follow-up issue.

## Out of Scope

- **`replay()` no-options path**: `recover_rebuild_from_workspace_wal` calls `replay()` which has
  no options struct and no IPC channel; wiring it is a separate issue.
- **IPC / UI changes**: `build_progress_fn` and the `type:"progress"` IPC notification format are
  unchanged. No app-side changes.
- **`WalReplayProgress` telemetry event**: The issue does not require a new structured telemetry
  variant — a plain log line is sufficient. Whether to emit via `eprintln!` or a new telemetry
  variant is an implementation decision for the Plan stage.
- **`ReplayProgress` struct additions**: All required fields already exist. No struct changes are
  needed.
- **ETA computation on the backend**: Rate/ETA, if included, is best-effort and informational only.
- **Changes to Python service wrapper, app UI, or IPC protocol**.

## Source References

- `liminis-graph-core/src/replay.rs` — `ReplayProgress` struct (lines ~100–108), `replay_opts`,
  progress_fn call sites (~175–188 per-file, ~317–335 per-1000-mutations).
- `liminis-graph-core/src/handlers.rs` — `build_progress_fn` (~2025), `handle_rebuild_from_wal`
  (~1275).
- `liminis-graph/src/main.rs` — `StderrSink` setup; `eprintln!` and telemetry both route to stderr.
- `liminis-graph/src/sink.rs` — `StderrSink` implementation.
- #135 — Added `files_total`, `failed_lines_so_far`, `legacy_skipped_lines_so_far` to
  `ReplayProgress`; confirms all fields are available.
- Liminis app #863 — UI loses progress on navigate-away; this issue provides the durable backstop.
