# Feature Specification: Fix startup migration ordering so legacy WAL files are relocated, not left loose and invisible

**Feature Branch**: `fabrik/issue-437`
**Created**: 2026-08-19
**Status**: Specified
**Input**: User description: "On startup the two migrations run in the wrong order, so a `.graphiti`-era workspace ends up with its WAL files loose at the top level of `.lcg/wal/` and never relocated into the per-group subdirectory. By the service's own documented contract, that content is then invisible to the process — it is not read as a fallback."

## Background

The service's startup sequence runs two independent migrations that must, in practice, execute in a specific order, but nothing currently enforces that order — they simply happen to be called ~480 lines apart in `crates/service/src/main.rs`, in the wrong sequence:

- **`main.rs:520`** — `lcg_core::wal_group::migrate_wal_root_if_needed(&startup_wal_root)` (ADR-0378): relocates any loose `<wal_root>/*.jsonl` files into `<wal_root>/<group_id>/`.
- **`main.rs:1001`** — `migration::migrate_workspace(Path::new("."), …)`: the legacy `.graphiti/` → `.lcg/` workspace move.

For a `.graphiti`-era workspace, at the point line 520 runs, `.lcg/wal` does not exist yet — the WAL files are still under `.graphiti/wal/`. `migrate_wal_root_if_needed` correctly no-ops on a missing root (there is a dedicated test for exactly that case: `wal_group.rs::migrate_is_noop_for_missing_root`). Only afterwards, at line 1001, does `migrate_workspace` move `.graphiti/wal/001.jsonl` to `.lcg/wal/001.jsonl` — loose, at the WAL root, with nothing left to relocate it into a per-group subdirectory. The per-group migration has already run for this startup and will not run again.

The consequence is spelled out in `main.rs`'s own error text at the per-group migration call site: pre-378 loose top-level WAL content at the WAL root *"stays on disk untouched but becomes invisible to this process ... it is not read as a fallback."* For anyone upgrading from a `.graphiti`-era workspace, the WAL is silently unreadable — no error, no warning, just an empty stream. The data remains intact on disk; the process behaves as though it does not exist. This is a data-visibility bug on the oldest upgrade path the project supports.

Note that the `.lcg`-era upgrade path is **not** affected — verified by hand against the released 0.13.2 binary: a workspace that already has a flat `.lcg/wal/*.jsonl` layout (no `.graphiti/` directory involved) migrates correctly, with all files relocated into `.lcg/wal/liminis/`. The bug is specific to starting from `.graphiti/`, because only then does the per-group relocation run before the files it needs to move actually exist.

This defect is adjacent to, but distinct from, #431 (now closed), which fixed the per-group migration not stamping `.wal-generation.json`. Both are defects in the same migration chain: #431 left a migrated stream unidentified; this issue leaves it unmoved and invisible. #431 having landed means the realistic-corpus verification for this fix (see FR-007) now exercises a `.wal-generation.json` sidecar as part of the legacy fixture, which was not yet true when this issue was first filed.

> **Post-Research correction**: the ordering described above was diagnosed from the two calls' *textual* line-number order in `main.rs`, not their call-graph order. The Research stage for this issue ran the (then-quarantined) regression test directly against current `main` and found it passes: `migrate_wal_root_if_needed` (`main.rs:520` at the time) is only ever reached via `bootstrap_app_state`, which `async_main` calls strictly *after* its own `migrate_workspace` call (`main.rs:1001` at the time) — so for the default WAL path, the two migrations already run in the correct order today, and this specific ordering defect does not currently reproduce. The FRs and acceptance scenarios below remain the right target behavior; the PR that resolves this issue satisfies them by hardening the already-correct implicit ordering dependency with explicit comments and a strengthened regression test, not by reordering working code. See the PR description for the full account, and issue #442 for a distinct, narrower gap (a configured `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR` is not honored by `migrate_workspace`) found during review and tracked separately.

### How it surfaced

`migration_binary_tests::binary_migrates_legacy_workspace_on_startup` (`crates/service/tests/migration_binary.rs:91`) asserts that a legacy `.graphiti`-era fixture ends up with its WAL file at `.lcg/wal/liminis/001.jsonl`. It began failing on `main` once CI's `test` gate was fixed to actually surface failures (#430); before that, the failure was masked (#430 also quarantined this specific test with `#[ignore = "#437: ..."]` so the now-working gate could land without blocking on this issue). This is *not* the embedder-startup problem it was originally suspected to be: the test has its own `spawn_stub_embedder()` since `dda0130`, the binary starts and becomes ready fine, and the panic is at the post-migration layout assertion (`migration_binary.rs:162` in the pre-quarantine version), not at the readiness wait.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Legacy workspace WAL files survive the upgrade (Priority: P1)

An operator running a `.graphiti`-era workspace upgrades to a binary built after ADR-0378 (per-group WAL layout). On first startup against the old workspace, the service migrates the workspace layout and relocates all WAL content into the correct per-group directory, so every entry the WAL previously held is still visible to the running process afterward.

**Why this priority**: This is the core defect. Without it, upgrading from a `.graphiti`-era workspace causes silent, undetected data loss from the process's point of view — the exact failure mode described in the issue.

**Independent Test**: Build a `.graphiti/wal/` fixture containing multiple `*.jsonl` files (and a `.wal-generation.json` sidecar, reflecting current post-#431 behavior), start the binary against that workspace with no prior `.lcg/` directory, and after startup completes assert that every WAL file is present under `.lcg/wal/<group_id>/` and that no `*.jsonl` file remains loose directly under `.lcg/wal/`.

**Acceptance Scenarios**:

1. **Given** a `.graphiti`-era workspace containing `wal/001.jsonl`, **When** the binary starts against that workspace for the first time, **Then** `.lcg/wal/liminis/001.jsonl` exists and no `*.jsonl` file exists loose directly under `.lcg/wal/`.
2. **Given** a `.graphiti`-era workspace containing several WAL files (e.g. `001.jsonl` through `005.jsonl`) and a `.wal-generation.json` sidecar, **When** the binary starts, **Then** every WAL file is relocated to `.lcg/wal/liminis/`, the generation sidecar ends up correctly associated with that group directory, and none of the relocated content is lost or left loose at the WAL root.
3. **Given** a workspace that is already in the `.lcg`-era flat-WAL layout (no `.graphiti/` directory present, `.lcg/wal/*.jsonl` loose at the root), **When** the binary starts, **Then** behavior is unchanged from today — all files are still relocated into `.lcg/wal/liminis/` (regression guard on the path the issue confirms already works).
4. **Given** a fresh workspace with no `.graphiti/` and no pre-existing `.lcg/wal`, **When** the binary starts, **Then** both migrations remain no-ops and startup proceeds normally (regression guard on `migrate_is_noop_for_missing_root`).

---

### User Story 2 - Ordering intent is documented, not incidental (Priority: P2)

A future contributor who touches startup sequencing in `main.rs` can tell, from a comment at the relevant call site(s), why the two migrations have the ordering/repetition relationship they do, so a routine reshuffle of startup code doesn't silently reintroduce this bug.

**Why this priority**: The issue explicitly calls out that the current bug exists *because* the ordering was incidental (two calls ~480 lines apart with no stated dependency). Fixing the immediate bug without recording why the order matters leaves the codebase exposed to the same class of regression.

**Independent Test**: Code review of the diff — a reviewer unfamiliar with this issue can read the comment(s) at the migration call site(s) and understand the ordering constraint without consulting this issue or its history.

**Acceptance Scenarios**:

1. **Given** the fixed startup code, **When** a reviewer reads the call site(s) for `migrate_wal_root_if_needed` and `migrate_workspace`, **Then** an in-code comment explains why the order (or repetition) matters.

---

### Edge Cases

- Empty `.graphiti/wal/` directory (directory exists, no `*.jsonl` files inside): migration must no-op cleanly, no error.
- No `.graphiti/` directory at all (fresh workspace): both migrations remain no-ops, per existing behavior.
- Both `.graphiti/` and `.lcg/` present simultaneously: this is the existing "workspace schism" fatal-error case handled elsewhere in `main.rs`/`migration.rs` and is unaffected by this fix — out of scope here.
- A crash or interruption partway through startup migration: `migrate_wal_root_if_needed` is documented as idempotent and crash-safe (e.g. its handling of a partially-relocated `.wal-generation.json`); the fix must preserve that guarantee — a second startup attempt must still converge on the correct end state.
- Loose top-level WAL content that somehow persists even after this fix (e.g. from an unrelated future regression): whether to detect and report this to the operator, rather than silently ignoring it as today, must be explicitly decided as part of this work (see FR-008) — the issue accepts "no" as long as the decision is recorded, not skipped.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: On startup against a `.graphiti`-era workspace, the legacy `.graphiti/` → `.lcg/` move (`migrate_workspace`) MUST have relocated WAL files into `.lcg/wal/` before the per-group WAL-root migration (`migrate_wal_root_if_needed`) inspects that directory for loose files to relocate into `<wal_root>/<group_id>/`. (The specific mechanism — reordering the two calls, or making the per-group migration run again, idempotently, after `migrate_workspace` — is a Research/Plan decision, not constrained here.)
- **FR-002**: After startup completes against a `.graphiti`-era workspace that contained WAL files, no `*.jsonl` WAL file remains loose directly under `<wal_root>` (e.g. `.lcg/wal/`) — every WAL file must reside under `<wal_root>/<group_id>/`.
- **FR-003**: The startup code MUST carry an explicit comment, at the call site(s) that encode the ordering (or repetition) relationship between the two migrations, stating why the relationship matters — so it is not silently broken by a future reshuffle of startup code.
- **FR-004**: The fix MUST NOT regress the already-correct `.lcg`-era upgrade path (a workspace already in the flat `.lcg/wal/*.jsonl` layout must continue to migrate correctly into `.lcg/wal/<group_id>/`).
- **FR-005**: The fix MUST NOT regress the existing no-op behavior of `migrate_wal_root_if_needed` for a missing or empty WAL root, covered today by `wal_group.rs::migrate_is_noop_for_missing_root`.
- **FR-006**: `migration_binary_tests::binary_migrates_legacy_workspace_on_startup` MUST pass with its `#[ignore = "#437: ..."]` attribute removed.
- **FR-007**: The fix's correctness MUST be validated against a legacy WAL corpus more realistic than the existing test fixture's single `001.jsonl` file — multiple WAL files plus a `.wal-generation.json` sidecar (reflecting the now-landed #431 behavior) — in addition to the existing single-file test case.
- **FR-008**: The work MUST include an explicit, recorded decision on whether loose top-level WAL content still found after this fix (e.g. surfaced by some future, unrelated regression) should be detected and reported to the operator rather than silently ignored as it is today. Rejecting detection/reporting is an acceptable outcome, but the decision and its reasoning must be visible in the resulting code/comments or PR description — silence on the question is not acceptable.

### Key Entities

- **WAL root**: The top-level directory (default `.lcg/wal/`, overridable via `LCG_WAL_DIR`/`GRAPHITI_WAL_DIR`) that contains one subdirectory per `group_id` since ADR-0378.
- **Group WAL directory**: `<wal_root>/<group_id>/`, the per-group subdirectory that `*.jsonl` WAL files and their sidecars (`.wal-generation.json`, `.wal-bounds.json`, `.checkpoints/`) must live under to be visible to the service.
- **Legacy `.graphiti/` workspace**: The pre-`.lcg` workspace layout, migrated wholesale by `migrate_workspace` into the current `.lcg/` layout.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Starting the binary against a `.graphiti`-era workspace containing 5+ loose legacy WAL files results in 100% of those files residing under `.lcg/wal/<group_id>/` and 0 remaining loose directly under `.lcg/wal/`.
- **SC-002**: `binary_migrates_legacy_workspace_on_startup` passes with its `#[ignore]` attribute removed.
- **SC-003**: All existing migration-related tests continue to pass unchanged — including the `.lcg`-era upgrade path and the missing-WAL-root no-op case — with no regressions introduced by this fix.
- **SC-004**: A reviewer reading only the startup code (no external issue history) can state, in their own words, why the two migrations must run in the order/relationship this fix establishes.

## Assumptions

- `migrate_wal_root_if_needed` is idempotent and crash-safe as currently documented in `crates/core/src/wal_group.rs`; this fix relies on that existing property rather than re-implementing relocation logic.
- #431 (generation-file stamping) has already landed on `main`, so the realistic-corpus verification in FR-007 exercises current migration behavior, including `.wal-generation.json` stamping — not the pre-#431 behavior the original issue text was written against.
- The default group id (`liminis`, i.e. `DEFAULT_GROUP_ID`) is the correct relocation target for a legacy single-group workspace's WAL files, consistent with the already-working `.lcg`-era migration path.
- Fixing the ordering defect (FR-001/FR-002) is sufficient to eliminate the invisible-data-loss scenario described in the issue going forward; this issue does not need to also provide recovery tooling for workspaces already affected in a prior release (see Out of Scope).

## Out of Scope

- Recovering WAL data for any workspace that has already gone through the broken migration order in a previously-released binary and now has orphaned, invisible loose content at `.lcg/wal/` — that is a separate operational/recovery concern, not a code-behavior fix, and is not addressed here.
- Building a general-purpose "detect any loose/unexpected file under the WAL root" monitoring or alerting feature. FR-008 requires only that the yes/no decision be made and recorded for this migration's specific loose-top-level-WAL-content case, not a broader tool.
- Any change to the ADR-0378 per-group WAL layout itself, or to `migrate_workspace`'s handling of non-WAL workspace content.
- The `.graphiti`+`.lcg` simultaneous-presence ("workspace schism") fatal-error path — unaffected by and unrelated to this fix.

## Source References

- `crates/service/src/main.rs:520` — per-group WAL-root migration call (`migrate_wal_root_if_needed`)
- `crates/service/src/main.rs:1001` — legacy workspace migration call (`migrate_workspace`)
- `crates/service/src/migration.rs:73` — `migrate_workspace` definition
- `crates/core/src/wal_group.rs` — `migrate_wal_root_if_needed`, `migrate_is_noop_for_missing_root`, and related idempotency/crash-safety documentation
- `crates/service/tests/migration_binary.rs:91` — `binary_migrates_legacy_workspace_on_startup` (currently `#[ignore]`d pending this fix)
- ADR-0378 — the per-group WAL root
- #431 — sibling defect: migration did not stamp `.wal-generation.json` (closed, landed on `main`)
- #430 — armed the CI gate that exposed this issue; added the quarantine on the test this issue must un-ignore
- #429 — fixed the other three tests that were failing behind the previously-masked gate
