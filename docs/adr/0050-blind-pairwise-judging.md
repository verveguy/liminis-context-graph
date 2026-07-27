# ADR-0050: Blind Pairwise Judging for the Extraction-Quality Eval Harness

**Status**: Accepted
**Date**: 2026-07-27
**Issues**: #269

## Context

`lcg-eval` (ADR-0048) has always scored every candidate against a privileged `--reference`
backend via judged precision/recall/F1. That measures **similarity to the reference**, not
quality — the reference is one more model's output, not ground truth, so a candidate that
extracts an entity the reference missed is scored as a false positive and penalised for being
right. The existing noise-floor pattern (the same spec configured twice under different
backend names) controls for sampling variance but not for this directional bias: every
candidate is graded on reference-likeness, which structurally advantages the reference's own
second run over any other model. #248's full-corpus benchmark needs a second, reference-agnostic
signal to answer "is the candidate good enough" — separate from reference-F1's "how much would
existing graph content shift if we swapped models," which stays exactly as biased-toward-the-
reference as it should be for a migration-risk question.

This ADR documents the design decisions behind adding blind pairwise judging as an additive
mode, not a replacement.

## Decisions

### 1. `JudgeCache` generalized via an internal untagged enum, not a second cache file

`JudgeVerdict` (`matched`/`unmatched_reference`/`unmatched_candidate`) and the new
`PairwiseVerdict` (`winner`/`rationale`) have fully disjoint field sets, so a
`#[serde(untagged)] enum CachedVerdict { Judge(JudgeVerdict), Pairwise(PairwiseVerdict) }`,
`#[serde(flatten)]`-embedded into `CacheEntry`, disambiguates unambiguously on the field names
alone — no discriminant tag is needed. `get`/`insert` keep their exact pre-#269 signatures and
on-disk JSONL shape for `JudgeVerdict` entries byte-for-byte unchanged; new `get_pairwise`/
`insert_pairwise` methods share a single `insert_internal` disk-then-memory write path. A cache
file already on a user's disk from before this issue keeps loading without any migration step —
verified by a test that hand-writes an old-format line and confirms it still loads.

A wrong-variant lookup (e.g. `get_pairwise` on a key holding a `JudgeVerdict`) returns `None`
rather than panicking — fails closed, matching the pre-existing miss behavior for an absent
key.

### 2. Deterministic slot assignment via SHA-256, not `DefaultHasher`

FR-003 requires a re-run to reproduce the exact same result — no wall-clock, no RNG. `std::
collections::hash_map::DefaultHasher`'s output isn't guaranteed stable across Rust/std
versions, which would silently break that guarantee the moment the toolchain moves. `sha2` is
already a workspace dependency (`judge_cache::cache_key` uses it), so `pairwise::
chunk_pair_seed(chunk_key, backend_a, backend_b)` computes `u64::from_be_bytes(sha256(
"{chunk_key}|{backend_a}|{backend_b}")[0..8])`; `seed % 2` picks which physical backend is
presented as slot A in the "primary" order for that (chunk, pair) combination.

The seed only fixes which physical backend occupies which slot in the primary call — FR-004's
required second order is **always** judged too, as the deterministic flip of whichever ordering
the seed picked as primary. Both orders are judged unconditionally; the seed's role is
reproducibility of *which* physical backend the primary order presents as slot A, not whether
both orders run.

### 3. Chunk text embedded symmetrically in the cache key, not in `cache_key`'s signature

`judge_cache::cache_key`'s existing order-sensitivity on its two operands already gives the two
slot orders distinct keys (`cache_key_changes_with_reference_or_candidate` pins this). Rather
than adding a `chunk_text` parameter to `cache_key` (a matching-mode signature change, out of
scope per the issue), pairwise's `judged_pairwise` helper calls `cache_key(prompt_name,
judge_model, &json!({"chunk": chunk_text, "slot": slot_a}), &json!({"chunk": chunk_text, "slot":
slot_b}))` — the chunk text rides inside both operands. This needed zero changes to
`judge_cache.rs`'s `cache_key` function and ties every cached verdict to the specific chunk it
was judged against.

### 4. One `judge_pairwise` call per axis, not one call covering all three

Mirrors reference mode's three separate `judge()` calls (one per axis) rather than a combined
response. Keeps `PairwiseVerdict` a single-axis `{winner, rationale}` shape, cache-keyed per
axis exactly like `JudgeVerdict` is today — and keeps FR-005's per-axis distinction (misses
content vs. invents content) structurally impossible to accidentally collapse in a shared
response parser.

### 5. Win rate excludes ties from the denominator

`AxisTally::win_rate_a()` computes `wins_a / (wins_a + wins_b)`, not `wins_a / chunks_compared`.
An unbiased judge's baseline tie rate is not zero — genuinely equivalent extractions should tie
— so folding ties into the denominator would push a genuinely unbiased 50/50 noise-floor pair
below the SC-001 calibration band for a reason unrelated to position bias. Excluding ties keeps
the win rate answering "when the judge picked a side, which side did it pick," which is what
SC-001 is actually trying to detect. `win_rate_a()` defaults to `0.5` (no signal either way)
when there are zero decisive (non-tied) comparisons, rather than `0.0`/`NaN`.

### 6. Calibration band and inconsistency threshold are new, chosen constants

Neither existed anywhere in the repo before this issue.

- **`CALIBRATION_BAND = 0.45..=0.55`** (SC-001's literal text): two independent samples of the
  same model, judged pairwise, should split near 50/50; a win rate outside this band, *for the
  calibration pair specifically*, more likely reflects judge position bias than a genuine
  quality difference between the two samples.
- **`ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD = 0.20`**: above a 1-in-5 flip rate when the slot
  order reverses, the judge's position bias is comparable in magnitude to whatever quality
  signal the win rate carries, so the aggregate result is not distinguishable from noise. Also a
  stderr-only warning.

Both live as public consts in `crates/eval/src/pairwise.rs`. Neither warning blocks a run —
consistent with the existing missing-`ANTHROPIC_API_KEY` warning precedent — so the report
artifact itself stays pure data; a caller that wants to enforce the thresholds does so by
reading the warnings or the raw `order_inconsistency_rate`/`win_rate` fields itself.

**The calibration-band note fires for every pair, not just the calibration control, and is
worded conditionally rather than asserting bias.** `main.rs::warn_on_calibration_and_inconsistency`
cannot determine *which* configured pair is the calibration control in general: the most
common pattern — two independently-recorded `cassette:path=` files of the same model (e.g. the
#248 runbook's `baseline`/`candidate`) — shares no spec string between the two backends, so a
same-spec-equality heuristic (which *would* catch the same-live-spec-twice noise-floor pattern)
silently fails to fire on exactly the pair SC-001 cares about, which would leave the primary
documented scenario without its required warning. Rather than ship a heuristic that is wrong
for the scenario the ADR itself walks through, the note fires unconditionally and states both
readings: "if this is your calibration pair, suspect judge bias; if it's a genuine
candidate-vs-candidate pair, a win rate outside the band is the expected, desired signal." The
order-inconsistency warning has no such ambiguity — position bias is position bias regardless
of which pair exhibits it — so it remains an unconditional, unqualified warning for every pair.

### 7. FR-011 degenerate-pair rejection scoped narrowly to `cassette:path=` equality

The existing `--reference` noise-floor pattern deliberately configures the same *live* spec
(e.g. `anthropic:model=X`) twice under different backend names — each run independently samples
the model, so the two outputs are not degenerate, and User Story 2's mandatory calibration
control depends on exactly this pattern being judgeable under `--judge-mode pairwise`. Two
`cassette:path=<PATH>` backends sharing the identical path, by contrast, are guaranteed
byte-identical (a cassette replay is deterministic) — a genuinely degenerate, trivial 100%-tie
comparison computed and buried in the report for no informational value.

`cli.rs::cassette_path(spec)` hand-parses the `path=` value out of a `cassette:` spec (mirroring
the file's existing kind-prefix extraction convention, e.g. the `--record-cassette`
cassette-kind rejection) rather than depending on `backend::parse_backend_spec` — no new
dependency from `cli.rs` onto `backend.rs`. The check is gated on `judge_mode != Reference` and
runs at CLI parse time, before any judge call, naming the offending backend names in the error.

### 8. Reference-mode judge calls are skipped entirely under `--judge-mode pairwise`

`main.rs::run()` passes `None` for the judge client to `score_candidate` whenever `judge_mode ==
Pairwise`, regardless of whether `ANTHROPIC_API_KEY` is set — not "run reference-mode judging
and ignore the result." A user who explicitly asked for pairwise-only shouldn't silently pay for
reference-mode judge calls they didn't request; `--judge-mode both` is the way to get both
passes' judge spend.

### 9. Report structure: one new `Option<Vec<PairwiseReportEntry>>` field, `skip_serializing_if`-guarded

SC-004 requires `--judge-mode` omitted to leave `Report::to_json()`'s output byte-identical to
the pre-#269 shape. `Report`/`CandidateReport` had no `skip_serializing_if` convention before
this issue (e.g. `vocabulary_compliance: Option<...>` always serializes, `null` when absent), so
a naively-added field would change the JSON shape (an added key, even `null`) versus today's
output. `pairwise: Option<Vec<PairwiseReportEntry>>` carries `#[serde(skip_serializing_if =
"Option::is_none")]` so the key is **entirely absent**, not present-but-null, whenever pairwise
mode wasn't requested. A golden JSON string captured from `sample_report().to_json()` *before*
the field existed is pinned in a regression test
(`json_report_is_byte_identical_to_pre_pairwise_golden_sc004`) so any future accidental key
addition/reordering to this path is caught immediately, not discovered downstream.

`render_human_readable()` mirrors this: the `== pairwise judging ==` section is emitted only
when `self.pairwise` is `Some` and non-empty.

### 10. `BackendPair` realized as plain nested-index iteration, not a struct

The spec's "Key Entity" `BackendPair` needed no behavior beyond "every unordered pair of
configured backends" — `pairwise::score_all_pairs` iterates `for i in 0..n { for j in
(i+1)..n }` over `run_results` (insertion order, matching `--backend` configuration order)
rather than introducing a dedicated type that would add nothing over the plain loop.

## Consequences

- A cache file recorded before #269 keeps working with zero migration step — confirmed by
  `old_format_matching_only_cache_line_still_loads_unchanged`.
- FR-004's mandatory dual-order judging, combined with FR-008's all-pairs requirement, multiplies
  judge-call volume relative to reference mode: a 3-backend `--judge-mode pairwise` run costs 3
  pairs × 2 orders × 3 axes = 18 judge calls per chunk, versus reference mode's 2 candidates × 1
  order × 3 axes = 6. Documented in `README.md`'s "Cost implications" section so nobody runs
  `--judge-mode both --all` against a cold cache unprepared. Still zero **extraction** calls
  either way (FR-009) — the judge cache applies identically to both modes.
- The 45–55% calibration band and 20% order-inconsistency threshold are this repo's first
  attempt at numeric judge-bias thresholds; they are not derived from a controlled study of this
  specific judge model, only from the literal band SC-001 specifies and a round "1 in 5"
  threshold reasoned from first principles (Decision 6). A future maintainer with more pairwise
  runs under their belt may find either number needs revisiting — if so, update
  `pairwise::CALIBRATION_BAND_LOW`/`_HIGH`/`ORDER_INCONSISTENCY_UNTRUSTED_THRESHOLD` and this
  ADR together, not just one of the two.
- Per the Assumptions section of the spec: SC-001's calibration control detects position bias
  (does the judge favor whichever slot a given call presents first), but it does **not** detect
  same-family stylistic affinity (a judge preferring outputs that read like its own family's
  style, independent of slot position). That limitation is documented, not solved, by this
  feature.
- Ontology-mode cassette pairwise judging is explicitly out of scope (cassette keys differ under
  ontology mode — `crates/core/src/cassette.rs:69-88` — a candidate follow-up issue).

## Related

- ADR-0044: the cassette record/replay seam pairwise mode scores over; `Error::CassetteMiss` is
  the same loud-miss signal FR-010's per-chunk skip tally relies on.
- ADR-0048: the eval harness architecture this feature extends — the judge-as-standalone-client
  pattern, the ported judge-prompt/cache-key conventions, and the "harness never reimplements
  HTTP/JSON client logic" principle all carry over unchanged to `judge_pairwise`.
- ADR-0049: the most recent precedent for a CLI-flag-driven mode override in this harness
  (`--ontology-mode`), whose "loud, `Result`-based rejection over silent fallback" philosophy
  `--judge-mode`'s FR-011 validation follows.
- `crates/eval/src/pairwise.rs`: `PairwiseAxis`, `chunk_pair_seed`, `AxisTally`, `score_pair_axis`,
  `score_all_pairs`, the two threshold consts.
- `crates/eval/src/judge.rs`: `PairwiseWinner`, `PairwiseVerdict`, `PAIRWISE_JUDGE_PROMPT`,
  `JudgeClient::judge_pairwise`.
- `crates/eval/src/judge_cache.rs`: `CachedVerdict`, `get_pairwise`/`insert_pairwise`.
- `crates/eval/src/cli.rs`: `JudgeMode`, `--judge-mode`, `cassette_path`, the FR-011 validation.
- `crates/eval/src/report.rs`: `PairwiseReportEntry`, `Report::pairwise`, the SC-004 golden test.
