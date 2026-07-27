# `lcg-eval` operational scripts

Run these in order. They exist because the full-corpus benchmark (#248) is a
multi-hour, real-money operation with several ways to silently waste it, and every
one of those traps has now been hit at least once.

| Script | What it does | Cost |
|---|---|---|
| `01-start-server.sh` | Starts `mlx_lm.server` **with thinking disabled** and verifies it | free |
| `02-timing-check.sh` | Measures tok/s and projects full-corpus runtime | free |
| `03-capture-qwen.sh` | Captures the qwen cassette, no hosted spend | local compute only |
| `04-full-run.sh` | The real benchmark: noise floor + hosted-vs-qwen | **real money** |

```bash
crates/eval/scripts/01-start-server.sh
crates/eval/scripts/02-timing-check.sh          # do not skip
crates/eval/scripts/03-capture-qwen.sh 25       # validate on 25 chunks first
crates/eval/scripts/03-capture-qwen.sh          # then the full corpus
export ANTHROPIC_API_KEY=...
crates/eval/scripts/04-full-run.sh
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

**Never replay a cassette into both hosted legs.** `baseline` and `candidate` are
deliberately two *independent* live Anthropic runs; their disagreement **is** the
noise floor. Feeding one cassette to both makes them byte-identical and judged F1
becomes 1.000 by construction. See #263 for the replay backend.

**Always run `02` before `03` or `04`.** It is the 30-second check that would have
caught thinking mode before a 15-hour capture, and it fails loudly if reasoning
tokens are present or the projected runtime is out of line with the April baseline.

## Reference

- `docs/eval-full-corpus-runbook.md` — the prose runbook these automate
- `docs/history/extraction-eval-2026-04.md` — April 2026 model rankings and latency
  baselines on this same M3 Ultra
- `docs/extraction-quality-evaluation.md` — methodology
