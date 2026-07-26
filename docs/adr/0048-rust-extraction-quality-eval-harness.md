# ADR-0048: Rust Extraction-Quality Eval Harness — Architecture and Judge Design

**Status**: Accepted
**Date**: 2026-07-26
**Issues**: #228

## Context

There was no way to measure extraction quality in this repo. That gap blocked a real
decision: the local/OpenAI-compatible extraction adapter (ADR-0041) makes "fully local"
extraction reachable, but its own Consequences section concedes quality "can only be
confirmed by manual testing on macOS with Apple Intelligence enabled, not by this repo's
standard CI."

Prior research existed in a separate private repo (`verveguy/liminis-framework`,
`eval/extraction-quality/`) but had two disqualifying problems: it was a Python package
(13 modules, `pyproject` + `uv.lock`) in an otherwise `cargo`-only project, and its numbers
had already gone stale — the harness replayed message arrays captured from a *different*
codebase's prompts, so a 2026-04-30 prompt restructuring silently invalidated every absolute
F1 number without the harness noticing.

This issue builds the harness only. The full hosted-vs-local model comparison (Anthropic vs.
qwen3.6-27b, capturing cassettes, publishing results) is #248 — a maintainer-run operation
requiring a local model server, an API key, and real spend.

## Decisions

### 1. New `crates/eval` workspace member, not an `examples/` binary

The harness needs multiple internal modules (corpus loading, backend construction, a runner,
judge scoring, an on-disk cache, strict-string metrics, failure-taxonomy bucketing, report
emission) plus its own integration-test directory — more than a single `examples/*.rs` file
supports cleanly, and FR-001 phrases the crate-vs-example choice as "e.g.", leaving it open.

**Consequence, stated explicitly**: because CI's `cargo build`/`test`/`clippy`/`fmt` steps run
unscoped across the whole workspace, `crates/eval` is compiled and linted on every PR even
though FR-011 requires the harness itself to run only on demand. This is mitigated by keeping
every default `cargo test` in the crate network-free — anything that would spend money or need
a live endpoint is either a canned-fake test (the norm here) or would need `#[ignore]` (none
currently required, since all tests use `ConfigurableExtractor`/in-memory fakes).

### 2. New `crates/core` telemetry event, not an `Err(Error)`-only inference

FR-007 requires structured-output reliability (malformed/unparseable JSON rate, defensive-parse
recoveries) as a first-class metric, separate from F1. Before this issue, `OaiExtractor`'s
`parse_oai_entity_response`/`parse_oai_edge_response` returned an `OaiChatOutcome::ParseError`
with zero telemetry on malformed JSON, and a successful parse looked identical whether or not
`extract_json_block`'s fence/prefix stripping was needed — the "recovered" case was entirely
unobservable from outside the module.

**We added `TelemetryEvent::StructuredOutputParse { ts_ms, model, call_type, outcome }`**
(`outcome`: `"clean"` | `"recovered"` | `"malformed"`), emitted from `do_extract_entities`/
`do_extract_edges` on every `Success`/`ParseError` outcome (not `BudgetExhausted`, which is
already covered by the existing `ExtractionTruncated` event and is a distinct failure mode —
truncation, not malformed structure). `OaiChatOutcome::Success` was changed from a bare
`Success(T)` to `Success { value: T, defensive_parse: bool }`, computed by comparing
`extract_json_block(content)` against `content.trim()`.

The alternative — inferring a narrower metric purely from `Err(Error)` variant matching on
`extract()`'s return value — was rejected because it structurally cannot observe "recovered via
defensive parse" at all: that's precisely the axis ADR-0041 flags as the local path's
distinguishing risk relative to the hosted path (schema-enforced `tool_use` vs.
`response_format: json_object` + prompt instruction), so leaving it unmeasurable would defeat
FR-007's purpose. `AnthropicExtractor` is expected to report near-zero malformed/recovered
counts, structurally, since `tool_use` extraction (ADR-0010) doesn't go through this parse path
at all — this asymmetry is itself part of what the metric is for.

### 3. The judge is a standalone Anthropic client, not `AnthropicExtractor` reused

The prior Python harness's judge is a raw `messages.create` call, not an implementation of its
own `Extractor`-equivalent abstraction — it asks a free-form semantic-equivalence question
("do these two extractions convey the same information?") with its own prompt and response
shape, entirely independent of whichever backends are under comparison (including when
comparing two non-Anthropic candidates against each other).

**We ported that shape directly**: `crates/eval/src/judge.rs`'s `AnthropicJudgeClient` makes a
minimal, direct Anthropic Messages API call, duplicating `AnthropicExtractor::do_classify_entities`'s
plain-text-call pattern (~30 lines) rather than exporting `extractor.rs`'s private
`extract_json_block` helper out of `crates/core` for this one caller. `JudgeClient` is a small
trait (mirroring `Extractor`'s `BoxFuture`-returning shape) specifically so tests can substitute
a canned/mocked judge — judge calls cost real money, and no test in this crate's default `cargo
test` run should make one.

### 4. Judge prompt, P/R/F1 derivation, and failure taxonomy are ported verbatim

FR-008 requires the judge prompt and failure taxonomy to come from the prior Python harness as
data/config, not be re-derived from intuition — the judge prompt and taxonomy are the hard-won
part of this work (the prior research found strict-string scoring put a sonnet-vs-sonnet
comparison at 0.771 on edges, a 23% floor from pure wording variance, while the judge scored it
at 0.978; SC-004 exists to reproduce that property).

`crates/eval/src/judge.rs`'s `JUDGE_PROMPT` constant, and the precision/recall/F1 derivation
(`precision_recall_f1`, with the ported 1.0/0.0 empty-denominator defaults) are copied verbatim
from the Python source. `crates/eval/src/failure_taxonomy.rs`'s bucket rules (`article_dropped`,
`modifier_dropped`, `granularity_merged`, `case_or_format`, `missing_entity` for entity misses;
their extra-side counterparts; `inverted_edge`, `synonym_relation`, `missing_edge`, `extra_edge`
for edges) are ported against the actual `failure_taxonomy.py` source
(`verveguy/liminis-framework@main:eval/extraction-quality/failure_taxonomy.py`, fetched directly
via `gh api repos/verveguy/liminis-framework/contents/...` — the private repo is read-accessible
via `gh`, not just describable from research notes), not re-derived from a prose summary of its
rules. Two normalization details only surface by reading the actual source rather than a
description of its behavior, and are called out explicitly in `failure_taxonomy.rs`'s module
doc: the article check only strips a leading `"the "` (not `"a "`/`"an "` — those cases still
get classified correctly, just via the generic token-subset check rather than a dedicated
article bucket), and the modifier/granularity token-subset check splits on whitespace only,
without folding `-`/`_` to spaces or stripping punctuation (that folding is scoped to the
separate `case_or_format` check only). Getting these two details wrong wouldn't have broken
tests written against an intuitive reading of the rules, only silently diverged bucket
assignments from the ported source on hyphenated/punctuated names — exactly the class of gap
FR-008 exists to prevent.

### 5. Judge cache key scheme is ported from the source, extended with `judge_model`

FR-005/SC-003 require re-runs against the same corpus and backends to make zero new judge
calls. Getting the cache key's *scope* right is load-bearing: too narrow and re-runs pay
repeatedly; too broad and stale entries silently reuse verdicts across semantically different
comparisons.

**We reused the source's scheme with one addition**: `sha256(json({"cand": ..., "judge_model":
..., "prompt": ..., "ref": ...}, sort_keys=True))[:24]`. The ported original pinned a single
fixed judge model and so never needed the model in its key; this harness exposes
`--judge-model` as a run-time choice (Decision 4 doesn't apply that same constraint to the
judge, unlike backend selection), so without `judge_model` in the key, switching judge models
against an existing cache file would silently return another model's verdicts instead of
re-judging. `crates/eval/src/judge_cache.rs`'s `canonical_json` recursively sorts object keys
before serializing — `serde_json` here is built with the workspace's `preserve_order` feature,
so a `Map` serializes in insertion order, and inserting already-alphabetically-sorted key/value
pairs reproduces Python's `sort_keys=True` output exactly, including nested objects. The cache
itself is an on-disk, append-only JSONL file (mirroring `CassetteWriter`'s convention —
re-opening an existing path never truncates), loaded fully into memory at startup so a cache
hit never touches disk on the read path; a disk write is durable before the in-memory map is
updated, so a failed write can never leave a verdict visible in-process without also being
persisted.

### 6. Default corpus subset is the first 50 chunks of the #217 fixture, not all 228

`corpus_prose.jsonl` (`crates/core/tests/fixtures/real_corpus_wal/`, #217) has 228 chunks. Two
extraction calls per chunk (entities, then edges) times N backends, plus a judge call per
scored comparison for every non-cache-hit item, makes the full corpus nontrivial recurring
spend if it were the default. The Assumptions section of the spec requires "the default corpus
is kept small enough to be affordable," and the prior Python harness's own defaults were
40–75-chunk subsets, not its full corpora.

`crates/eval/src/corpus::select_subset` deterministically takes the first N chunks (file order
is already fixed/committed, so this is reproducible run-to-run) — 50 by default
(`cli::DEFAULT_LIMIT`), overridable with `--limit N` or `--all`. No second corpus is curated;
this reuses #217's manifest exactly, per FR-009.

### 7. Hand-rolled CLI parsing, no `clap`

Matches the existing rationale in `crates/service/src/cli.rs`: `lcg-eval`'s flag surface
(`--backend`, `--reference`, `--limit`/`--all`, `--record-cassette`, `--judge-cache`,
`--judge-model`, `--output`, `--corpus`) is a handful of flags that doesn't clear the bar for a
new dependency, and a pure `parse_args(&[String]) -> Result<CliMode, String>` function is
exactly as unit-testable as a `clap`-derived parser would be.

### 8. `eval.yml` runs only a baseline-vs-itself smoke pass

FR-011 requires the harness to run on demand, following the existing `bench.yml`
`workflow_dispatch`-only pattern — not on every PR. `.github/workflows/eval.yml` runs a small
(`--limit 5` by default) comparison with the *same* backend spec configured as both
`--reference` and a second `--backend`, checking SC-004's noise-floor property (judged F1 near
1.0, strict F1 materially lower) using only the `ANTHROPIC_API_KEY` secret CI already has
access to. The full hosted-vs-local comparison (#248) needs a local model server and is
explicitly out of this workflow's scope — it is a maintainer-run operation, not CI.

## Consequences

- `AnthropicExtractor`/`OaiExtractor` are reused completely unchanged at the call-site level
  (FR-003) — the harness never reimplements HTTP/JSON client logic. The only production-code
  change in `crates/core` is the new `StructuredOutputParse` telemetry event and the
  `OaiChatOutcome::Success` shape change needed to compute it.
- Because the harness calls `Extractor::extract()` directly, which renders this engine's actual
  `prompts::*` functions, a prompt-template edit is structurally visible to the eval or breaks
  its build (SC-005) — there is no captured/copied prompt text anywhere in `crates/eval` to go
  stale, which is the specific failure mode that invalidated the prior Python harness's results.
- FR-012's cassette-recording support (a single corpus pass yielding both the eval report and a
  recorded cassette, avoiding paying for the corpus twice per model in #248) required no new
  capability from `crates/core/src/cassette.rs` — `crates/eval/src/backend.rs` wraps a
  configured backend's `Arc<dyn Extractor>` in the already-existing `RecordingExtractor`
  decorator (#232) before running it through the harness.
- The judge-model default (`claude-sonnet-4-6`) and its cost are a recurring, real expense for
  every non-cache-hit comparison; the on-disk cache (Decision 5) is what makes repeated
  development-time runs affordable, not a nice-to-have.
- The failure-taxonomy bucket *rules* are faithfully ported (Decision 4), but the Rust
  implementation is a fresh translation of documented rules rather than a line-for-line port of
  Python source unavailable to this repo's contributors — a future discrepancy against the
  original Python behavior would need to be resolved by re-reading the rule descriptions in
  issue #228's Research stage output, not by diffing against Python source in this repo.

## Related

- ADR-0041: `OaiExtractor` and the `Arc<dyn Extractor>` generalization this harness's
  multi-backend orchestration depends on; its Consequences section names this issue as the
  mechanism that re-baselines the extraction-quality findings it cites.
- ADR-0044: the cassette record/replay seam (`RecordingExtractor`/`ReplayingExtractor`) FR-012's
  cassette-capture support wraps unmodified.
- ADR-0010: `tool_use` structured-output extraction — the reason `AnthropicExtractor` is
  expected to report near-zero `StructuredOutputParse` malformed/recovered counts, structurally.
- `crates/core/src/telemetry.rs`: `TelemetryEvent::StructuredOutputParse`.
- `crates/core/src/extractor.rs`: `OaiChatOutcome::Success { value, defensive_parse }`,
  `do_extract_entities`/`do_extract_edges` emission sites.
- `crates/eval/src/judge.rs`, `judge_cache.rs`, `failure_taxonomy.rs`, `metrics.rs`: the ported
  judge prompt, cache-key scheme, taxonomy rules, and strict-string comparator.
- `crates/eval/src/corpus.rs`: the #217 fixture loader and deterministic default-subset
  selection.
- README.md's "Extraction-quality eval harness" section: user-facing usage, backend-addition,
  and cost documentation (FR-013).
- #248: the maintainer-run full-corpus hosted-vs-local model comparison built on this harness.
