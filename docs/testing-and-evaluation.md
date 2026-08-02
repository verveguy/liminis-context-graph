---
layout: default
title: Testing & Evaluation
---

# Testing & Evaluation

## Record/replay cassettes

Every test that exercises the real extraction pipeline used to face a choice: pay for a live
LLM call, or fall back to `MockExtractor`'s fixed `Alice`/`Acme Corp` output regardless of input.
Neither lets you regression-test a prompt change, a response-parsing change, or the ingest
pipeline's real entity/edge yield without spending money on every run. **LLM cassettes** close
that gap: record one real extraction pass to a file, then replay it deterministically and for
free — with no network access — for as long as the recorded calls still match.

### Recording

Set `LCG_RECORD_LLM=<path>` and run a real ingest (`ANTHROPIC_API_KEY` or a local extractor must
still be configured normally — recording wraps whichever provider is resolved). Every extraction
call — `knowledge_add_episode`'s entity/edge extraction, and the `knowledge_reprocess_*` type
classification calls — appends one line to `<path>`. Re-running recording against an existing
path always **appends**, never truncates, matching the WAL's convention.

### Replaying

Set `LCG_REPLAY_LLM=<path>` and run the identical ingest again. Extraction is served entirely
from the cassette: no provider is resolved, no `ANTHROPIC_API_KEY`/`--extractor-*` flag is
needed, and no network call is ever made. `LCG_RECORD_LLM` and `LCG_REPLAY_LLM` are mutually
exclusive — setting both is a startup error.

A replay request that doesn't match any recorded entry — because the episode text differs, or
because a prompt/parsing change altered what's semantically being asked — fails immediately with
an identifiable cassette-miss error rather than silently falling through to a live call or
producing divergent output. **To re-record after a cassette miss**: delete or move aside the
stale cassette (or point `LCG_RECORD_LLM` at a fresh path), re-run the affected ingest with
recording enabled against a live provider, then switch back to `LCG_REPLAY_LLM` to confirm the
new cassette replays cleanly.

### Format

A cassette is plain, uncompressed JSONL — one JSON object per line, no envelope. Each record
carries a `key` (a SHA-256 hex digest used for matching), `call_type` (`extract`,
`classify_entities`, or `classify_relations`), `provider`, `model`, an RFC 3339 `timestamp`,
the human-readable `request` content, and the call's `response`. Records are matched by `key`
alone, independent of file order — a cassette assembled from multiple recording runs (or, for
`LlmRouter`, from more than one primary/fallback leaf) replays correctly as a single flat file.
Two calls with identical semantic content are served FIFO, in the order they were recorded.

**What's in the matching key, precisely** (and what isn't): for `extract`, the rendered entity
and edge system/user prompts plus `episode_body`/`group_id`/`reference_time`/
`custom_instructions`/`source_type` — rendering the prompts (not just hashing the raw options)
means editing a prompt template or the injected ontology correctly invalidates stale cassette
entries. For `classify_entities`/`classify_relations`, the raw call arguments only. Timestamps,
request nonces, and anything transport-specific (headers, API keys, URLs) are never part of the
key, and never reach the cassette at all — the record/replay seam sits at the `Extractor` trait
boundary, strictly above HTTP request construction, so there is nothing credential-shaped for it
to see or need to scrub. See the
[`crates/core/src/cassette.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/core/src/cassette.rs)
module doc for the full, authoritative scope (including one documented, narrow gap around the
edge extraction user prompt).

Because cassettes are plain JSONL with no credential material, they're safe to commit as test
fixtures — see
[`crates/core/tests/fixtures/README.md`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/core/tests/fixtures/README.md)
for this repo's fixture-capture conventions.

### Failure-record sidecar

A failed extraction call — an HTTP error, a malformed/unparseable response, or budget
exhaustion that persists after one retry — appends one record to a sidecar file,
`<cassette-path>.failures.jsonl`. A call that ends in an error never produces a cassette
record (the cassette's success-only invariant is unaffected). Edge-budget exhaustion is the one
non-fatal class: the call still succeeds with an empty edge list, so it produces both a cassette
record and a sidecar record — entity-budget exhaustion, by contrast, is fatal to the call and
produces only the sidecar record. This is created wherever a cassette is being recorded (both
`LCG_RECORD_LLM` and `lcg-eval --record-cassette`) — never in replay mode, since no live failure
can occur there. The file is created eagerly (empty, if no failures occur) alongside the
cassette itself.

Each record is a JSON object with:

| Field | Description |
|-------|-------------|
| `ts_ms` | Unix epoch milliseconds |
| `model` | The model name in force for this call |
| `call_type` | `"entities"` or `"edges"` |
| `chunk_key` | The episode name (production) or corpus chunk title (`lcg-eval`), or `null` |
| `classification` | `"http_error"`, `"truncation"`, or `"malformed"` |
| `raw_body` | The **complete** raw response body — never truncated to a prefix |
| `finish_reason` | The provider's stop/finish reason, or `null` for an HTTP-level failure |
| `completion_tokens` | Output token count, or `null` if unavailable |
| `max_tokens` | The `max_tokens` value in force for the failing call |

A single sidecar file is capped at 20MB; once appending would exceed that, it's rotated to a
numbered `<cassette-path>.failures.N.jsonl` file (matching the WAL's own byte-size rotation
convention) so a long-running service's sidecar can't grow without limit. Individual records are
never truncated to hit this cap — only the aggregate is bounded. See
[ADR-0306](adr/0306-extraction-failure-sidecar-and-truncation-visibility.md) for the design
rationale, and [Telemetry](telemetry.md#extraction_failure) for the event that drives this sink.

## Extraction-quality eval harness

The `lcg-eval` binary (`crates/eval`) measures extraction quality directly against this
engine's own prompts and extractor clients — no captured/copied prompts, so a prompt change
either updates the eval or breaks its build. It closes the gap noted in
[ADR 0041](adr/0041-local-openai-compatible-extraction-adapter.md): the local extraction
adapter's quality claim used to rest on a manual-testing caveat instead of anything
measurable. See [extraction-quality-evaluation.md](extraction-quality-evaluation.md) for the prior
research findings this harness re-baselines, and
[eval-full-corpus-runbook.md](eval-full-corpus-runbook.md) for the maintainer-run full-corpus model
comparison (hosted Anthropic vs. local qwen3.6-27b) built on top of it, with the exact commands.

### Running the harness

```bash
export ANTHROPIC_API_KEY=sk-ant-...   # hosted baseline + LLM-as-judge scoring
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=local \
  --reference baseline
```

This runs both backends over the default corpus subset (the first 50 chunks of the public Simple
English Wikipedia fixture,
`crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl`) and prints a report with, per
backend: strict-string and LLM-as-judge F1 for entities/edges/summaries, latency percentiles,
error rate, and structured-output reliability (clean/recovered/malformed JSON parse counts).
Pass `--output report.json` to also write the report as JSON. Run `cargo run -p lcg-eval --
--help` for the full flag reference.

Each candidate also carries a `truncated` count — `retry_succeeded` (a doubled `max_tokens`
retry recovered) and `exhausted` (it didn't) — surfaced separately from `clean`/`recovered`/
`malformed`. Edge-budget exhaustion is deliberately non-fatal (it returns an empty edge list
rather than erroring), which is otherwise indistinguishable in the report from a chunk where the
model genuinely extracted zero edges; a non-zero `exhausted` count on a chunk means the low
count is suppressed output, not a quality signal. The human-readable report only prints a
`truncated:` line when the count is non-zero, so a clean run's output is unchanged. Pair this
with `--record-cassette` to get the exact raw response for any exhausted call from the
[failure-record sidecar](#failure-record-sidecar).

To validate the judge itself rather than compare backends, point `--reference` and a second
`--backend` at the *same* spec (a baseline-vs-itself run): the judged score should land near
1.0 (pure wording-variance noise floor) while the strict-string score is materially lower —
this is what the `eval.yml` workflow's on-demand smoke pass checks.

### Previewing a run (`--dry-run`)

Before committing to a multi-hour, real-money run, add `--dry-run` to any invocation:

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=baseline.jsonl \
  --backend candidate=anthropic:model=claude-haiku-4-5-20251001 \
  --reference baseline \
  --all \
  --dry-run
```

This resolves every `--backend` spec exactly the way a real run would — the replay-or-live
decision, a cassette backend's on-disk record count, and the requested scope (`--limit N`
or the full corpus) — and prints the plan without making a single outbound call. It also
names any guard that would abort a real run: two backends resolving to the same cassette
(by path or by byte-identical content), or a cassette that's corrupt or has a duplicate
key. `--dry-run` itself always exits 0 for a syntactically valid invocation, even when the
plan shows a guard that would abort a real run — the point is to see the plan, not to run
the guard as a separate pass-fail check. Combining `--dry-run` with `--record-cassette`
writes nothing.

These are the same guards a real run enforces unconditionally before touching the network:
a duplicate-keyed or otherwise corrupt cassette is rejected at load time (distinguishable by
error type — `Error::CassetteDuplicateKey` vs. `Error::CassetteCorrupt` — not just by exit
code), and two cassette backends that would make the comparison degenerate (identical path
or identical content) are rejected before any extraction happens. A cassette covering fewer
chunks than the requested scope is not an abort condition — it's reported as a coverage
note, since the shortfall already shows up honestly in `error_rate`. `--dry-run` and a real
run share this resolution code exactly (see [ADR-0052](adr/0052-lcg-eval-dry-run-shares-the-real-run-resolution-path.md)),
so the preview cannot drift from what actually happens.

A pair of backends `--record-cassette`d fresh in the *same* invocation can't be checked this
way — there's nothing on disk to hash until the run finishes — so that half of the identity
guard runs post-run instead, before the report is ever printed or written: if two freshly
recorded cassettes come out byte-identical, the run still fails loudly, just after capture
rather than before it.

### Adding a candidate backend

`--backend NAME=SPEC` is repeatable. `SPEC` is one of:

- `anthropic[:model=<MODEL>]` — the hosted baseline, via `AnthropicExtractor`. Reads
  `ANTHROPIC_API_KEY`.
- `oai-http:url=<URL>[,model=<MODEL>]` — an OpenAI-compatible local endpoint over HTTP, via
  `OaiExtractor`.
- `oai-uds:path=<SOCKET_PATH>[,model=<MODEL>]` — the same, over a Unix domain socket (e.g. a
  local `mlx_lm.server` instance).
- `cassette:path=<PATH>` — replay a previously recorded cassette instead of making live LLM
  calls, via `ReplayingExtractor`. Makes zero outbound requests; a cassette miss fails loudly
  with `Error::CassetteMiss` rather than falling through to a live call. Cannot be combined
  with `--record-cassette` for the same backend name (recording a replay is meaningless).

No new backend *kind* should be needed for a new model — point an `oai-http`/`oai-uds` spec
at any OpenAI-compatible server. Adding a genuinely new provider means extending
`crates/eval/src/backend.rs`'s `BackendKind`/`build_extractor` the same way `OaiExtractor` was
added to `crates/core/src/extractor.rs` — reuse an existing `Extractor` implementation rather
than writing new HTTP/JSON client logic in the harness.

Add `--record-cassette NAME=PATH` to wrap a configured backend in a cassette recorder
(see [Record/replay cassettes](#recordreplay-cassettes) above) so a single corpus pass yields
both the eval report and a recorded cassette. To replay a cassette recorded this way on a
later run without paying for the extraction calls again, use a `cassette:path=<PATH>` backend
spec instead — see [eval-full-corpus-runbook.md](eval-full-corpus-runbook.md)'s "Resuming a
partial run" section for a worked example.

### Running under an ontology (`Open`/`Strict`)

By default every run above is **freeform**: the model invents its own entity/relation type
vocabulary, and `ExtractOptions.ontology` is `None`. Pass `--ontology <PATH>` to load an
`Ontology` from a bare YAML file (not necessarily inside a `.lcg`-rooted workspace — this is a
standalone eval fixture) and thread it through every extraction call instead, exercising the
same `Open`/`Strict` prompt-injection regimes production ingestion uses:

- `--ontology <PATH>` — load the ontology. Omit for the unchanged freeform behavior.
- `--ontology-mode <open|strict>` — which regime to apply; defaults to `strict` when
  `--ontology` is given without it, and overrides any `mode:` the file itself declares. Rejected
  as a usage error if given without `--ontology` (there's nothing to apply the mode to).

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=anthropic \
  --backend local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions,model=local \
  --reference baseline \
  --ontology crates/core/tests/fixtures/real_corpus_wal/ontology.yaml \
  --ontology-mode strict
```

The report's top-level `ontology_mode` field records which regime produced it
(`"freeform"`/`"open"`/`"strict"`), and — `Strict` only — each candidate also carries a
`vocabulary_compliance` metric: how often that backend emitted an entity or relation type
outside the ontology's declared vocabulary, tracked separately from
`structured_output.{clean,recovered,malformed}` so a model producing syntactically valid JSON
that simply ignores the closed type list isn't scored as if its structured-output reliability
were perfect. See [eval-full-corpus-runbook.md](eval-full-corpus-runbook.md)'s "Running the
ontology mode matrix" section for the full freeform/`Open`/`Strict` three-command comparison
procedure and `crates/eval/scripts/run_mode_matrix.sh` for a runnable version of it.

### Blind pairwise judging (`--judge-mode`)

The reference-mode report above measures **similarity to `--reference`**, not quality — the
reference is one more model's output, not ground truth, so a candidate that extracts something
the reference missed is scored as a false positive for being right. `--judge-mode pairwise`
adds a second, reference-agnostic signal: for every pair of configured backends, the
judge sees the source chunk plus the two extractions *unlabelled* (slot A / slot B, no backend
name, model id, or provider reachable by the judge) and picks which better captures the
content, per axis (entities/edges/summary). No backend is privileged.

```bash
cargo run --release -p lcg-eval -- \
  --backend baseline=cassette:path=baseline.jsonl \
  --backend candidate=cassette:path=candidate.jsonl \
  --backend qwen=cassette:path=qwen.jsonl \
  --reference baseline \
  --judge-mode pairwise
```

- `--judge-mode <reference|pairwise|both>` (default `reference`) — `reference` is the
  unchanged pre-pairwise behavior (omitting the flag leaves output byte-identical). `pairwise`
  runs only the blind pairwise pass above — the reference-mode judge calls above are skipped
  entirely, not run-and-ignored, so a candidate that's only interested in the pairwise signal
  doesn't pay for reference-mode judge calls it didn't ask for. `both` runs both passes.
- Every chunk is judged in **both** slot orders (an extraction placed in slot A once, slot B
  once), with slot assignment derived deterministically from a hash of the chunk key and the
  two backend names — never wall-clock or RNG, so a re-run reproduces the same result exactly.
  Agreeing verdicts count as a win for the agreed side; disagreeing verdicts count as a tie and
  increment an **order-inconsistency** counter — a judge that flips its answer when the
  operands swap is reporting position bias, not model quality, and that must surface as a
  number rather than be averaged away.
- The report's `pairwise` section (present only when `--judge-mode` requested it — absent
  entirely, not null, under the default `reference` mode) lists, per backend pair and per
  axis: wins, losses, ties, win rate (excluding ties from the denominator), the
  order-inconsistency rate, chunks compared, and chunks skipped (present on only one side of
  the pair — e.g. differing cassette coverage — never silently counted as a loss).
- **The reference-vs-candidate pair is always included** — pairwise mode covers every
  unordered pair among the configured `--backend`s, not just candidates against the
  designated reference.
- **Judge calibration control**: configure the same model as two independently-recorded
  cassettes under different backend names (the pairwise analogue of the reference-mode
  noise-floor pattern above) and judge that pair too. Two independent samples of the same
  model should split near 50/50 on every axis. A run prints a stderr note for **every** pair
  whose win rate falls outside **45–55%** (`pairwise::CALIBRATION_BAND_LOW`/`_HIGH`) — which
  pair is *the* calibration control is operator knowledge the harness can't derive from
  `--backend` specs alone (two independently-recorded `cassette:path=` files of the same
  model, the pattern above, share no spec string to detect it by), so the note doesn't assert
  bias outright: if the flagged pair is your calibration control, the deviation likely means
  judge position bias and every pairwise result in the run should be treated with suspicion;
  if it's a genuine candidate-vs-candidate pair, landing outside the band is the expected,
  desired signal (the whole point of pairwise judging), not evidence of bias. A separate
  warning fires for every pair whenever its order-inconsistency rate exceeds **20%**
  (`pairwise::ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD`) — above that, the judge is flipping
  its answer often enough that the win rate isn't distinguishable from noise; this one is not
  conditional on which pair is the calibration control. Neither warning blocks the run; both
  are stderr-only, so the report artifact itself stays pure data. See
  [ADR-0050](adr/0050-blind-pairwise-judging.md) for the rationale behind both numbers.
- A degenerate pair — two backends whose specs resolve to the *identical* `cassette:path=`
  — is rejected at CLI parse time, before any judge call, naming the offending backend names.
  This does **not** reject the same *live* spec (e.g. `anthropic:model=X`) configured twice
  under different names — that's the calibration pattern above, which produces two
  independently-sampled, non-degenerate outputs and must keep working.
- Pairwise mode reuses the same `run_results` reference mode already produced — it makes
  **zero additional extraction calls**, whether the backends are live or `cassette:path=`
  replays, and reuses `--judge-cache` under a disjoint `prompt_name` family so pairwise and
  reference-mode cache entries can never collide.

### Cost implications

Every corpus chunk costs two extraction calls (entities, then edges) per configured backend,
plus one LLM-as-judge call per scored comparison (entities/edges/summaries) against the
`--reference` backend. Judge calls are the expensive part — they hit a hosted model
(`claude-sonnet-4-6` by default, `--judge-model` to override) regardless of which backends are
under test. The **on-disk judge cache is mandatory, not optional**: pass `--judge-cache
<path>` (default `judge_cache.jsonl` in the current directory) and re-runs against the same
corpus and backends make zero new judge calls — always reuse the same cache path across
repeated runs rather than deleting it. The default corpus subset (50 chunks, override with
`--limit N` / `--all`) is sized to keep a default run affordable; widening it multiplies cost
roughly linearly in chunk count. Without `ANTHROPIC_API_KEY` set, the harness still runs and
reports strict-string F1, but skips judge scoring entirely (no cost, no judged F1 in the
report).

`--judge-mode pairwise`/`both` multiplies judge-call volume further: every unordered backend
pair (C(N,2), not N-1) is judged in both slot orders across all three axes — a 3-backend
`pairwise` run costs 3 pairs × 2 orders × 3 axes = 18 judge calls per chunk, versus reference
mode's 2 candidates × 1 order × 3 axes = 6. Still **zero extraction calls** either way — the
judge cache applies identically, so a re-run against the same `--judge-cache` path costs
nothing.
