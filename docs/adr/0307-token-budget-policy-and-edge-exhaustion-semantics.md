# ADR-0307: Token-Budget Policy and Edge Budget-Exhaustion Semantics

**Status**: Accepted
**Date**: 2026-08-01
**Issues**: #307

## Context

`INITIAL_MAX_TOKENS: u32 = 8192` was hardcoded, identically, at all four extraction call sites
in `crates/core/src/extractor.rs` (Anthropic entities/edges, OAI-compatible entities/edges). It
was not derived from measurement, was uniform across entity calls (small output) and edge calls
(large output) despite the ontology alone adding +1,734 chars to the edge prompt, and was not
proportional to input size — a 204-char chunk and a 16,670-char chunk got the identical budget.
Corpus chunks measure median 743 chars, p95 6,411, max 16,670
(`docs/history/extraction-eval-2026-07.md`).

Separately, and more seriously: on edge-budget exhaustion after the existing one-shot doubling
retry, the entity path already returned `Err` — but the edge path returned `Ok(vec![])`,
deliberately, "not fatal", "matches Anthropic". That made a truncated chunk byte-identical, from
the caller's perspective, to a chunk where the model genuinely found zero edges. #306 (merged as
PR #308) made the truncation event itself observable — a `chunk_key` on
`TelemetryEvent::ExtractionTruncated`, a per-candidate `truncated` count in the eval report, and
a `<cassette>.failures.jsonl` sidecar carrying the complete raw response — but #306 explicitly
deferred changing `do_extract_edges`'s *return contract*. A chunk-level consumer (the WAL writer,
an eval scorer working chunk-by-chunk) still could not tell "truncated" from "genuinely empty"
from the `ExtractionResult` alone.

This is not hypothetical: `qwen3.6-35b-a3b` returned zero edges on two chunks where Haiku found
36 and 38 and `qwen3.6-27b` found 49 each — enough to account for its entire measured edge-recall
deficit. Its edge recall (0.878) is close enough to the 27b's (0.900) that the gap may be
entirely an artifact of those two chunks.

Two constraints bound the design: the Anthropic Messages API requires `max_tokens` as a
mandatory field, so "no limit" was never an available option on the hosted path; and a cap is a
genuine runaway guard, not a hypothetical one — Ollama's gemma-4 was observed generating the full
8,192-token budget from a 1,500-char input, which uncapped becomes generation until context
exhaustion (minutes of GPU per chunk locally, unbounded spend on a hosted model).

## Decisions

### 1. Edge budget exhaustion now returns `Err`, matching the entity path (FR-004)

**We changed both providers' `do_extract_edges` terminal `BudgetExhausted` arm from
`eprintln! + Ok(vec![])` to `Err(Error::Ipc("edge extraction budget exhausted after retry"))`** —
byte-for-byte the same message shape the entity path already used.

Two independent reasons, and the second is the one that actually settles it:

- **Control flow.** `Err` matches the entity path, so the two halves of a single `extract()` stop
  having contradictory semantics, and it puts the failure where the retry actually lives:
  `process_chunk` returns to the app's job queue, which owns re-enqueueing. A chunk that is
  absent is detectable and re-ingestible. A chunk that landed with entities and zero edges is
  neither — in the graph it is indistinguishable from a chunk that genuinely had no relations,
  which is wrong as an *inference*, not merely incomplete.
- **Measurement.** `Ok(vec![])` did not just hide the failure from the eval, it corrupted the
  score. The chunk got scored as "the model found zero edges", attributing our own truncation to
  the model's recall — exactly the `qwen3.6-35b-a3b` scenario above. `Err` excludes the chunk and
  counts it as an error instead, which the existing scored-chunk arithmetic
  (`scored = 228 − own_errors − 1`) already handles correctly.

The one real cost of `Err` is discarding the entities already extracted before the edge call
failed, since `do_extract`'s `do_extract_edges(...).await?` propagates the error before
constructing an `ExtractionResult`. That is answered by Decision 3 below, not by keeping
`Ok(vec![])`. `Ok(vec![])` was the only one of the three options considered (`Err`, a flagged
partial result, or keeping `Ok(vec![])` and relying on #306's telemetry alone) that left the
graph structurally unable to describe its own gaps — a consumer working from `ExtractionResult`
had no way to even ask the question, let alone answer it.

### 2. A single formula, uniform ceiling, differentiated only by call type (FR-001/FR-002/FR-006)

Replaced the four constants with one shared module, `crates/core/src/token_budget.rs`:

```
initial_max_tokens = clamp(chunk_len_bytes * ratio, FLOOR, ceiling)
```

- `MAX_TOKENS_FLOOR: u32 = 4_096` — compiled-in, not configurable.
- `MAX_TOKENS_CEILING_DEFAULT: u32 = 32_768` — overridable via
  `LCG_EXTRACTION_MAX_TOKENS_CEILING` (FR-003), uniform across both providers and both call
  types.
- `ENTITY_TOKENS_PER_INPUT_BYTE: f64 = 1.0`, `EDGE_TOKENS_PER_INPUT_BYTE: f64 = 1.5` — edges get a
  higher per-byte ratio than entities, addressing the issue's own critique that a uniform budget
  binds hardest exactly where content is densest.

**Call type and provider are different axes.** FR-006's "uniform ceiling" requirement governs
providers, not call types — a hosted vs. self-hosted split was explicitly rejected (see Decision
4), but giving edges a larger *ratio* than entities is compatible with a uniform *ceiling*,
because the ceiling itself never differs by call type either. Against the corpus percentiles
(median 743, p95 6,411, max 16,670 bytes): median chunks land on the floor for both call types;
the corpus max computes to 16,670 tokens (entities) / 25,005 tokens (edges) — both comfortably
under the 32,768 ceiling, leaving headroom for a retry-doubling that should now rarely be needed
(SC-003, validated structurally by
`token_budget::tests::compute_initial_max_tokens_corpus_max_chunk_stays_under_ceiling`). The
documented WebBrain 257KB pathological input would immediately saturate the ceiling on the first
attempt — the intended backstop behavior, not a bug.

No per-model context-window registry exists anywhere in this codebase (`assets/llm_pricing.json`
has cost fields only), so FR-002's "ceiling bounded by the model's context window" resolves to:
the formula's ceiling *is* the FR-006 uniform value, not a literal per-model lookup — there is
nothing in-repo to look up against, and building one was out of scope.

Only the ceiling is configurable (FR-003), matching User Story 3's acceptance scenario exactly;
the floor and both ratios are corpus-derived shape parameters, not operator knobs. The env var is
re-read per call rather than cached in a `OnceLock`, matching the existing WAL-knobs precedent in
`app_state.rs` — the cost is negligible next to the network round-trip it precedes, and it avoids
test-isolation problems a cached value would introduce. An invalid value — non-numeric, or below
the compiled-in `MAX_TOKENS_FLOOR` — logs a warning and falls back to the default (soft fallback,
not a hard `Error::Config`) — same shape as `LCG_WAL_MAX_BYTES_PER_FILE`, chosen over
`LCG_REPLAY_BATCH_SIZE`'s hard-error shape because an extraction call is not worth aborting
outright over a malformed ceiling override when a safe default exists. The below-floor case is
rejected rather than silently accepted because `u32::clamp` panics if its `min` argument (the
floor) exceeds its `max` (the ceiling) — `compute_initial_max_tokens` additionally treats the
floor as authoritative if it is ever called directly with an out-of-range ceiling, so the panic
is unreachable from either entry point, not just the env-var path.

A ceiling *at or just above* the floor is a distinct, non-panicking case that is intentionally
left unvalidated: `clamp(chunk_len_bytes * ratio, FLOOR, ceiling)` collapses to a near-constant
`FLOOR` for every call once `ceiling` is close to `MAX_TOKENS_FLOOR`, which silently defeats
FR-002's proportional scaling (though not FR-006's runaway guard — the ceiling still bounds
generation, just at a smaller value). This is documented as an operator-facing caveat in
README.md's `LCG_EXTRACTION_MAX_TOKENS_CEILING` row rather than enforced in code: unlike the
below-floor case, it cannot panic, and a hard minimum-above-floor policy would need its own
arbitrary threshold with no corpus-derived basis (Decision 2 already treats the floor and ratios
as fixed, non-operator-tunable shape parameters for exactly this reason).

### 3. `entities_extracted: Option<usize>` on the failure record answers Decision 1's real cost (FR-007)

`TelemetryEvent::ExtractionFailure` and `ExtractionFailureRecord` (both from #306) gain a new
`#[serde(default)] entities_extracted: Option<usize>` field — count-only, not the full entity
list. `do_extract_edges` already receives `entity_names: &[String]` as a parameter in both
providers, so `entity_names.len()` is available with zero signature changes at every edge-call
failure site (HTTP error, malformed body, and budget-exhausted alike, since entities always
succeed before edges run in `do_extract`). It is `Some(count)` at every `call_type: "edges"`
failure site and `None` at every `call_type: "entities"` site, where there is nothing to report
yet.

Carrying the *full* `Vec<ExtractedEntity>` (names, types, summaries — richer forensics) would
have required threading the complete entity list, not just names, into `do_extract_edges` across
both providers — a real signature change for a benefit this decision judged not to be worth the
blast radius. The count is enough to answer the question `entities_extracted` exists to answer:
"was this chunk's entity extraction wasted, or did edges alone fail?" — *"extracted 63 entities,
then blew the edge budget"* is the diagnostic signal, and the count alone delivers it. If richer
forensics prove necessary later, the count-only field is a strict subset of that data and does
not need to be removed to add it.

This is the resolution to the tension between production (wants a clean `Err` to retry on) and
benchmarking (wants maximum forensic detail): #306 already decoupled the two channels —
`ExtractionFailureRecord` is written by a telemetry sink, independent of what `extract()`
returns — so Decision 1's `Err` does not need to also be a rich payload. The forensics live in
the sidecar; the return value stays a plain, retryable `Result`.

### 4. Uniform ceiling across providers, not a hosted-specific clamp (FR-006)

An earlier draft of this decision proposed a uniform *formula* but a lower *absolute ceiling* on
the hosted Anthropic path, reasoning that a local runaway costs GPU minutes while a hosted one
costs metered money, and that Haiku empirically needs less headroom (0–0.4% errors, never
approached the cap) than qwen (15–32% more content for the same input).

**That framing was reversed and rejected.** Cost control is not the driving concern here; the
requirement is narrower — stop non-termination (a response that never ends) — and
less-than-optimal token efficiency is explicitly acceptable. Under that framing, a
provider-specific clamp only earns its complexity if per-call spend is being optimized, and it is
not: **the ceiling is a single uniform value across the hosted Anthropic path and self-hosted/
OAI-compatible models.** One formula, one ceiling, one code path.

The ceiling is sized generously — well above plausible legitimate need — because its job is to
catch genuine non-termination (the gemma-4 case: 8,192 tokens generated from a 1,500-char input),
not to trim well-behaved responses. The design deliberately biases toward headroom over
tightness, because the two failure modes are asymmetric: a few extra tokens cost a little money,
while truncating a well-behaved response corrupts a measurement (Decision 1) or silently loses
production knowledge in a system with no retry signal to recover it — the far more expensive
failure. `max_tokens` remains a weak instrument for cost control specifically — it bounds one
call, and real runaway spend in this system looks like 228 chunks each running slightly over, not
one pathological response a lower per-call ceiling would catch — but since cost is not the driver
here, the design does not contort around it. No per-provider clamp, per-run/aggregate
cost-budget guard, or spend-tracking machinery was added; if session/job-level runaway bounding
is ever needed, it belongs at that layer, as a separate issue.

### 5. Retry-doubling is preserved, but skipped once already at the ceiling

The existing "double once on exhaustion, second exhaustion is terminal" mechanism is unchanged in
shape, but the doubled value is now clamped to the ceiling, and if the current value has
*already reached* the ceiling when it exhausts, no retry is attempted at all — doubling from
there would just resend an identical request and fail identically, wasting a full round-trip.
Implemented as `next_retry_max_tokens(current, ceiling) -> Option<u32>`, returning `None` when
there is no room left to grow; both providers now check this before mutating their retry state,
in place of the old unconditional `current * 2`.

### 6. `LlmRouter`'s permanent-fallback switch now also fires on edge exhaustion — deliberately, not as a side effect

`LlmRouter::do_extract` latches `primary_failed` permanently for the rest of the process's
lifetime the first time `primary.extract()` returns `Err`, if a fallback is configured. Before
Decision 1, edge-budget exhaustion never reached this path (`Ok(vec![])`); it now does, since
`extract()`'s `?` propagates the edge-path `Err` exactly like any other extraction failure.

**No code change was made to `llm_router.rs`.** Treating an edge-exhaustion `Err` identically to
a transport/HTTP failure — tripping the same one-shot switch — is consistent with Decision 1's
own "matches the entity path" rationale, not a new failure mode requiring special-casing.
Decision 2's proportional cap should make exhaustion rare going forward, which is also why this
is judged an acceptable rare-case cost rather than something worth `LlmRouter` special-casing
`BudgetExhausted` apart from other error causes. This is recorded here as a deliberate decision,
not an unexamined side effect, and is covered by a regression test
(`llm_router::tests::edge_budget_exhaustion_error_triggers_the_permanent_fallback_switch`) that
would fail loudly if this interaction is ever revisited.

## Consequences

- No chunk in a corpus run can be recorded as a clean success while its edge output was actually
  truncated (SC-001) — an edge-exhausted chunk is now excluded and counted as an error.
- A synthetic runaway is still halted at the (now proportional, still bounded) ceiling, and the
  halt remains visible via #306's existing `ExtractionTruncated`/`ExtractionFailure` telemetry
  (SC-002) — this issue changes the budget the halt fires at and the edge path's return value,
  not the halt/visibility mechanism itself.
- The corpus max chunk (16,670 chars) is validated structurally to stay under the ceiling on both
  call types (SC-003's structural half); no live run against the real Anthropic/qwen backends
  ships with this change — that requires production credentials and a live or cassette run this
  environment does not have access to, and is a manual maintainer follow-up, mirroring ADR-0306's
  own precedent for its golden-corpus re-run. Re-running the qwen3.6-35b-a3b comparison under the
  new policy (SC-004) is part of that same follow-up.
- A single edge-budget exhaustion event can now permanently switch a production run to its
  fallback model for the rest of the session (Decision 6) — accepted as consistent with existing
  `Err`-triggers-fallback semantics, not flagged as a regression, and covered by a test that
  documents the interaction rather than leaving it implicit.
- `ExtractionFailureRecord`/`TelemetryEvent::ExtractionFailure`'s new field is
  `#[serde(default)]`, so `<cassette>.failures.jsonl` sidecars written before this change remain
  parseable.
- `LCG_EXTRACTION_MAX_TOKENS_CEILING` is the only new operator-facing configuration surface
  (README.md's Environment Variables table); the floor and both per-call-type ratios are not
  configurable, by design (Decision 2).

## Related

- ADR-0306: Extraction-Failure Sidecar and Truncation Visibility — the direct predecessor this
  issue builds on. Its own Consequences section named this issue explicitly as the deferred
  follow-up ("no change to `ExtractionResult`, to edge-budget-exhaustion semantics ... This is a
  manual maintainer follow-up"). FR-007's new field is additive to the record/event ADR-0306
  designed with exactly this kind of non-destructive extension in mind.
- ADR-0044: LLM Cassette Record/Replay Seam — unaffected further by this issue; the trait
  boundary ADR-0306 already extended for failure telemetry is not touched again here.
- `crates/core/src/token_budget.rs`: `ExtractionCallType`, `MAX_TOKENS_FLOOR`,
  `MAX_TOKENS_CEILING_DEFAULT`, `resolve_max_tokens_ceiling`, `compute_initial_max_tokens`,
  `next_retry_max_tokens`.
- `crates/core/src/extractor.rs`: both providers' `do_extract_entities`/`do_extract_edges`,
  `emit_extraction_failure`.
- `crates/core/src/telemetry.rs`: `TelemetryEvent::ExtractionFailure`'s `entities_extracted`
  field.
- `crates/core/src/extraction_failures.rs`: `ExtractionFailureRecord`'s `entities_extracted`
  field.
- `crates/core/src/llm_router.rs`: the unchanged permanent-fallback switch and its new
  regression test documenting Decision 6.
- `crates/core/tests/cassette_record_replay.rs`: the new edge-path-truncation-returns-`Err`
  integration test (`edge_budget_exhaustion_after_retry_returns_err_with_entities_extracted_count`).
- `docs/history/extraction-eval-2026-07.md`: the qwen3.6-35b-a3b analysis and corpus
  chunk-length percentiles this ADR's numbers are derived from.
- README.md's Environment Variables table: `LCG_EXTRACTION_MAX_TOKENS_CEILING`.
