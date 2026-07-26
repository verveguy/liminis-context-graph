# Feature Specification: Rust extraction-quality eval harness (replay + LLM-as-judge) over the public Wikipedia corpus

**Feature Branch**: `fabrik/issue-228`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "Rust extraction-quality eval harness (replay + LLM-as-judge) over the public Wikipedia corpus"

## Background

There is no way to measure extraction quality in this repo. That gap is now blocking a real decision: the local/OpenAI-compatible extraction adapter (#212) makes "fully local" reachable, but its ADR concedes quality *"can only be confirmed by manual testing on macOS with Apple Intelligence enabled, not by this repo's standard CI"* — so the restored "extraction can run fully local" claim rests on nothing measurable.

Prior research exists (see the companion documentation issue) but has two disqualifying problems for reuse here:

1. **It's a Python package** — 13 modules, `pyproject` + `uv.lock` + venv. This repo has standalone Python *scripts* but no Python package/dependency tree, and adding one taxes every contributor and CI run in an otherwise `cargo`-only project.
2. **Its numbers went stale, structurally.** The harness replayed message arrays captured from a *separate* codebase's prompts. When those prompts were restructured (2026-04-30), every absolute F1 number silently became untrustworthy — the harness had no coupling to the prompts it was measuring.

This issue is scoped to the harness only. Implement a Rust extraction-quality eval harness **in this repo**, calling this engine's own prompts and extractor clients, scored by an LLM-as-judge, over the public Wikipedia corpus. Being in-repo is the point: a prompt change either updates the eval or breaks its build, which structurally prevents the staleness class that invalidated the prior results.

The actual model comparison runs (a full-corpus pass against hosted Anthropic and local qwen3.6-27b, capturing cassettes and publishing measured results) are split out to **#248**, because that work needs a local model server, an API key, and real spend — a maintainer-run operation rather than pipeline work. This issue builds the harness; it does not execute that comparison.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Compare hosted vs. local extraction quality (Priority: P1)

A maintainer wants to know whether an OpenAI-compatible local extraction backend produces comparable output quality to the hosted Anthropic baseline, so they can decide whether "fully local" extraction is viable.

**Why this priority**: This is the core problem the issue exists to solve — the local extraction adapter's quality claim currently rests on an ADR caveat instead of measurable evidence.

**Independent Test**: Run the harness with the Anthropic baseline and one OpenAI-compatible local endpoint configured over the default corpus; confirm a comparison report is produced with per-candidate F1 (nodes/edges/summaries), latency percentiles, and error rate.

**Acceptance Scenarios**:

1. **Given** the harness is configured with the Anthropic baseline and a local OpenAI-compatible backend, **When** the maintainer runs the harness over the default corpus, **Then** it produces a report with per-candidate F1 for nodes/edges/summaries, latency percentiles, and error rate for each backend.
2. **Given** a previous run has populated the on-disk judge cache, **When** the harness is re-run against the same corpus and backends, **Then** no new judge calls are made.

---

### User Story 2 - Validate the judge against a known noise floor (Priority: P1)

A maintainer wants confidence that the LLM-as-judge scoring is meaningfully better than strict string matching before trusting any comparison it produces.

**Why this priority**: The judge prompt and taxonomy are the hard-won part of this work (strict-string scoring put sonnet-vs-sonnet at 0.771 on edges — a 23% floor from pure wording variance — while the judge put it at 0.978). If the ported judge doesn't reproduce that property, its scores can't be trusted.

**Independent Test**: Run a baseline-vs-itself comparison (same backend as both "candidate" and "reference") and confirm the judged score lands near the noise floor while the strict-string-match score is materially lower.

**Acceptance Scenarios**:

1. **Given** the same backend is used as both candidate and reference, **When** the harness scores the comparison, **Then** the judged metric reports a near-noise-floor score and the strict-string-match metric reports a materially lower score.

---

### User Story 3 - Measure structured-output reliability (Priority: P2)

A maintainer wants to know how often a candidate backend fails to produce parseable structured output, separately from extraction quality, because the local path coerces structure via `response_format: json_object` + prompt instruction (ADR-0041) rather than schema-enforced tool use — a failure mode the hosted path structurally can't exhibit the same way.

**Why this priority**: This is precisely the axis where the local path may diverge from the hosted path, and it's an axis the inherited prior research could not measure.

**Independent Test**: Run the harness against a backend and confirm the report includes a structured-output reliability metric (malformed/unparseable JSON rate, defensive-parse recoveries) alongside F1.

**Acceptance Scenarios**:

1. **Given** a backend run over the corpus, **When** the report is generated, **Then** it includes malformed/unparseable JSON rate and defensive-parse recovery count as first-class metrics, not folded into F1.

---

### User Story 4 - Capture cassettes during a full-corpus benchmark pass (Priority: P2)

A maintainer running the full-corpus comparison (#248) wants a single pass per backend to yield both the eval report and a cassette recording, so the corpus isn't traversed (and paid for) twice per model.

**Why this priority**: Cassette recording (#232) and this harness serve different but overlapping full-corpus passes; without this capability, #248 would need to run the corpus twice per backend, doubling cost.

**Independent Test**: Run the harness with cassette recording enabled for a backend and confirm both the eval report and a cassette are produced from the same run.

**Acceptance Scenarios**:

1. **Given** cassette recording is enabled for a backend, **When** the harness runs that backend over the corpus, **Then** it produces both the eval report and a recorded cassette from the single pass.

**Note**: This story is blocked on #232, which provides the recording interface this harness builds against.

---

### Edge Cases

- A backend request fails outright (timeout, connection error, non-2xx) — the error is counted in the error-rate metric rather than silently dropped or crashing the run.
- A backend returns unparseable/malformed structured output — this is counted in the structured-output reliability metric, not silently treated as a zero-F1 extraction result.
- A corpus item scores identically across repeated runs against the same cached judge entries (cache hit produces the same score, not a fresh judge call).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The harness MUST exist as a Rust workspace member (e.g., a `crates/eval` crate or example binary) — no Python package, `pyproject`, or `uv.lock` added to the repo.
- **FR-002**: The harness MUST load a corpus of source chunks and run each through N configured extractor backends.
- **FR-003**: The harness MUST reuse the existing extractor clients — `AnthropicExtractor` for the hosted baseline and `OaiExtractor` (#212) for any OpenAI-compatible local endpoint — and MUST NOT reimplement HTTP/JSON client logic.
- **FR-004**: The harness MUST call this engine's actual prompts from `crates/core/src/prompts/` directly, not a captured or copied version, so a prompt change is visible to the eval.
- **FR-005**: The harness MUST score candidate output against a reference using LLM-as-judge, with a persistent on-disk cache so re-runs perform no new judge calls for previously-scored comparisons.
- **FR-006**: The harness MUST emit a report containing, per candidate backend: F1 for nodes, edges, and summaries; latency percentiles; and error rate.
- **FR-007**: The report MUST include structured-output reliability as a first-class metric (malformed/unparseable JSON rate, defensive-parse recoveries), reported separately from F1.
- **FR-008**: The judge prompt and failure taxonomy MUST be ported verbatim from the prior Python harness, as data/config — not re-derived from intuition.
- **FR-009**: The corpus MUST be the public Simple English Wikipedia set curated in #217 (CC-BY-SA, pinned revisions); the harness MUST share that manifest rather than curating a second corpus, and MUST NOT use private/personal corpora.
- **FR-010**: Local models MUST be reachable via an OpenAI-compatible server (e.g., `mlx_lm.server`) through `OaiExtractor` — no Python/MLX bindings added in-repo.
- **FR-011**: The harness MUST be runnable on demand (not on every PR), following the existing `bench.yml` pattern.
- **FR-012**: The harness MUST support enabling cassette recording (#232) per backend during a run, so a single full-corpus pass can yield both the eval report and a recorded cassette.
- **FR-013**: The project MUST document how to run the harness, how to add a candidate backend, and the cost implications of doing so.

### Key Entities *(if applicable)*

- **Corpus chunk**: A source text chunk (from the public Wikipedia manifest, #217) fed to an extractor backend as input.
- **Extractor backend**: A configured extraction candidate (e.g., Anthropic baseline, an OpenAI-compatible local endpoint) driven through the existing `AnthropicExtractor`/`OaiExtractor` clients and this engine's prompts.
- **Judge score / cache entry**: The LLM-as-judge's scoring of a candidate's output against a reference, persisted on disk keyed so identical comparisons are never re-scored.
- **Report**: The harness's output artifact — per-candidate F1 (nodes/edges/summaries), latency percentiles, error rate, and structured-output reliability.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The repository remains `cargo`-only after this work lands — no `pyproject`, `uv.lock`, or Python package is present.
- **SC-002**: Running the harness over the public Wikipedia corpus through at least two backends (hosted Anthropic baseline + one OpenAI-compatible local endpoint) produces a comparison report.
- **SC-003**: A second run of the harness against the same corpus and backends performs zero new judge calls (fully served from the on-disk cache).
- **SC-004**: A baseline-vs-itself comparison scores near the noise floor under judged metrics and materially lower under strict string matching, reproducing the property that motivated using a judge at all.
- **SC-005**: A change to `crates/core/src/prompts/` is visible in the eval's behavior (or breaks its build), demonstrating structural coupling between the harness and the live prompts.
- **SC-006**: Documentation exists covering how to run the harness, how to add a candidate backend, and the cost implications of doing so.
- **SC-007**: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --release` all pass.

## Assumptions

- The public corpus manifest curated in #217 is available and reusable as-is; this work does not curate a second corpus.
- #232 (cassette recording) lands before or alongside this work, providing the recording interface the harness's cassette-capture capability (FR-012) is built against. This issue's cassette-recording support is blocked on #232.
- Judge calls incur real cost against a hosted model; the default corpus is kept small enough to be affordable, and the on-disk cache is mandatory rather than optional.
- The full-corpus model comparison (hosted Anthropic vs. local qwen3.6-27b, with published results) is out of scope here and is tracked as #248, a maintainer-run operation requiring a local model server, an API key, and real spend.

## Out of Scope *(optional)*

- The historical findings write-up (companion docs issue).
- Re-running the full 13-model matrix from the prior research.
- Changing any production default.
- Porting the Python orchestration scripts, MLX client, or snapshot tooling.
- Executing the full-corpus model comparison itself (hosted Anthropic vs. local qwen3.6-27b) and publishing its results — that is #248.
- Running this harness in per-PR CI.

## Source References *(optional)*

- #212 — local/OpenAI-compatible extraction adapter and its ADR (quality caveat this issue addresses).
- #217 — curated public Simple English Wikipedia corpus manifest (reused, not re-curated).
- #232 — cassette recording interface; this issue's cassette-capture support (FR-012) is blocked on it.
- #248 — full-corpus model comparison execution and published results (split out of this issue's scope).
- ADR-0041 — local path's structured-output coercion via `response_format: json_object` + prompt instruction, relevant to FR-007's structured-output reliability metric.
