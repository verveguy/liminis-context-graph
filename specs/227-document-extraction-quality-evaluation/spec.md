# Feature Specification: Document Extraction-Quality Evaluation — Methodology, Model Rankings, and Local-LLM Guidance

**Feature Branch**: `fabrik/issue-227`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "This project has no published guidance on which LLM to use for extraction, yet it is the single biggest quality and cost lever a user has. Worse, the question is live: the local/OpenAI-compatible extraction adapter (#212) makes 'fully local' reachable, and its own ADR concedes that genuine end-to-end extraction quality against the real sidecar can only be confirmed by manual testing, not by CI. Substantial prior research exists but lives only in private repos this engine was extracted from — a 13-configuration x 2-corpora extraction-quality evaluation with LLM-as-judge scoring, and a separate finding that Apple Foundation Models were evaluated and rejected for entity extraction. Port the evaluation's methodology, findings, and resulting guidance into this repo as documentation, scrubbed of private paths and private-corpus references."

## Background

This project currently gives no answer to "can I run extraction fully local, and what does it cost me in quality?" ADR-0041 (`docs/adr/0041-local-openai-compatible-extraction-adapter.md`) made local extraction *technically reachable* via `--extractor-uds`/`--extractor-http`, but its own Consequences section concedes that genuine end-to-end extraction quality against the real sidecar "can only be confirmed by manual testing on macOS with Apple Intelligence enabled, not by this repo's standard CI" — and deliberately withholds a default-socket auto-detection tier for extraction specifically because the bundled sidecar's backend (Apple Foundation Models) has "prior evidence of inadequate extraction quality."

That prior evidence is real and substantial — a 13-configuration x 2-corpora extraction-quality evaluation using LLM-as-judge scoring, plus a dedicated finding that Apple Foundation Models were assessed and rejected for entity/relationship extraction specifically (context window and capability judged insufficient for the task's quality bar) — but it exists only in the private repos this engine was extracted from. No OSS user or contributor can see it. README's "Extractor: local or hosted" section and ADR-0041 both already forward-reference this issue (`#227`) as the place this evaluation will be published; right now that reference points at nothing.

This issue ports that prior work into this repo as documentation: the replay-based methodology, the LLM-as-judge finding that makes the other numbers interpretable, the resulting model rankings, the dedup finding, and the Apple Foundation Models assessment — rewritten and privacy-reviewed, not copied, and framed unambiguously as historical prior art rather than a current guarantee. It directly informs #212 (whose default extractor-selection behavior this evaluation justified) and pairs with #228 (the in-repo Rust eval harness that will eventually make these numbers refreshable) and #248 (a benchmark run already capturing fresh, on-this-engine cassettes for the hosted-vs-local comparison). Building the harness, and re-running the full evaluation against this engine's current pipeline, are both out of scope here.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator deciding between local and hosted extraction (Priority: P1)

An operator setting up `liminis-context-graph` wants to decide whether to run extraction fully locally (against the bundled CoreML sidecar or another local OpenAI-compatible endpoint) or against the hosted Anthropic API. Today they can only find a bare assertion ("not recommended for extraction quality") with no numbers behind it. They read the new doc and find concrete, ranked local-model options with judged F1 scores against the hosted baseline, and a direct recommendation for both a quality-first and a speed-first choice.

**Why this priority**: This is the entire motivating gap in the issue — the project has "no published guidance on which LLM to use for extraction, yet it is the single biggest quality and cost lever a user has."

**Independent Test**: Open the new doc (reached via the README link) and confirm it contains a rankings table naming specific local models with judged F1 scores, an explicit quality-first recommendation, an explicit speed-first recommendation, and a statement that hosted (Anthropic) remains the quality baseline.

**Acceptance Scenarios**:

1. **Given** the doc is published and linked from README, **When** a reader looks up whether to run extraction locally, **Then** they find named local-model options ranked by judged F1 (nodes/edges/summaries) against the hosted noise floor, with explicit quality-first and speed-first recommendations.
2. **Given** the doc is published, **When** a reader looks up why the bundled sidecar's default model isn't used for extraction automatically, **Then** they find the explicit Apple Foundation Models rejection rationale (context window, capability) consistent with ADR-0041's reference to this evaluation.

---

### User Story 2 - Contributor correctly interpreting the F1 numbers (Priority: P1)

A contributor or reviewer sees an extraction-quality F1 number and, without context, could easily misread it as a hard ceiling or misread two numbers computed by different scoring methods as directly comparable. The doc must teach the LLM-as-judge finding first, before presenting any rankings, so the numbers that follow are interpretable rather than misleading.

**Why this priority**: The issue calls this out explicitly as "the key methodological result" and states plainly that "without this, local-vs-hosted numbers are uninterpretable."

**Independent Test**: Read the doc's methodology section and confirm it presents the sonnet-vs-sonnet self-comparison scored two ways — strict-string F1 on edge relation wording (0.771) versus LLM-as-judge F1 on the same comparison (0.978) — with the wording-variance explanation (e.g., "won" vs "won_award" scored as a mismatch under strict string comparison but a match under judged scoring).

**Acceptance Scenarios**:

1. **Given** the doc, **When** a reader encounters the rankings table, **Then** they have already been shown (earlier in the doc) that a same-model self-comparison produces a 0.771 strict-string F1 purely from wording variance, versus 0.978 under judged scoring — establishing that all F1 numbers in the doc are judged, not strict-string, scores.
2. **Given** the doc, **When** a reader wants to know the practical ceiling for any candidate, **Then** they find the noise floor stated explicitly (sonnet-vs-sonnet judged F1: nodes 0.990, edges 0.978, summaries 0.900), together with the roles used to establish it (sonnet for extraction, `qwen-9b` for dedup).

---

### User Story 3 - Future maintainer of the runnable eval harness (#228) referencing prior art (Priority: P2)

Someone implementing or using the in-repo Rust eval harness (#228), or reviewing the fresh on-this-engine benchmark (#248), wants to know what methodology to replicate and what historical numbers to treat as directional prior art — not a target to reproduce exactly — so they can sanity-check fresh harness output against a known-relative ranking.

**Why this priority**: Directly named in the issue's Risks/Dependencies as pairing with #228; secondary because #228/#248 are separate issues and not blocked on this one being perfect.

**Independent Test**: Read the doc's staleness/provenance section and confirm it names precisely what changed since the numbers were measured (a 2026-04-30 prompt restructure, a different private corpus, a Python rather than this repo's Rust pipeline) and states that #228 and #248 are what re-baseline them.

**Acceptance Scenarios**:

1. **Given** the doc, **When** a reader asks "can I trust these exact numbers today", **Then** the doc states plainly that they predate a prompt restructure and were measured on a different corpus/pipeline, and should be read as relative-ranking prior art, not current guarantees.

---

### Edge Cases

- **Reader treats historical numbers as current guarantees**: the staleness/provenance caveat must be unmissable — visible in the doc's introduction/summary, not buried only as a trailing footnote.
- **Reader tries to identify the private corpora from their "shape" description**: shape descriptions (chunk-count bucket, domain character) must stay generic enough that they don't fingerprint the specific private dataset — no exact counts, no sample text, and no subject matter detail beyond the anonymised character description in FR-014.
- **A ruled-out model's failure is misread as "scored low" when it was actually a pipeline failure**: `mistral-small-3`'s 100% error rate is a different failure mode than a model that was graded and simply fell short of the quality bar (e.g. `qwen2.5-72b`, `llama-3.3-70b`). The doc must distinguish these rather than presenting one flat "ruled out" list.
- **Reader assumes the published configuration list is the complete, verbatim original evaluation matrix**: it is a reconstruction from summarized results (see FR-015), not a transcription — the doc must say so explicitly rather than implying completeness or exact label fidelity.
- **Doc content drifts out of sync with ADR-0041 or #212's spec**, which already state parts of this finding (e.g. ADR-0041's Consequences section, `specs/212-.../spec.md`'s Assumptions section) — the new doc should be the canonical, detailed home for this material, with those existing references pointing to it rather than restating it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A new document MUST be added under `docs/` covering the extraction-quality evaluation's methodology, findings, and resulting guidance. Exact filename/path is left to the Research/Plan stage.
- **FR-002**: The doc MUST describe the replay-based methodology — comparing candidates against frozen message arrays so every candidate sees identical inputs — and explain why this is preferred over re-running the full pipeline per candidate (removes pipeline/pass-order variance as a confound between candidates).
- **FR-003**: The doc MUST present the LLM-as-judge finding with both figures for the sonnet-vs-sonnet self-comparison: strict-string F1 on edge relation wording = **0.771**, versus LLM-as-judge F1 on the same comparison = **0.978**, and explain the cause (wording variance, e.g. "won" vs "won_award", scored as mismatched by strict string comparison but equivalent by judged scoring) as roughly a 23% disagreement floor from wording alone.
- **FR-004**: The doc MUST present the noise floor — sonnet-vs-sonnet judged F1: nodes **0.990**, edges **0.978**, summaries **0.900** — as the practical ceiling against which every other candidate's judged F1 should be read, and MUST note that this noise-floor configuration paired sonnet (extraction) with `qwen-9b` (dedup), established via self-comparison under the judged metric.
- **FR-005**: The doc MUST present a rankings table (judged F1: nodes/edges/summaries, where recorded) containing:
  - `qwen3.6-27b`: **0.894 / 0.852 / 0.900** — the local winner, roughly 7 percentage points off the hosted noise floor.
  - `qwen3.6-35b-a3b`: **0.879 / 0.764 / 0.800** — roughly 14 points off the noise floor, but roughly 4x faster than `qwen3.6-27b`.
  - `qwen3.6-27b-thinking` (thinking-mode variant of the local winner): scored **worse on nodes** than the non-thinking baseline, at roughly **10x the latency** — included as a "more compute did not help" data point, without a fabricated precise F1 figure since none was recorded.
  - The ruled-out set, clearly distinguished by failure mode: models graded but below the quality bar (`qwen2.5-72b`, `llama-3.3-70b`, `gemma-3-27b`, `deepseek-r1-distill-32b`, `qwen-claude-distill`) versus a model with a pipeline-level failure rather than a low score (`mistral-small-3`, 100% error rate).
- **FR-006**: The doc MUST state the dedup finding: F1 = **1.000** across every evaluated candidate, including the smallest (9B, i.e. `qwen-9b`) model, and its implication — no model upgrade is needed for the dedup role specifically, independent of the extraction-model choice.
- **FR-007**: The doc MUST state that Apple Foundation Models were assessed for entity/relationship extraction and are **not recommended**, with the stated reason (context window too small, intelligence insufficient for the task's quality bar), and MUST note that Apple Foundation Models are the default backend reachable via the bundled sidecar's `/v1/chat/completions` route (i.e., the model an operator would get if that route were auto-selected for extraction).
- **FR-008**: The doc MUST give explicit, direct guidance: quality-first local choice = `qwen3.6-27b`; speed-first local choice = `qwen3.6-35b-a3b`; hosted (Anthropic) remains the quality baseline.
- **FR-009**: The doc MUST carry an explicit staleness/provenance caveat, visible in the doc's introduction (not only as a trailing footnote), stating that: the absolute numbers predate a 2026-04-30 prompt restructure; they were measured against a different (private) corpus; they were measured against a Python pipeline, not this repo's Rust pipeline; they should be read as historical prior art indicating **relative ranking**, not current guarantees for this engine; and that issue #228 (the in-repo Rust eval harness) and issue #248 (a benchmark run already capturing fresh cassettes comparing the hosted baseline against `qwen3.6-27b` on this engine) are what would re-baseline them.
- **FR-010**: The doc MUST NOT contain private corpus names/content, internal repo paths, or workspace names, or any sample extraction text. The two evaluation corpora MUST be described only via the anonymised shape/character description in FR-014 — never by proper name or identifying subject-matter detail.
- **FR-011**: All relative links within the new doc MUST resolve to real files/sections in this repo, and the doc MUST render as valid GitHub-flavored markdown.
- **FR-012**: README's existing forward-reference to this issue in the "Extractor: local or hosted" section (and the adjacent Principle 3 mention) MUST be updated to link directly to the new doc once it is published, rather than pointing at the bare issue number.
- **FR-013**: The doc SHOULD be cross-referenced from ADR-0041's Consequences section (which currently states this finding inline) so ADR-0041 points to the new doc as the detailed source rather than restating the finding itself.
- **FR-014**: The doc MUST describe the two evaluation corpora by shape and character only, as follows, and MUST present the cross-corpus quality-degradation finding as a named methodological result (not omitted):
  - **Corpus A** (a small, curated corpus): ~40 chunks, ~130 extraction calls. Character: personal reading notes on a fiction series — narrative prose with a dense cast of named characters, places, and factions, and relatively few technical/typed relations.
  - **Corpus B** (a larger, sampled corpus): ~75 chunks sampled from a ~360-chunk personal knowledge base, ~290 extraction calls. Character: a mixed personal/technical knowledge base — design notes, decisions, and reference material, with higher relational density than Corpus A.
  - **Cross-corpus finding**: quality dropped most on **edges** moving from Corpus A to Corpus B — the leading model (`qwen3.6-27b`) lost roughly **9 percentage points on edges** — attributed to Corpus B's greater relational density. This finding (local-model quality degrading on higher-relational-density content, which is closer to what this engine targets) MUST be presented as more significant than either corpus's absolute per-model number in isolation.
- **FR-015**: The doc MUST state explicitly that the set of evaluated configurations is a **reconstruction from summarized results, not a verbatim transcription** of the original evaluation matrix: the original matrix covered 13 configurations, including at least one mode variant of an already-listed model (`qwen3.6-27b-thinking`) and a hosted routing combination used to establish the noise floor (`sonnet` for extraction + `qwen-9b` for dedup, compared via `sonnet-vs-sonnet` under the judged metric); the doc publishes only the attested subset (FR-005/FR-006) and does not claim the reproduced list is exhaustive or that every label matches the original harness's own naming exactly.

### Key Entities

Not applicable — this is a documentation-only change; no persisted data types are introduced or modified.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A new document exists under `docs/` and contains, at minimum: the replay-based methodology, the 0.771 → 0.978 LLM-as-judge finding, the noise floor, the full rankings table (including the ruled-out set with failure mode distinguished, and the thinking-mode variant finding), the dedup finding, the cross-corpus quality-degradation finding, and the Apple Foundation Models assessment.
- **SC-002**: The doc is reachable via a link from README's "Extractor: local or hosted" section.
- **SC-003**: The doc's staleness/provenance caveat appears in the doc's introduction/summary, not only in a trailing footnote — a reader cannot reach the rankings table without first passing the caveat.
- **SC-004**: A privacy read-through (checklist covering corpus names, internal repo paths, workspace names, and sample extraction text) confirms none appear anywhere in the doc.
- **SC-005**: Every relative link in the new doc resolves to an existing file or section, and the doc renders correctly as GitHub-flavored markdown.
- **SC-006**: The doc explicitly states that the published configuration list is a reconstruction, not a verbatim transcription, of the original 13-configuration matrix.

## Assumptions

- The exact filename/path for the new document under `docs/` is left to the Research/Plan stage.
- No markdown-lint or link-check tool is currently configured in this repo's CI (`.github/workflows/`); satisfying FR-011/SC-005 may mean a manual review pass rather than an automated gate, unless Research finds existing tooling this issue overlooked.
- Per-corpus judged F1 breakdowns are only available for the leading model's edge-quality drop (~9pp, Corpus A → Corpus B, FR-014); no per-corpus breakdown exists for the other candidates, and the doc must not imply one does.
- The list of 13 configurations named in the original issue text is a reconstruction from summarized results (FR-015); some labels or the exact count breakdown may not exactly match the original private evaluation's own labelling, and the doc must not overstate its completeness.
- This doc's scope covers extraction-model quality guidance only; it does not cover embedding-model evaluation or guidance (embedding already has a working fully-local story and is not in question).
- "Historical prior art" framing means the doc is allowed to go stale in its absolute numbers without being incorrect — it is explicitly not a claim about this engine's current pipeline, per FR-009.

## Out of Scope

- The runnable Rust eval harness itself (#228) — a separate issue.
- The full-corpus, cassette-capturing benchmark run (#248) — a separate issue whose results this doc's staleness caveat forward-references but does not itself contain.
- Re-running the evaluation or producing fresh numbers against this engine's current pipeline/corpus.
- Changing any default extractor-selection behavior (ADR-0041 Decision 3 and #212's design stand as-is).
- Embedding-model evaluation or guidance.

## Source References

- ADR-0041 (`docs/adr/0041-local-openai-compatible-extraction-adapter.md`) — Consequences section already states this finding inline and forward-references #227/#228.
- README, "Extractor: local or hosted" section and the Principle 3 introduction — existing forward-reference to #227, ready to be swapped for a direct doc link.
- `specs/212-local-openai-compatible-extraction/spec.md` — Assumptions section records the same prior-art constraint and cites #227/#228.
- Issue #228 — the in-repo Rust eval harness that will keep this guidance current / eventually re-baseline it.
- Issue #248 — a benchmark run already capturing fresh, on-this-engine cassettes comparing the hosted baseline against `qwen3.6-27b`, referenced by this doc's staleness caveat (FR-009) as a source of current-engine numbers.
