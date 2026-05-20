# Feature Specification: Lint Sweep — 6 Clippy Errors + 20 fmt Diffs from Rust 1.94

**Feature Branch**: `fabrik/issue-20`
**Created**: 2026-05-20
**Status**: Draft
**Input**: Issue #20 — "Lint sweep: 6 clippy errors + 20 fmt diffs from Rust 1.94 new lints"

## Background

After PR #17 switched CI back to Linux and PR #19 fixed the ML-deps grep false positive, `cargo clippy --release -- -D warnings` and `cargo fmt --check` run for the first time on CI under Rust 1.94 and will fail. Local triage on macOS confirms 6 clippy errors and ~20 files with fmt diffs.

This is blocking the `test (ubuntu-latest)` CI job from reaching green. The fixes are all idiomatic rewrites with no semantic behavior change — they exist because Rust 1.94 introduced new lint rules (`is_none_or`, `needless_splitn`, `trim_split_whitespace`) that flag previously-accepted patterns.

**Sequencing**: Three of the six clippy errors are in `liminis-graph-core/src/db.rs`, which is also edited by PR #18 (issue #16). This issue must not start until PR #18 merges to avoid a painful rebase.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — CI Passes Clean on ubuntu-latest (Priority: P1)

The CI `test (ubuntu-latest)` job completes without lint or formatting failures under Rust 1.94.

**Why this priority**: This is the immediate blocker — no merge is possible while the job is red, regardless of logic correctness.

**Independent Test**: Run `cargo clippy --release -- -D warnings` and `cargo fmt --check` locally and confirm zero warnings/errors and zero fmt diffs.

**Acceptance Scenarios**:

1. **Given** the patched codebase, **When** `cargo clippy --release -- -D warnings` runs on `liminis-graph-core`, **Then** it exits 0 with no warnings.
2. **Given** the patched codebase, **When** `cargo fmt --check` runs on `liminis-graph-core`, **Then** it exits 0 with no diff output.

---

### User Story 2 — All Existing Tests Continue to Pass (Priority: P1)

The changes are purely cosmetic / idiomatic rewrites. No test should be affected.

**Why this priority**: Preserving correctness is non-negotiable. These are mechanical substitutions, but regression-checking is still required.

**Independent Test**: Run `cargo test` on `liminis-graph-core` and confirm the full test suite passes.

**Acceptance Scenarios**:

1. **Given** the patched codebase, **When** `cargo test` runs, **Then** every test that passed before the patch still passes.

---

### Edge Cases

- `cargo fmt` may reformat test assert macros across multiple lines. The diff must be reviewed to confirm it is purely cosmetic (no logic change).
- The `is_none_or` substitution for `map_or(true, ...)` must be verified to preserve short-circuit evaluation semantics (it does — `is_none_or` is `Option::is_none() || f(inner)`).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Replace both `map_or(true, |…| …)` calls in `src/db.rs` (lines 466, 553) with `is_none_or(|…| …)`.
- **FR-002**: Replace the redundant closure `|e| value_as_string(e)` in `src/db.rs` (line 805) with the function reference `value_as_string`.
- **FR-003**: Replace `.splitn(2, ':')` in `src/extractor.rs` (line 49) with `.split(':')`.
- **FR-004**: Remove the redundant `.trim()` before `.split_whitespace()` in `src/replay.rs` (line 87) and `src/wal.rs` (line 79).
- **FR-005**: Run `cargo fmt` across all 20 files listed in the issue and commit the formatting changes.
- **FR-006**: No semantic behavior change is introduced by any of these edits.

### Key Files

The following files require changes:

**Clippy fixes**:
- `liminis-graph-core/src/db.rs` (lines 466, 553, 805)
- `liminis-graph-core/src/extractor.rs` (line 49)
- `liminis-graph-core/src/replay.rs` (line 87)
- `liminis-graph-core/src/wal.rs` (line 79)

**Fmt-only fixes** (bulk `cargo fmt`):
- `liminis-graph-core/src/extractor.rs`
- `liminis-graph-core/src/handlers.rs`
- `liminis-graph-core/src/lib.rs`
- `liminis-graph-core/src/llm_router.rs`
- `liminis-graph-core/src/replay.rs`
- `liminis-graph-core/src/schema.rs`
- `liminis-graph-core/src/search.rs`
- `liminis-graph-core/src/telemetry.rs`
- `liminis-graph-core/src/wal.rs`
- `liminis-graph-core/tests/concurrent_rw_integration.rs`
- `liminis-graph-core/tests/db_dedup.rs`
- `liminis-graph-core/tests/dedup_integration.rs`
- `liminis-graph-core/tests/integration_spike.rs`
- `liminis-graph-core/tests/ipc_parity.rs`
- `liminis-graph-core/tests/ldb_spike_ipc.rs`
- `liminis-graph-core/tests/telemetry_ipc.rs`
- `liminis-graph-core/tests/wal_appender.rs`
- `liminis-graph-core/tests/wal_compat.rs`
- `liminis-graph-core/tests/wal_replay.rs`
- `liminis-graph-core/tests/wal_serialization.rs`

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `cargo clippy --release -- -D warnings` exits 0 with no output on `ubuntu-latest`.
- **SC-002**: `cargo fmt --check` exits 0 with no diff on `ubuntu-latest`.
- **SC-003**: `cargo test` passes with no regressions.
- **SC-004**: The diff contains no logic changes — only the six idiomatic substitutions and whitespace/formatting changes.

## Assumptions

- PR #18 (issue #16) has already merged before this branch is worked on, so `src/db.rs` is in its post-#18 state when the clippy fixes are applied.
- Rust 1.94 is the toolchain version in use on CI (matching the version that introduced these lints).
- `cargo fmt` is run with the project's existing `rustfmt.toml` settings (no configuration change needed).
- The fmt diffs are purely cosmetic (line-break normalization, trailing comma insertion in assert_eq, etc.) — no logic change.

## Out of Scope

- Fixing any other clippy warnings beyond the six enumerated above.
- Upgrading the Rust toolchain version.
- Changes to CI configuration (`.github/workflows/`).
- Any refactoring beyond the mechanical lint fixes.

## Source References

- `liminis-graph-core/src/db.rs:466, 553, 805`
- `liminis-graph-core/src/extractor.rs:49`
- `liminis-graph-core/src/replay.rs:87`
- `liminis-graph-core/src/wal.rs:79`
- PR #17 (CI switch to Linux), PR #18 (issue #16, db.rs edits — must merge first), PR #19 (ML-deps fix)
