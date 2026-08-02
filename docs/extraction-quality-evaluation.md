---
layout: default
title: Extraction-Quality Evaluation
---

# Extraction-quality evaluation: methodology, model rankings, and local-LLM guidance

This document answers the question `liminis-context-graph` has otherwise left unanswered: **can
extraction run fully local, and what does it cost in quality?** It ports the methodology,
findings, and resulting guidance from a prior extraction-quality evaluation into this repo, scrubbed
of private corpus names, internal paths, and sample extraction text.

## Read this first: what these numbers are and aren't

The evaluation below predates this engine. Concretely:

- It was measured **before** a 2026-04-30 prompt restructure (this repo's extraction prompts were
  ported from graphiti afterward — see `specs/92-port-graphiti-s-extraction/spec.md`).
- It was measured against a **different, private corpus** than anything in this repo.
- It was measured against a **Python pipeline**, not this repo's Rust pipeline.

Treat everything below as **historical prior art indicating relative ranking** — which local model
family leads, roughly how far behind the hosted baseline it falls, which approaches failed — not as
a current guarantee for this engine. Issue [#228](https://github.com/verveguy/liminis-context-graph/issues/228)
(an in-repo Rust eval harness) and issue [#248](https://github.com/verveguy/liminis-context-graph/issues/248)
(a benchmark run comparing the hosted baseline against `qwen3.6-27b`, on this engine and corpus,
with cassettes captured for both — see the [full-corpus runbook](eval-full-corpus-runbook.md) for
the exact maintainer-run procedure) are what will re-baseline these numbers against the current
pipeline. Until then, this is the best evidence available, and it's why
[ADR-0041](adr/0041-local-openai-compatible-extraction-adapter.md) does not auto-select the bundled
sidecar's model for extraction.

**Every figure below and in the next section describes freeform extraction only** — the model
inventing its own entity/relation type vocabulary. Issue
[#266](https://github.com/verveguy/liminis-context-graph/issues/266) added `--ontology`/
`--ontology-mode` to `lcg-eval` plus a corpus-derived fixture so the same corpus and backends can
also be measured under `Open`/`Strict`, but **no maintainer has run that mode matrix yet** — see
the runbook's ["Running the ontology mode matrix"](eval-full-corpus-runbook.md#running-the-ontology-mode-matrix-266)
section for the procedure. Do not read the rankings below as applying to an ontology-constrained
workspace until that matrix has actually been run; #266's own fixture derivation notes observed
that freeform entity typing already converges to a small set on this corpus (so a closed
vocabulary may move entity F1 relatively little) while freeform relation naming does not converge
at all (heavy synonym clustering), making edge F1 the figure most likely to move once a closed
vocabulary is applied — but that is a hypothesis to verify by running the matrix, not a measured
result.

## Measured results (this engine) — Status: Pending, not yet measured

This section is reserved for #248's measured figures once a maintainer runs the
[full-corpus runbook](eval-full-corpus-runbook.md) and supplies its report and cassettes. **No
run has been executed yet as of this section being added** — the numbers below are deliberately
absent rather than estimated or copied from the historical section above, per the spec's Edge
Cases ("must say so explicitly rather than fabricate placeholder numbers"). Whatever #248
eventually measures here will itself be a **freeform-only** result, per the same caveat above —
it does not by itself say anything about `Open`/`Strict` extraction, which needs the separate
mode-matrix run #266 documents.

Once a run completes, this section is replaced (not the historical section below, which stays as
labeled prior art) with:

- Judged F1 for nodes/edges/summaries for `qwen3.6-27b`, read against this engine's own measured
  hosted-vs-itself noise floor (not the historical 0.990/0.978/0.900 figures above, which were
  measured on a different pipeline and corpus).
- Structured-output reliability (clean/recovered/malformed JSON parse counts) for both the
  hosted and local backends — a dimension the historical evaluation above did not track at all,
  since the predecessor pipeline had no equivalent telemetry.
- A direct statement of whether this engine's local-vs-hosted gap is in line with, narrower
  than, or wider than the inherited ~7 percentage-point gap — and, per FR-007, whether that
  changes the README's "quality-verified" framing.

## Ontology-constrained results (`Open`/`Strict`) — Status: Pending, not yet measured

This section is reserved for the freeform/`Open`/`Strict` mode-matrix figures (#266) once a
maintainer runs `crates/eval/scripts/run_mode_matrix.sh` (or the equivalent hand-typed commands
in the runbook) and supplies the three resulting reports. **No mode-matrix run has been executed
yet** — same discipline as the section above: no placeholder or estimated numbers here until a
real run produces them.

Once a run completes, this section is replaced with:

- Per-backend judged/strict F1 for entities and edges under `Open` and `Strict`, compared
  directly against the freeform figures in the section above — specifically, whether the
  freeform model ranking holds or reorders under a closed vocabulary (this issue's central
  question — see the spec's User Story 2).
- The `Strict`-mode vocabulary-compliance rate (FR-007) per backend — how often each candidate
  emitted a type outside the ontology's declared vocabulary, distinct from JSON-syntax
  structured-output reliability.
- A direct statement of whether entity F1 moved materially less than edge F1 under `Strict`, per
  the hypothesis in "Read this first" above, and whether that changes any local-model
  recommendation stated elsewhere in this document.

## Methodology: replay against frozen inputs, not a fresh pipeline run per candidate

The evaluation used a **record-and-replay** design: a single reference run drove a real indexing
pipeline once, with every LLM call traced to a message array — full pre-mutation prompt, response,
timing, and call site. Each candidate model then replayed the *same* captured calls: identical
prompts, identical inputs, identical dispatch (which calls go to the "extraction" role vs. the
"dedup" role). The only variable between runs was the model answering the call.

This is deliberately preferred over re-running the full pipeline once per candidate. A fresh
end-to-end run per candidate would let each model's own extractions feed its own downstream dedup
decisions, so different candidates would face different inputs by the time you get to comparing
their outputs — pipeline and pass-order variance would be a confound sitting on top of the actual
model-quality difference you're trying to measure. Freezing the inputs removes that confound: every
candidate is graded against exactly the same task.

## Why every F1 number here is a judged score, not a strict-string one

The first pass at scoring used strict-string comparison: entity name-set overlap, and an exact
tuple match on edges (source, target, relation-label). This produced misleadingly low edge scores
across every candidate, including a same-model self-comparison — running the reference model
against itself.

The cause turned out to be wording variance, not quality. A model would extract the same real-world
relationship on both runs but label it slightly differently — for example `won` on one pass and
`won_award` on the other. Strict string comparison scores that as a complete miss, even though it's
the same edge.

The reference model compared against itself scored:

- **Strict-string F1 on edges: 0.771** — roughly a 23% "disagreement" floor from wording variance
  alone, despite comparing a model against itself.
- **LLM-as-judge F1 on the same comparison: 0.978** — using a second model to align items by
  semantic meaning (not exact string) before computing precision/recall, this same self-comparison
  scores far closer to what it should: near-perfect agreement.

Because strict-string scoring is this misleading even on a same-model self-comparison, **every F1
figure in the rest of this document is an LLM-as-judge score**, not a strict-string one. Numbers
from the two metrics are not comparable to each other.

## The noise floor

Before ranking any local candidate, it's necessary to know the practical ceiling: how much
disagreement exists even between two runs of the intended hosted configuration. That's the noise
floor, established via self-comparison under the judged metric, pairing the extraction role with
a hosted model and the dedup role with a small local model (`qwen-9b`):

| | nodes | edges | summaries |
|---|---:|---:|---:|
| **Noise floor (judged F1)** | **0.990** | **0.978** | **0.900** |

Read every other candidate's judged F1 in the tables below as "distance from this ceiling," not
"distance from 1.0."

## The two evaluation corpora

Two corpora were used to check that rankings weren't an artifact of one particular kind of content,
described here only by shape and character (no titles, paths, or subject-matter detail beyond
this):

- **Corpus A** (a small, curated corpus): ~40 chunks, ~130 extraction calls. Character: personal
  reading notes on a fiction series — narrative prose with a dense cast of named characters,
  places, and factions, and relatively few technical/typed relations.
- **Corpus B** (a larger, sampled corpus): ~75 chunks sampled from a ~360-chunk personal knowledge
  base, ~290 extraction calls. Character: a mixed personal/technical knowledge base — design notes,
  decisions, and reference material, with higher relational density than Corpus A.

**Cross-corpus finding**: quality dropped most on **edges** moving from Corpus A to Corpus B — the
leading local model (`qwen3.6-27b`) lost roughly **9 percentage points on edges**, a materially
larger drop than its ~2-point drop on nodes. This is attributed to Corpus B's greater relational
density: more, and more varied, relationships per chunk gives a model more chances to phrase or
miss an edge. This finding matters more than either corpus's absolute per-model number in isolation
— it says local-model extraction quality degrades specifically on higher-relational-density
content, which is closer to what this engine's own use cases target than the lower-density corpus
is.

## Rankings

Judged F1 (nodes / edges / summaries), read against the 0.990 / 0.978 / 0.900 noise floor above.

| Candidate | nodes | edges | summaries | Notes |
|---|---:|---:|---:|---|
| `qwen3.6-27b` | **0.894** | **0.852** | **0.900** | Local winner. ~7 points off the noise floor on average (nodes and edges individually trail by more; summaries ties). |
| `qwen3.6-35b-a3b` (MoE) | 0.879 | 0.764 | 0.800 | ~14 points off the noise floor, but roughly **4x faster** than `qwen3.6-27b` — a mixture-of-experts model with a much smaller active-parameter count per token. |
| `qwen3.6-27b-thinking` (thinking-mode variant of the winner) | *lower than non-thinking, same model* | — | — | Scored **worse on nodes** than the non-thinking baseline above, at roughly **10x the latency**. Included as a "more compute did not help" data point; no precise figure is reproduced here since the original result predates this doc and shouldn't be treated as a re-verified number. |

**Ruled out — graded, but below the quality bar.** These were run through the full evaluation and
scored, but fell meaningfully short of the leading candidates above: `qwen2.5-72b`, `llama-3.3-70b`,
`gemma-3-27b`, `deepseek-r1-distill-32b`, `qwen-claude-distill` (a Qwen model distilled from a
hosted-model teacher — the distillation did not close the gap to the un-distilled winner).

**Ruled out — pipeline failure, not a quality score.** `mistral-small-3` is a distinct failure
mode from the models above: it produced a **100% error rate**, failing to produce usable
structured output at all, rather than producing usable output that was simply graded lower. Don't
read this as "scored worst" — it never produced a comparable score.

### This rankings table is a reconstruction, not a verbatim transcription

The original evaluation covered **13 configurations**, including at least one mode variant of an
already-listed model (`qwen3.6-27b-thinking`, above) and a hosted routing combination used to
establish the noise floor itself (the hosted extraction model paired with `qwen-9b` for dedup,
compared against itself under the judged metric). What's published above is the **attested
subset** — the candidates and figures that could be confirmed from summarized results — not a
transcription of the original matrix. Treat the list as representative, not exhaustive, and don't
assume every label here matches the original evaluation's own naming exactly.

## Dedup finding

Across every evaluated candidate that completed, including the smallest model tested (a 9B-parameter
model, `qwen-9b`), dedup scored **F1 = 1.000**. Dedup — deciding whether two extracted entities
refer to the same real-world thing — did not differentiate between models at all in this
evaluation. The implication: no model upgrade is needed for the dedup role specifically, independent
of whatever extraction-model choice is made.

## Apple Foundation Models: assessed and not recommended for extraction

Apple Foundation Models were assessed for entity/relationship extraction as part of this evaluation
and are **not recommended** — the model's context window and general capability were judged
insufficient for this task's quality bar.

This matters concretely for this repo: Apple Foundation Models are the backend served by the
bundled CoreML sidecar's `/v1/chat/completions` route — the same route an operator would get if
that socket were auto-selected for extraction. This finding is why [ADR-0041](adr/0041-local-openai-compatible-extraction-adapter.md)
deliberately does **not** include a default-socket auto-detection tier for extraction (unlike the
embedder, which does auto-detect the same sidecar): a live sidecar being present is not, by itself,
evidence that its default model is a good extraction choice.

## Guidance

- **Quality-first, fully local**: `qwen3.6-27b`. The best-scoring local candidate on both corpora,
  roughly 7-9 points of judged F1 below the hosted noise floor depending on corpus and metric.
- **Speed-first, fully local**: `qwen3.6-35b-a3b`. Roughly 4x faster than `qwen3.6-27b` at a
  further quality cost (~14 points off the noise floor) — a reasonable trade for high-volume
  indexing where throughput matters more than the last few points of extraction fidelity.
- **Hosted (Anthropic) remains the quality baseline.** Nothing in this evaluation argues for
  moving away from the hosted API when quality is the priority; it exists to make the *local*
  trade-off legible, not to unseat the hosted default.

Both local recommendations are reachable today via `--extractor-uds`/`--extractor-http` pointed at
an OpenAI-compatible server running the chosen model (e.g. `mlx_lm.server`) — see
[README: Extractor: local or hosted](../README.md#extractor-local-or-hosted) for the flags and
selection precedence.
