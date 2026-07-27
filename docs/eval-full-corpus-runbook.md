# Full-corpus extraction benchmark runbook (#248)

This is the maintainer-run procedure for producing #248's benchmark data: a full-228-article
`lcg-eval` run comparing the hosted Anthropic baseline against a local `qwen3.6-27b`, with LLM
cassettes recorded for both. It exists because this run needs a live, spend-authorized
`ANTHROPIC_API_KEY` and a reachable local `qwen3.6-27b` server — neither is available inside
Fabrik's sandbox (see the spec's Background at `specs/248-benchmark-run-full-corpus/spec.md`).
No new `crates/eval`/`crates/core` mechanism is needed to run this — everything below uses
flags `lcg-eval` (#228) and `--record-cassette` (#232) already support today.

Once you've run this and hold a completed JSON report plus the two cassette files, see
[docs/extraction-quality-evaluation.md](extraction-quality-evaluation.md)'s "Measured results
(this engine)" section for where the figures go, and FR-007/SC-004 in the spec for how the
README's "quality-verified" wording should change to match whatever the numbers turn out to be
— including walking that claim back if the local model scores materially worse than the
inherited figures.

## Scripted path (recommended)

`crates/eval/scripts/` automates everything below and encodes several traps that have
each cost a run. Prefer it over copy-pasting the commands in this document:

```bash
crates/eval/scripts/01-start-server.sh          # starts mlx with thinking DISABLED
crates/eval/scripts/02-timing-check.sh          # projects runtime; do not skip
crates/eval/scripts/03-capture-qwen.sh 25       # validate on 25 chunks, no hosted spend
crates/eval/scripts/04-full-run.sh              # the real benchmark
```

> **Thinking mode must be disabled on the local server.** `qwen3.6` defaults to
> emitting `<think>` reasoning, which is ~10x slower *and* scores worse on
> enumeration tasks — `docs/history/extraction-eval-2026-04.md` measured
> `qwen3.6-27b-thinking-only` at 112.7s p50 versus 10.9s without, and judged it
> "operationally non-viable". Start the server with
> `--chat-template-args '{"enable_thinking": false}'`, which `01-start-server.sh`
> does and then verifies. A full-corpus capture in thinking mode projects to 15+
> hours and yields a cassette of the known-bad configuration.

> **Use the model id the server advertises**, i.e.
> `model=mlx-community/Qwen3.6-27B-4bit`. `mlx_lm.server` treats an unrecognised id
> as a HuggingFace repo to fetch, so `model=qwen3.6-27b` fails *every* call with
> `Repository Not Found` and leaves an empty cassette with no other symptom.

## Prerequisites

1. **A reachable local OpenAI-compatible server hosting `qwen3.6-27b`** — e.g. `mlx_lm.server`
   serving an HTTP or Unix-domain-socket endpoint. `lcg-eval` talks to it exactly like any other
   `oai-http`/`oai-uds` backend (see README's "Extraction-quality eval harness" section).
2. **`ANTHROPIC_API_KEY` set in the environment, unconditionally.** This is stricter than the
   harness's general graceful-degradation behavior: judge-scoring (LLM-as-judge F1) does degrade
   gracefully to strict-string-F1-only when the key is unset, but every command below also
   configures at least one `anthropic` backend for the hosted leg itself, and
   `build_extractor` (`crates/eval/src/backend.rs`) errors immediately at startup if the key is
   missing — there is no partial/degraded path for the backend construction itself. Set the key
   before running, not after a failure partway through.
3. Run from the repo root (or anywhere — `--corpus` defaults to the #217 fixture at
   `crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl`, resolved via
   `CARGO_MANIFEST_DIR` at compile time, not the invoking shell's working directory). **Do not
   pass `--corpus`** unless you deliberately want a different input — both legs must see the
   byte-identical committed fixture (FR-004), not a freshly refetched Wikipedia pull.

## The combined run (recommended)

A single `lcg-eval` invocation with three `--backend` entries produces both benchmark legs while
reducing the separate-run cost from three full-corpus Anthropic passes to two:

- `baseline=anthropic` — the reference. Cassette-recorded; this cassette **is**
  `anthropic-<model>.jsonl` (FR-002).
- `candidate=anthropic` — a second, independent hosted run of the same spec. Compared against
  `baseline` under `--reference baseline`, this pair **is** the hosted-vs-itself noise floor
  (FR-001) — a judged F1 near the established ceiling while strict-string F1 is materially
  lower, per the property `.github/workflows/eval.yml`'s small-scale smoke pass already checks.
- `qwen=oai-http:...,model=qwen3.6-27b` — the local candidate. Cassette-recorded to
  `qwen3.6-27b.jsonl` (FR-002). Compared against `baseline`, this pair **is** the hosted-vs-qwen
  comparison (FR-002/SC-002).

```bash
export ANTHROPIC_API_KEY=sk-ant-...

cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic:model=claude-haiku-4-5-20251001 \
  --backend candidate=anthropic:model=claude-haiku-4-5-20251001 \
  --backend qwen=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=qwen3.6-27b \
  --reference baseline \
  --all \
  --record-cassette baseline=anthropic-claude-haiku-4-5-20251001.jsonl \
  --record-cassette qwen=qwen3.6-27b.jsonl \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248.json
```

**The local backend spec must spell out `model=qwen3.6-27b` explicitly.** `oai-http`/`oai-uds`
silently default the model id to the literal string `"local"` when `model=` is omitted
(`crates/eval/src/backend.rs`) — leaving it off would produce a valid-looking report and
cassette mislabeled with the wrong model name, undermining SC-002/SC-005's point of attributing
results to the actual model tested. Swap the URL for wherever your `mlx_lm.server` (or
equivalent) is actually listening; use `--backend qwen=oai-uds:path=/tmp/qwen.sock,model=qwen3.6-27b`
instead if you're serving over a Unix domain socket.

`--all` runs the full 228-chunk corpus — 2 extraction calls per chunk per backend (6 total
here), plus one judge call per scored comparison per chunk. This is real, non-trivial spend;
see README's "Cost implications" subsection under "Extraction-quality eval harness" for the
full breakdown. The `--judge-cache` path above is mandatory in the sense that omitting it
defaults to `judge_cache.jsonl` in the current directory — pick an explicit path you'll keep
around, since a same-corpus, same-backend re-run reuses every cached judge score for free (see
"Re-running after a partial failure" below).

## Reading the report

Each `candidates[]` entry (`crates/eval/src/report.rs`) is keyed by `backend_name`:

- **`candidate`** (vs. `baseline`) is the noise floor: `judged_entity_f1`/`judged_edge_f1`/
  `judged_summary_f1` should land near the established ceiling (0.990/0.978/0.900 in the
  ported historical figures — see `docs/extraction-quality-evaluation.md`) while
  `strict_entity_f1`/`strict_edge_f1` are materially lower, purely from wording variance.
- **`qwen`** (vs. `baseline`) is the actual hosted-vs-local comparison: read its judged F1 as
  "distance from the noise floor established by `candidate`," not "distance from 1.0."
- **`baseline`**'s own entry (compared against itself as the reference) is a trivial, always-
  perfect self-comparison — ignore it; it exists because the harness reports one entry per
  configured backend, not because it's informative on its own.

Every candidate also carries `chunks_run`, `chunks_scored` (can be smaller than `chunks_run` if
either side errored on some chunks — the two aren't necessarily scored over the same sample
count), `errors`/`error_rate`, `latency.{p50_ms,p95_ms,p99_ms}`, and
`structured_output.{clean,recovered,malformed,malformed_rate}` — the structured-output
reliability figures FR-005/SC-002 need, straight off the harness with no extra flags.

## Re-running after a partial failure

The on-disk judge cache (`--judge-cache`) is keyed on corpus content, backend, and judge model,
so re-running the exact same command after a partial failure makes **zero new judge calls** for
any chunk/backend pair already scored — only genuinely new work is paid for again.

If only one leg needs re-running (e.g. the local server dropped mid-run but the hosted leg
completed), split into two invocations instead of re-running all three backends. If the hosted
`baseline` leg already completed and its cassette is on disk, replay it instead of re-running it
live — see "Resuming a partial run" below. Otherwise, re-run it live as shown here.

```bash
# Noise-floor leg alone (FR-001):
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic:model=claude-haiku-4-5-20251001 \
  --backend candidate=anthropic:model=claude-haiku-4-5-20251001 \
  --reference baseline \
  --all \
  --record-cassette baseline=anthropic-claude-haiku-4-5-20251001.jsonl \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248_noise_floor.json

# Hosted-vs-qwen leg alone (FR-002):
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic:model=claude-haiku-4-5-20251001 \
  --backend qwen=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=qwen3.6-27b \
  --reference baseline \
  --all \
  --record-cassette baseline=anthropic-claude-haiku-4-5-20251001.jsonl \
  --record-cassette qwen=qwen3.6-27b.jsonl \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248_hosted_vs_qwen.json
```

Both commands above append to the same `anthropic-claude-haiku-4-5-20251001.jsonl` cassette
(`CassetteWriter::open` always appends, matching the WAL's own convention) — running them one
after another, or re-running one after a failure, does not corrupt or truncate it.

## Resuming a partial run

If the `baseline` leg already completed and captured a cassette (e.g. `#248`'s run, where
`baseline` finished but the `qwen` leg died on an unrelated fault later in the same invocation),
replay that cassette instead of re-paying for the `baseline` extraction calls. `lcg-eval` accepts
a `cassette:path=<PATH>` backend spec (#263) that builds `lcg_core::cassette::ReplayingExtractor`
— it makes zero outbound LLM requests, matching each call by content hash against the recorded
cassette and failing loudly with `Error::CassetteMiss` on any chunk it can't match.

```bash
# Resume: replay baseline from its captured cassette, run only qwen live.
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=anthropic-claude-haiku-4-5-20251001.jsonl \
  --backend qwen=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=qwen3.6-27b \
  --reference baseline \
  --all \
  --record-cassette qwen=qwen3.6-27b.jsonl \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248_resumed.json
```

**Replay applies to `baseline` only — never point `candidate` at a `cassette:` spec.**
`baseline` and `candidate` are deliberately two *independent* live samples of the same spec; their
disagreement is the noise-floor measurement itself (FR-001 above). Replaying the same cassette
into both makes them byte-identical, so judged F1 becomes 1.000 by construction and the noise
floor stops meaning anything. Only ever run `candidate` live.

Do not add `--record-cassette` for a backend whose spec is `cassette:...` — `lcg-eval` rejects
this combination at startup (recording a replay is meaningless: there's no live call to capture).

**The #248 capture is 226 records against the 228-chunk fixture.** Two chunks will
`Error::CassetteMiss` on every replay of that file, permanently. This is not corpus drift or a
replay bug: `RecordingExtractor::extract` (`crates/core/src/cassette.rs`) propagates any `Err`
from the wrapped extractor via `?` *before* it appends a cassette record, so a chunk whose live
extraction call failed during the original recording run (rate limit, transient network error, an
unrecoverable malformed response) is counted as an error in that run's own report but produces
zero cassette entry — there is nothing to replay for it, by construction. The specific two
chunks/cause can't be reconstructed after the fact from the cassette file alone (only the
original run's own logs would show that); `Error::CassetteMiss` on those two chunks on every
future replay is the expected, correct outcome, not a bug to chase.

## Committing the cassettes

Once both cassette files exist, commit them as plain, uncompressed JSONL — the same convention
#232 established (one record per line: `key`, `call_type`, `provider`, `model`, `timestamp`,
`request`, `response`) and README's "Record/replay cassettes" section documents in full. If
either file turns out to be large enough to warrant it, follow the size-management precedent in
`crates/core/tests/fixtures/real_corpus_wal/README.md` (uncompressed JSONL, not gzip — git
already compresses blobs, and gzip defeats diffability and delta compression across future
re-captures) rather than introducing a new convention.

## Verifying deterministic replay (SC-005)

Run the cassette directly through `lcg-eval`'s real `--backend NAME=cassette:path=<PATH>` spec
(#263) — no separate test harness or bespoke code is needed; the ordinary CLI invocation itself is
the verification, since a `cassette:` backend makes zero outbound requests by construction
(`ReplayingExtractor` holds no HTTP client) and fails loudly on any unmatched chunk:

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=anthropic-claude-haiku-4-5-20251001.jsonl \
  --reference baseline \
  --all \
  --output eval_report_248_replay_check.json
```

A clean run (aside from the two known `Error::CassetteMiss` chunks documented above) with no
`ANTHROPIC_API_KEY` set and no network access confirms both the zero-live-calls property (FR-002)
and that the cassette replays deterministically against the full 228-chunk corpus (SC-005). Repeat
with `qwen3.6-27b.jsonl` to verify the second cassette the same way. This replaces the
`#[ignore]`d code-sketch approach an earlier draft of this runbook described — the CLI wiring
that sketch was written to anticipate now exists directly, so no separate integration test file is
needed here (`crates/eval/tests/harness_integration.rs` already covers the pipeline's correctness
as an integration test, with a hand-built cassette).

## Pairwise judging pass (#269)

Everything above measures **similarity to `baseline`** via judged precision/recall/F1 — a
candidate that extracts something `baseline` missed is scored as a false positive for being
right. `--judge-mode pairwise` adds a second, reference-agnostic pass over the same three
cassettes: the judge sees the source chunk plus two *unlabelled* extractions and picks which
better captures the content, per axis, with no backend privileged. It is a pure scoring-layer
pass — zero extraction calls, re-runnable for free against cassettes already on disk (FR-009,
SC-003).

**This pass needs a `candidate` cassette that "The combined run" above does not currently
capture.** The existing noise-floor leg intentionally never replays `candidate` from a cassette
(see "Resuming a partial run" above — replaying it would make the two samples byte-identical and
destroy the reference-mode noise-floor measurement), but it also never *records* one. To capture
all three cassettes needed for the command below, add `--record-cassette
candidate=anthropic-claude-haiku-4-5-20251001-candidate.jsonl` to "The combined run"'s command —
recording `candidate`'s live calls doesn't change anything about how `candidate` is scored in
reference mode, it just additionally captures the cassette this pairwise pass needs.

Once all three cassettes exist (`baseline`, `candidate`, `qwen`), run:

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=anthropic-claude-haiku-4-5-20251001.jsonl \
  --backend candidate=cassette:path=anthropic-claude-haiku-4-5-20251001-candidate.jsonl \
  --backend qwen=cassette:path=qwen3.6-27b.jsonl \
  --reference baseline \
  --all \
  --judge-mode pairwise \
  --judge-cache eval_judge_cache_248.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_248_pairwise.json
```

No `ANTHROPIC_API_KEY`-gated extraction backend is configured here — all three are `cassette:`
replays — so the run makes zero outbound extraction requests regardless (FR-009); judge calls
still require `ANTHROPIC_API_KEY` (the judge is a standalone client, ADR-0048 Decision 3,
independent of the backends under test).

This produces three backend pairs, each judged on all three axes (entities/edges/summary):

- **`baseline` vs `candidate` is the mandatory calibration control** (User Story 2) — two
  independent samples of the same model. Each axis's win rate should land within
  **45–55%** (`CALIBRATION_BAND_LOW`/`_HIGH`, ADR-0050); a loud stderr warning fires naming the
  observed rate and axis if not. Do not treat the other two pairs' results as trustworthy
  without checking this one first.
- **`baseline` vs `qwen`** and **`candidate` vs `qwen`** are the actual hosted-vs-local blind
  comparisons — read the win rate as "how often the judge picked `qwen`'s extraction over the
  hosted one when neither was labelled," a different question from reference-F1's "how much
  would existing graph content shift if we swapped models."

Every pair/axis result also carries an `order_inconsistency_rate` — never trust a win rate
without checking it alongside (FR-007). Above **20%**
(`ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD`, ADR-0050), the judge is flipping its answer often
enough when the slot order reverses that the win rate isn't distinguishable from position-bias
noise; a loud stderr warning fires for this too. `chunks_skipped` depends on each pair's actual
cassette coverage — chunks present on only one side are excluded from that pair's tally, never
counted as a loss (FR-010). The known `baseline`/`qwen` coverage (226/223, a 221 overlap) means
that pair is expected to report a nonzero skip count; `candidate` is freshly recorded per this
runbook, so its overlap with the other two isn't known ahead of a run and may be zero.

Re-running the exact command above against the same `--judge-cache` path makes zero new judge
calls (SC-005) — free to re-run after tweaking the report format or investigating a surprising
result.

## Running the ontology mode matrix (#266)

Everything above measures **freeform extraction only** — `ExtractOptions.ontology` was always
`None`. `lcg-eval` also accepts `--ontology <PATH>`/`--ontology-mode <open|strict>` (#266), so the
same corpus and backends can be re-run under `Open` and `Strict` and compared against the
freeform baseline above. **No maintainer has executed this matrix yet** — this section documents
the exact procedure, following #248's own precedent of shipping mechanism-plus-runbook rather
than the paid run itself (see the spec's Background/User Story 2 at
`specs/266-extraction-eval-measures-only/spec.md`).

### Prerequisites

Same as the combined run above (a reachable local OpenAI-compatible server,
`ANTHROPIC_API_KEY` set unconditionally, run from the repo root) plus:

4. **The FR-005 ontology fixture**, committed at
   `crates/core/tests/fixtures/real_corpus_wal/ontology.yaml` alongside the corpus fixture — its
   header documents how its entity/relation-type distribution was derived from this exact
   corpus's freeform extraction output, so `Open`/`Strict` runs are never a degenerate comparison
   against types that don't actually occur in the text (this issue's own Edge Case). The same
   file drives both `Open` and `Strict`: `--ontology-mode` on the CLI always overrides the
   file's own `mode: strict` declaration (FR-002), so there is no separate `ontology-open.yaml`.

### The three commands

`crates/eval/scripts/run_mode_matrix.sh` runs all three below in sequence (see the script's own
header comment for its env-var overrides: `LOCAL_BACKEND_SPEC` is required, everything else has
a default matching the commands here). Each mode records `baseline`'s hosted leg to its own
cassette file — a cassette recorded under one mode's rendered system prompts never matches
another mode's replay (see Edge Cases in the spec), so `anthropic-freeform-*.jsonl`,
`anthropic-open-*.jsonl`, and `anthropic-strict-*.jsonl` are three genuinely distinct captures,
not the same file reused:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
FIXTURE=crates/core/tests/fixtures/real_corpus_wal/ontology.yaml

# 1. Freeform baseline — no --ontology flag, unchanged from every prior run.
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic:model=claude-haiku-4-5-20251001 \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=qwen3.6-27b \
  --reference baseline \
  --all \
  --record-cassette baseline=anthropic-freeform-claude-haiku-4-5-20251001.jsonl \
  --judge-cache eval_judge_cache_266.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_266_freeform.json

# 2. Open — declared types preferred; the model may still invent others.
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic:model=claude-haiku-4-5-20251001 \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=qwen3.6-27b \
  --reference baseline \
  --all \
  --ontology "$FIXTURE" --ontology-mode open \
  --record-cassette baseline=anthropic-open-claude-haiku-4-5-20251001.jsonl \
  --judge-cache eval_judge_cache_266.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_266_open.json

# 3. Strict — only declared types are ever accepted.
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic:model=claude-haiku-4-5-20251001 \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=qwen3.6-27b \
  --reference baseline \
  --all \
  --ontology "$FIXTURE" --ontology-mode strict \
  --record-cassette baseline=anthropic-strict-claude-haiku-4-5-20251001.jsonl \
  --judge-cache eval_judge_cache_266.jsonl \
  --judge-model claude-sonnet-4-6 \
  --output eval_report_266_strict.json
```

The three commands triple the cost of the single combined run described above — plan for three
full-corpus passes' worth of extraction and judge spend, not one, before running this. The
`--judge-cache` path is shared across all three invocations deliberately: the cache key already
incorporates the rendered prompt content (which differs by mode), so freeform/`Open`/`Strict`
judge verdicts for the same underlying comparison never collide in one cache file, and reusing
it just means a fourth re-run of any single mode makes zero new judge calls, exactly as the
combined run above does.

### Reading the three reports

Each report's top-level `ontology_mode` field (FR-003) is `"freeform"`, `"open"`, or `"strict"` —
read that field first, don't infer the mode from the filename alone, since reports are meant to
be archived and compared side by side. Per FR-004, `structured_output.{clean,recovered,malformed}`
is reported identically to the freeform run's own figures — this is the metric most likely to
improve once a closed vocabulary removes open-ended type naming from the model's task, per this
issue's own hypothesis that a closed vocabulary should narrow the local/hosted structured-output
gap (ADR-0041).

Only the `Strict` report carries a `vocabulary_compliance` field per candidate (FR-007) —
`null`/absent on both the freeform *and* the `Open` report, since the metric isn't applicable
outside `Strict` (an `Open` ontology never rejects a type, so there is nothing to count). It
counts, separately from JSON-syntax validity, how often a candidate emitted an entity or relation
type outside the fixture's declared vocabulary — a model can produce perfectly valid JSON that
simply ignores the closed type list, and that failure mode would otherwise be invisible if folded
into `structured_output`.

Diff the three reports' per-backend `judged_entity_f1`/`judged_edge_f1`/`strict_entity_f1`/
`strict_edge_f1` figures directly — any reordering of the model ranking between modes is now
visible without re-running anything (SC-002). Per FR-005 fixture's own derivation notes: entity
typing already converged under freeform extraction on this corpus (a closed vocabulary should
move entity F1 relatively little), while relation naming did not converge at all (heavy synonym
clustering across hundreds of distinct freeform relation names) — so edge F1 is the figure most
likely to move materially between freeform and `Strict`, and is worth writing up explicitly
whether or not it actually does.
