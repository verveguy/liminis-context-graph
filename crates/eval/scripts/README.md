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
| `05-score-only.sh` | Re-score three captured cassettes; resumes a died run | judge calls only |
| `06-ontology-matrix.sh` | The Open/Strict ontology arms, each with its own noise floor | **real money**, ~$92 + overnight |
| `test-scripts.sh` | Tests what's still shell-level — completeness thresholds, resume decisions, MODES/LIMIT/report-naming contracts | free, seconds |

```bash
cargo build --release -p lcg-eval               # the scripts check for this and refuse without it
crates/eval/scripts/01-start-server.sh
crates/eval/scripts/02-timing-check.sh          # do not skip; exits nonzero if the projection is bad
crates/eval/scripts/03-capture-qwen.sh 25       # validate on 25 chunks first
crates/eval/scripts/03-capture-qwen.sh          # then the full corpus
export ANTHROPIC_API_KEY=...
crates/eval/scripts/04-full-run.sh              # replays 03's cassette; no local server needed
crates/eval/scripts/05-score-only.sh            # only if 04's judging phase died

DRY_RUN=1 crates/eval/scripts/06-ontology-matrix.sh   # ALWAYS preview first
crates/eval/scripts/06-ontology-matrix.sh            # then the Open/Strict arms
```

**`06` is the only script here that spends money on startup, so it has two brakes.**
`DRY_RUN=1` prints the per-leg REPLAY/LIVE decision and the report names it would use,
without executing anything, and it works before the binary is built or the key is set.
`MODES=""` is a deliberate no-op: the assignment uses `${MODES-…}` rather than
`${MODES:-…}` precisely so an *empty* value means "do nothing" instead of falling back to
the full default matrix, which is how an early structural test of it began a live capture
by accident.

`DRY_RUN=1` no longer previews the conditions that would abort a real run (identical
cassettes, a corrupt or duplicate-keyed cassette) — those guards now live in `lcg-eval`
itself (#279), which enforces them identically on `--dry-run` and a real invocation, before
any outbound call. Run `lcg-eval --dry-run --backend ...` directly (see the top-level
README's "Extraction-quality eval harness" section) to preview them without spending
anything; `06`'s own `DRY_RUN=1` is purely about the shell's own resume/mode-looping
decisions now.

It reuses the #248 freeform cassettes rather than re-capturing them, so `MODES="freeform
open strict"` costs nothing extra for the freeform arm.

`DRY_RUN=1` proves the per-leg replay/live plan; `LIMIT=3` proves the *live* path for
pennies before an overnight commits to it. Smoke artifacts carry a `.limitN` suffix, so a
partial capture can never be picked up as a full one — a `LIMIT=210` run would otherwise
leave a cassette that passes the 90%-of-228 completeness bar and gets replayed as if whole.

They run from any checkout — the repo root is resolved from the script's own location —
and share `_common.sh`, which holds the repo/model/port resolution, the release-binary
check, the server health check, and the HTTP error handling that each script would
otherwise repeat and get subtly wrong in five places.

Overrides, all optional:

| Variable | Applies to | Effect |
|---|---|---|
| `LCG_EVAL_REPO` | all | Target a different checkout (cassettes and the binary live in whichever produced them) |
| `LCG_EVAL_MODEL` / `LCG_EVAL_PORT` | `01`–`03`, `06` | Local server identity (set in `_common.sh`, so every script that talks to the server honours them) |
| `LCG_EVAL_VENV` | `01` only | Python env used to *launch* the server. The other scripts only probe an already-running one, so overriding this has no effect on them |
| `LCG_EVAL_WORK` | all | Work directory (default `/tmp/eval248`, must be yours) |
| `LCG_EVAL_HAIKU` | `04`–`06` | Hosted model id |
| `LCG_EVAL_MAX_CHUNK_S` | `02` | Per-chunk ceiling before it refuses to green-light a capture |
| `LCG_EVAL_JUDGE_MODE` | `04`–`06` | `reference` \| `pairwise` \| `both`. **Defaults differ**: `04`/`05` use `reference` (~$21), `06` uses `both` (~$38/mode) so its arms stay comparable with the freeform run, which was judged with pairwise |
| `LCG_EVAL_JUDGE_MODEL` | `06` | Judge model. Changing it mid-matrix invalidates the comparison — it is part of the cache key and of what each F1 means |
| `LCG_EVAL_JUDGE_CACHE` | `06` | Judge cache path (shared with `run_mode_matrix.sh` on purpose: keys derive from prompt content, not backend names, so verdicts are reusable across both) |
| `LCG_EVAL_ALLOW_NO_KEY` | `05` | Permit a fully-cached re-score with no API key |
| `LCG_EVAL_ONTOLOGY` | `06` | Ontology fixture path |
| `MODES` | `06` | Which arms to run. **Empty means none** — see above |
| `DRY_RUN` | `06` | Print the plan, execute nothing |
| `LIMIT` | `06` | Run the **live** path over the first N chunks — the cheap end-to-end smoke test. Artifacts are suffixed `.limitN` so a partial capture can never be read as a full one |
| `REPORT_PREFIX` | `06` | Report filename prefix. The default deliberately differs from `run_mode_matrix.sh`'s, because that script's reports have no noise floor and the two are otherwise indistinguishable by name |

`test-scripts.sh` runs on every PR (CI's `eval script guards` step) and needs no network,
API key, model server, or built binary. It covers what's still genuinely shell-level here —
completeness-threshold resume decisions, mode/LIMIT looping, artifact naming. The guards
that decide whether a run's *result* would be trustworthy (corrupt/duplicate/identical
cassettes, truncated captures) moved into `lcg-eval` itself (#279) and are covered by that
crate's own Rust tests instead — see `crates/eval/src/plan.rs` and
`crates/eval/src/report.rs`.

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
(the judge client is only constructed when the key is present), which is what makes `03`
free. `03` unsets it defensively —
otherwise a leftover export would bill judge calls to score qwen against itself.

**Replay the reference, never the noise floor.** Since #263 landed the
`cassette:path=<PATH>` backend, `04` replays `baseline` and `qwen` from recorded
cassettes and runs only **one** live hosted leg — `candidate`. That leg has to stay
live: its disagreement with `baseline` **is** the noise floor, two independent Haiku
samples of the same corpus. Point it at a cassette and both legs become byte-identical,
judged F1 is 1.000 by construction, and the measurement destroys itself while still
producing a plausible-looking report — `lcg-eval` itself now refuses to run rather than
let that happen (#279). A cassette miss is a per-chunk error (`Error::CassetteMiss`), not a
crash, so partial coverage shows up honestly in `error_rate`.

**Judge failures are survivable now, but resume with `05`, not `04`.** They used to be
fatal: an error propagated out of `score_candidate` and killed the run, which is how the
#248 scoring phase died 17 calls into ~1340. Since #271 and #277 a failed judge call costs
its chunk and is counted (`judge_errors` in the report, per-pair for pairwise), transport
errors retry on the same ladder as 429/529, and a systemic failure trips a circuit breaker
after 10 consecutive errors rather than grinding through thousands of doomed calls — which
is exactly what happened when the account's spend limit was reached mid-run: 10 real
failures, then 100 short-circuited, and a complete report anyway.

Nothing is lost when a run does die: cassettes are on disk and each verdict appends to the
judge cache the moment it arrives. But **resume with `05`** — `04` re-runs the live
extraction leg, and you would pay for it a second time for no benefit.

Read `judge_errors` before trusting the averages. A nonzero value means some chunks are
missing from the affected axis, and for pairwise it is reported per pair — one #248 pair
finished on 113 chunks rather than 223 because the limit was hit near the end.

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
caught thinking mode before a 15-hour capture, and it **exits nonzero** if reasoning
tokens are present or the projected runtime exceeds `LCG_EVAL_MAX_CHUNK_S` (default 60s
per chunk, against an April baseline of ~11s).

**A reachable port is not a healthy server.** `03` and `04` re-verify the served model id
and that thinking is off, rather than trusting that `01` was the thing that started what
is listening. An operator pointing `03` at a server left over from a reboot or an earlier
session would otherwise repeat the 15-hour thinking-mode capture — the check has to live
at the point of use, not only at the point of starting the server.

**Exit status 0 is not a successful capture.** A wrong model id fails every call and still
exits cleanly, leaving an empty cassette. `03` validates the artifact: record count
against expected, error rate under 10%, and a report summary. Trust the file, not the
exit code.

## Reference

- `docs/eval-full-corpus-runbook.md` — the prose runbook these automate
- `docs/history/extraction-eval-2026-04.md` — April 2026 model rankings and latency
  baselines on this same M3 Ultra
- `docs/extraction-quality-evaluation.md` — methodology
