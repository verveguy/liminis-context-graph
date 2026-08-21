# Feature Specification: `migrate_workspace` must honor `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` when relocating legacy `.graphiti/wal`

**Feature Branch**: `fabrik/issue-442`
**Created**: 2026-08-21
**Status**: Specified
**Input**: User description: "`migration::migrate_workspace` (`crates/service/src/migration.rs`, Step 4) always moves `.graphiti/wal/` to the hardcoded path `.lcg/wal/`, regardless of any `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` override. Meanwhile, the per-group WAL-root migration (`wal_group::migrate_wal_root_if_needed`, called from `bootstrap_app_state` in `crates/service/src/main.rs`) resolves its WAL root (`startup_wal_root`) from those same env vars. For a `.graphiti`-era workspace with a default WAL path, both migrations agree on `.lcg/wal` and everything works correctly (this is what #437 hardened). For a `.graphiti`-era workspace where `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` is set to a non-default path, the two migrations disagree, and legacy WAL content ends up loose at `.lcg/wal` instead of being relocated into `<configured_wal_root>/<group_id>/`."

## Background

Startup for a `.graphiti`-era workspace runs two WAL-relocation steps in sequence, and both must agree on the destination WAL root for legacy content to survive the upgrade intact:

1. `migration::migrate_workspace` (Step 4, `crates/service/src/migration.rs`) moves the whole `.graphiti/wal/` directory to a `.lcg`-layout destination.
2. `wal_group::migrate_wal_root_if_needed` (called from `bootstrap_app_state` in `crates/service/src/main.rs`) then scans the resolved WAL root — `startup_wal_root`, computed from `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` with a `.lcg/wal` fallback — for loose legacy files and relocates them into `<wal_root>/<group_id>/`.

Step 1 always writes to the hardcoded path `.lcg/wal`, while step 2 always reads from whatever `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` currently resolves to. When no override is set, both steps land on the same path (`.lcg/wal`) and the hand-off works — this is the case #437 hardened with an explicit call-ordering guarantee and a regression test (`crates/service/tests/migration_binary.rs`). When an override to a non-default path *is* set, step 1's output and step 2's input are different directories: step 1 deposits legacy WAL files at `.lcg/wal`, step 2 looks in the configured directory instead, finds nothing there to migrate, and the legacy content is left loose at `.lcg/wal` — invisible to the running service, which only reads WAL data from the configured per-group directories under the WAL root it was told to use.

This was found during review of #437 (PR #441) by CodeRabbit as a pre-existing gap orthogonal to the migration-*ordering* defect #437 fixed. It was deliberately deferred to keep #441 scoped, and `bootstrap_app_state`'s doc comment in `main.rs` currently flags it as a known limitation pending this issue.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Custom WAL root survives the `.graphiti` → `.lcg` upgrade (Priority: P1)

An operator runs liminis-context-graph with `LCG_WAL_DIR` (or the deprecated `GRAPHITI_WAL_DIR`) set to a non-default path — for example, to place the WAL on a separate volume. Their workspace is still on the legacy `.graphiti/` layout from before the `.lcg/` migration. On the next startup, the binary migrates the workspace and the operator expects their previously-recorded WAL content (pending episodes, not-yet-applied mutations) to remain fully intact and visible to the service afterward, in the same per-group layout the service currently reads from.

**Why this priority**: Without this, upgrading a workspace with a custom WAL root silently drops WAL content out of the service's view. The files aren't deleted, but they become invisible to the running service, which is functionally equivalent to data loss until an operator manually discovers and relocates them — the same failure class #437 already fixed for the default path.

**Independent Test**: Start from a `.graphiti/wal/*.jsonl` legacy fixture, launch the binary with `LCG_WAL_DIR` set to a path other than `.lcg/wal`, and confirm the legacy WAL files end up under `<configured_wal_root>/<group_id>/` and are visible to the service (e.g., via `knowledge_status` or by resuming from them), with nothing left loose at `.lcg/wal` or at the configured root's top level.

**Acceptance Scenarios**:

1. **Given** a `.graphiti`-era workspace with legacy `.graphiti/wal/*.jsonl` files and `LCG_WAL_DIR` set to a custom, non-default path, **When** the binary starts, **Then** the legacy WAL files end up under `<configured_wal_root>/<group_id>/` (the same per-group layout used for the default path), not loose at `.lcg/wal`.
2. **Given** the same setup but using the deprecated `GRAPHITI_WAL_DIR` variable instead of `LCG_WAL_DIR`, **When** the binary starts, **Then** the outcome is identical to Scenario 1 (both env vars resolve to the same configured root, per existing precedent elsewhere in the codebase).
3. **Given** a `.graphiti`-era workspace with **no** `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` override set, **When** the binary starts, **Then** behavior is unchanged from today: legacy WAL content lands at `.lcg/wal/<group_id>/` exactly as the existing #437 regression test verifies.
4. **Given** a workspace where a prior run already partially completed this migration (e.g., process was killed mid-relocation), **When** the binary is restarted with the same `LCG_WAL_DIR` value, **Then** the migration resumes/completes idempotently without duplicating, losing, or corrupting any WAL file — consistent with `migrate_workspace`'s existing partial-resume behavior for its other steps.

---

### Edge Cases

- The configured WAL root (`LCG_WAL_DIR`/`GRAPHITI_WAL_DIR`) already exists and already contains content (e.g., from a fresh `.lcg`-era group that was created before this migration ran) at the time legacy `.graphiti/wal` content needs to be relocated into it. Neither data set may be silently overwritten or lost.
- `LCG_WAL_DIR` and `GRAPHITI_WAL_DIR` are both set to different values — the precedence between them must match the existing resolution behavior already used by `startup_wal_root` (`LCG_*` takes priority; see `lcg_env_var`), not introduce a second, inconsistent resolution rule.
- The configured WAL root path is relative vs. absolute — the destination must resolve consistently between whatever performs the `.graphiti/wal` relocation and whatever performs the subsequent per-group scan, so both steps agree on the same on-disk location.
- A loose `.wal-generation.json` sidecar (per #431) is present among the legacy `.graphiti/wal` files being relocated to a custom root — it must still be relocated ahead of any fresh generation-id minting, per the existing precedent in `wal_group::migrate_wal_root_if_needed`.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When migrating a `.graphiti`-era workspace, the workspace-migration step that relocates `.graphiti/wal` MUST deposit that content where the per-group WAL-root migration (`wal_group::migrate_wal_root_if_needed`) will actually look for it — i.e., the WAL root resolved from `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR`, falling back to `.lcg/wal` only when neither is set.
- **FR-002**: The two migration steps MUST use one consistent WAL-root resolution (same env vars, same precedence, same fallback) rather than each independently computing a path that could diverge.
- **FR-003**: For a workspace with no `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` override, migration behavior and output MUST remain exactly as they are today (`.lcg/wal/<group_id>/`) — this is not a behavior change for the case #437 already covers and tests.
- **FR-004**: End-to-end, a `.graphiti`-era workspace started with a custom WAL root MUST result in all legacy WAL content visible under `<configured_wal_root>/<group_id>/`, with none left loose at either `.lcg/wal` or the top level of the configured root.
- **FR-005**: If the configured WAL root destination already contains content at the time relocation would occur, existing content MUST NOT be silently overwritten or lost; the resolution matches the conflict-handling precedent already established elsewhere in `migrate_workspace` (e.g., its existing "skip if destination already exists" and hard-link-based conflict detection for other steps).
- **FR-006**: The migration MUST remain idempotent and crash-safe: restarting after a partial relocation (any custom WAL root) does not duplicate, lose, or corrupt WAL files, consistent with `migrate_workspace`'s existing partial-resume model.
- **FR-007**: The known-limitation doc comment on `bootstrap_app_state` in `crates/service/src/main.rs` (added by #437, citing this issue) MUST be updated or removed once this is fixed, so it no longer describes a limitation that has been resolved.

### Key Entities

- **Legacy WAL directory**: `.graphiti/wal/`, containing loose `*.jsonl` WAL files (and possibly a loose `.wal-generation.json` sidecar) from a pre-`.lcg` installation.
- **Configured WAL root**: The directory resolved from `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR`, falling back to `.lcg/wal` — the single destination both migration steps must agree on.
- **Per-group WAL directory**: `<wal_root>/<group_id>/`, the final resting place for WAL files after both migration steps have run.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Starting the binary against a `.graphiti`-era workspace fixture with `LCG_WAL_DIR` (or `GRAPHITI_WAL_DIR`) set to a non-default path results in 100% of the legacy `.graphiti/wal/*.jsonl` files present under `<configured_wal_root>/<group_id>/` after startup, with zero files left loose at `.lcg/wal` or at the configured root's top level.
- **SC-002**: The existing #437 regression test suite (default WAL path, no env override) continues to pass unmodified — this fix introduces no regression for the already-covered default case.
- **SC-003**: A new automated regression test exists, analogous to the existing #437 test in `crates/service/tests/migration_binary.rs`, that exercises the non-default `LCG_WAL_DIR` path end-to-end and fails without this fix.

## Assumptions

- This issue covers workspaces that are still on the legacy `.graphiti/` layout at the time they are first started with a custom WAL root (i.e., `.graphiti/wal` still exists on disk). Workspaces that were already migrated under the pre-fix (buggy) behavior — where `.graphiti/` is already gone and legacy content is already sitting loose at `.lcg/wal` despite a configured custom root — are **out of scope** for this issue; recovering those is a separate, one-time repair concern, not a repeat of the migration path this issue fixes going forward.
- `LCG_WAL_DIR` and `GRAPHITI_WAL_DIR` precedence and fallback behavior follow the existing, already-established resolution used elsewhere in the codebase (`lcg_env_var("LCG_WAL_DIR", "GRAPHITI_WAL_DIR")`); this issue does not change that precedence, only ensures both migration steps use it consistently.
- The specific mechanism for how the two migration steps come to agree on the destination (e.g., passing the resolved WAL root into `migrate_workspace`, reordering, or another reconciliation) is a technical design decision left to the Research/Plan stages, per the issue's own "Expected" section, which explicitly leaves this open.

## Out of Scope

- Recovering workspaces already left in the broken state by this bug prior to the fix landing (see Assumptions).
- Any change to the `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` precedence or fallback semantics themselves.
- Changes to migration steps other than the `.graphiti/wal` relocation (Step 4 of `migrate_workspace`).

## Source References

- `crates/service/src/migration.rs` — `migrate_workspace`, Step 4 (hardcoded `.lcg/wal` destination).
- `crates/core/src/wal_group.rs` — `migrate_wal_root_if_needed` (reads from the configured WAL root).
- `crates/service/src/main.rs` — `bootstrap_app_state` (resolves `startup_wal_root`; doc comment cites this issue as a known limitation).
- `crates/service/tests/migration_binary.rs` — existing #437 regression test for the default-path case.
- #437 / PR #441 — the migration-ordering fix during whose review this issue was found.
