# Feature Specification: Autonomous WAL-Corruption Self-Recovery

**Feature Branch**: `fabrik/issue-151`
**Created**: 2026-06-18
**Status**: Draft
**Input**: User description: "Make liminis-graph autonomously recover from a corrupt/torn lbug WAL — detecting the condition on startup and self-healing to a complete, fully-indexed graph with no external orchestrator."

## Background

liminis-graph is increasingly used as a self-sustained service in contexts beyond the liminis Electron app. In those contexts there is no app to drive a multi-step recovery workflow, so the engine must detect WAL corruption and execute recovery entirely on its own.

Today, when lbug detects a torn WAL tail, the engine enters a `degraded` state and waits for an external caller to invoke `knowledge_recover`. Even when that call arrives, the existing `drop_lbug_wal` handler only reopens `liminis.db` at its last checkpoint — it does not resume replay from the checkpoint to end-of-WAL, and it does not rebuild FTS/HNSW indexes. The result is a graph that is silently incomplete and unsearchable.

A complete recovery sequence was validated by hand on 2026-06-18 against a real workspace whose reload was interrupted by a disk-full event (torn `liminis.db.wal`):

1. `drop_lbug_wal` — reopened `liminis.db` at its last lbug checkpoint (~4.4 s; renames torn WAL tail aside). The main DB file is intact — lbug only refuses to auto-truncate a torn WAL tail.
2. **Episode-cursor**: since there is no persisted WAL-replay seq cursor, the last ingested episode in the DB is used as a resume marker. Episodes are ingested in WAL order, so the last queryable episode's `seq` field is a safe (conservative) lower bound on how far the checkpoint got. Derivation: `retrieve_episodes(group, 1)` → last episode `uuid` → scan `.lcg/wal/*.jsonl` for that uuid → read `"seq": N`.
3. `rebuild_from_wal { from_seq: N }` — drops FTS, replays all WAL mutations at `seq ≥ N` onto the existing DB (overlap re-applies cleanly — 0 duplicate-PK failures observed), then rebuilds FTS + HNSW. Recovered the 1 missing episode; search restored.

**Result**: ~107 s vs an estimated ~7 h full re-replay — a 200× speedup by resuming from checkpoint instead of replaying from scratch.

**Why episode-cursor instead of persisting a seq cursor**: the resume point is derivable from data already in the DB, requires no additional instrumentation, and works retroactively on databases that were already crashed before this feature existed.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Service Self-Recovers from Torn WAL on Startup (Priority: P1)

A liminis-graph service is started against a workspace whose lbug WAL was torn (e.g., by a disk-full or OS kill mid-write). Without any external coordination, the engine detects the corrupt WAL, executes the full recovery sequence (checkpoint-drop → episode-cursor resume → reindex), and comes up **healthy** with all episodes present and FTS/vector search working. The operator does not need to know a recovery happened.

**Why this priority**: This is the core value of the feature. Operators running liminis-graph as a standalone service have no external orchestrator to call `knowledge_recover`, so a stuck-in-degraded startup is a hard outage.

**Independent Test**: Start the engine against a workspace with a synthetically torn `liminis.db.wal` (truncate or corrupt the WAL tail). Assert: (a) the engine does not expose a degraded status; (b) all episodes that were in the WAL are queryable; (c) FTS and HNSW vector search return results.

**Acceptance Scenarios**:

1. **Given** a workspace with a torn lbug WAL tail, **When** the engine starts, **Then** it detects the corrupt-WAL condition, executes `drop_lbug_wal` → episode-cursor derivation → `rebuild_from_wal { from_seq: N }` → `build_indices_and_constraints`, and reaches a healthy state — no degraded mode entered.
2. **Given** a workspace with a torn WAL where the main `liminis.db` is intact, **When** startup recovery completes, **Then** all WAL episodes (including those replayed in the resume pass) are queryable via `handle_get_episodes`, and FTS / HNSW searches return results.
3. **Given** a workspace with a torn WAL where `drop_lbug_wal` fails (main DB is also torn), **When** startup recovery runs, **Then** the engine falls back to a full `rebuild_from_workspace_wal` and still reaches a healthy state.
4. **Given** a clean workspace (no torn WAL), **When** the engine starts, **Then** startup recovery logic is not triggered; normal startup proceeds as before.

---

### User Story 2 — One IPC Call Triggers Full Recover→Resume→Reindex (Priority: P1)

An external caller (e.g., the liminis app, a management script, or a health-check loop) that detects a workspace is degraded can issue a single IPC command and receive back a fully recovered, fully indexed graph. The caller does not need to orchestrate multiple steps or know the internal recovery sequence.

**Why this priority**: Even in environments that do have an orchestrator, the multi-step nature of the current recovery (`drop_lbug_wal` → separate `rebuild_from_workspace_wal` call → separate index rebuild) is fragile. A single idempotent call is safer and simpler for all callers.

**Independent Test**: Start the engine in degraded mode (skip startup recovery, or inject degraded state). Issue a single IPC recovery command. Assert: the command returns a response that includes counts for each phase (episodes in DB before/after, mutations replayed, indexes built), and the engine is healthy after the call completes.

**Acceptance Scenarios**:

1. **Given** an engine in degraded state (torn WAL, no startup recovery), **When** the IPC recovery command is issued, **Then** it executes the full sequence and returns a structured response with: `episodes_before`, `mutations_replayed`, `episodes_after`, `indexes_rebuilt: true`.
2. **Given** the engine is already healthy (no torn WAL), **When** the IPC recovery command is issued, **Then** it is idempotent — it returns a response indicating no recovery was needed and does not corrupt or re-replay any data.
3. **Given** a recovery command is issued while a previous recovery is still running, **When** the second call arrives, **Then** it either waits for the first to complete (serialized) or returns a clear "recovery in progress" response — it does not start a second concurrent recovery.
4. **Given** the checkpoint-resume path fails (episode-cursor not derivable), **When** the IPC recovery command runs its fallback, **Then** it executes a full `rebuild_from_workspace_wal` and still returns a successful response upon completion.

---

### User Story 3 — Episode-Cursor Derivation Is Observable via Telemetry (Priority: P2)

An operator or developer can confirm — from logs or a status response — exactly what the engine did during recovery: which phase ran, what seq N was derived, how many mutations were replayed in the resume pass, and whether a fallback occurred.

**Why this priority**: Recovery is a rare, high-stakes event. Operators need to audit what happened and confirm the graph is complete. Silent recovery with no diagnostics would make it impossible to distinguish a full recovery from a partial one.

**Independent Test**: Run startup recovery against a known WAL. Inspect engine logs (or a status/telemetry response). Assert presence of: the detected corruption event, the derived `from_seq` value, mutations-replayed count for the resume pass, and the final episode count.

**Acceptance Scenarios**:

1. **Given** startup recovery runs, **When** each phase completes, **Then** the engine emits a structured log entry for: (a) WAL corruption detected, (b) `drop_lbug_wal` completed with elapsed time, (c) episode-cursor derived with `seq=N`, (d) resume replay completed with `mutations_replayed=M`, (e) index build completed, (f) recovery succeeded.
2. **Given** a fallback to full replay occurred, **When** recovery completes, **Then** the log entries indicate which fallback was triggered and why (e.g., "drop_lbug_wal failed: <reason>" or "episode-cursor: no episodes in DB, using from_seq=0").
3. **Given** the IPC recovery command completes, **When** the caller inspects the response, **Then** phase counts are present in the returned payload.

---

### User Story 4 — Episode-Cursor Fallback Handles No-Episode and No-WAL-Match Cases (Priority: P1)

When the episode-cursor derivation cannot produce a seq (the DB is empty, or the last episode's uuid does not appear in any WAL jsonl file), recovery falls back gracefully: `from_seq = 0` (full replay from beginning) rather than aborting.

**Why this priority**: The fallback logic is as important as the happy path for correctness. A failed episode-cursor must not leave the engine stuck or silently incomplete.

**Independent Test**: (a) Run recovery against a workspace where the DB has been fully cleared (no episodes). Assert recovery proceeds with `from_seq = 0` and completes successfully. (b) Run recovery against a workspace where the last episode uuid appears in no WAL file. Assert the same fallback behavior.

**Acceptance Scenarios**:

1. **Given** a DB with no episodes (freshly dropped), **When** episode-cursor derivation runs, **Then** `from_seq = 0` is used and the replay runs from the beginning of the WAL.
2. **Given** a DB whose last episode uuid is not found in any `.lcg/wal/*.jsonl` file, **When** episode-cursor derivation runs, **Then** `from_seq = 0` is used (conservative: never skip mutations).
3. **Given** `from_seq = 0` is used because no episode cursor was derivable, **When** replay completes, **Then** telemetry indicates the cursor fallback reason (no episodes / uuid-not-found) so the operator can distinguish it from a normal partial resume.

---

### Edge Cases

- **Torn main DB (drop_lbug_wal fails)**: If `drop_lbug_wal` fails because `liminis.db` itself is unreadable, the engine must fall back to full `rebuild_from_workspace_wal`. Recovery still completes (assuming WAL files are intact).
- **No WAL files present**: If `.lcg/wal/` contains no `.jsonl` files, replay has nothing to do. Recovery completes as a no-op after the checkpoint drop.
- **Episode uuid in multiple WAL files**: The first occurrence (lowest seq) must be used as the resume point if the same uuid appears in more than one WAL file (e.g., from a previously partial replay that logged the episode twice). Conservative: take the minimum seq found.
- **WAL files unreadable during uuid scan**: If one or more WAL jsonl files cannot be read during the uuid search, the unreadable files are skipped and a warning is emitted. If the target uuid is never found after scanning all readable files, use `from_seq = 0`.
- **Duplicate-PK mutations on resume**: Mutations at `seq ≥ N` that were already applied (because the checkpoint had advanced past N) re-apply cleanly (idempotent). This is a known property of the existing replay path and is relied upon here.
- **Concurrent startup**: The engine serializes startup; this is not a race condition concern. The IPC recovery command should serialize with any in-flight replay (no concurrent replays).
- **Recovery during an in-flight rebuild**: If a `rebuild_from_workspace_wal` is already running when startup recovery detects a torn WAL, behavior is undefined and out of scope — the engine should emit an error and not attempt to run two replays simultaneously.
- **Rebuilt index counts in response**: The existing `rebuild_result.indexes_created` field counts `CREATE INDEX` lines *in the WAL* (always 0 in practice), not the post-replay index build. The recovery response MUST report post-replay index build status separately (e.g., a boolean `indexes_rebuilt` flag) to avoid misleading callers.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: On startup, when the engine detects a corrupt/torn lbug WAL condition (the condition currently causing it to enter `degraded` state), it MUST automatically initiate the self-recovery sequence without waiting for an external IPC call.
- **FR-002**: The self-recovery sequence MUST be: (1) `drop_lbug_wal` to reopen `liminis.db` at its last lbug checkpoint; (2) episode-cursor derivation to determine `from_seq: N`; (3) `rebuild_from_wal { from_seq: N }` to replay mutations at seq ≥ N; (4) `build_indices_and_constraints` to rebuild FTS and HNSW indexes; (5) transition the engine to healthy state.
- **FR-003**: Episode-cursor derivation MUST: retrieve the last episode from the DB (ordering by ingestion — the last episode in WAL-ingestion order); scan all `.lcg/wal/*.jsonl` files for that episode's `uuid`; read the `"seq"` field from the matching line; use that value as `from_seq`.
- **FR-004**: `from_seq` derivation MUST be conservative: the derived seq N MUST be inclusive (replay starts at seq ≥ N, not > N), ensuring no mutations are skipped. Re-applying already-committed mutations is safe due to idempotency.
- **FR-005**: If `drop_lbug_wal` fails (main DB is also torn or unreadable), the recovery sequence MUST fall back to `rebuild_from_workspace_wal` (full replay from scratch).
- **FR-006**: If the episode-cursor cannot be derived (no episodes in DB, or last episode uuid not found in any WAL file), `from_seq` MUST default to 0 (replay from beginning).
- **FR-007**: The engine MUST expose a single IPC command that performs the complete recover→resume→reindex sequence. This command MUST be idempotent: calling it on a healthy engine MUST produce a no-op response with no data mutation.
- **FR-008**: The IPC recovery command response MUST include structured phase counts: number of episodes in DB before recovery, number of WAL mutations replayed in the resume pass, number of episodes in DB after recovery, and a boolean indicating whether indexes were rebuilt.
- **FR-009**: The engine MUST NOT enter `degraded` state when autonomous startup recovery succeeds. Degraded state is reserved for conditions where recovery itself fails (e.g., both WAL and main DB are unrecoverable).
- **FR-010**: The engine MUST emit structured log entries at each phase of recovery: corruption detected, checkpoint drop completed (with elapsed time), episode-cursor derived (with seq value), resume replay completed (with mutation count), index build completed, and recovery success/failure.
- **FR-011**: Startup recovery MUST NOT interfere with normal startup when no WAL corruption is detected. The existing startup path MUST be unchanged for healthy workspaces.
- **FR-012**: Integration tests MUST cover: (a) torn WAL → startup self-recovery → healthy engine with all episodes and working FTS/HNSW; (b) single IPC call triggers full recovery on a degraded engine; (c) episode-cursor derivation from last episode uuid; (d) fallback to `from_seq=0` when no episodes exist; (e) fallback to `from_seq=0` when uuid not found in WAL; (f) fallback to full `rebuild_from_workspace_wal` when `drop_lbug_wal` fails.

### Key Entities

- **Self-recovery sequence**: The ordered four-step process (checkpoint-drop → episode-cursor → resume-replay → reindex) that transitions the engine from corrupt-WAL to healthy.
- **Episode-cursor**: The mechanism for deriving `from_seq` from the last episode in the DB: last episode uuid → WAL file scan → seq value. A zero-seq fallback applies when derivation fails.
- **`from_seq`**: The inclusive WAL sequence number used as the resume starting point for `rebuild_from_wal`. All WAL mutations at seq ≥ `from_seq` are replayed.
- **IPC recovery command**: A new or extended IPC entrypoint that executes the full self-recovery sequence idempotently. Naming is deferred to the Plan stage; the requirement is one call, one response, no multi-step orchestration.
- **Degraded state**: The existing engine condition entered when lbug detects a torn WAL. After this feature, degraded state should only be entered if the recovery sequence itself fails.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A liminis-graph service started against a workspace with a torn lbug WAL tail reaches healthy state (all WAL episodes queryable, FTS and HNSW search returning results) with no external intervention.
- **SC-002**: Startup self-recovery completes without the engine ever reporting a degraded status to callers (assuming the main DB is intact).
- **SC-003**: A single IPC recovery command issued to a degraded engine produces a healthy engine with correct episode counts — `episodes_after` equals the total episode count in the WAL.
- **SC-004**: The IPC recovery command is idempotent: issuing it a second time on an already-healthy engine returns a success response with no data change and `mutations_replayed = 0`.
- **SC-005**: Episode-cursor derivation correctly identifies the resume seq: after recovery, the number of re-replayed mutations is bounded by the WAL content between `from_seq` and WAL end (not the entire WAL), demonstrating checkpoint-resume rather than full replay.
- **SC-006**: All fallback paths produce a healthy (not stuck/degraded) engine: `drop_lbug_wal` failure falls back to full rebuild; episode-cursor failure falls back to `from_seq=0`.
- **SC-007**: All pre-commit gates pass: `cargo fmt --all`, `cargo test`, `cargo clippy --release --all-targets -- -D warnings`.

## Assumptions

- **A1**: The existing `drop_lbug_wal` IPC handler correctly renames the torn WAL aside and reopens `liminis.db` at its last lbug checkpoint. The implementation reuses this existing primitive rather than reimplementing WAL drop.
- **A2**: Episodes are ingested in WAL seq order, so the last queryable episode's seq is a conservative (never-too-high) lower bound on how far the checkpoint replay got. This was confirmed in the hand-validated recovery session.
- **A3**: Re-applying WAL mutations at seq ≥ N where some of those mutations were already applied by the checkpoint is idempotent and produces 0 duplicate-PK failures. This was observed empirically in the hand-validated recovery.
- **A4**: `state.wal_dir` points to the `.lcg/wal/` directory containing the `.jsonl` WAL files. The episode-cursor scan reads these files directly (no IPC intermediary).
- **A5**: The "group" parameter for `retrieve_episodes` needed to find the last episode is determinable from engine state (e.g., the workspace group). The Research stage must confirm the correct group identifier to use here.
- **A6**: The engine serializes IPC request handling and does not run concurrent replays. The IPC recovery command does not need additional locking beyond what the engine already enforces.
- **A7**: WAL jsonl files contain a `"seq"` field on every line alongside the episode `"uuid"` field. The Research stage must confirm the exact field names and line format.
- **A8**: `indexes_created` in the existing `RebuildResult` reflects `CREATE INDEX` lines *in the WAL* (always 0 in practice), not the post-replay index build. The recovery response must track post-replay index builds separately.

## Out of Scope

- **Persisting a WAL replay seq cursor**: The episode-cursor approach is explicitly chosen over adding a persistent seq cursor. Adding a persistent cursor is a separate, future concern.
- **WAL corruption detection beyond lbug's own check**: This feature detects the corrupt-WAL condition using the same signal lbug already provides (the condition that currently causes degraded state). Deeper WAL integrity checking is not in scope.
- **Recovery from corrupt WAL *content* (semantically invalid mutations)**: This covers torn WAL *tails* (incomplete writes). Handling semantically invalid WAL mutations is covered by existing `failed_lines` / `fidelity_warning` logic and is not changed here.
- **Changing WAL write format or rotation behavior**: Only the startup and recovery read paths are changed.
- **Parallel recovery execution**: Recovery runs serially. Parallel replay is out of scope (see ADR-0047 HNSW serialization constraint).
- **Progress bar UI during recovery**: A progress indicator during long replays is complementary (see #135) but not part of this issue.

## Source References

- `liminis-graph-core/src/handlers.rs` — `knowledge_recover` (lines ~1863, ~1899), `handle_rebuild_from_wal` (lines ~1263–1295), `handle_get_episodes` (~546), startup/degraded detection
- `liminis-graph-core/src/replay.rs` — `ReplayOptions.from_seq`
- Issue #135 — replay progress bar (complementary)
- Issue #139 — batch WAL replay writes (complementary; a concurrent issue, same recovery path)
- ADR-0047 — HNSW index writes must stay serialized (constraint on parallelism)
