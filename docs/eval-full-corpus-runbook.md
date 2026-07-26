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
completed), split into two invocations instead of re-running all three backends. Note this still
re-pays for the `baseline` extraction calls in the second invocation, since `--reference` must
name a `--backend` that's actually run in the same invocation as the leg being scored — there's
no way to score the local leg on this run using cassette-replayed results from your prior
attempt's cassette (`lcg-eval`'s `BackendKind` only builds live `anthropic`/`oai-http`/`oai-uds`
extractors, not a replay-from-cassette kind).

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

## Committing the cassettes

Once both cassette files exist, commit them as plain, uncompressed JSONL — the same convention
#232 established (one record per line: `key`, `call_type`, `provider`, `model`, `timestamp`,
`request`, `response`) and README's "Record/replay cassettes" section documents in full. If
either file turns out to be large enough to warrant it, follow the size-management precedent in
`crates/core/tests/fixtures/real_corpus_wal/README.md` (uncompressed JSONL, not gzip — git
already compresses blobs, and gzip defeats diffability and delta compression across future
re-captures) rather than introducing a new convention.

## Verifying deterministic replay (SC-005)

`lcg-eval` itself has no "replay" backend kind — its `BackendKind` only builds live
`anthropic`/`oai-http`/`oai-uds` extractors. Verifying that a committed cassette replays
deterministically with zero live calls goes through the same primitive
`crates/core/tests/cassette_record_replay.rs` already exercises,
`lcg_core::cassette::ReplayingExtractor::load`. There's nothing to load yet in this Fabrik run
(no cassette exists), so this is left as a code sketch for a follow-up `#[ignore]`d integration
test, once the cassettes are committed — following the existing precedent set by
`crates/core/tests/real_corpus_replay_perf.rs` for slow, explicitly-invoked-only tests. SC-005
covers **both** committed cassettes, so the sketch below verifies each one:

```rust
// crates/eval/tests/cassette_replay_determinism.rs (sketch — write once cassettes exist)
use lcg_core::cassette::ReplayingExtractor;
use lcg_core::extractor::{ExtractOptions, Extractor};
use lcg_core::types::SourceType;
use lcg_eval::corpus::{default_corpus_path, load_corpus};

const REFERENCE_TIME: &str = "2026-01-01T00:00:00Z"; // match runner.rs's constant exactly

async fn assert_cassette_replays_every_corpus_chunk(cassette_path: &str) {
    let corpus = load_corpus(default_corpus_path()).unwrap();
    let replayer = ReplayingExtractor::load(cassette_path).unwrap();

    for chunk in &corpus {
        // Same ExtractOptions shape `run_backend` (crates/eval/src/runner.rs) used to
        // produce the recording — matching it exactly is what makes the request hash
        // match the cassette's recorded key. A successful `Ok(_)` for every chunk, with
        // zero outbound HTTP calls (ReplayingExtractor never dials out, by construction),
        // is the SC-005 property.
        let opts = ExtractOptions {
            episode_body: &chunk.prose,
            group_id: "eval",
            source_type: SourceType::Text,
            custom_instructions: None,
            reference_time: REFERENCE_TIME,
            ontology: None,
        };
        replayer.extract(opts).await.expect("cassette entry for this chunk");
    }
}

#[tokio::test]
#[ignore]
async fn qwen_cassette_replays_every_corpus_chunk_with_zero_live_calls() {
    assert_cassette_replays_every_corpus_chunk("qwen3.6-27b.jsonl").await;
}

#[tokio::test]
#[ignore]
async fn anthropic_cassette_replays_every_corpus_chunk_with_zero_live_calls() {
    assert_cassette_replays_every_corpus_chunk("anthropic-claude-haiku-4-5-20251001.jsonl").await;
}
```

Run both with `cargo test -p lcg-eval --release --test cassette_replay_determinism -- --ignored`
once written, mirroring how `real_corpus_replay_perf.rs`'s tests are run today.
