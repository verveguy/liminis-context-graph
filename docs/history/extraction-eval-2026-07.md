# Extraction-quality eval — hosted vs local, and the effect of an ontology (2026-07)

**Status:** Findings doc. Captured 2026-07-29/31 against `liminis-context-graph` at
`d431a94`, using the `lcg-eval` harness in `crates/eval/` and the operational scripts in
`crates/eval/scripts/`. Everything below is reproducible: cassettes and judge caches are
local-only (gitignored), but the harness, corpus fixture and ontology fixture are all in
the repo.

This supersedes [`extraction-eval-2026-04.md`](extraction-eval-2026-04.md) as the current
baseline. That document evaluated the *previous* Python/graphiti pipeline; this one runs
against the Rust implementation and its own ported prompts, so absolute numbers are finally
comparable to what the service actually does.

---

## TL;DR

1. **`qwen3.6-27b` (4-bit MLX, local) reaches 87–90% of the hosted noise-floor ceiling** on
   judged F1, and **wins ~84% of blind pairwise comparisons against Claude Haiku** on
   entities. The two metrics disagree, and the disagreement is the finding — see
   [Reference F1 and blind pairwise measure different things](#reference-f1-and-blind-pairwise-measure-different-things).

2. **The noise floor is not optional.** Two *independent samples of the same hosted model*
   score 0.882 judged / **0.380 strict-string** against each other on edges. An F1 quoted
   without that ceiling is uninterpretable — qwen's strict edge F1 of 0.219 reads as a
   catastrophe against 1.0 and as 58% of achievable against 0.380.

3. **An ontology collapses relation vocabulary 8.9–11.1× at no measurable quality cost.**
   836–1203 distinct relation names become 80–131; in-vocabulary edge share goes 18.7–24.1%
   → 93.1–97.4%; edge *counts* stay approximately stable, moving −1.0% to +5.5%. Judged F1
   moves <1pp — because the judge already normalises name variants — while **strict-string
   edge F1 rises 36–60%**.

4. **Local models comply with a declared vocabulary.** qwen sits at 93.1% in-vocabulary
   against Haiku's 96.5–97.4%, in `open` mode where the types are only a *preference*. Its
   escapes are mostly relations the fixture doesn't model, not defiance.

5. **A self-contradictory prompt cost more than accuracy: it cost reproducibility.** Before
   [#281](https://github.com/verveguy/liminis-context-graph/issues/281), two runs of the
   same model on the same corpus differed by **58%** on `Concept` extraction. After: **5%**.

---

## What was measured

| item | detail |
|---|---|
| corpus | `crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl` — 228 chunks of Wikipedia spaceflight/Apollo prose. Median 743 chars, p95 6,411, max 16,670 |
| hosted | `claude-haiku-4-5-20251001`, run **twice independently** (`baseline`, `candidate`) |
| local | `mlx-community/Qwen3.6-27B-4bit` via `mlx_lm.server`, **thinking disabled** |
| judge | `claude-sonnet-4-6`, thinking explicitly disabled |
| ontology | `crates/core/tests/fixtures/real_corpus_wal/ontology.yaml` — 12 entity types, 25 relation types, 86 aliases, 78 keywords |
| modes | `freeform` (no ontology) and `open` (declared types preferred, others permitted) |

`strict` mode was **not run** — see [What was not tested](#what-was-not-tested).

### Why three backends

`baseline` and `candidate` are two *independent live runs of the same hosted model*. Their
disagreement is the **noise floor**: the ceiling any candidate could reach, given that
extraction is nondeterministic. `run_mode_matrix.sh`'s two-backend shape cannot measure it,
which is why `06-ontology-matrix.sh` exists.

Judging runs in two modes. **Reference** scores each candidate against `baseline` via
LLM-as-judge semantic matching. **Blind pairwise** ([#269](https://github.com/verveguy/liminis-context-graph/issues/269))
shows the judge the source chunk plus two *unlabelled* extractions and asks which better
captures it, in both slot orders. Reference F1 measures *similarity to the reference*;
pairwise measures *quality*, with no privileged backend.

---

## Results

### Judged F1 against each arm's own noise floor

| axis | freeform qwen | ceiling | % achievable | open qwen | ceiling | % achievable |
|---|---:|---:|---:|---:|---:|---:|
| entities | 0.850 | 0.956 | **88.9%** | 0.853 | 0.953 | **89.5%** |
| edges | 0.765 | 0.882 | **86.7%** | 0.774 | 0.884 | **87.6%** |
| summaries | 0.842 | 0.942 | 89.4% | 0.850 | 0.945 | 89.9% |

### Precision / recall — the diagnostic split

**Freeform arm, scored against the `baseline` Haiku sample.**

| leg | entities P | entities R | edges P | edges R |
|---|---:|---:|---:|---:|
| Haiku (2nd sample) | 0.949 | 0.974 | 0.884 | 0.902 |
| **qwen** | **0.767** | **0.986** | **0.694** | 0.900 |

> **These will not reproduce the F1s above via the harmonic-mean formula, and that is
> expected.** Every figure in this doc is a **mean of per-chunk values** — `scoring.rs:172`
> is `average(&judged_entity_f1s)`, and P and R are averaged the same way. The mean of
> per-chunk harmonic means is always ≤ the harmonic mean of the averaged P and R, so the
> reported F1 sits ~1pp below what these columns suggest, consistently and for every leg
> including the Haiku pair (0.956 vs 0.961; 0.850 vs 0.863). It is an artifact of
> macro-averaging, not of pooling across arms.

**On entities, qwen's recall exceeds the hosted model's own second sample** (0.986 vs
0.974). Its precision is much lower. It finds essentially everything Haiku finds while
**emitting ~32% more entity nodes** (3,481 vs the Haiku sample's 2,639) — and is penalised
for the surplus because the reference is Haiku, not truth.

**On edges the recall claim does not hold**: 0.900 vs 0.902 is a tie, not an advantage. The
qwen story is an entity-recall story; on edges it matches Haiku's recall while giving up
19pp of precision.

### Blind pairwise — qwen's win rate over decisive comparisons

| axis | vs baseline (freeform → open) | vs candidate (freeform → open) |
|---|---|---|
| entities | 84.0% → **83.9%** | 83.7% → **86.9%** |
| edges | 75.8% → 69.7% | 78.5% → 74.7% |
| summaries | 66.5% → 69.9% ⚠ | 66.7% → 73.6% ⚠ |

⚠ **The open arm's summary axis failed calibration (55.7%, band is 45–55%) — treat those two
figures as unreliable.** Entities and edges passed in both arms.

### Extraction volume and vocabulary

| leg | mode | entities | edges | distinct relation names | edges in-vocabulary (exact / variant-tolerant) |
|---|---|---:|---:|---:|---:|
| Haiku baseline | freeform → open | 2611 → 2709 | 2552 → 2692 | **836 → 94** | 24.1% / 36.2% → **96.5% / 96.7%** |
| Haiku candidate | freeform → open | 2639 → 2711 | 2582 → 2704 | **887 → 80** | 23.2% / 34.1% → **97.4% / 97.4%** |
| qwen | freeform → open | 3481 → 3596 | 3398 → 3363 | **1203 → 131** | 18.7% / 33.3% → **93.1% / 93.9%** |

**Read the two vocabulary columns together — the gap between them is the finding.** *Exact*
counts an edge as in-vocabulary only if its type string is one of the fixture's 25 relation
types. *Variant-tolerant* also accepts a type that contains, or is contained by, a declared
one — so `IS_LOCATED_IN` counts as `LOCATED_IN`. Freeform, the two differ by **10.9–14.6pp**
(baseline 12.1, candidate 10.9, qwen 14.6): a large share of "compliance" is only near-miss
naming, which is what makes 836 names unqueryable. Under the ontology the two columns
converge to within 0.8pp, because the model is emitting the declared string rather than a
paraphrase of it. Both columns are computed over every edge in the cassettes, not sampled; the harness's own `vocabulary_compliance`
field is `null` for these runs, so these are recomputed from the cassettes directly.

Entity-type conformance was already 97.5–98.9% freeform and reached 99.9% under the
ontology — **the intervention is almost entirely about edges**, which matches the prompt
mechanics: the ontology adds +1734 chars to the edge system prompt and only +195 to the
entity one.

### Strict-string F1 — where the collapse *is* visible

| leg | freeform edges | open edges | |
|---|---:|---:|---|
| Haiku candidate | 0.380 | **0.516** | +36% |
| qwen | 0.219 | **0.351** | +60% |

---

## Findings

### Reference F1 and blind pairwise measure different things

Reference F1 puts qwen ~11% below the ceiling. Blind pairwise says a judge reading the
source prefers qwen ~84% of the time on entities. Both are correct.

Reference F1 measures *similarity to Haiku*. Haiku is not ground truth — it is one more
model's output — so a candidate that extracts something Haiku missed is scored a false
positive and **penalised for being right**. The precision/recall split shows this directly:
qwen's recall is *higher* than the hosted model's own second sample.

We tested whether qwen's surplus was fabrication by checking every extracted name against
the source chunk it came from. **Untraceable-name rates agreed to within 0.1pp: 0.6% / 0.5%
/ 0.6%**,
and the residue was benign normalisation (`Houston` → `Houston, Texas`). The extra output is
real content, not noise.

A Sonnet judge scoring Haiku against qwen should, if anything, carry same-family affinity
for Haiku. Haiku lost anyway, so the result is conservative in that direction.

### The ontology's benefit is schema quality, and F1 cannot see it

The judge prompt explicitly instructs it to treat `won` ≈ `won_award`, `located_in` ≈
`is_located_in` as equivalent. **It was already controlling for the exact divergence the
ontology collapses**, so judged F1 barely moves (<1pp) while strict-string edge F1 — which
compares literal type strings — rises 36–60%.

The operational difference is large and unscored by this harness: **836 relation names is
not a schema you can query, aggregate over, or write Cypher against. 94, with 25 covering
97% of edges, is.** Edge counts stayed approximately stable across the three legs — +5.5%,
+4.7%, −1.0% — so the constraint disciplined *naming* without suppressing *content*. The
point is the absence of a large drop, not exact equality: a vocabulary constraint that
worked by making the model emit fewer edges would be a cost, and that is not what happened.

**Trap for future readers:** running this matrix and concluding "the ontology did nothing"
from flat judged F1 would be wrong. Look at strict F1 and at the vocabulary table.

### A contradictory prompt cost reproducibility, not just accuracy

Until [#281](https://github.com/verveguy/liminis-context-graph/issues/281), the entity
prompt said *"NEVER extract vague or standalone abstract concepts"* while its own ontology
offered `Concept: An abstract idea, principle, or theoretical framework`. Effects on this
corpus:

| | pre-fix | post-fix |
|---|---:|---:|
| Haiku `Concept` entities (two samples) | 38 and 60 — **58% spread** | 154 and 147 — **5% spread** |
| Haiku edges | 2289 | 2552 (+11.5%) |

**"Spread" here is `(max − min) / min`** across the two independent samples: `(60−38)/38 =
58%` before, `(154−147)/147 = 5%` after. The convention matters — the *same* pre-fix data
reads **44.9%** against the mean (`22/49`, the standard relative-percent-difference formula)
or **36.7%** against the max (`22/60`). Both sound like a materially smaller problem than
58%. `min` is used because the question being asked is "how much more did the larger run
find than the smaller one".

The new `Concept` entities came almost entirely **out of** Product (−37), Event (−33) and
Organization (−17), while Person and Location — unambiguously concrete — did not move. The
model was extracting these subjects all along and **forcing them into whichever concrete
bucket fit least badly.** Every one was already in the graph under a wrong label,
deduplicating and linking against the wrong neighbours.

The reproducibility collapse is the more serious half. The contradiction resolved
differently by sampling *and* by input size — a community report measured **zero** `Concept`
entities on a 257k-char page while short chunks yielded plenty. Two ingests of the same
document could type the same subject differently.

### Local-model compliance is not the obstacle

qwen diverges hardest freeform — 1203 distinct relation names, of which only 1.9% are
declared types (10.1% allowing variants), covering 18.7% of edges (33.3% allowing variants)
— yet reaches **93.1% in-vocabulary** under `open`, against Haiku's 96.5–97.4%. Its
escapes are `ALSO_KNOWN_AS` (29), `LAUNCHED_BY` (21), `NAMED_AFTER` (13), `CARRIES_TO` (7),
`OCCURS_WHEN` (6). Only one of those is defiance: `LAUNCHED_BY` is the declared `LAUNCHED`
with an inverted direction. The rest are relations a 25-type fixture simply doesn't model.
The gap to Haiku is ~3.4pp, which is a smaller effect than the choice of ontology mode —
compliance is not what limits the local model.

### Latency

`qwen3.6-27b` measured **p50 39.8s / p95 212.6s / p99 377.9s** against a 62.4s mean over the
full corpus, versus Haiku's ~8.9s/chunk. **Quote p50 with p95/p99; never the mean** — the
per-chunk range is ~9s to ~730s and the mean sits between p50 and p95 describing nothing.

So the local model is roughly **4.5× slower at p50**, and far worse in the tail.

---

## qwen3.6-35b-a3b — the MoE is a wash on quality, and the trade is reliability

Added 2026-08-01. The April eval ranked this MoE ~8.8pp behind `qwen3.6-27b` on edges and
sold it as the speed-for-quality option. **On this corpus that deficit does not reproduce.**

> **Correction (#314, added 2026-08-02): the error/chunk-loss numbers in this section are
> inflated by a since-fixed defect, not a genuine qwen3.6-35b-a3b reliability gap.**
> `ExtractedEntity.summary` had no `#[serde(default)]`, so a response with valid JSON and every
> entity present but missing only the `summary` field failed deserialization and **discarded
> the entire chunk's entities**, then reported the chunk as `structured_output.malformed` —
> indistinguishable from a genuinely unparseable response. The #306 failure-sidecar re-capture
> that motivated #314 found exactly this on two of this run's seven "malformed" chunks
> (`Apollo 11`: 29 entities, `Astronaut`: 18 entities — both valid JSON, both missing only
> `summary`), alongside at least one genuinely broken chunk (`History of the Earth`). #314 made
> a missing `summary` default to an empty string instead of failing the whole chunk, and split
> the failure classification so schema-shortfall and truly-unparseable content are no longer
> the same bucket. **This section's numbers are left as originally recorded — see FR-006 in
> [#314](https://github.com/verveguy/liminis-context-graph/issues/314) — but every error-rate,
> chunk-loss, and "excluded chunks" claim below should be read as an upper bound, not a
> corrected measurement.** A re-run with the fix applied would very likely show a lower error
> rate, entities from more chunks retained (with some empty summaries), and a residue of
> genuinely-broken chunks only. That re-run is out of scope for #314 (FR-006 requires
> annotation, not re-measurement) and has not happened as of this note.

| leg | judged entities | judged edges | judged summaries | strict edges | error rate |
|---|---:|---:|---:|---:|---:|
| Haiku 2nd sample (**ceiling**) | 0.956 | 0.882 | 0.942 | 0.380 | 0.0% |
| `qwen3.6-27b` (incumbent) | 0.850 | 0.765 | 0.842 | 0.219 | **0.9%** |
| `qwen3.6-35b-a3b` | **0.861** | 0.762 | **0.857** | 0.208 | **3.1%** |

As a share of the achievable ceiling: entities **90.0% vs 88.9%**, edges **86.4% vs 86.7%**,
summaries **91.0% vs 89.4%**. Those gaps are smaller than the ceiling's own width. Treat the
two as tied on quality.

Note the MoE's edge F1 is essentially unchanged between the two evals (0.764 April → 0.762
here); what moved is the **27b's**, on a different corpus. So this is not the MoE improving —
it is a reminder that cross-corpus rank ordering is not transitive, and that a quality gap
measured on one corpus should not be quoted as a property of a model.

**The real trade is reliability, not quality.** The MoE is **1.7× faster on token throughput**
(55.9 vs 33 tok/s) and **~2.1× faster in corpus wall-clock** (1.87h vs ~4h). Those are
different ratios and should not be quoted as one: wall-clock also absorbs queuing, retries
and the per-call latency tail, so it outruns the raw token rate. It errors **3.5× more
often** (7 chunks vs 2). Its tail is also
worse: p50 16.8s but **p99 321s**, a five-minute worst chunk. All 7 *counted* failures were
malformed parses (`structured_output.malformed: 7`, equal to `errors`) — **at least 2 of these
7 are the #314 missing-`summary` defect, not genuinely malformed output; see the correction
above.** Because entity-side
budget exhaustion returns `Err` and would appear in `errors`, that equality rules truncation
out **on the entity path only** — it says nothing about edges, for the reason in the warning
below. (A direct probe at 17000 chars *does* truncate, but it bypasses the retry path; don't
quote it as the pipeline's behaviour.)

Two rates are easy to confuse here. Per **call**, malformed is 7/446 = **1.57%**; per
**chunk** it is 7/228 = **3.07%**, because a chunk needs both its entity and edge call to
succeed. The chunk rate is the one that matters operationally and the one quoted above. **Both
rates are inflated by #314** — the missing-`summary` defect described in the correction above
means some of these 7 "malformed" chunks were valid extractions discarded over one absent
string field, not model failures.

⚠ **The edge comparison has a known defect — do not quote the edge gap.** qwen35 returned
**zero edges on two chunks where Haiku found 36 and 38 and qwen27b found 49 each**. Zero is
not plausible extraction on those chunks, so ~98 edges — 3.2% of the 3,036 it extracted
— are missing. Its
edge recall is 0.878 against qwen27b's 0.900 — a 2.2pp gap that these two chunks could
account for entirely. Treat edges as *unresolved* rather than tied.

The cause cannot currently be determined, and that is the real finding: on edge budget
exhaustion the extractor returns `Ok(vec![])` (extractor.rs:410, 1334 — deliberate, "not
fatal"), which is byte-identical in the cassette to a model genuinely emitting
`{"edges": []}`. The one signal that would separate them, `ExtractionTruncated`, is discarded
by the eval's sink (`crates/eval/src/runner.rs:145` matches only `StructuredOutputParse`). So
a truncated chunk is recorded as **clean**, and the harness cannot distinguish "found
nothing" from "we gave up". Entity-side truncation *is* caught (it returns `Err`), which is
why `errors == malformed == 7` is trustworthy for entities and silent for edges.

⚠ **Read the quality numbers with the scoring asymmetry in mind.** Failed chunks are excluded
from scoring, so the MoE is scored over 220 chunks against the 27b's 225 and Haiku's 227.

Those counts look off-by-one against the error counts, and the missing chunk is real:
**`scored = 228 − own_errors − 1`**, because the `baseline` reference arm itself failed one
chunk, and a chunk with no scorable reference is unscorable for *every* leg. So 228−1−1=226
would be wrong too — the baseline's own row is 228−1=227, and every other leg pays that same
1 on top of its own errors: candidate 228−0−1=227, qwen27b 228−2−1=225, MoE 228−7−1=220. The
`0.0%` error rate shown for the ceiling row is the *candidate* sample, which genuinely failed
nothing; the lost chunk belongs to `baseline`.

The excluded chunks are exactly the ones the MoE could not parse, which flatters it — most
likely the longest, densest chunks, which are also the hardest. A 1.1pp entity advantage
should not survive that caveat as a claim of superiority; "tied" is the defensible reading.
**This "flatters it" framing is itself #314-affected**: some of those excluded chunks (e.g.
`Apollo 11`, `Astronaut`) were not genuinely hard-and-unparseable — they were fully valid
extractions dropped over a missing `summary` field, so their exclusion does not necessarily
correlate with chunk difficulty the way this paragraph assumes.

**Operationally:** the MoE is the right pick only where throughput dominates and a 3.1%
chunk-loss rate is acceptable — bulk backfill, for instance (**this 3.1% figure is inflated by
#314; see the correction above**). For interactive ingest the 27b's
0.9% and tighter tail are worth more than the 1.7× speed, since a lost chunk is silent
missing knowledge rather than a visible error.

## What was not tested

- **`strict` mode.** Not run. `open` already reaches 93.1–97.4% compliance at no quality cost,
  so strict's remaining case is *guaranteeing* 100% rather than improving quality — at the
  risk of dropping relations the fixture doesn't model. Note `vocabulary_compliance` in the
  report only populates in strict mode, so that metric is still unmeasured.
  **"Open beats freeform" is established; "open beats strict" is not.**
- **Other local models.** `gemma-4-26b-a4b` **was run, and could not be scored** — it never
  reached the quality stage, so its extraction quality remains unmeasured. Three builds
  across two runtimes all emit malformed JSON at rates far above the incumbent's:

  | build / runtime | measured | failure rate |
  |---|---|---:|
  | `mlx-community/…-4bit` (mlx_lm) | full 228-chunk capture | **34.2%** |
  | `lmstudio-community/…-4bit` (mlx_lm) | 10 distinct 3000-char windows | **~10%** (1/10; n is small, interval roughly 2–40%) |
  | `gemma4:26b` GGUF (Ollama) | 4 sizes | degenerate both directions — 8192-token runaway on a 1500-char input, near-empty elsewhere |

  Incumbent `qwen3.6-27b` measures 0.4–2.2% on the same corpus. The failure is corrupt JSON
  *mid-document* (`Expecting ':' delimiter`), not truncation — which matters because the OAI
  path retries only HTTP 429/529 and budget exhaustion, so a corrupt body is an unrecoverable
  chunk error. The `lmstudio` build is markedly healthier than the `mlx-community` one
  (34.2% → ~10%), which points at conversion quality rather than the weights alone.

  **This is a build/runtime observation, not a verdict on gemma-4.** Sampling parameters,
  context length, and prompt shape were all excluded as causes; the behaviour is consistent
  with open upstream reports against this model on MLX. A fair quality comparison needs a
  build that emits valid JSON, and we do not currently have one.

  Priors from the April eval pointed the same way but do not substitute for measurement:
  `gemma-3-27b` ranked well behind qwen (edges 0.634 vs 0.852) at a 3.1% error rate. Note the
  other April prior — that MoE architectures trade edge quality for speed — **was tested here
  and did not hold** (see the `qwen3.6-35b-a3b` section above), which is the point: a prior is
  a reason to measure, not a substitute for it.
- **Downstream graph quality.** The judge was asked which extraction better captures the
  source, not which produces a better graph. qwen's ~32% surplus means more dedup pressure,
  more storage, more retrieval noise. Unmeasured here.
- **Any corpus but this one.** Single domain, English, encyclopaedic prose, 228 chunks.

---

## Methodology notes worth keeping

**Calibration is a hard gate, not a nicety.** Two independent samples of the same model,
judged blind, must split near 50/50. This run: 48.6/52.5/49.7 (freeform, all pass) and
52.4/50.6/**55.7** (open — summary axis **fails**). Without that control a win rate is
unanchored, and the failing axis would have been quoted as a result.

**Order-inconsistency bounds trust.** Each pair is judged in both slot orders; disagreement
counts as a tie and is reported. Rates ran 14–36%, worst on edges. A win rate published
without its inconsistency rate is not interpretable.

**Cassettes key on rendered prompts.** Changing a prompt invalidates every recorded cassette
and every judge-cache entry derived from it. #281's prompt fix discarded ~$27 of judging and
forced a full re-capture. Land prompt changes and re-baselines together.

**Judge failures must be non-fatal, and systemic ones must trip a breaker.** A ~6000-call
scoring phase will hit transient faults; making one fatal discards hours
([#277](https://github.com/verveguy/liminis-context-graph/issues/277)). But an exhausted
spend limit fails *every* call — a circuit breaker after 10 consecutive failures
([#275](https://github.com/verveguy/liminis-context-graph/issues/275)) turned one such event
into a complete report plus 2233 calls not made.

**A local server can be deterministic, which silently invalidates repeat-sampling.**
`mlx_lm.server` returned byte-identical output for three identical calls at `temperature:
1.0` (sha256 `d61e717b…`), and an explicit `seed` changed nothing. Temperature is not
ignored — `0.0` and `1.0` give different outputs — but neither varies across repeats. So
sending the same prompt N times measures **one draw counted N times**, and a "0/6 failures"
result carries exactly as much information as 0/1. This is easy to miss because the
arithmetic looks like a sample size. Estimate reliability across **distinct inputs**
instead; that is what the 228-chunk corpus already does, and why per-chunk error rate is
the number to trust. The same caution applies to any hosted endpoint with caching enabled.

**Reasoning/thinking modes hurt this task.** The April eval measured
`qwen3.6-27b-thinking-only` at 112.7s p50 versus 10.9s, *and* scoring worse on enumeration.
`01-start-server.sh` disables it and fails loudly if reasoning tokens return; the judge sets
`thinking: {"type": "disabled"}` explicitly, because Sonnet 5 defaults it *on* while Sonnet
4.6 defaults it off.
