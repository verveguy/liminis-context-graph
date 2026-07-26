# Feature Specification: Benchmark run — full-corpus extraction on Anthropic vs local qwen3.6-27b, capturing cassettes for both

**Feature Branch**: `fabrik/issue-248`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "Benchmark run: full-corpus extraction on Anthropic vs local qwen3.6-27b, capturing cassettes for both"

## Background

The README tells users that extraction can run fully local against *"a quality-verified model
like `qwen3.6-27b`"* (#212). That claim is **inherited, not measured**:

- The supporting figures (judged F1 0.894 / 0.852 / 0.900 vs a 0.990 / 0.978 hosted noise
  floor) come from the **predecessor Python pipeline**, with **pre-2026-04-30 prompts**, on a
  **private corpus** — see #227.
- This engine's local path is materially different: the Anthropic path gets
  **schema-enforced tool use** (`tools`/`tool_choice`/`tool_use`), while `OaiExtractor`
  coerces structure via `response_format: {"type": "json_object"}` plus a literal shape
  instruction and defensive parsing (ADR-0041). That is a weaker guarantee, so the
  local-vs-hosted gap on this engine may be **wider** than the inherited 7pp.
- #212's own ADR concedes end-to-end quality *"can only be confirmed by manual testing…
  not by this repo's standard CI."*

Separately, no LLM cassette existed at the time this issue was filed (#232), so extraction had
no deterministic regression coverage.

**Both gaps close with the same work.** A full-corpus extraction pass per model, with cassette
recording enabled, produces the benchmark data *and* the cassette in one run. Running them
separately would pay for the corpus twice per model.

### What has changed since this issue was filed

Both blocking dependencies have since merged:

- **#228** shipped `crates/eval` (`lcg-eval` binary): a Rust extraction-quality eval harness
  that calls this engine's own prompts/extractor clients, scored by an LLM-as-judge, with
  full support for `--backend NAME=SPEC` (hosted Anthropic or any OpenAI-compatible local
  endpoint), `--reference` (for a self-comparison noise floor), `--all` (full 228-article
  corpus), `--record-cassette NAME=PATH`, `--judge-model`/`--judge-cache`, and a report
  covering judged F1 (nodes/edges/summaries), latency percentiles, error rate, and
  structured-output reliability (clean/recovered/malformed JSON parse counts, per ADR-0048's
  `StructuredOutputParse` telemetry).
- **#232** shipped `crates/core/src/cassette.rs`: the record/replay mechanism (`LCG_RECORD_LLM`/
  `LCG_REPLAY_LLM`, `RecordingExtractor`/`ReplayingExtractor`) that `--record-cassette` wraps.

So the tooling this issue originally needed already exists in full. What remains is running it
and publishing the results — this issue does not need to add new capability to `crates/eval`
or `crates/core`.

### Why the actual runs cannot happen inside this Fabrik pipeline

The full-corpus runs require two things this sandbox does not have:

1. A local OpenAI-compatible server hosting `qwen3.6-27b` (e.g. `mlx_lm.server`) — `mlx_lm` is
   not installed here and the model (~16 GB) is not in the local cache.
2. A live, spend-authorized `ANTHROPIC_API_KEY` for the hosted extraction leg and for
   LLM-as-judge scoring — no such key is present in this environment.

This is not a new discovery: the issue's own Risks/Dependencies section, ADR-0048's own
Context section, and `.github/workflows/eval.yml`'s inline comments all already describe #248
as **"a maintainer-run operation requiring a local model server and real spend."** It mirrors
the precedent already set by #217 (the golden WAL/corpus capture) and #232 (the cassette
mechanism itself), both of which shipped mechanism and documented procedure while explicitly
deferring their own one-time paid/local capture step to a maintainer, rather than attempting it
unattended during automated implementation.

Given that, this spec scopes the Fabrik-automatable deliverable as **a documented runbook and
documentation scaffolding**, and treats **executing the runbook, committing the resulting
cassettes, and filling in measured figures** as a manual maintainer follow-up. See Assumptions
below.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A maintainer can run the full benchmark from documented commands alone (Priority: P1)

As a maintainer with a local `mlx_lm.server` running `qwen3.6-27b` and a paid
`ANTHROPIC_API_KEY`, I want an exact, copy-pasteable runbook using the existing `lcg-eval`
harness to run (a) a hosted-vs-itself noise-floor pass and (b) a hosted-vs-qwen comparison,
both over the full 228-article corpus with cassette recording enabled for every backend
involved — so I can produce the benchmark data and both cassettes in one paid/compute pass,
without re-deriving flags from source.

**Why this priority**: nothing else in this issue can happen until the runs exist, and the
runs must happen outside Fabrik regardless of how the rest of the issue is scoped.

**Independent Test**: given `ANTHROPIC_API_KEY` and a reachable local `qwen3.6-27b` endpoint, a
maintainer follows only the documented runbook (no source-reading, no undocumented flags) and
ends up with two cassette files and two eval reports.

**Acceptance Scenarios**:

1. **Given** the runbook, **When** a maintainer runs the documented noise-floor command,
   **Then** it invokes `lcg-eval` over the full corpus (`--all`) with the same backend spec
   configured as both `--reference` and a second `--backend` under different names, cassette
   recording enabled for at least the reference backend, producing a cassette and a report
   whose judged F1 lands near the established ceiling while strict-string F1 is materially
   lower (the noise-floor property `eval.yml`'s small-scale smoke pass already checks).
2. **Given** the runbook, **When** a maintainer runs the documented hosted-vs-qwen command,
   **Then** it invokes `lcg-eval` over the full corpus with cassette recording enabled for
   *both* backends, producing two cassettes (`anthropic-<model>.jsonl` and
   `qwen3.6-27b.jsonl`) and one report covering, per backend, judged F1 for
   nodes/edges/summaries, latency percentiles, error rate, and structured-output reliability
   (clean/recovered/malformed counts).

---

### User Story 2 - Measured results replace inherited figures in the documentation (Priority: P2)

As a maintainer who has run the benchmark and holds the resulting cassettes and eval reports, I
want #227's documentation and the README's local-extraction wording updated to carry the
measured figures for this engine, clearly distinguished from the inherited prior-art figures —
so anyone reading the README or the eval-quality doc sees what was actually measured on this
engine and corpus, not just ported research from a predecessor pipeline.

**Why this priority**: this is the actual point of the issue (closing the "inherited, not
measured" gap), but it structurally depends on User Story 1's output existing first — it
cannot be done from placeholder or assumed numbers.

**Independent Test**: given a completed eval report (JSON output from `lcg-eval --output`), the
documentation update can be written and reviewed without re-running anything.

**Acceptance Scenarios**:

1. **Given** a completed run's JSON report, **When** the documentation is updated, **Then**
   `docs/extraction-quality-evaluation.md` gains a new, dated section presenting this engine's
   measured nodes/edges/summaries judged F1 for `qwen3.6-27b` against the established noise
   floor, plus structured-output failure rates for both backends — clearly labeled as measured
   on this engine and corpus, and visibly distinct from the existing ported/historical section
   (which remains, relabeled as prior art rather than being overwritten or deleted).
2. **Given** the same report, **When** the README is updated, **Then** its "quality-verified"
   wording and its `qwen3.6-27b` guidance reflect the measured result — including weakening or
   removing the "quality-verified" framing if the measured local-vs-hosted gap is materially
   worse than the inherited figures.

---

### Edge Cases

- **The measured result is worse than the inherited claim.** Documentation MUST report the
  actual number and the README MUST be edited to match, even if that means walking back
  "quality-verified" language — the issue explicitly anticipates this ("Be prepared for the
  claim to fail... the honest outcome is amending the README, not re-running until the number
  looks acceptable").
- **`ANTHROPIC_API_KEY` unset when the runbook commands are run.** The existing harness already
  degrades gracefully — it still runs and reports strict-string F1, but skips judged scoring
  and reports no judged F1. The runbook must state this as a precondition rather than let a
  maintainer discover it mid-run.
- **Re-running the runbook after the judge cache is already populated.** The harness's existing
  on-disk judge cache makes a same-corpus, same-backend re-run free (no new judge calls) — the
  runbook should note this so re-runs (e.g. after a partial failure) aren't assumed to be costly.
- **This issue's Fabrik-driven work completes before a maintainer has run the benchmark.** User
  Story 2's documentation update cannot happen yet in that case — the resulting PR must say so
  explicitly rather than fabricate placeholder numbers or claim the benchmark is done.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A documented runbook MUST give the exact commands, using the existing `lcg-eval`
  binary, to run a hosted-vs-itself noise-floor pass over the full 228-article corpus
  (`--all`), with cassette recording enabled.
- **FR-002**: The same runbook MUST give the exact commands to run a hosted-Anthropic-vs-local-
  qwen3.6-27b comparison over the same full corpus, with cassette recording enabled for both
  backends, producing `anthropic-<model>.jsonl` and `qwen3.6-27b.jsonl`.
- **FR-003**: The runbook MUST state its operational prerequisites explicitly: a reachable
  local OpenAI-compatible server hosting `qwen3.6-27b` (e.g. `mlx_lm.server`), and a valid
  `ANTHROPIC_API_KEY` for both the hosted extraction leg and LLM-as-judge scoring.
- **FR-004**: Both runs MUST use the committed `corpus_prose.jsonl` fixture
  (`crates/core/tests/fixtures/real_corpus_wal/`, #217) as input — the runbook MUST NOT
  instruct refetching Wikipedia, so both runs see byte-identical inputs.
- **FR-005**: The eval report from the hosted-vs-qwen run MUST cover, per backend: judged F1
  for nodes, edges, and summaries; latency percentiles; error rate; and structured-output
  reliability (clean/recovered/malformed JSON counts) — all already produced by the existing
  harness, so this requirement is about the runbook invoking it correctly, not building new
  reporting.
- **FR-006**: Once a maintainer supplies a completed run's output (report + cassettes),
  `docs/extraction-quality-evaluation.md` MUST gain a new, dated section presenting this
  engine's measured figures, explicitly distinguished from the existing ported/historical
  section (which MUST remain, relabeled as prior art rather than overwritten).
- **FR-007**: Once measured figures exist, the README's local-extraction wording (the
  "quality-verified" claim and the `qwen3.6-27b` guidance) MUST be updated to reflect what was
  actually measured on this engine — including weakening or removing the "quality-verified"
  framing if the measured gap is materially worse than the inherited figures.
- **FR-008**: The two cassette files, once produced, MUST be committed in the JSONL format
  #232 already established (one record per line: `key`, `call_type`, `provider`, `model`,
  `timestamp`, `request`, `response`), size-managed consistent with the existing fixture-
  hygiene precedent (#217's compressed/trimmed WAL fixture) if they turn out to be large.
- **FR-009**: This issue's Fabrik-automated deliverable (runbook + documentation scaffolding)
  MUST NOT attempt to install `mlx_lm`, download `qwen3.6-27b`, start a local model server, or
  make live `ANTHROPIC_API_KEY` calls — those are maintainer-run steps outside this pipeline's
  sandbox, consistent with #217's and #232's precedent for operations of this kind.

### Key Entities

- **Noise-floor report**: the hosted-vs-itself eval report establishing the judged-F1 ceiling
  against which the local candidate is read.
- **Hosted-vs-qwen report**: the eval report comparing `claude-haiku-4-5-20251001` against
  `qwen3.6-27b` over the full corpus.
- **`anthropic-<model>.jsonl` / `qwen3.6-27b.jsonl`**: the two committed LLM cassettes captured
  during the full-corpus runs.
- **Measured-results doc section**: the new, dated section in
  `docs/extraction-quality-evaluation.md` distinguishing this engine's measured figures from
  the ported historical figures.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A maintainer can execute the full benchmark (both legs) using only the
  documented runbook commands, without reading source code or asking a clarifying question.
- **SC-002**: The hosted-vs-qwen eval report includes per-backend judged F1
  (nodes/edges/summaries), latency percentiles, error rate, and structured-output reliability,
  over the full 228-article corpus.
- **SC-003**: `docs/extraction-quality-evaluation.md` contains a section presenting this
  engine's measured figures once a run is supplied, visibly distinct from the historical/
  inherited section.
- **SC-004**: The README's "quality-verified" language matches the measured result, whatever it
  turns out to be — including a downgraded claim if warranted.
- **SC-005**: The two committed cassette files replay deterministically with zero live calls,
  verified using the existing #232 replay mechanism.

## Assumptions

- The full-corpus extraction runs themselves (installing `mlx_lm`, downloading `qwen3.6-27b`,
  running `mlx_lm.server`, and making the live `ANTHROPIC_API_KEY` calls) cannot happen inside
  this Fabrik pipeline's sandbox — confirmed directly (no `ANTHROPIC_API_KEY` in this
  environment, `mlx_lm` not installed) and independently corroborated by the issue's own Risks
  section, ADR-0048, and `.github/workflows/eval.yml`'s comments, all of which already describe
  #248 as a maintainer-run operation. This mirrors the precedent set by #217 and #232, both of
  which shipped mechanism/documentation while deferring their own paid/local capture step to a
  maintainer follow-up.
- Given #228 and #232 are both fully merged, `lcg-eval` already supports every mechanism this
  issue needs (`--backend`, `--reference`, `--all`, `--record-cassette`, `--judge-model`,
  `--judge-cache`, `--corpus`) — no new code is expected in `crates/eval` or `crates/core`; the
  Fabrik-driven work is a documented runbook plus documentation scaffolding, not new harness
  features.
- This issue's Fabrik-driven PR is expected to close the "runbook exists, documentation is
  ready to receive results" portion of the original Acceptance Criteria. The portions requiring
  actual measured numbers (cassettes committed, docs/README carrying measured figures) depend
  on a maintainer executing the runbook afterward and supplying the output, fed back via a
  follow-up commit or comment.
- "`claude-haiku-4-5`" in the original issue text refers to this repo's existing default
  extraction model id, `claude-haiku-4-5-20251001` (the `LCG_EXTRACTION_LLM` default).
- Corpus/fixture paths, cassette record/replay mechanics, and the eval harness's reporting
  fields are exactly what #217/#228/#232 already shipped — this spec does not re-specify them,
  only how #248 uses them.

## Out of Scope

- Evaluating additional local models beyond `qwen3.6-27b` (a documented follow-up).
- Changing the default extraction backend.
- Apple Foundation Models evaluation (already assessed and rejected — see
  `docs/extraction-quality-evaluation.md`).
- Building any new eval-harness functionality — #228 already delivered `lcg-eval` in full; this
  issue only uses it.
- Actually executing the two full-corpus runs as part of this Fabrik pipeline run (see
  Assumptions) — that is a maintainer follow-up.

## Source References

- #227 / `docs/extraction-quality-evaluation.md`: the documentation this issue's measured
  figures get published into.
- #228 / `crates/eval`, ADR-0048: the eval harness (fully merged).
- #232 / `crates/core/src/cassette.rs`, ADR-0044: the record/replay mechanism.
- #212 / #217: README's "Extractor: local or hosted" section, `corpus_prose.jsonl` fixture.
- `.github/workflows/eval.yml`: existing small-scale noise-floor smoke-check precedent.
