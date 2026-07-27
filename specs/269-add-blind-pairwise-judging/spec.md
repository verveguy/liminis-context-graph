# Feature Specification: blind pairwise judging for lcg-eval

**Feature Branch**: `fabrik/issue-269`
**Created**: 2026-07-27
**Status**: Draft
**Input**: User description: "eval: add blind pairwise judging so quality is measured without a privileged reference"

## Background

`lcg-eval` currently scores every candidate against a privileged reference backend via
judged precision/recall/F1. That measures **similarity to the reference**, not quality.
The reference is not ground truth — it is one more model's output — so a candidate that
extracts an entity the reference missed is scored as a false positive and penalised for
being right. The noise-floor leg (a second independent sample of the reference model)
controls for sampling variance but not for this directional bias: every candidate is
graded on reference-likeness, which structurally advantages the reference's own second
run over any other model.

This feature adds a blind pairwise mode: present the judge with the source chunk and
two *unlabelled* extractions, ask which better captures the content, and aggregate to a
win rate. No backend is privileged.

This is an addition, not a replacement. The two metrics answer different questions and
#248 needs both: pairwise answers "is the candidate good enough", reference-F1 answers
"how much would existing graph content shift if we swapped models" — a migration-risk
question for which deviation-from-current-production is exactly the right framing.

The #248 full-corpus benchmark has three captured cassettes (baseline Haiku, an
independent candidate Haiku sample, and qwen3.6-27b over 228 chunks). Pairwise judging
is a pure scoring-layer pass over those recordings, so it costs **zero extraction** —
judge calls only, re-runnable against data already on disk.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Quality comparison without a privileged reference (Priority: P1)

As someone evaluating whether a local model can replace hosted extraction, I run
`lcg-eval --judge-mode pairwise` over existing cassettes and get a win rate per pair, so
that a candidate which is *better* than the reference is not penalised for deviating
from it.

**Why this priority**: This directly addresses the core problem motivating the issue —
without it, there is no blind, reference-agnostic quality signal at all, and the
existing reference-F1 metric remains structurally biased toward the reference model.

**Independent Test**: Run `lcg-eval --judge-mode pairwise` against the three existing
#248 cassette backends (`cassette:path=<baseline>`, `cassette:path=<candidate>`,
`cassette:path=<qwen>`) and confirm the report contains, for each of the three backend
pairs, per-axis (entities/edges/summary) win/loss/tie counts, win rate, and chunks
compared — with zero outbound extraction calls.

**Acceptance Scenarios**:

1. **Given** three cassette backends recorded from the #248 corpus, **When**
   `lcg-eval --judge-mode pairwise` is run, **Then** the report includes, for each of
   the three backend pairs and for each axis (entities, edges, summary), a win rate,
   loss count, tie count, order-inconsistency rate, and chunks-compared count.
2. **Given** `--judge-mode` is omitted, **When** `lcg-eval` runs, **Then** no pairwise
   section appears in the report and the reference-mode report is byte-identical to the
   pre-existing output (SC-004).

---

### User Story 2 - Judge calibration control (Priority: P1)

As someone who must trust the win rate before acting on it, I get the
reference-vs-its-own-independent-sample pair judged under the identical pairwise
protocol. Two independent samples of the same model should split near 50/50; a
systematic deviation is measuring judge position bias rather than model quality. This
is the pairwise analogue of the noise floor and it must not be optional — an
uncalibrated win rate is unanchored.

**Why this priority**: An uncalibrated win rate cannot be trusted or acted upon. This
control is mandatory, not an optional extra, because it is the only signal that
distinguishes "the candidate is genuinely different" from "the judge has a position
bias."

**Independent Test**: Configure the baseline Haiku cassette and its independent
second-sample cassette as the reference and a candidate backend, run
`lcg-eval --judge-mode pairwise`, and confirm the report surfaces this pair's per-axis
win rate and order-inconsistency rate, with a loud warning if any axis's win rate falls
outside 45–55%.

**Acceptance Scenarios**:

1. **Given** the reference backend and an independent second sample of the same model,
   **When** judged pairwise, **Then** each axis's reported win rate falls within
   45–55%, or the run emits a loud warning naming the observed rate and the axis it
   applies to (SC-001).
2. **Given** any pairwise run, **When** the report is generated, **Then** every reported
   pair/axis combination includes its order-inconsistency rate alongside its win rate —
   a win rate is never reported without it (FR-007).

---

### User Story 3 - No new extraction spend (Priority: P2)

As someone paying for this, a pairwise pass over cassette backends makes zero outbound
extraction calls and reuses the existing judge cache, so re-running a scored comparison
is free.

**Why this priority**: Cost control. Without this guarantee, re-running comparisons to
refine the report format or investigate a surprising result would repeatedly incur
judge-call cost, discouraging the iteration this feature exists to enable.

**Independent Test**: Run `lcg-eval --judge-mode pairwise` twice in succession against
the same cassettes and the same `--judge-cache` path; confirm the run log shows zero
outbound extraction requests on both runs, and zero new judge calls on the second run.

**Acceptance Scenarios**:

1. **Given** all backends are `cassette:path=<PATH>`, **When** pairwise mode runs,
   **Then** the run log shows zero outbound extraction requests (SC-003).
2. **Given** an immediately repeated, identical pairwise run against the same judge
   cache, **When** it executes, **Then** zero new judge calls are made — every
   comparison is served from `JudgeCache` (SC-005).

---

### Edge Cases

- One or both extractions empty for a chunk → a defined verdict per axis, not a panic.
- Judge returns malformed JSON → same recovery/error accounting as the existing
  reference-mode matching path.
- A pair where both sides resolve to the *same* cassette backend (identical `--backend`
  spec used under two different names, or the same name given as both sides of a
  pairing) is degenerate and MUST be rejected at CLI validation time with a clear error
  naming the offending backend — consistent with the existing convention in
  `crates/eval/src/cli.rs` of rejecting nonsensical backend configurations up front
  (duplicate backend names, `--record-cassette` paired with a `cassette:` backend, etc.)
  rather than silently computing and burying a trivial 100%-tie result inside the
  report.
- Very large extractions pushing the pairwise prompt past the judge's context window —
  behavior matches whatever recovery the reference-mode judge path already has for
  oversized payloads; no new truncation behavior is introduced by this feature.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Add `--judge-mode <reference|pairwise|both>`, defaulting to `reference`.
  Omitting the flag must leave existing output byte-identical.

- **FR-002**: The pairwise judge prompt MUST include the source chunk text. The current
  `JudgeClient::judge(prompt_name, reference, candidate)` signature
  (`crates/eval/src/judge.rs:90-99`) never sees the episode body — sufficient for
  deciding whether two entity names are semantically equivalent, but **not** sufficient
  for deciding which of two extractions better captures a source it cannot read. Add a
  `judge_pairwise` method rather than overloading `judge`, so the matching path is
  untouched.

- **FR-003**: The two extractions MUST be presented unlabelled (slot A / slot B) with no
  backend name, model id, or provider reachable by the judge. Slot assignment MUST be
  deterministic and reproducible — derived from a hash of the chunk key plus the backend
  names, not from a random source — so a re-run reproduces the run exactly. Workflow/
  eval scripts in this repo cannot rely on wall-clock or RNG for reproducibility.

- **FR-004**: Each pair MUST be judged in **both** slot orders. Agreeing verdicts count
  as a win for the agreed side; disagreeing verdicts count as a tie and increment an
  order-inconsistency counter. A judge that flips its answer when the operands swap is
  reporting position bias, and that must surface as a number rather than be averaged
  away.

- **FR-005**: The pairwise verdict is a winner in `{A, B, tie}` **per axis** (entities,
  edges, summary) plus a brief rationale. Collapsing the axes to one preference loses
  the distinction between "misses content" and "invents content", which have very
  different operational consequences for a graph.

- **FR-006**: Reuse `JudgeCache` with a distinct `prompt_name` so pairwise entries can
  never collide with matching-mode entries. Note `cache_key` is already order-sensitive
  in its reference/candidate operands (pinned by
  `cache_key_changes_with_reference_or_candidate`), so the two slot orders yield
  distinct keys with no cache changes required.

- **FR-007**: The report MUST emit, per backend pair and **per axis** (entities, edges,
  summary — consistent with the existing `CandidateReport`'s convention of never
  folding judged entity/edge/summary F1 together, `crates/eval/src/report.rs`): wins,
  losses, ties, win rate, the order-inconsistency rate, and the number of chunks
  compared. A win rate published without its inconsistency rate is not interpretable.

- **FR-008**: Pairwise mode MUST cover all backend pairs present in the run, explicitly
  including reference-vs-candidate — that pair is the calibration control of User
  Story 2, not an incidental extra.

- **FR-009**: Pairwise mode MUST work end to end with every backend being
  `cassette:path=<PATH>`, making zero extraction calls.

- **FR-010**: Chunks not present in **both** sides of a pair (cassette coverage differs
  — the #248 captures are 226/223 with a 221 overlap) MUST be excluded from that pair's
  tally and reported as a skipped count, never silently treated as a loss.

- **FR-011**: A pairwise run in which both sides of a configured pair resolve to the
  identical backend (see Edge Cases) MUST be rejected at CLI validation time with an
  error identifying the offending backend name(s), before any judge call is made.

### Key Entities

- **PairwiseVerdict**: The judge's per-axis, per-chunk output for one ordered pair of
  extractions — winner in `{A, B, tie}` for each of entities/edges/summary, plus a
  rationale string.
- **BackendPair**: An unordered pair of configured backend names compared under
  pairwise mode; the run covers every such pair present in the invocation (FR-008).
- **PairwiseReportEntry**: The aggregated per-pair, per-axis report row — wins, losses,
  ties, win rate, order-inconsistency rate, chunks-compared count, and skipped-chunk
  count (FR-007, FR-010).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Over the #248 cassettes, the two independent Haiku samples judged
  pairwise produce, on each axis (entities, edges, summary), a win rate within 45–55%.
  Any axis outside that band causes the run to report a loud judge-bias warning naming
  the observed rate and the axis.
- **SC-002**: The order-inconsistency rate is reported for every pair/axis combination,
  and a documented threshold above which the pairwise result is not to be trusted.
- **SC-003**: A full pairwise pass over three cassette backends makes zero extraction
  calls, verifiable from the run log.
- **SC-004**: With `--judge-mode` omitted, existing reference-mode reports are unchanged
  (regression test against a recorded report).
- **SC-005**: An immediately repeated identical pairwise run makes zero judge calls, all
  served from `JudgeCache`.

## Assumptions

- The judge model is a different family from neither candidate in general, but for the
  #248 run a Sonnet judge scores Haiku against qwen. The SC-001 control detects position
  bias; it does **not** detect same-family stylistic affinity. That limitation is
  documented rather than solved here.
- Existing cassettes remain the input; no re-capture is required.
- Ontology-mode cassettes are a separate axis (keys differ — `cassette.rs:69-88`) and
  are out of scope for this issue.
- The report's win/loss/tie/win-rate/inconsistency-rate fields are broken out per axis
  (entities, edges, summary) rather than collapsed into one pair-level figure — inferred
  from FR-005's explicit rationale for keeping axes distinct and from the existing
  `CandidateReport`'s convention of reporting judged entity/edge/summary F1 as separate
  fields, never combined.
- A degenerate pair (both sides the identical backend) is rejected at CLI validation
  time rather than computed and reported as a trivial 100% tie — consistent with
  `crates/eval/src/cli.rs`'s existing pattern of rejecting nonsensical backend
  configurations before any run starts.

## Out of Scope

- Ontology-mode cassette pairwise judging (separate key scheme; a candidate follow-up
  issue).
- Re-capturing or recording new cassettes — this feature is a pure scoring-layer pass
  over cassettes already on disk.
- Any change to the existing reference-mode judge path, prompt, or cache key scheme.
- Truncation or chunking strategy for oversized pairwise prompts beyond whatever the
  reference-mode judge path already does — noted as an edge case, not solved here.

## Source References

- `crates/eval/src/judge.rs:90-99` — `JudgeClient::judge` signature that FR-002's new
  `judge_pairwise` method is added alongside.
- `crates/eval/src/judge_cache.rs` — `JudgeCache`/`cache_key`, reused per FR-006.
- `crates/core/src/cassette.rs:69-88` — cassette key scheme; ontology-mode keys differ
  and are out of scope.
- `crates/eval/src/report.rs` — existing `CandidateReport`/`Report` structures whose
  per-axis convention this feature's report extends.
- `crates/eval/src/cli.rs` — existing backend-configuration validation conventions
  (duplicate names, cassette/record-cassette conflicts) that FR-011 follows.
- Issue #248 — full-corpus benchmark and the three cassettes this feature scores.
- Issue #232 / #263 — cassette recording/replay infrastructure.
