# `lcg-eval` operational scripts

Run these in order. They exist because the full-corpus benchmark (#248) is a
multi-hour, real-money operation with several ways to silently waste it, and every
one of those traps has now been hit at least once.

| Script | What it does | Cost |
|---|---|---|
| `01-start-server.sh` | Starts `mlx_lm.server` **with thinking disabled** and verifies it | free |
| `02-timing-check.sh` | Measures tok/s and projects full-corpus runtime | free |
| `03-capture-qwen.sh` | Captures the qwen cassette, no hosted spend | local compute only |
| `04-full-run.sh` | The real benchmark: noise floor + hosted-vs-qwen | **real money**, one live leg |

```bash
crates/eval/scripts/01-start-server.sh
crates/eval/scripts/02-timing-check.sh          # do not skip
crates/eval/scripts/03-capture-qwen.sh 25       # validate on 25 chunks first
crates/eval/scripts/03-capture-qwen.sh          # then the full corpus
export ANTHROPIC_API_KEY=...
crates/eval/scripts/04-full-run.sh              # replays 03's cassette; no local server needed
```

## The traps these encode

**Thinking mode is on by default and ruins the benchmark.** `qwen3.6` emits
`<think>` reasoning before its answer. Measured: 1627 completion tokens for a
one-sentence input, ~1400 of them discarded reasoning. That is ~10x slower and
*scores worse* on enumeration — `docs/history/extraction-eval-2026-04.md` measured
`qwen3.6-27b-thinking-only` at **112.7s p50** vs **10.9s** without, and called it
"operationally non-viable". A full-corpus capture in thinking mode projects to 15+
hours and produces a cassette of the known-bad configuration. `01` disables it and
**fails loudly if reasoning tokens still come back**.

**The model id must match what the server advertises.** `mlx_lm.server` treats an
unrecognised id as a HuggingFace repo to download, so `model=qwen3.6-27b` fails every
call with `Repository Not Found` — a 100% error rate that only shows up as an empty
cassette. Use `mlx-community/Qwen3.6-27B-4bit`.

**Cassettes append, they never truncate** (pinned by
`writer_appends_without_truncating_across_multiple_opens`). Re-running without moving
the old file silently produces duplicate entries. `03` and `04` move any existing
cassette aside automatically.

**Judge scoring is key-gated, so capture and scoring are separable.** With
`ANTHROPIC_API_KEY` unset, `lcg-eval` skips LLM-as-judge entirely
(`main.rs:98-105`), which is what makes `03` free. `03` unsets it defensively —
otherwise a leftover export would bill judge calls to score qwen against itself.

**Replay the reference, never the noise floor.** Since #263 landed the
`cassette:path=<PATH>` backend, `04` replays `baseline` and `qwen` from recorded
cassettes and runs only **one** live hosted leg — `candidate`. That leg has to stay
live: its disagreement with `baseline` **is** the noise floor, two independent Haiku
samples of the same corpus. Point it at a cassette and both legs become byte-identical,
judged F1 is 1.000 by construction, and the measurement destroys itself while still
producing a plausible-looking report. A cassette miss is a per-chunk error
(`Error::CassetteMiss`), not a crash, so partial coverage shows up honestly in
`error_rate`.

**Cassettes are not reusable across ontology modes.** `opts.ontology` feeds both
`entity_system_prompt` and `edge_system_prompt`, and those are hashed into the cassette
key (`cassette.rs:69-88`). Adding `--ontology` therefore changes *every* key, and a
freeform cassette misses on all of them. The ontology axis of #266 needs its own
capture — including another full local qwen pass. Budget the hours; do not assume the
freeform capture carries over.

**Quiesce the machine before any timing run.** MLX inference on Apple Silicon is bound
by *memory bandwidth x active parameters per token*
(`docs/history/extraction-eval-2026-04.md`), and a concurrent compile-and-test job
contends for exactly that bandwidth — not just CPU. During the #248 capture a Fabrik
worker running `wal_replay` tests sat at ~460% CPU alongside the benchmark, which
inflates measured per-chunk latency. Recorded cassette *content* is unaffected, but
**latency percentiles taken under load must not be quoted as the model's throughput**.
`02` now reports load average and names any process over 100% CPU, and warns when the
machine is not quiet.

**Expect a long tail, and never quote the mean.** Per-chunk latency on the real corpus
ranges from ~9s to ~730s — a single hard chunk can drag a cumulative average by 10s or
more. The completed 228-chunk qwen capture measured **p50 39.8s, p95 212.6s, p99 377.9s
against a 62.4s mean**: the mean overstates the typical chunk by more than half, and
sits between p50 and p95 where it describes nothing. Report p50 with p95/p99 alongside.

While a run is in flight, judge it by its trailing window rather than its running mean —
a rising cumulative average with a flat trailing window is outliers, not degradation.

**Always run `02` before `03` or `04`.** It is the 30-second check that would have
caught thinking mode before a 15-hour capture, and it fails loudly if reasoning
tokens are present or the projected runtime is out of line with the April baseline.

## Reference

- `docs/eval-full-corpus-runbook.md` — the prose runbook these automate
- `docs/history/extraction-eval-2026-04.md` — April 2026 model rankings and latency
  baselines on this same M3 Ultra
- `docs/extraction-quality-evaluation.md` — methodology
