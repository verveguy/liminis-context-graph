# Feature Specification: WAL File Rotation by Size and Entry Count

**Feature Branch**: `fabrik/issue-84`
**Created**: 2026-05-25
**Status**: Draft
**Input**: User observation 2026-05-25 — the application WAL at `<workspace>/.lcg/wal/` is the canonical, git-trackable backup of the knowledge graph. Without periodic rotation, the active file grows unboundedly and quickly hits GitHub's friction thresholds (50 MB soft warning, 100 MB hard reject, plus PR diffs become unreviewable past a few MB).

## Background

After liminis-graph#74 (P0 WAL wiring, merged 2026-05-25) the application WAL is now written on every mutation. Live evidence from demo-notebook after a fresh ~50-episode ingestion: `wal: {exists: true, file_count: 1, byte_size: 3933176}` — i.e. one ~4 MB file for a modest dataset.

Scaled up to a real long-running personal-knowledge workspace (months of accumulated mutations), the single active WAL file grows past:

- **50 MB** — GitHub web UI starts warning, search starts failing on the file
- **100 MB** — GitHub rejects the push (hard limit; requires git-lfs to override)
- **~10 MB** — practical limit where PR diffs become unreviewable; "what changed in the WAL this week" stops being a sensible question
- **Multi-MB single commits** — bloat repo packfiles, slow clones, eat developer time

The `WalWriter` (`liminis-graph-core/src/wal.rs:35-58`) already accepts a `max_events_per_file: usize` constructor parameter (passed `10_000` at `app_state.rs:82`) and exposes a `rotate()` method consumed by `knowledge_prepare_checkpoint`. But the per-event log path is not currently checked against `max_events_per_file` — rotation only happens on the explicit checkpoint call, which today is triggered manually (or not at all).

**Net effect:** the existing rotation infrastructure is half-wired. Add the per-event byte-size and event-count checks, wire them into the write path, verify that multi-file replay already works, and the WAL stays under GitHub's thresholds without user action.

The WAL is positioned as the canonical git-trackable backup ("commit your workspace, your knowledge graph rebuilds from the WAL on any machine"). That promise dies the first time a user can't push their workspace. An unbounded WAL file also makes PR diffs in workspace repos useless. Rotation gives small, reviewable per-period diffs and avoids retroactive cleanup pain from bloated packfiles.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — WAL Rotates Before Hitting GitHub's Soft Limit (Priority: P1)

When the active WAL file approaches a configurable byte threshold (default well below GitHub's 50 MB warning), the WalWriter MUST close the current file, open a new one with the next sequence number, and continue writing to the new file. The closed file becomes immutable and remains in the WAL directory.

**Why this priority**: This is the load-bearing feature. Without it the WAL silently grows past usable size and either GitHub rejects the push or the user discovers it themselves.

**Independent Test**: Configure a tempdir workspace with `LCG_WAL_MAX_BYTES_PER_FILE=1000000` (1 MB). Ingest enough chunks to write ~3 MB of mutations. Assert the WAL directory contains at least 3 files, none larger than ~1 MB (allowing one mutation's worth of overshoot), all with monotonically increasing sequence numbers in their filenames.

**Acceptance Scenarios**:

1. **Given** the active WAL file's size exceeds `max_bytes_per_file`, **When** the next mutation arrives, **Then** the current file is closed (with a final fsync), a new file is opened with the next sequence number, and the mutation is written to the new file.
2. **Given** rotation happened mid-chunk processing, **When** subsequent mutations for the same chunk arrive, **Then** they all land in the new file together (no chunk straddles two files).
3. **Given** the writer is initialized in a workspace with pre-existing rotated WAL files, **When** the writer starts, **Then** it determines the highest sequence number used and continues from `(N+1)`.

---

### User Story 2 — WAL Rotates by Entry Count as a Secondary Threshold (Priority: P2)

In addition to byte-size rotation, the WAL MUST rotate when the active file reaches `max_events_per_file` events (default 10,000), to bound replay time and keep per-file structure predictable for git diff review.

**Why this priority**: The existing `WalWriter::new` already accepts this knob. Wiring it to actually trigger rotation in the write path is small. Lower priority than byte-size because byte-size is the GitHub-facing constraint.

**Acceptance Scenarios**:

1. **Given** `max_events_per_file = 10000`, **When** the 10001st event is about to be written, **Then** rotation fires before the write.
2. **Given** both byte-size and event-count thresholds are configured, **When** either threshold is crossed, **Then** rotation fires (logical OR).

---

### User Story 3 — Rotation Is Configurable per Workspace (Priority: P3)

Both `max_bytes_per_file` and `max_events_per_file` thresholds MUST be configurable via env vars (`LCG_WAL_MAX_BYTES_PER_FILE`, `LCG_WAL_MAX_EVENTS_PER_FILE`), with sane defaults that keep files comfortably under GitHub's soft limit.

**Acceptance Scenarios**:

1. **Given** no env vars are set, **When** the writer initializes, **Then** defaults apply: `max_bytes_per_file = 5 MB`, `max_events_per_file = 10,000`.
2. **Given** `LCG_WAL_MAX_BYTES_PER_FILE=10485760` (10 MB) is set, **When** the writer initializes, **Then** rotation uses the larger threshold.
3. **Given** an invalid env-var value (e.g., `LCG_WAL_MAX_BYTES_PER_FILE=foo`), **When** the writer initializes, **Then** the default applies and a warn-level log notes the bad value.

---

### User Story 4 — Replay Handles Multiple WAL Files in Sequence (Priority: P1)

The existing WAL replay path (`knowledge_rebuild_from_wal`) MUST iterate WAL files in sequence-number order and replay each in turn, producing the same end state as a single-file WAL would.

**Why this priority**: Rotation is useless if replay can't read multiple files. If the current replay logic picks only the latest file, it silently drops history — making this a correctness bug.

**Acceptance Scenarios**:

1. **Given** a workspace with 5 rotated WAL files, **When** `knowledge_rebuild_from_wal` runs against an empty DB, **Then** all events from all 5 files are replayed in chronological order.
2. **Given** a workspace where one rotated file is malformed (truncated, corrupt), **When** replay runs, **Then** replay reports the error for that file and continues with the remaining files (best-effort), exposing the count via the progress channel.

---

### Edge Cases

- **Single mutation exceeds `max_bytes_per_file`**: Write it anyway (the file becomes oversized for that single line); rotation kicks in on the next mutation. Log at warn level.
- **`max_bytes_per_file = 0`**: Treat as "no byte limit"; rely on event count only.
- **Very small threshold** (e.g., 1 KB): Rotation churns per-mutation. Allow it; document that very small thresholds produce many small files.
- **First mutation after a service restart**: The existing init path opens the first file (`scan_max_seq + 1`). Rotation logic only applies to subsequent rolls.
- **Rotation triggered while a chunk is mid-flight** (per FR-006): Buffer rotation until all chunk mutations are in the current file; then rotate before the next chunk starts.
- **Filesystem error on close** (e.g., out of space) during rotation: Propagate the error; the current mutation fails. Service continues, retry on next mutation.
- **Disk fsync fails on rotation** (rare): Log at error level; do not silently swallow. The closed file may be incomplete; replay should skip it gracefully.
- **A WAL file is manually deleted between rotations**: `scan_max_seq` finds the highest remaining number; new file opens with `max + 1` (may skip the deleted file's number). Replay handles gaps gracefully.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `WalWriter::log_mutation` MUST check the active file's byte size before writing and rotate if writing would cause `max_bytes_per_file` to be exceeded (except when no bytes limit is set, i.e., `max_bytes_per_file = 0`).
- **FR-002**: `WalWriter::log_mutation` MUST check the active file's event count before writing and rotate if it would exceed `max_events_per_file`.
- **FR-003**: Rotation MUST close the current file atomically (final fsync) before opening the next, so a rotated file is durable on disk before any further writes occur.
- **FR-004**: Sequence numbers in filenames MUST be zero-padded and monotonically increasing (e.g., `wal-000000.jsonl`, `wal-000001.jsonl`) so lexicographic sort equals chronological order. Existing filename scheme must be preserved if already compatible; otherwise migrated.
- **FR-005**: On startup, `WalWriter::new` MUST scan the WAL directory for existing files and resume from `max(existing_sequence) + 1` (the existing `scan_max_seq` function may already do this; verify and fix if not).
- **FR-006**: Chunk boundaries MUST be respected: rotation MUST NOT happen mid-chunk. If a chunk is in progress, rotation defers until the chunk's mutations have all been flushed, then fires at the chunk boundary.
- **FR-007**: Rotation events MUST be observable via telemetry — a `wal_rotated` event with `from_seq`, `to_seq`, `closed_bytes`, and `closed_events` fields.
- **FR-008**: `knowledge_rebuild_from_wal` MUST iterate WAL files in sequence-number order and replay each in turn. (Verify whether already true; fix if not.)
- **FR-009**: A workspace whose WAL is committed to git MUST produce git-reviewable per-period diffs — a typical week's worth of mutations must fall under the GitHub PR-diff usability threshold (≤ ~1 MB per file with defaults).
- **FR-010**: Defaults: `max_bytes_per_file = 5 * 1024 * 1024` (5 MB), `max_events_per_file = 10_000`. Both env-overridable via `LCG_WAL_MAX_BYTES_PER_FILE` and `LCG_WAL_MAX_EVENTS_PER_FILE`.

### Key Entities

- **WAL file** (`.jsonl`): A single append-only log file in `{workspace_root}/.lcg/wal/`. Files are named `wal-NNNNNN.jsonl` with zero-padded six-digit sequence numbers.
- **Rotation threshold**: A configurable limit (bytes or events) that triggers closing the current WAL file and opening a new one with the next sequence number.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With defaults, ingesting 10,000+ mutations against a tempdir workspace produces multiple WAL files, none exceeding 5 MB.
- **SC-002**: A workspace whose `.lcg/wal/` is committed to a GitHub repo can be pushed and pulled without GitHub size warnings.
- **SC-003**: `knowledge_rebuild_from_wal` against a multi-file WAL produces the same end state as a `knowledge_rebuild_from_wal` against a hypothetical single-file equivalent (verified by entity/relationship counts and a deterministic hash).
- **SC-004**: Rotation respects chunk boundaries — no chunk's mutations span two files (verified by inspecting JSONL chunk-id sequences in test output).
- **SC-005**: Env-var overrides work and take precedence over defaults.
- **SC-006**: Telemetry includes a `wal_rotated` event on each rotation, with file sizes and counts captured at rotation time.
- **SC-007**: New tests pass; existing tests pass unchanged.

## Assumptions

- **A1**: `WalWriter::new` already takes `max_events_per_file` and `scan_max_seq` already exists (verified by code read 2026-05-24).
- **A2**: GitHub's relevant thresholds: 50 MB per-file soft warning, 100 MB hard reject.
- **A3**: 5 MB default keeps a single file at ~10% of GitHub's soft limit — comfortable headroom for one-off oversized lines.
- **A4**: Workspaces commit WAL files alongside source markdown in the workspace repo. The user's expectation is that PR diffs of WAL changes are reviewable.
- **A5**: Multi-file replay is straightforward (iterate directory, sort by sequence, replay each in turn). If the existing code only loads the latest file, that's a correctness bug this issue fixes.
- **A6**: Chunk boundaries are detectable in the writer via the existing `with_chunk` API from liminis-graph#74's WAL wiring. If they aren't, that's a prerequisite fix.

## Out of Scope

- Compaction / merging of rotated WAL files (separate future optimization).
- Archiving / cold-storage of old WAL files outside the workspace dir.
- Git-LFS integration.
- Changing the WAL line format (JSONL stays).
- Backfilling sequence-numbering for workspaces with pre-existing non-numbered WAL files.
- A "max files" limit that deletes old rotated files; the WAL is retained forever as backup.

## Source References

- `liminis-graph-core/src/wal.rs:35-58` — `WalWriter` (constructor, `rotate()`, `log_mutation`)
- `liminis-graph-core/src/wal.rs:60` — `scan_max_seq`
- `liminis-graph-core/src/app_state.rs:80-82` — `WalWriter::new` call site (where size threshold needs to be plumbed in)
- `liminis-graph-core/src/replay.rs` — `WalReplayer` (multi-file replay path to verify/fix)
- Issue #74 — P0 WAL wiring (merged 2026-05-25, this issue's direct predecessor)
- Issue #29 — Tier 2 WAL admin (`prepare_checkpoint` / `rebuild_from_wal`) — adjacent lifecycle work
- Issue #73 — `WalWriter` reset in `clear_all` — adjacent lifecycle work
