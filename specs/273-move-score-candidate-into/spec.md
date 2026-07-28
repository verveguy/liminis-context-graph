# Feature Specification: make lcg-eval's scoring loop testable

**Feature Branch**: `fabrik/issue-273`
**Created**: 2026-07-28
**Status**: Draft
**Input**: User description: "eval: move score_candidate into the library so the scoring loop is testable"

## Background

`score_candidate` and `judged_f1` are private functions in `crates/eval/src/main.rs`, so
they are unreachable from `crates/eval/tests/harness_integration.rs` and from any unit
test outside that binary. They hold the harness's entire scoring control flow, and it is
the one part of `lcg-eval` with no test coverage — `judge.rs`, `report.rs`, `pairwise.rs`
and `metrics.rs` are all well covered.

This was raised in review on #271 (finding 3) and deliberately deferred there, because
#271 was the fix needed to resume a stalled benchmark run and this is a refactor.

**Why it matters now**: #271 (merged) added real behaviour to that untested region: judge
failures became non-fatal, and precision/recall began flowing into the report. Nothing
verifies either. Specifically untested today:

- An `Err` from one axis's `judged_f1` increments `judge_errors` and lets the loop
  continue to the next axis and the next chunk, rather than aborting the candidate.
- A failure on one axis leaves the *other two* axes' data points for that chunk intact —
  the distinction the `judge_errors` doc comment makes explicit.
- `judged_entity_precision` / `judged_entity_recall` / `judged_edge_precision` /
  `judged_edge_recall` are populated from the `Ok` arm and averaged over the right
  denominator.
- `chunks_scored` counts pairs where both sides extracted successfully, independent of
  whether judging then succeeded.

The `JudgeClient` trait already exists specifically so tests can substitute a canned judge
without network calls (`StaticJudge` in `judge.rs`), so the seam for testing this is
present — the code is simply on the wrong side of the binary boundary.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The scoring loop is unit-testable (Priority: P1)

As someone changing scoring behaviour, I can drive `score_candidate` from a test with a
scripted `JudgeClient` — including one that fails on selected calls — and assert the
resulting `CandidateReport`, so that error handling and metric aggregation are verified
rather than reasoned about.

**Why this priority**: This is the one region of `lcg-eval` with zero test coverage, and
it just gained non-fatal-error and precision/recall behaviour (#271) that nothing checks.

**Independent Test**: Call `lcg_eval::scoring::score_candidate` directly from a test in
`crates/eval` with a scripted `JudgeClient` and fixture `BackendRunResult`s, with no
network access, and assert on the returned `CandidateReport`.

**Acceptance Scenarios**:

1. **Given** a `JudgeClient` that fails on the entity axis for one chunk but succeeds on
   the edge and summary axes for that chunk, **When** `score_candidate` runs, **Then**
   `judge_errors` is incremented by one, the candidate is not aborted, and the edge/summary
   data points for that chunk still contribute to their respective averages.
2. **Given** a `JudgeClient` that fails on every call for one axis across all chunks,
   **When** `score_candidate` runs, **Then** that axis's reported value is `None` (not
   `0.0`), and `judge_errors` reflects every failed call.
3. **Given** mixed success/failure across chunks, **When** `score_candidate` runs, **Then**
   precision, recall, and F1 for each axis are averaged over only the successful calls.
4. **Given** a scripted `JudgeClient` that returns an `Err` from the underlying judge call
   versus one where the judge call succeeds but the cache write fails, **When**
   `score_candidate` runs, **Then** the two failure modes are distinguishable in the
   surfaced error message.

---

### User Story 2 - No behaviour change (Priority: P2)

As someone relying on existing reports, the move is a pure relocation: identical output
for identical input, verified by the existing SC-004 golden in `report.rs`.

**Why this priority**: This is a refactor, not a feature change. If the golden test
changes, the move introduced a regression.

**Independent Test**: Run the existing `report.rs` SC-004 golden test unmodified after the
move and confirm it still passes.

**Acceptance Scenarios**:

1. **Given** the existing SC-004 golden report test in `report.rs`, **When** the code is
   built after `score_candidate`/`judged_f1`/`average` move into the library, **Then** the
   test passes without modification to its expected output.

---

### Edge Cases

- A judge that fails on every call for one axis but succeeds on others.
- A chunk where the reference extraction failed — must not reach judging at all.
- Empty candidate list.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Move `score_candidate`, `judged_f1`, and `average` from `main.rs` into the
  `lcg_eval` library (e.g. a `scoring` module), exported for test use. `main.rs` keeps
  only CLI wiring.
- **FR-002**: No behavioural change. The SC-004 golden in `report.rs` must pass unaltered.
- **FR-003**: Add tests using a scripted `JudgeClient` covering: a failure on one axis
  increments `judge_errors` and does not abort; the other axes for that chunk still
  contribute; precision/recall/F1 are averaged over only successful calls; an all-failures
  axis yields `None` rather than `0.0`.
- **FR-004**: Add a test that a cache-write failure and a judge-call failure are
  distinguishable in the surfaced message (#271 labels them differently, untested).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `score_candidate` is exercised by at least one test that never makes a
  network call.
- **SC-002**: The SC-004 report golden passes without modification, demonstrating the move
  changed no output.
- **SC-003**: Mutating the `Err` arm of any of the three `judged_f1` call sites to abort
  instead of count causes at least one test to fail.

## Assumptions

- `JudgeClient` stays the seam; no new mocking infrastructure is needed.
- Pairwise scoring (`pairwise.rs`) is already in the library and already tested; this is
  only about the reference-mode path that stayed behind in `main.rs`.
- #271 (non-fatal judge-error handling, the `judge_errors` field, and per-axis
  precision/recall) is merged to `main`, so this issue proceeds as a pure relocation with
  no scope change: move the existing behaviour verbatim and add the tests that were
  previously impossible.

## Out of Scope

- Any change to `judged_f1`'s scoring semantics, the retry ladder, or `JudgeCache`
  behaviour — this issue is a pure relocation plus tests, not a behaviour change.
- Pairwise scoring (`pairwise.rs`) — already in the library and already tested.

## Source References

- `crates/eval/src/main.rs` — current location of `score_candidate`, `judged_f1`, `average`.
- `crates/eval/src/judge.rs` — `JudgeClient` trait and `StaticJudge` test double.
- `crates/eval/src/report.rs` — `CandidateReport`, SC-004 golden test.
- `crates/eval/tests/harness_integration.rs` — existing integration test surface, currently
  unable to reach `score_candidate`.
- PR #271 — `fix(eval): survive a judge that corrects itself, and stop discarding
  precision/recall` (merged).

## Labels

fabrik:yolo
