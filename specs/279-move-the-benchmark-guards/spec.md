# Feature Specification: move the benchmark guards out of shell and into lcg-eval

**Feature Branch**: `fabrik/issue-279`
**Created**: 2026-07-29
**Status**: Draft
**Input**: User description: "eval: move the benchmark guards out of shell and into lcg-eval — the `crates/eval/scripts/` wrappers have accumulated decision logic that belongs to `lcg-eval`'s own domain: cassette completeness, distinguishing a corrupt cassette from a duplicated one, resolving replay-vs-live per backend, detecting two legs that would destroy the noise floor, and validating a report against the cassettes it was scored from. Move the guards into the binary and let the scripts shrink to what shell is good at: read environment, loop over modes, invoke the binary."

## Background

`crates/eval/scripts/_common.sh`, `04-full-run.sh`, and `05-score-only.sh` each embed a
Python heredoc that hashes cassette files, checks for duplicate keys, and computes coverage
overlap — logic that decides whether a multi-hour, real-money benchmark run is trustworthy.
`lcg-eval` (the Rust binary these scripts invoke) already has every input those decisions
need: the corpus size, the requested scope (`--limit` or full corpus), the parsed cassette
records, and the report it writes at the end of a run. The scripts reimplement that
knowledge in bash and inline Python, where it cannot be typed, is awkward to unit test, and
diverges by platform.

This move was raised by review experience, not by inconvenience. PR #278 (`feat(eval): add
the three-backend ontology matrix script`) took **nine review rounds** on a single shell
script, and the findings were not a scatter of unrelated mistakes — they were the same
failure repeating in forms that only shell permits:

| Finding | Why shell allowed it |
|---|---|
| `md5 -q` reported two different files as identical on Linux | platform builtin instead of a library; failed only where CI runs |
| `expected * 9 / 10` accepted 205/228 as "at least 90%" | integer arithmetic by hand |
| a corrupt cassette reported as "appended to, not moved aside" | one exit code standing in for several failure modes, with no type to separate them |
| a 90% proportion used where `chunks_run - errors` was already known | reimplementing arithmetic the report contained |
| the identity guard silently disabled under `LIMIT` | an omitted argument became a default, and the default disabled the check |
| `${VAR:-}` vs `${VAR-}`, twice | shell semantics with no compiler |
| two heredoc quoting failures | embedding Python inside shell |

None of those failure modes can occur once the same logic is a typed Rust function with unit
tests. For contrast, the Rust changes made the same day (#271, #275, #276, #277) each took
one or two review rounds, because types and existing tests caught the mistakes first — #276's
cache-disjointness bug was caught by a pre-existing test in seconds.

`crates/eval/scripts/test-scripts.sh` (67 shell-level checks, wired into CI as PR #278 adds
it) has stopped the immediate bleeding, but it is testing logic that should not live in shell
at all. This issue moves that logic — and the tests that pin it — into `lcg-eval` itself, so
the check does not depend on which wrapper invokes it, or on a wrapper existing at all.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - the binary refuses a run that cannot produce a valid measurement (Priority: P1)

As someone about to spend money on a multi-hour benchmark run, `lcg-eval` itself rejects the
configurations that would make the result meaningless — two backends resolving to the same
cassette, a cassette with duplicate keys, a cassette that does not cover the requested scope
— so the check does not depend on which wrapper invoked it, or on a wrapper existing at all.

**Why this priority**: This is the actual defect class the nine review rounds on PR #278
were catching by hand. Without it, every new script (or a bare `lcg-eval` invocation with no
wrapper) can silently produce an invalidated report.

**Independent Test**: Run `lcg-eval` directly (no shell wrapper) against a hand-crafted
duplicate-keyed cassette, a corrupt cassette, and two `--backend` specs pointing at the same
cassette path; each must fail loudly with a distinct, typed error before any outbound call.

**Acceptance Scenarios**:

1. **Given** two `--backend` specs resolve to the same cassette path, **When** `lcg-eval` is
   invoked, **Then** it fails before making any outbound request and names both backends and
   the shared path.
2. **Given** two `cassette` backends point at different paths whose file contents hash
   identically, **When** `lcg-eval` is invoked, **Then** it is rejected the same way as a
   shared path.
3. **Given** a cassette file contains two records with the same key, **When**
   `ReplayingExtractor::load` loads it, **Then** loading fails with an error naming the file
   and an example duplicate key, instead of silently queuing the duplicate for FIFO replay.
4. **Given** a cassette is malformed JSON, contains a non-object record, a record missing
   `key`, or a record with a non-string `key`, **When** it is loaded, **Then** it is reported
   as corrupt, distinguishable in the error type (not just the exit code) from a
   duplicate-key rejection.
5. **Given** a cassette covers fewer chunks than the requested scope (full corpus or
   `--limit N`), **When** `lcg-eval` runs, **Then** the coverage shortfall is reported
   explicitly rather than left for the caller to infer from a proportion.

---

### User Story 2 - a dry run shows the resolved plan (Priority: P1)

As someone previewing an expensive run, `lcg-eval --dry-run` prints what it would do — each
backend's resolved spec, replay-or-live decision, cassette record count against the
requested scope, and output paths — and exits without making a single outbound call. The
plan cannot drift from the real run, because the same resolution code produces both.

**Why this priority**: PR #278 needed this exact capability (`DRY_RUN=1`) reimplemented in
shell, and it had already drifted from the real run's decisions twice during review (dry-run
printing cassette names the run would not use; dry-run omitting the conditions that would
abort a real run). A dry-run that can drift from the real path is worse than no dry-run.

**Independent Test**: Run `lcg-eval --dry-run` with a mix of `cassette` and live backend
specs, with no API key set and no server running; it must print the resolved plan and exit 0
with no network activity.

**Acceptance Scenarios**:

1. **Given** a valid multi-backend invocation, **When** run with `--dry-run`, **Then** it
   prints, per backend, the resolved spec, the replay-or-live decision, the cassette record
   count (for cassette backends), and the requested scope (`--limit N` or full corpus), and
   exits 0 having made zero outbound calls.
2. **Given** `--dry-run` is combined with `--record-cassette`, **When** run, **Then** no
   cassette file is created or modified.
3. **Given** a configuration that would fail one of the User Story 1 guards (e.g. identical
   cassettes, a duplicate-keyed cassette), **When** run with `--dry-run`, **Then** the plan
   output names the guard that would fail the real run, so a preview cannot look clean for a
   configuration that would abort.

---

### User Story 3 - the wrappers get smaller, not smarter (Priority: P2)

As someone maintaining `crates/eval/scripts/`, each script reads environment, loops over
modes, and invokes the binary. Guard logic removed from a script is deleted, not duplicated,
and the corresponding cases in `test-scripts.sh` move to Rust tests.

**Why this priority**: This is the cleanup that makes User Stories 1 and 2 actually pay off —
without it, the same logic exists twice (once correct in Rust, once stale in shell) and the
next reviewer has no way to know which copy is authoritative.

**Independent Test**: Diff `_common.sh`, `04-full-run.sh`, `05-score-only.sh`, and
`06-ontology-matrix.sh` before and after; the "after" versions contain no hashing, no
completeness-threshold arithmetic, and no cassette-record parsing.

**Acceptance Scenarios**:

1. **Given** the guards now live in `lcg-eval`, **When** `_common.sh`, `04-full-run.sh`,
   `05-score-only.sh`, and `06-ontology-matrix.sh` are inspected, **Then** they contain only
   environment reads, mode/leg looping, and binary invocations — no hashing, no completeness
   arithmetic, no cassette parsing.
2. **Given** a `test-scripts.sh` case exercised guard logic that has moved into `lcg-eval`,
   **When** the migration is complete, **Then** an equivalent Rust unit test exists and the
   shell case is removed rather than left as a second copy.
3. **Given** a `test-scripts.sh` case exercises genuinely shell-level behavior (env-var
   contracts, mode looping, artifact naming), **When** the migration is complete, **Then**
   that case remains in `test-scripts.sh` unchanged.

---

### Edge Cases

- `--dry-run` combined with `--record-cassette`: nothing should be written.
- A cassette that is valid but covers a *superset* of the requested scope (a full capture
  replayed under `--limit 3`) must be accepted, not flagged as a coverage mismatch.
- Backends legitimately sharing a *model* but not a cassette must not be rejected — the
  identity guard applies to cassette content/path, not to model name.
- An empty cassette: distinguish "loaded successfully with zero records" from "unreadable" —
  both are failure-adjacent but are different diagnoses for a caller to act on.
- A cassette record count that *equals* the requested scope exactly must be accepted (the
  coverage check is `>=`, not `>`).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `lcg-eval --dry-run` resolves every `--backend` spec, reports for each whether
  it would replay or run live, and exits 0 without any outbound request. Output includes the
  cassette record count and the requested scope (`--limit N` or the full corpus).

- **FR-002**: `ReplayingExtractor::load` rejects a cassette containing duplicate keys, with an
  error naming the file and an example key. Replay currently serves duplicates FIFO, so a
  chunk is silently scored against a stale verdict.

- **FR-003**: Distinguish a corrupt cassette from a duplicated one in the error type, not by
  exit code. Malformed JSON, a record that is not an object, a missing `key`, and a
  non-string `key` are all *corruption*; only repeated keys are *duplication*.

- **FR-004**: Reject a configuration where two backends resolve to the same cassette path, or
  where two cassette backends' contents hash equal. Two identical legs make judged F1 1.000
  by construction and the noise floor read as zero — a silent invalidation that still emits a
  plausible report.

- **FR-005**: Report cassette coverage against the requested scope. The binary knows the
  corpus size and `--limit`, so a cassette that covers fewer chunks than requested is
  reported as such rather than inferred from a proportion by a caller.

- **FR-006**: After writing a report, verify each recorded cassette's record count equals that
  backend's `chunks_run - errors`, and fail if not. A mismatch means the capture was truncated
  and the report was scored against partial data. A high `error_rate` is a quality signal, not
  a truncation signal, and must not fail the run.

- **FR-007**: Delete the moved logic from `_common.sh`, `04-full-run.sh`, `05-score-only.sh`
  and `06-ontology-matrix.sh` rather than leaving both copies. Migrate the corresponding
  `test-scripts.sh` cases to Rust tests, keeping only those that test genuinely
  shell-level behaviour (env-var contracts, mode looping, artifact naming).

### Key Entities *(if the feature involves data)*

- **Backend Spec**: The parsed and resolved form of a `--backend NAME=SPEC` argument
  (`anthropic`, `oai-http`, `oai-uds`, or `cassette:path=<PATH>`), including its derived
  replay-or-live decision. `--dry-run` and a real run resolve backend specs through the same
  code path.
- **Cassette**: A JSONL file of recorded `Extractor` calls (`crates/core/src/cassette.rs`).
  A cassette load can succeed, fail as *corrupt* (malformed JSON, non-object record, missing
  or non-string `key`), or fail as *duplicated* (a repeated key) — three distinct outcomes,
  not one exit code.
- **Dry-Run Plan**: The output `--dry-run` produces: one entry per backend with its
  replay/live decision, cassette record count, and the requested scope, plus any guard that
  would abort a real run — without performing the run.
- **Report Validation**: The post-write check comparing each recorded cassette's record count
  against `chunks_run - errors` for that backend, run automatically after `lcg-eval` writes a
  report.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `lcg-eval --dry-run` over the existing #248 cassettes makes zero outbound calls,
  verifiable from the run log, and names each backend's replay/live decision.
- **SC-002**: Each guard has a Rust unit test, including the cases that reached production in
  shell: a duplicate-keyed cassette, a corrupt cassette, two backends sharing a path, two
  cassette backends with identical content, and a truncated capture.
- **SC-003**: `06-ontology-matrix.sh` is materially smaller, with no completeness arithmetic,
  no hashing, and no cassette parsing.
- **SC-004**: The reference-mode report for a given corpus and backend set is unchanged by
  this refactor — it moves checks, it does not change measurement.
- **SC-005**: Mutating any moved guard to a no-op fails at least one Rust test.

## Assumptions

- `--dry-run` is a new flag on the existing CLI, not a separate binary.
- The judge cache and cassette file formats do not change; this is validation and reporting
  only.
- `run_mode_matrix.sh` benefits from the same guards for free once they are in the binary, and
  is not otherwise in scope.
- `--dry-run` exits 0 for a syntactically valid invocation (parseable flags and backend
  specs) even when the resolved plan shows a guard that would abort a real run — the failing
  guard is reported in the plan output (User Story 2, Acceptance Scenario 3), not surfaced as
  a nonzero exit. A nonzero exit from `--dry-run` is reserved for a usage error (e.g. an
  unparseable `--backend` spec), consistent with FR-001's "exits 0 without any outbound
  request."
- `_common.sh`, `04-full-run.sh`, and `05-score-only.sh` already exist on `main` and already
  contain the Python-heredoc guard logic this issue targets (hashing, duplicate-key checks,
  coverage arithmetic) independent of any other in-flight work. `06-ontology-matrix.sh` and
  `test-scripts.sh`, however, exist only on the still-open PR #278
  (`feat/ontology-matrix-script`). This issue's FR-007 scope over those two files applies
  once PR #278 has merged to `main`; if it has not merged by the time this issue reaches
  Implement, the `06-ontology-matrix.sh`/`test-scripts.sh` portion of FR-007 is deferred
  without blocking FR-001 through FR-006 and the `_common.sh`/`04`/`05` portion of FR-007.

## Out of Scope

- Changes to the cassette JSONL file format or the judge cache format.
- Changes to `run_mode_matrix.sh` itself (it inherits the guards once they live in the
  binary, per the Assumptions above, but is not otherwise touched).
- Any change to the *measurement* logic (F1 computation, judge prompts, pairwise scoring) —
  this issue is validation and reporting only (SC-004).
- The ontology-matrix script's own domain logic (mode-to-cassette-name mapping, the
  three-backend noise-floor design) introduced by PR #278 — this issue migrates that script's
  *guards* into the binary, not its feature logic.

## Source References

- `crates/eval/scripts/_common.sh`, `04-full-run.sh`, `05-score-only.sh` (on `main`) — the
  Python-heredoc hashing, duplicate-key, and coverage-overlap checks this issue moves.
- `crates/core/src/cassette.rs` — `ReplayingExtractor::load` and `CassetteRecord`, the
  existing FIFO-duplicate-serving behavior FR-002 changes.
- `crates/eval/src/{cli.rs,backend.rs,report.rs}` — existing `--backend`/`--limit`/`--output`
  parsing and the `CandidateReport` `chunks_run`/`errors` fields FR-006 validates against.
- PR #278 (`feat/ontology-matrix-script`, open as of this spec) — introduces
  `06-ontology-matrix.sh`, `test-scripts.sh`, and `validate_report.py`, all referenced by
  FR-007.
