> ## Historical context — please read before relying on the numbers
>
> This document was written in April 2026 against an **earlier implementation** of Liminis's knowledge-graph pipeline — a Python service built on the [graphiti](https://github.com/getzep/graphiti) framework, which `liminis-context-graph` (this repository, Rust) was created to replace. The eval pre-dates `liminis-context-graph`'s existence.
>
> **What likely still holds (qualitative findings):**
> - Model-family rankings on entity/edge extraction at the prompt shapes graphiti uses (and which `liminis-context-graph` ports — see `specs/92-port-graphiti-s-extraction/spec.md`). `qwen3.6-27b` as the strongest fully-local option, MoE bandwidth advantage being real on Apple Silicon, reasoning models *hurting* enumeration tasks, distillation-from-Claude not delivering the expected gains.
> - The methodology — record-and-replay against a Sonnet reference run, paired with LLM-as-judge scoring — is reusable infrastructure regardless of which graph implementation sits underneath.
>
> **What needs re-baselining against the Rust pipeline:**
> - Absolute F1 numbers. `liminis-context-graph` ports graphiti's prompts but with its own restructuring (issue #92, merged 2026-05-26). Re-running the same matrix against the current pipeline is a known follow-up, not yet done.
> - Latency numbers. Different IPC layer, different concurrency model.
>
> **Internal references preserved as-is for fidelity:** mentions of "production routing," "demo-notebook," `graphiti_service.py`, `liminis-framework/`, and named project memories refer to Liminis-internal operations at the time of writing — not to defaults or paths in this open-source project. A future `liminis-context-graph`-targeted benchmark harness will replace the methodology described in the "eval harness as reusable infra" section below.
>
> ---

# Extraction-quality eval — local-LLM model selection (2026-04)

**Status:** Findings doc, internal. Captured 2026-04-30. Eval harness lives at `liminis-framework/eval/extraction-quality/` on branch `eval/extraction-quality`. All numbers below are reproducible from the harness; all traces and judge cache are local-only (gitignored except the canonical demo-notebook snapshot).

## TL;DR

Tested 13 model configurations as drop-in replacements for Claude Sonnet+Haiku in the graphiti indexing pipeline, on two corpora (demo-notebook 41 chunks / production 75-chunk subset of 358). Validated with an LLM-as-judge metric after the strict-string metric proved misleading.

**Recommendation:**
- **Cloud route:** keep current production routing — `sonnet-4-6` extract + `qwen-9b` dedup. Nothing here justifies a change.
- **Fully-local quality-first: `mlx-community/Qwen3.6-27B-4bit`** as a single model for both extract and dedup. ~7pp F1 gap to sonnet on demo, ~10pp on production. ~16GB resident, ~6-11s p50 extract latency on M3 Ultra.
- **Fully-local speed-first: `mlx-community/Qwen3.6-35B-A3B-4bit`** (MoE, 3B active). ~14pp gap to sonnet but 3-4× faster than the 27B dense. Operational-friendly for high-volume indexing.

Three plausible-sounding hypotheses fell over: distillation-from-Claude (qwen-claude-distill), reasoning distillation (deepseek-r1-distill-32b), and thinking-mode (qwen3.6-27b with `enable_thinking=True`). Each was either lower-quality, much slower, or both.

## Why this eval

Graphiti's pipeline calls Claude Sonnet for entity/edge extraction and Haiku for dedup decisions. Per call, that's a few cents of API cost and a network round-trip. At indexing scale we wanted to know: can we drop the Anthropic dependency entirely and run on-device? If so, with what quality cost?

Production routing today is `sonnet-4-6 extract + qwen-9b dedup` (see `project_llm_routing` memory). The cloud↔local split is a values choice for cost vs quality; the eval was scoped to give us defensible numbers to make that call.

## Methodology

### Record-and-replay

A "reference run" drives a real graphiti ingest with traced LLMClient wrappers — every call to `generate_response` is logged as JSONL with full pre-mutation messages, response_model, response, timing, prompt_name. Candidate models then replay the captured calls: same prompts, same input, same dispatch (small→dedup, else→extract via `RoleRoutingClient`). The only thing that varies between runs is the model.

This is faster and more rigorous than re-running graphiti's full pipeline for each candidate (which would also vary downstream search inputs as different extractions feed different dedup decisions). For a fair model-quality comparison, candidates need to see identical inputs.

Trace format and the trace machinery are in the harness sister repo: `liminis-framework/eval/extraction-quality/src/eval_extraction/{tracer,replay,reference}.py`.

### Strict metric, then LLM-as-judge

First-pass scoring exact-string-matched on response shapes:
- Entities: name set Jaccard
- Edges: (source, target, fact_type) tuple Jaccard
- Dedup: id→duplicate_idx map exact-equality

This produced misleadingly low edge F1 across the board. Manual diff inspection revealed why: candidates were extracting the right edges but using slightly different relation wording. `(X)—[won]→(Y)` vs `(X)—[won_award]→(Y)`. Same edge, different label. The strict metric counted these as complete misses.

Sonnet-vs-sonnet under strict scored F1=0.771 on edges — a 23% disagreement floor purely from Anthropic non-determinism on relation wording. Local candidates scored 0.3-0.5 against that floor, which was uninterpretable.

Replaced with LLM-as-judge using Sonnet. For each (reference response, candidate response) pair, the judge aligns items by semantic meaning and returns matched pairs + unmatched_reference (false negatives) + unmatched_candidate (false positives). P/R/F1 derived from those counts. Judge prompt at `judge.py:JUDGE_PROMPT` — explicitly handles wording variations, name normalization, edge direction symmetry.

Sonnet-vs-sonnet under judged: F1=0.978 on edges, 0.990 on nodes. That's the true noise floor.

Judging is cached by `hash(prompt_name, ref_response, cand_response)` to JSONL — re-runs are free. Across the eval we made ~700 judge calls for ~$5 in API cost.

### Cross-corpus validation

Demo-notebook is fiction/world-building prose. Production indexes mixed content: meeting notes, technical docs, journal entries, etc. Single-corpus rankings could be corpus-specific. Validated on a 75-chunk subset of production (358 total) — same matrix, fresh reference run, same judge.

## Candidates evaluated

13 configurations across four families:

**Baseline / control**
- `sonnet-4-6` + `haiku-4-5` (the noise-floor reference)
- `sonnet-4-6` + `qwen-9b` (current production routing)

**Qwen family**
- `qwen-9b` (Qwen3.5-9B-MLX-4bit) — current dedup model
- `qwen2.5-72b` (Qwen2.5-72B-Instruct-4bit) — large dense
- `qwen3.6-27b` (Qwen3.6-27B-4bit) — newest dense mid-size
- `qwen3.6-35b-a3b` (Qwen3.6-35B-A3B-4bit) — MoE, 3B active
- `qwen3.6-27b-thinking` — same model, `enable_thinking=True`
- `qwen-claude-distill` (Qwen3.5-27B-Claude-4.6-Opus-Distilled) — distilled from Claude Opus

**Cross-family**
- `llama-3.3-70b` (Llama-3.3-70B-Instruct-4bit) — Meta dense large
- `gemma-3-27b` (gemma-3-27b-it-4bit) — Google dense mid-size
- `deepseek-r1-distill-32b` (DeepSeek-R1-Distill-Qwen-32B-4bit) — reasoning distillation
- `mistral-small-3-24b` (Mistral-Small-3-24B-Instruct-2501-4bit) — Mistral mid-size

**Attempted, didn't run**
- `glm-4.6` — `mlx-community/GLM-4.6-MLX-4bit` returns 401 from HF Hub (gated or doesn't exist; the agent that recommended it was wrong about availability)

All MLX models at 4-bit quantization. Variant registry at `liminis-framework/eval/extraction-quality/src/eval_extraction/clients.py:_VARIANTS`.

## Results — demo-notebook (judged F1)

| variant | nodes | edges | summaries | extract_nodes p50 | err |
|---|---:|---:|---:|---:|---:|
| sonnet-qwen9b (noise floor) | 0.990 | 0.978 | 0.900 | 2.0s | 0.0% |
| **qwen3.6-27b-only** | **0.894** | **0.852** | **0.900** | 5.7s | 0.0% |
| qwen3.6-35b-a3b-only | 0.879 | 0.764 | 0.800 | 1.6s | 0.0% |
| qwen-claude-distill-only | 0.796 | 0.837 | 0.900 | 17.5s | 2.3% |
| qwen2.5-72b-only | 0.778 | 0.679 | 0.800 | 12.9s | 0.0% |
| qwen3.6-27b-thinking-only | 0.772 | 0.846 | 0.900 | 61.1s | 0.0% |
| gemma-3-27b-only | 0.762 | 0.634 | 0.800 | 7.0s | 3.1% |
| llama-3.3-70b-only | 0.757 | 0.619 | 0.800 | 12.2s | 0.0% |
| qwen9b-only | 0.712 | 0.672 | 0.900 | 1.7s | 0.0% |
| deepseek-r1-distill-32b-only | 0.702 | 0.608 | 0.780 | 16.6s | 0.0% |
| mistral-small-3-only | error: 100% | — | — | — | 100% |

Dedup F1 = 1.000 across all candidates that completed. Dedup is solved at this scale; no model differentiation on that role.

## Results — production cross-corpus (judged F1, 75-chunk subset, 289 paired calls)

| variant | nodes | edges | summaries | nodes p50 | err |
|---|---:|---:|---:|---:|---:|
| **qwen3.6-27b-only** | **0.877** | **0.761** | 0.911 | 10.9s | 0.0% |
| qwen3.6-27b-thinking-only | 0.851 | 0.751 | 0.911 | 112.7s | 0.7% |
| qwen3.6-35b-a3b-only | 0.844 | 0.699 | 0.886 | 3.6s | 0.3% |
| deepseek-r1-distill-32b-only | 0.637 | 0.540 | 0.909 | 22.2s | 1.3% |
| mistral-small-3-only | error: 100% | — | — | — | 100% |

**Ranking holds.** qwen3.6-27b is the leader on both corpora. qwen3.6-35b-a3b is consistently second among the working candidates. The relative gaps are similar (~3pp on nodes, ~6-9pp on edges).

**Quality drops modestly on production.** Edges take the biggest hit (qwen3.6-27b -9.1pp on edges vs -1.7pp on nodes). Production text has more relational density and domain-specific verbiage; relations are harder to extract consistently.

**Dedup latency exploded.** qwen3.6-27b dedupe_nodes p50 went from 10.4s on demo to 37.1s on production (still 100% accurate). Production chunks have larger entity batches per dedup call. This is a real constraint for indexer throughput planning — at 37s p50 dedup, a chunk with 5 dedup checks blocks for ~3 minutes.

## Negative findings (the interesting failures)

These are the "tried this, didn't work" results that justify the candidate breadth.

### Distillation-from-Claude doesn't help

`qwen-claude-distill` is a Qwen3.5-27B explicitly trained to mimic Claude Opus. Same footprint as our winner (qwen3.6-27b at ~16GB). Hypothesis: should match Sonnet quality on Sonnet-style tasks more closely than a general-purpose Qwen.

Result: lower quality than the un-distilled qwen3.6-27b on every dimension (-10pp nodes), 3× slower (17.5s vs 5.7s extract_nodes p50), 2.3% error rate (only candidate with non-zero errors apart from gemma and mistral). One extract_edges call hit a 9-minute p95.

The distillation appears to compromise structured-output reliability — the model is less consistent at producing valid JSON for graphiti's response models.

### Reasoning models hurt extraction

Two takes on the reasoning-tuning idea — both regressed.

`qwen3.6-27b-thinking-only` (same base model with `enable_thinking=True`):
- Demo-notebook: F1 nodes dropped from 0.894 → 0.772 (-12pp)
- Production: F1 nodes 0.851 (better than its own demo result! but still below non-thinking 0.877)
- 10× slower per call (61s demo, 113s production)

`deepseek-r1-distill-32b-only` (Qwen 32B distilled with R1 reasoning traces):
- Both corpora: -20pp+ on nodes vs the leader (-19pp demo, -24pp production)
- 3× slower than qwen3.6-27b
- 1.3% error rate on production

Reading: chain-of-thought tokens lead the model away from concrete entity recall — it spends reasoning budget on framing rather than enumeration. May help on harder reasoning tasks; hurts on enumeration tasks.

### Thinking mode inverted between corpora

Worth flagging: thinking-mode scored worse than non-thinking on demo-notebook (-12pp on nodes) but better than its own demo result on production (+8pp on nodes, though still below non-thinking baseline). My read: simple chunks (demo's fiction prose) confuse thinking mode — it over-thinks small extractions into different shapes. Complex chunks (production's mixed content) benefit. Either way it remains operationally non-viable at 113s p50 latency on production data.

### Cross-family didn't surprise

`gemma-3-27b` (Google) at qwen3.6-27b's footprint scored -13pp on nodes, -22pp on edges, with a 3.1% error rate (worst non-Mistral). Different family, lower instruction-tuning fidelity for structured output.

`llama-3.3-70b` (Meta) at qwen2.5-72b's footprint scored basically identically (-1pp on nodes, +1pp on edges). Confirms the dense-70B class is a single bandwidth-bound bucket — different architectures, similar physics, similar quality on this task.

### Mistral failed deployment-side

`mistral-small-3-24b` returned 100% errors on both corpora. The model loaded into MLX but every inference produced output that failed graphiti's response_model validation. Likely a chat-template mismatch or tokenizer incompatibility with mlx-lm's `apply_chat_template`. Didn't dig deeper — the symptom was clear: not deployable as-is.

### MoE bandwidth advantage is real

`qwen3.6-35b-a3b` is nominally larger than `qwen-9b` (35B vs 9B params) but **faster per call** because only 3B params activate per token. On M3 Ultra (819 GB/s memory bandwidth), this maps to ~80-150 effective tok/s on the 35B-MoE vs ~30-60 tok/s on the 9B dense.

The architectural lesson: for bandwidth-bound inference, total parameter count is misleading. Active parameters per token is the real cost.

## Failure-mode taxonomy (top two locals)

Bucketised the residual disagreements after the judge had already reconciled semantic equivalents. Both leaders show similar patterns:

| bucket | qwen3.6-27b | qwen3.6-35b-a3b |
|---|---:|---:|
| missing_edge | 24 | 24 |
| missing_entity | 19 | 21 |
| inverted_edge | 3 | 3 |
| synonym_relation | 2 | 3 |
| modifier_dropped | 0 | 3 |
| extra_edge | 26 | 38 |
| extra_entity | 25 | 23 |
| granularity_split | 5 | 4 |

Same families of failure. Neither model has a category the other doesn't. qwen3.6-35b-a3b emits noticeably more spurious edges (38 vs 26) — consistent with its lower edges F1.

Specific patterns worth noting:
- Both miss abstract concepts ("philosophical imperialism", "rationalism as colonialism") that Sonnet caught
- Both invert occasional edges (`(X)—[leads]→(Y)` vs `(Y)—[led_by]→(X)`)
- 35b-a3b drops modifiers ("Australian communication style" → "Australian") more often than 27b
- 27b's `extra_entity` cases include things like "Hell", "British" — pulling out generic terms that Sonnet treated as adjectives

These patterns are within tolerance for graphiti's downstream dedup, which would unify most of them into the same KG nodes/edges anyway.

## What we didn't measure

Honest list of remaining uncertainty:

1. **End-to-end KG fidelity.** Per-call F1 ≠ final-graph F1. Graphiti's downstream dedup smooths a lot — running each candidate through the full ingest and structurally diffing the resulting LadybugDB against sonnet's would be the gold standard. Several hours' work; we didn't do it.
2. **Self-consistency.** Single run per candidate. Re-running the same model would show variance. We assume sonnet-vs-sonnet at F1=0.99 is a reasonable noise estimate.
3. **Long-term drift.** Models update; today's qwen3.6-27b might score differently in 6 months. Eval is reproducible; just re-run.
4. **Search/retrieval quality.** What end users experience is search results, not extraction F1. The chain `extraction → dedup → search` has compounding effects we didn't model.
5. **Workspace diversity.** Two corpora is more than one but still narrow. Different content domains (legal docs, code, multilingual) might rank differently.
6. **Larger MoE points.** We tested one MoE size (Qwen3.6-35B-A3B). DeepSeek-V2-Lite (16B-A2.4B) and Mixtral-8x7B (~13B active) are unexplored.

## The eval harness as reusable infra

`liminis-framework/eval/extraction-quality/` is a self-contained uv project with these capabilities:

- `eval-extraction snapshot WORKSPACE` — capture an indexing-queue.jsonl into a stable corpus file
- `eval-extraction reference SNAPSHOT --extraction X --dedup Y` — drive a real graphiti ingest with traced clients
- `eval-extraction replay TRACE --extraction X --dedup Y` — re-run a captured trace against a different (extraction, dedup) pair
- `eval-extraction matrix SNAPSHOT --variants-filter ...` — orchestrate baseline + replays end-to-end
- `eval-extraction judge` — LLM-as-judge re-scoring across all candidate traces
- `eval-extraction report` / variants generate markdown summaries + per-call HTML diffs

Adding a new candidate is a `clients.py` registry entry plus a one-line variant. Adding a new corpus is `snapshot WORKSPACE` plus `matrix --variants-filter`. The judge cache makes re-runs cheap (cache-key by content hash).

This was disposable spike infrastructure built for one decision — but it's general enough that any future "is model X better than model Y on graphiti's prompts?" question can be answered in an afternoon plus a few dollars of API.

## Hardware context

All numbers above measured on:
- Mac Studio M3 Ultra
- 96GB unified memory
- 819 GB/s memory bandwidth
- 60-core GPU, 28-core CPU (20P/8E)
- 32-core ANE (idle — see `apple-neural-engine-opportunities.md`)

For LLM inference at 4-bit quantization, the binding constraint is memory bandwidth × active parameters per token. M3 Ultra's bandwidth is ~2× M3 Max's; absolute latency numbers will roughly double on a Max-tier laptop. Quality numbers are hardware-independent.

## Action items (suggested)

1. **No production routing change required.** sonnet-4-6 + qwen-9b stays. The eval validates that this is a defensible choice — we know what we'd lose by going local, and the loss is non-trivial.
2. **If/when fully-local becomes a priority:** swap to `qwen3.6-27b` for both roles. Update `liminis-framework/framework/src/skills/knowledge-graph/scripts/graphiti_service.py:_build_role_client` and `liminis-framework/scripts/install-graphiti.sh` accordingly.
3. **Eval harness should ship to main eventually.** It's PR-ready as `eval/extraction-quality` branch in `liminis-framework`. Bundle into the framework repo so future model questions are tractable.
4. **Project memory updated** with cross-corpus confirmation. See `project_extraction_eval_results` for the canonical findings reference for future sessions.
