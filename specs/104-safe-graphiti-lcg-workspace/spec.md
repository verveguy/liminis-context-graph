# Feature Specification: Safe `.graphiti/` → `.lcg/` Workspace Migration That Restructures File Layout

**Feature Branch**: `fabrik/issue-104`
**Created**: 2026-05-26
**Status**: Draft
**Input**: User report 2026-05-26 — after the directory-schism fix (liminis#828) landed and liminis-app stopped pinning paths to the legacy `.graphiti/` namespace, the Rust binary's auto-migration was meant to rename `.graphiti/` → `.lcg/`. Migration runs cleanly but the resulting `.lcg/` still has the legacy file layout (e.g. `.lcg/db` as a file rather than `.lcg/db/liminis.db`). The binary's own defaults then conflict with the migrated layout (`create_dir_all(".lcg/db")` hits the existing file → EEXIST → crash loop). Today's only recovery is "rm -rf and re-ingest", which is unacceptable for production workspaces with real data.

## Background

The Graphiti → Liminis Context Graph rename is complete across user-facing names, env vars, repo references, and binary names. The remaining gap is the *workspace data layout*: legacy and new conventions don't match, and the auto-migration in `liminis-graph/src/main.rs` (currently lines 119-145) doesn't bridge them.

**Legacy layout** (produced when liminis-app passed `GRAPHITI_DB_PATH=<workspace>/.graphiti/db` etc.):

```
<workspace>/.graphiti/
  db                  (FILE — lbug Database)
  db.wal              (FILE — lbug internal WAL)
  wal/                (DIR — application WAL JSONL files)
  service.sock        (SOCKET — transient)
  ontology.yaml       (FILE — user-authored ontology)
  ontology-hash.json  (FILE — drift-detection persisted hash from liminis-graph#98)
```

**New layout** (binary defaults after liminis#828 stopped passing legacy env vars):

```
<workspace>/.lcg/
  db/                 (DIR — contains the DB file)
    liminis.db        (FILE — lbug Database, the former .graphiti/db)
    liminis.db.wal    (FILE — lbug internal WAL, the former .graphiti/db.wal)
  wal/                (DIR — application WAL — same structure)
  service.sock        (SOCKET — transient, regenerated each session)
  ontology.yaml       (FILE — same structure)
  ontology-hash.json  (FILE — same structure)
```

Today's migration just calls `std::fs::rename(".graphiti", ".lcg")`. The result is `.lcg/` with the legacy file layout still inside it. The binary then tries to use its new defaults against the legacy layout and crashes.

**Live evidence (demo-notebook 2026-05-26):**

1. Auto-migration fires: `.graphiti/db` (38 MB file) becomes `.lcg/db` (38 MB file).
2. Binary starts: tries `create_dir_all(".lcg/db")` to host the new `liminis.db` file.
3. Fails with `Os { code: 17, kind: AlreadyExists, message: "File exists" }`.
4. Crash-loops 3× and gives up. Knowledge graph unavailable.

For a development workspace the workaround is `rm .lcg/db` and re-ingest. For a production workspace with substantial graph data and significant LLM-extraction cost, that's not acceptable.

**Why this matters:**

- **Production workspaces.** Real users (specifically: the user's production machine, plus any future OSS users with established graphs) cannot tolerate data loss. The migration MUST preserve every byte and every file.
- **One-time event.** Migration runs once per workspace, never again. Getting it right matters because there's no second chance and no obvious recovery path if it goes wrong.
- **Crash-loop UX.** Today the migration fails silently from the user's perspective — the service crash-loops, the knowledge graph is unavailable, and the only signal is buried in service logs. Even a sophisticated user has to know to read `lastStderr` to diagnose.
- **OSS launch risk.** When liminis-graph ships externally, the first thing many users will do is upgrade. A broken migration kills first impressions.
- **Compounding risk over time.** Each release that ships with this broken migration creates more frustrated users. Fixing it once cleanly is much cheaper than handling each affected user manually.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Safe Migration With Full Data Preservation (Priority: P1)

When a workspace has only legacy `.graphiti/` (no `.lcg/`), the binary MUST migrate to `.lcg/` while *restructuring* the file layout to match the new conventions. All data (DB, both WALs, ontology, hash file) MUST be preserved. The binary MUST start cleanly post-migration.

**Why this priority**: this is the load-bearing user value. Migration without data loss is the entire reason this exists.

**Independent Test**: Take a workspace with a populated legacy `.graphiti/` layout (db file + db.wal file + wal/ dir + ontology.yaml + ontology-hash.json). Start the binary. Assert: (a) `.graphiti/` is gone; (b) `.lcg/` exists with the new layout (`db/liminis.db`, `db/liminis.db.wal`, `wal/`, `ontology.yaml`, `ontology-hash.json`); (c) `Db::open()` against the migrated DB succeeds; (d) entity/edge/episode counts match the pre-migration state.

**Acceptance Scenarios**:

1. **Given** a workspace with `.graphiti/db` (file), `.graphiti/db.wal`, `.graphiti/wal/` (dir with JSONL files), `.graphiti/ontology.yaml`, `.graphiti/ontology-hash.json`, **When** the binary starts, **Then** all five are moved to their new paths under `.lcg/` with the restructured layout.
2. **Given** a successful migration, **When** the binary continues startup, **Then** it opens the migrated DB and serves IPC requests normally — the workspace is fully functional with all its data.
3. **Given** entity count N pre-migration, **When** a `knowledge_status` is queried post-migration, **Then** entity count is still N. Same for relationships, episodes, WAL file count.

---

### User Story 2 — Atomic / Safe-to-Abort Migration (Priority: P1)

The migration MUST be safe to abort at any point. If the process is killed mid-migration (power loss, OOM, SIGKILL), the next start MUST either complete the migration or detect the partial state and refuse to proceed (rather than silently corrupting data).

**Why this priority**: partial migrations corrupting data are the single worst possible failure mode. Worse than crash-loops, worse than "won't start".

**Acceptance Scenarios**:

1. **Given** the migration is interrupted after moving `.graphiti/db` but before moving `.graphiti/wal/`, **When** the binary next starts, **Then** it detects the partial state (both `.graphiti/wal/` and `.lcg/db/liminis.db` exist) and either: (a) completes the remaining moves, OR (b) refuses to start with a clear actionable error message.
2. **Given** the migration is interrupted before the final cleanup (removing `.graphiti/`), **When** the binary next starts, **Then** it detects that the move steps are complete, removes the empty `.graphiti/`, and proceeds normally.
3. **Given** migration ever fails to move a specific file (e.g. permissions, disk full), **When** the failure occurs, **Then** the binary aborts the migration WITHOUT partial-deleting anything; the workspace is left in a recoverable state and a clear error is logged.

---

### User Story 3 — Migration Status Is Observable (Priority: P1)

The migration MUST log clearly at every step — at start, per-file-moved, on success, on failure. Failures MUST surface to liminis-app's UI / telemetry, not be buried in service stderr.

**Why this priority**: silent failures kill trust. Today's UX is "service crash-loops, user has no idea why". Must be observably better.

**Acceptance Scenarios**:

1. **Given** a workspace eligible for migration, **When** migration starts, **Then** a structured log line is emitted: `migration_started: from .graphiti/, to .lcg/, files_to_migrate: N, bytes_to_migrate: M`.
2. **Given** the migration proceeds, **When** each file moves, **Then** a structured log line is emitted: `migration_step: <file>, status: moved`.
3. **Given** migration completes, **When** the binary continues startup, **Then** a structured log line: `migration_complete: <file_count> files, <bytes> bytes, duration_ms: N`.
4. **Given** migration fails, **When** the failure occurs, **Then** a structured log line with the failing file, the OS error, and remediation guidance; the binary also surfaces this via the IPC startup-degraded path so liminis-app can show it in the UI.

---

### User Story 4 — Idempotency (Priority: P2)

If migration has already completed (only `.lcg/` exists with the new layout), the binary MUST detect this and skip migration entirely — no operation, no log noise.

**Acceptance Scenarios**:

1. **Given** a workspace with only `.lcg/` in the new layout, **When** the binary starts, **Then** no migration runs and no migration logs are emitted.
2. **Given** repeated restarts, **When** the migration check runs each time, **Then** only the first-ever start emits migration logs.

---

### User Story 5 — Concurrent `.lcg/` Detection (Priority: P2)

If both `.graphiti/` and `.lcg/` exist (the "schism" state), the binary MUST NOT auto-migrate. Instead, it MUST log a clear error and refuse to start, asking the user to manually resolve which is canonical.

**Why this priority**: blindly proceeding with both directories present could silently destroy whichever the user actually wanted to keep. Refusal-with-guidance is the only safe behavior.

**Acceptance Scenarios**:

1. **Given** a workspace with both `.graphiti/` and `.lcg/`, **When** the binary starts, **Then** it refuses to migrate, logs a clear error, and exits non-zero. liminis-app surfaces this to the user with cleanup guidance (move one aside, delete the other, etc.).
2. **Given** the user has resolved the schism (only one of `.graphiti/` or `.lcg/` remains), **When** they restart, **Then** the binary proceeds normally per User Story 1 or 4.

---

### User Story 6 — Backup Before Destruction (Priority: P3)

Before removing the source `.graphiti/` directory at the end of a successful migration, the binary SHOULD log the exact removal command and offer a configurable "keep `.graphiti.bak/`" mode for the paranoid.

**Why this priority**: nice safety net. The migration should already be safe (User Story 2), but for the truly cautious, having an explicit backup option is valuable.

**Acceptance Scenarios**:

1. **Given** `LCG_MIGRATION_KEEP_BACKUP=1` env var, **When** migration succeeds, **Then** `.graphiti/` is renamed to `.graphiti.bak/` instead of being deleted. The bak directory can be deleted by the user once they're confident.
2. **Given** the default (no env var), **When** migration succeeds, **Then** `.graphiti/` is deleted normally.

### Edge Cases

- **`.graphiti/` exists but is empty** (no db, no wal, no ontology). Migration is trivially complete: just create `.lcg/` (empty), delete `.graphiti/`.
- **`.graphiti/db` exists but is corrupted / can't be opened by lbug.** FR-005's validation step catches this; abort migration, leave source intact, surface error to user. Recovery via degraded-mode IPC (already implemented in liminis-graph#55).
- **`.graphiti/wal/` is large** (hundreds of MB). Move via `rename` (same filesystem) is constant-time; if cross-filesystem, requires copy+delete. v1 = assume same filesystem (always true for `.graphiti/` → `.lcg/` in the same workspace). Cross-filesystem detection + copy fallback is a future enhancement.
- **The user has an in-flight `liminis-app` writing to `.graphiti/` while migration runs.** Shouldn't happen — liminis-app spawns the binary; the binary then migrates before any client connections are accepted. Race window is between spawn and listen, no client has access yet. If somehow other processes have file handles, the rename fails and migration aborts safely.
- **Filesystem doesn't support atomic renames across directories** (Windows, FAT). Out of scope — liminis-graph targets macOS + Linux primarily. If/when Windows becomes a target, this edge case becomes relevant.
- **Disk full during migration.** Move operations fail with `ENOSPC`; FR-005's validation step or the move itself catches it; abort, leave source intact.
- **User has manually created `.lcg/` with custom contents** that aren't from a prior migration (e.g. an experiment). FR-007's schism detection refuses to overwrite; user must resolve manually.
- **`.graphiti/ontology-hash.json` exists but is from the directory-schism era** (wrote to wrong root). Moves cleanly to `.lcg/ontology-hash.json` as part of normal migration. Drift detection on next start may report "drifted" because the persisted hash was computed against a now-stale snapshot — that's the correct behavior, surfacing the historical schism to the user.
- **The migration `service.sock` is left behind in `.graphiti/`** as a non-regular-file. FR-008 handles — skip it; the new socket is created at `.lcg/service.sock` during binary listen-bind.

## Requirements *(mandatory)*

- **FR-001.** The migration logic in `liminis-graph/src/main.rs` MUST replace the current simple `rename(".graphiti", ".lcg")` with a multi-step file-aware migration. Each known legacy path moves to its specific new location.
- **FR-002.** The migration MUST handle each of the legacy → new mappings:
  - `.graphiti/db` (file) → `.lcg/db/liminis.db` (file inside new dir)
  - `.graphiti/db.wal` (file) → `.lcg/db/liminis.db.wal` (file inside new dir)
  - `.graphiti/wal/` (dir) → `.lcg/wal/` (dir, content unchanged)
  - `.graphiti/ontology.yaml` (file) → `.lcg/ontology.yaml` (file)
  - `.graphiti/ontology-hash.json` (file) → `.lcg/ontology-hash.json` (file)
  - `.graphiti/service.sock` (socket) → NOT migrated (transient; recreated)
  - Any unrecognized file in `.graphiti/` → moved to `.lcg/_unrecognized/<name>` with a warn log, NOT deleted. (Defense against future additions or third-party tooling.)
- **FR-003.** Migration steps MUST be performed in an order that allows partial-completion detection:
  1. Create `.lcg/db/` (new directory).
  2. Move `.graphiti/db` → `.lcg/db/liminis.db`.
  3. Move `.graphiti/db.wal` → `.lcg/db/liminis.db.wal` (if present).
  4. Move `.graphiti/wal/` → `.lcg/wal/`.
  5. Move `.graphiti/ontology.yaml` → `.lcg/ontology.yaml` (if present).
  6. Move `.graphiti/ontology-hash.json` → `.lcg/ontology-hash.json` (if present).
  7. Move any unrecognized files per FR-002 (last clause).
  8. Sanity-check: `Db::open(".lcg/db/liminis.db")` succeeds.
  9. Delete (or `.bak`-rename per User Story 6) the now-empty `.graphiti/`.
- **FR-004.** Migration MUST detect partial-completion state on startup. If `.graphiti/` and `.lcg/` both exist but `.lcg/db/liminis.db` is present (suggesting prior partial migration), the binary MUST attempt to complete the remaining steps rather than aborting.
- **FR-005.** Migration MUST validate by re-opening the migrated DB before deleting the source. If `Db::open` on the new path fails, abort with an error and DO NOT delete `.graphiti/`. The user can retry or roll back manually.
- **FR-006.** Structured logging at every step (per User Story 3). Use the existing telemetry sink to emit migration events that liminis-app can consume.
- **FR-007.** The binary MUST NOT auto-migrate when both `.graphiti/` and `.lcg/` exist as full layouts (the schism case). Refuse to start with a clear error per User Story 5.
- **FR-008.** Migration MUST handle the case where `.graphiti/` has a stale `service.sock` from a previous run — don't try to move it as a regular file; either skip it (preferred) or unlink it.
- **FR-009.** A new integration test MUST exercise the full migration end-to-end: set up a legacy-layout workspace with known content, run the migration, assert all FR-002 mappings are correct, assert DB opens and contains the expected data.
- **FR-010.** A negative-path test MUST simulate mid-migration failure (e.g. permission denied on one file move) and assert the workspace is left recoverable, not corrupted.

## Success Criteria *(mandatory)*

- **SC-001.** Demo-notebook test: take the current `.lcg/db` (file)-shaped workspace, copy it to a tempdir, simulate the legacy layout, run the new migration logic, assert the workspace ends in the new layout, the DB opens, entity counts match.
- **SC-002.** New integration test: create a legacy-layout workspace fixture with synthetic data (10 entities, 10 edges, ontology, hash file), run migration, assert all five FR-002 mappings landed correctly, DB opens, IPC `knowledge_status` returns expected counts.
- **SC-003.** Partial-failure test: simulate a permission-denied error on the third move step; assert the workspace is left recoverable (sources intact, partial-destination cleaned up); restart with permissions fixed completes the migration cleanly.
- **SC-004.** Idempotency test: run migration twice in a row on the same workspace; second run is a no-op with appropriate "already migrated" log.
- **SC-005.** Schism test: workspace with both `.graphiti/` and `.lcg/` present (real content in both); binary refuses to start with a clear actionable error; data in both is preserved.
- **SC-006.** Backup test: with `LCG_MIGRATION_KEEP_BACKUP=1`, migration produces `.graphiti.bak/` instead of deleting; bak content matches what would have been deleted.
- **SC-007.** Logging test: all migration events emit structured log lines with the required fields; liminis-app can pattern-match them for UI surfacing.
- **SC-008.** Real production workspace test (manual): user's production machine workspace can be migrated cleanly without `rm -rf` and without data loss.
- **SC-009.** Existing tests pass; the simple-rename behavior in main.rs is fully removed.

## Assumptions

- **A1.** The legacy layout is exactly what liminis-app's pre-#828 spawn code produced (db as file at `.graphiti/db`, etc.). Verified from observed demo-notebook state. If there are workspaces with a different "legacy" layout (e.g. very old liminis-app versions), they're out of scope; this issue migrates from the most recent legacy convention only.
- **A2.** `std::fs::rename` is atomic within a single filesystem on macOS + Linux for files. Inter-directory moves within a workspace fall under that.
- **A3.** The binary runs migration *before* it binds the IPC socket. No client can observe a partially-migrated state via IPC. (Verify against `main.rs` startup ordering.)
- **A4.** Users restart the binary after major upgrades; migration runs at the next startup. Hot-migration during an active session is out of scope.
- **A5.** The structured logging events for migration are consumed by liminis-app's existing service-state UI (or could be without much work). If not, FR-006 may need a separate UI piece — out of scope for this issue but worth knowing.
- **A6.** Sanity-checking via `Db::open` post-migration is fast enough (<1s on typical workspaces) to be part of the migration path. If lbug `open` becomes expensive on large DBs, the check can move to async or be skipped behind a flag.

## Out of Scope

- Migrating non-`.graphiti/`/`.lcg/` workspace state (e.g. `.liminis/logs/`, `.claude/skills/`). Those have their own conventions and aren't part of the graphiti rename.
- Rolling back a completed migration. v1 doesn't support `lcg → graphiti` reverse migration; the rename is one-way by design.
- Backing up the workspace to a remote location before migration. v1 = local-only.
- Multi-version migration paths (e.g. some hypothetical v0.5 layout). v1 = legacy `.graphiti/` (one specific layout) → current `.lcg/` (one specific layout).
- Validating ontology / WAL content after migration. v1 = just verify file moves and DB open. Content-validation is a separate concern.
- Migrating `.graphiti.bak/` directories from prior aborted runs.

## Source References

- **liminis-graph#59 (umbrella, merged):** the Graphiti → Liminis Context Graph rename.
- **liminis-graph#64 (merged):** added the simple `rename(".graphiti", ".lcg")` auto-migration. This issue replaces it.
- **liminis#828 (merged):** removed liminis-app's `.graphiti/` env-var pinning. Without this issue, #828 leaves users in the crash-loop state described in the Background.
- **liminis-graph#55 / ADR-0046 (merged):** degraded-mode IPC for unrecoverable startup states. FR-005's failure path uses this surface.
- **liminis-graph#98 (merged):** ontology drift detection. The `ontology-hash.json` file is in scope for FR-002's mappings.
- **Live evidence:** demo-notebook 2026-05-26 crash trace and recovery sequence. The user's manual `rm .lcg/db` was the only workaround; this issue obviates that.
- **Production workspace at risk:** user's other machine. Test-validating this issue's migration against the production workspace is part of SC-008.
- **OSS launch:** when liminis-graph ships externally, this migration is the first thing every upgrading user will hit. Getting it right is launch-critical.
