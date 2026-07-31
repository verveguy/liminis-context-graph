# Feature Specification: two e2e tests call `knowledge_rebuild_from_wal` without `force_clear` and have failed since the guard landed

**Feature Branch**: `fabrik/issue-301`
**Created**: 2026-07-30
**Status**: Draft
**Input**: User description: "`mcp_real_corpus_mutation_e2e` and `mcp_real_corpus_admin_data_e2e` both fail in `real-corpus-e2e`, and have done on every push to `main` since 2026-07-26. Both panic in the shared `structured()` helper on the same tool-level error: `knowledge_rebuild_from_wal: database already contains data and from_seq: 0 is a full rebuild... Pass force_clear: true...`. The product is correct here and the tests are stale."

## Background

`mcp_real_corpus_mutation_e2e` and `mcp_real_corpus_admin_data_e2e` both fail in the `real-corpus-e2e` workflow, and have done on every push to `main` since 2026-07-26. Both panic in the shared `structured()` helper on the same tool-level error:

```
IPC error: knowledge_rebuild_from_wal: database already contains data and from_seq: 0 is a
full rebuild. Replaying now would fail with a duplicate-primary-key error for every existing
node. Pass force_clear: true to clear the database before replaying, or clear it first with
knowledge_clear_all.
```

**The product is correct here and the tests are stale** — the opposite of #297, which is the third failing job in the same workflow.

The guard is deliberate and documented: `README.md` states that "a `from_seq: 0` full rebuild refuses to run against a non-empty database unless `force_clear: true` is passed." It landed on 2026-07-26 at 10:00, and the suite went red the same evening. Neither test file contains the string `force_clear` anywhere — confirmed by direct inspection of both files during this specification pass — so they have never satisfied it.

Both tests seed the fixture corpus and then issue one or more full rebuilds (`knowledge_rebuild_from_wal` with no `from_seq` override, i.e. `from_seq: 0`) against a database that, at that point, already holds data. Each such call site is exactly what the guard exists to refuse. Confirmed during this specification pass: `mcp_real_corpus_mutation_e2e.rs` has five call sites where the rebuild follows ingested writes with no intervening clear, plus one already preceded by `knowledge_clear_all`; `mcp_real_corpus_admin_data_e2e.rs` has four rebuild call sites across its user stories, in varying pre-states (a rebuild into a freshly-created workspace from a WAL dump, a rebuild against the populated primary client, a rebuild against a synthetic empty WAL, and a post-checkpoint rebuild). Which of these are actually unguarded full rebuilds against non-empty state, and what each is intended to verify, is for the Research stage to enumerate precisely — this section only establishes that the failure is real, reproducible, and not confined to a single call site per file.

## Why this went unnoticed

`real-corpus-e2e` runs only on push to `main` and nothing alerts on failure — 24 consecutive red runs over four days. Tracked separately as #298; this issue is only the test fix.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The e2e suite passes on `main` (Priority: P1)

As a maintainer relying on `real-corpus-e2e` to catch regressions in the mutation and admin data-plane surfaces, I need `mcp_real_corpus_mutation_e2e` and `mcp_real_corpus_admin_data_e2e` to pass on every push to `main`, using the rebuild API the way a real caller is expected to use it — not by weakening what the tests assert.

**Why this priority**: these two jobs have been red for four days on every push to `main`; a known-red signal in a workflow that has no alerting (#298) is actively dangerous because a genuine regression introduced alongside them would be invisible.

**Independent Test**: Run `cargo test --release --test mcp_real_corpus_mutation_e2e -- --ignored` and `cargo test --release --test mcp_real_corpus_admin_data_e2e -- --ignored` locally (or observe the corresponding `real-corpus-e2e` jobs on a push to `main`) and confirm both exit green.

**Acceptance Scenarios**:

1. **Given** `main` after this change, **When** `real-corpus-e2e` runs, **Then** `mcp_real_corpus_mutation_e2e` and `mcp_real_corpus_admin_data_e2e` both pass.
2. **Given** those tests, **When** they exercise a full rebuild (`from_seq: 0`) against a database that already contains data, **Then** they satisfy the guard the way a real caller would — either by passing `force_clear: true`, or by clearing first with `knowledge_clear_all` — chosen per call site according to what that call site is actually trying to verify, not applied uniformly without regard to intent.
3. **Given** the guard itself, **When** a full rebuild is attempted against a non-empty database without `force_clear: true`, **Then** at least one test still asserts the guard rejects it, so the guard's own behavior remains covered by the suite rather than becoming untested collateral of this fix.

---

### Edge Cases

- `force_clear: true` is destructive. Any call site that adopts it must either re-seed the fixture data it depends on afterward, or be restructured so it no longer depends on data that clearing would destroy.
- Different call sites within the same test file may warrant different remedies. For example, a rebuild whose purpose is to verify round-trip fidelity of data that already exists in the database is verifying different semantics than a rebuild meant to exercise a clear-then-replay path — read each call site's intent before choosing a fix, rather than applying one remedy uniformly across a file.
- A call site already preceded by `knowledge_clear_all` (or that rebuilds into a freshly created, still-empty workspace) may already satisfy the guard and need no change — confirm this rather than assuming every call site is broken.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every full-rebuild (`from_seq: 0`) call site in `mcp_real_corpus_mutation_e2e` and `mcp_real_corpus_admin_data_e2e` that runs against a database already containing data MUST satisfy the guard the way a real caller would — either by passing `force_clear: true`, or by clearing first with `knowledge_clear_all` — whichever matches what that specific call site is actually trying to verify.
- **FR-002**: The guard itself MUST NOT be relaxed, and no failing assertion in either test MUST be removed or marked `#[ignore]`. The product behavior that produces the error is correct; only the tests' use of the API changes.
- **FR-003**: At least one test in the suite MUST continue to assert that a full rebuild against a non-empty database, attempted without `force_clear: true`, is rejected — so the guard's rejection path stays covered by the suite rather than becoming untested collateral of this fix.
- **FR-004**: The remaining jobs in `real-corpus-e2e` (`real_corpus_e2e`, `mcp_real_corpus_e2e`, `mcp_real_corpus_admin_lifecycle_e2e`) MUST be audited for the same latent assumption (an unguarded full rebuild against a non-empty database). The PR description MUST state the result of that audit — which jobs were checked, and whether any would break if an equivalent guard were added to the operation(s) they exercise.

### Key Entities

Not applicable — this is a test-only change with no data model impact.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: All five jobs in the `real-corpus-e2e` workflow are green on `main`, in combination with the fix for #297 (the `indices_built` flag issue, out of scope here).
- **SC-002**: The suite contains a passing assertion that a full rebuild against a non-empty database, attempted without `force_clear: true`, is rejected by the guard.

## Assumptions

- The guard's behavior and error message, as quoted above, are correct and stable — this issue does not question or renegotiate the guard's semantics, only the tests' conformance to it.
- `README.md`'s documentation of the guard (a `from_seq: 0` full rebuild refuses to run against a non-empty database unless `force_clear: true` is passed) is authoritative for what "a real caller" is expected to do.
- The exact set of call sites needing a code change, and which of the two remedies (`force_clear: true` vs. pre-clearing with `knowledge_clear_all`) fits each one, is left to the Research/Plan stages to determine per call site — this spec fixes the required outcome (tests pass, guard stays covered, guard stays unweakened) rather than the mechanism.

## Out of Scope

- #297 — the `indices_built` flag on the third failing `real-corpus-e2e` job.
- #298 — the absence of any alert on a red post-merge suite.
- Any change to the guard's behavior, error message, or the `force_clear` / `knowledge_clear_all` API surface itself.

## Source References

- `crates/service/tests/mcp_real_corpus_mutation_e2e.rs` — the mutation-surface e2e suite (issues #202, #203, #205).
- `crates/service/tests/mcp_real_corpus_admin_data_e2e.rs` — the admin/data-plane e2e suite (issue #236).
- `.github/workflows/real-corpus-e2e.yml` — defines the five `real-corpus-e2e` jobs referenced in FR-004 and SC-001.
- `README.md` — documents the `from_seq: 0` / `force_clear` guard.
