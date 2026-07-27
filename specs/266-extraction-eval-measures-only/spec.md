# Feature Specification: Ontology-aware extraction-quality evaluation

**Feature Branch**: `fabrik/issue-266`
**Created**: 2026-07-27
**Status**: Draft
**Input**: User description: "Extraction eval measures only the freeform path — ontology-constrained (Open/Strict) modes are untested"

## Background

`crates/eval/src/runner.rs:113` hardcodes `ontology: None`. Every number the harness has ever
produced — and every number #248 will produce — describes **freeform extraction only**. The
April 2026 predecessor eval (`docs/history/extraction-eval-2026-04.md`) has no mention of
ontology, entity types, or schema either, so the inherited rankings share the gap.

This is not a minor parameter. `crates/core/src/prompts/mod.rs` substitutes an entire section
into the system prompt, giving **three distinct regimes**:

| Mode | Injected instruction |
|---|---|
| freeform (`None`) | `DEFAULT_ENTITY_TYPES_SECTION` — model invents its own type vocabulary |
| `OntologyMode::Open` | *"Prefer the listed entity types when they apply; you may use other types…"* |
| `OntologyMode::Strict` | *"Only extract entities whose type is exactly one of the listed types; do not invent or use types not in this list."* |

`build_fact_types_section` does the same for edges.

**These are different tasks.** Freeform extraction is open-ended naming — the model invents a
taxonomy as it goes. `Strict` is classification against a closed set. The April eval's central
negative finding was that reasoning-heavy models degrade because they spend budget on *"framing
rather than enumeration"* — which is precisely the cost that a supplied vocabulary removes.
Models that lost badly on freeform naming (`qwen-9b` at 0.712 nodes, or the 3-4x faster
`qwen3.6-35b-a3b` MoE) may be materially more competitive when the choice space is enumerated for
them.

Two further reasons this matters here specifically:

1. **Freeform is the fallback, not the intended shape.** Freeform LLM type classification was
   added as a patch because `add_episode()` without `entity_types` labelled everything
   `['Entity']`. Ontology-constrained extraction is arguably the production case, and it is the
   one with no measurements.
2. **It should move #248's headline reliability metric.** A closed type vocabulary is far easier
   to coerce into valid structured output than open-ended naming. The local path's
   `response_format: {"type": "json_object"}` guarantee is weaker than the hosted path's
   schema-enforced tool use (ADR-0041), so `Strict` is exactly where that gap should narrow — and
   #248 currently cannot see it.

### Why this issue does not itself produce mode-matrix numbers

#248 established the precedent that a full-corpus `lcg-eval` pass requires a local
OpenAI-compatible model server and a live, spend-authorized `ANTHROPIC_API_KEY` — neither is
available inside Fabrik's sandbox (`specs/248-benchmark-run-full-corpus/spec.md`). Multiplying
that by three ontology modes (freeform / Open / Strict) only deepens the same constraint. This
issue is scoped, like #248 and #217 before it, as **mechanism plus a documented, copy-pasteable
procedure** — adding `--ontology`/`--ontology-mode` to `lcg-eval`, a corpus-matched fixture, and
runbook/script updates so a maintainer can run the three-mode matrix later. Actually running that
matrix, comparing rankings across modes (SC-002), and rewriting the README's local-extraction
guidance with real figures (SC-004) are maintainer follow-up steps this Fabrik run cannot execute
itself — consistent with `Ontology`/`OntologyMode` already being complete in `crates/core` (this
is harness plumbing, not new core behaviour).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Choose a local model for an ontology-constrained workspace (Priority: P1)

As a maintainer deciding which local model to recommend, I want F1 and structured-output
reliability measured **with an ontology applied**, so that the recommendation reflects how the
pipeline is actually used rather than only its fallback path.

**Why this priority**: nothing else in this issue matters if the harness cannot exercise the
ontology-constrained code path at all.

**Independent Test**: given an ontology fixture file and any configured `--backend`, running
`lcg-eval --ontology <path> --ontology-mode strict` produces a report with per-backend F1 and
structured-output-reliability figures, and the run's system prompts visibly contain the
`Strict`-mode instruction (verifiable via `--record-cassette`, since the cassette records the
exact rendered `entity_system_prompt`/`edge_system_prompt`).

**Acceptance Scenarios**:

1. **Given** an ontology fixture file with entity and relation types, **When** a maintainer runs
   `lcg-eval --ontology <path> --ontology-mode strict --backend name=SPEC`, **Then** the harness
   passes the loaded ontology through `ExtractOptions.ontology` on every extraction call, and the
   resulting report gives per-backend judged F1 and structured-output-reliability figures for
   that run.
2. **Given** the same fixture and corpus, **When** the harness is run with `--ontology-mode open`
   instead of `strict`, **Then** the resulting report is comparable (same corpus, same backends)
   against both the `strict` run and a freeform run (no `--ontology` flag) on the same corpus.
3. **Given** no `--ontology` flag, **When** `lcg-eval` runs, **Then** behavior is unchanged from
   today: `ExtractOptions.ontology` is `None` and the report is a freeform-extraction report,
   exactly as every existing eval run and CI check already expects.

---

### User Story 2 - Know whether the model ranking is mode-dependent (Priority: P2)

As a maintainer writing the README's local-extraction guidance, I want to know whether the
freeform ranking holds under `Strict`, so that I don't state a flat recommendation that is only
true for one regime.

**Why this priority**: this is the actual point of the issue, but it structurally depends on User
Story 1's plumbing existing first, and — per Background — on a maintainer actually running the
matrix outside this sandbox.

**Independent Test**: given three completed JSON reports (freeform, Open, Strict) over the same
corpus and backends, a reviewer can read `docs/eval-full-corpus-runbook.md` and reconstruct
exactly which commands produced each report, and diff the reports' per-backend rankings without
re-running anything.

**Acceptance Scenarios**:

1. **Given** the same corpus and backends run in freeform, `Open`, and `Strict`, **When** the
   three reports are compared, **Then** any reordering of the model ranking between modes is
   visible in the reports (each report records which mode produced it — see FR-003) and can be
   written up without ambiguity about which report is which.
2. **Given** no mode-matrix run has yet been executed (the state this Fabrik-driven
   implementation leaves the repo in — see Background), **When** the README's local-extraction
   guidance is reviewed, **Then** it is edited to state plainly that the existing figures
   describe freeform extraction only and that ontology-constrained figures are not yet measured,
   rather than silently continuing to imply the freeform numbers apply universally.

---

### User Story 3 - Distinguish a vocabulary violation from malformed JSON (Priority: P3)

As a maintainer reading a `Strict`-mode report, I want to see, separately from JSON
parse-reliability, how often a model emits an entity or relation type outside the declared
vocabulary, so that a model which produces syntactically valid JSON but ignores the closed type
list is not scored as if its structured-output reliability were perfect.

**Why this priority**: valuable and specifically called out by the issue, but it refines a metric
rather than gating User Stories 1-2 — the harness is usable without it, just less informative
under `Strict`.

**Independent Test**: feed the harness a fixed extraction result (via a test double / recorded
cassette) containing one entity whose type is in the `Strict` vocabulary and one whose type is
not; assert the report's structured-output reliability counters (`clean`/`recovered`/`malformed`)
are unaffected by the out-of-vocabulary entity, while a distinct vocabulary-compliance counter
reflects it.

**Acceptance Scenarios**:

1. **Given** a `Strict`-mode run where a candidate backend emits valid JSON containing an entity
   type not in the ontology's declared `entity_types`, **When** the report is generated,
   **Then** `structured_output.{clean,recovered,malformed}` reflects only JSON-syntax validity
   (unaffected by the type violation), and a separate vocabulary-compliance metric (FR-007)
   records the violation.
2. **Given** the same scenario for a relation type not in `relation_types`, **When** the report is
   generated, **Then** the same distinction holds for edges.

---

### Edge Cases

- **Cassette compatibility is already handled and must not regress.** `ontology` participates in
  the cassette key via the rendered `entity_system_prompt`/`edge_system_prompt`
  (`crates/core/src/cassette.rs`), so a cassette recorded freeform correctly misses against a
  `Strict` run rather than silently replaying the wrong prompt. This must be verified (not just
  assumed) once `--ontology` reaches `ExtractOptions` — a silent match here would invalidate
  every cross-mode comparison.
- **A `Strict`-mode model emits a type outside the declared list.** Resolved by FR-007: this is a
  distinct failure mode from malformed JSON (the response can be syntactically perfect JSON that
  simply disobeys the vocabulary constraint) and MUST be tracked separately from
  `structured_output.{clean,recovered,malformed}`, not folded into it.
- **An ontology whose types don't occur in the corpus produces a degenerate comparison.** FR-005's
  fixture MUST be chosen against the actual corpus content (Wikipedia spaceflight/Apollo
  articles), not written generically.
- **`--ontology-mode` given without `--ontology`.** Per FR-002, the flag has no ontology to apply
  to — this MUST be rejected as a CLI usage error at argument-parsing time, not silently ignored,
  so a maintainer doesn't believe a mode was applied when it wasn't.
- **The loaded ontology fixture file declares its own `mode:` key.** Per FR-002, the CLI's
  `--ontology-mode` value (explicit or defaulted) is authoritative and overrides any `mode:`
  present in the loaded file — this lets one fixture file drive all three regimes in the matrix
  (FR-005 ships one fixture, not three), rather than requiring `--ontology-mode` to match the
  file's own declaration or be rejected as a conflict.
- **No mode-matrix run exists yet when this issue's implementation lands.** Per Background/User
  Story 2, this is the expected state, not a failure — downstream documentation MUST say so
  explicitly rather than fabricate placeholder rankings.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `lcg-eval` MUST accept `--ontology <PATH>`, loading an `Ontology` from the given
  file path and passing it through `ExtractOptions.ontology` on every extraction call in the run,
  replacing the current hardcoded `None` at `crates/eval/src/runner.rs:113`. Loading MUST work
  from a bare file path (this is a standalone eval fixture, not necessarily inside a
  `.lcg`-rooted workspace).
- **FR-002**: `lcg-eval` MUST accept `--ontology-mode <open|strict>`. When `--ontology` is given
  and `--ontology-mode` is omitted, the mode defaults to `strict`. When `--ontology-mode` is
  given, its value overrides any `mode:` declared inside the loaded ontology file (see Edge
  Cases). When `--ontology` is not given, `--ontology-mode` MUST be rejected as a usage error
  (see Edge Cases) rather than silently ignored.
- **FR-003**: The JSON report MUST record which regime produced it — freeform, `open`, or
  `strict` — so that freeform and constrained run outputs are never confused when compared or
  archived side by side.
- **FR-004**: The report's existing structured-output reliability figures
  (`clean`/`recovered`/`malformed`, per backend) MUST be produced correctly for ontology-
  constrained runs, exactly as they already are for freeform runs — this is the metric most
  likely to move once a closed vocabulary removes open-ended naming from the model's task.
- **FR-005**: A representative ontology fixture file MUST be added alongside the #217 corpus
  fixture (`crates/core/tests/fixtures/real_corpus_wal/`), with entity and relation types chosen
  to actually occur in that corpus's Wikipedia spaceflight/Apollo content, so mode-matrix runs
  are reproducible from a committed fixture without hand-authoring a workspace ontology.
- **FR-006**: `docs/eval-full-corpus-runbook.md` and a script under `crates/eval/scripts/` MUST
  document the exact commands to run the freeform/`Open`/`Strict` mode matrix over the same
  corpus and backends, following the existing runbook's precedent (explicit prerequisites,
  copy-pasteable commands, cost caveats) rather than introducing a new documentation convention.
- **FR-007**: When running under `Strict` mode, the harness MUST track and report, separately
  from `structured_output.{clean,recovered,malformed}`, how often a candidate backend emits an
  entity or relation type outside the ontology's declared vocabulary. This is a distinct failure
  mode from malformed JSON (Edge Cases) and MUST NOT be folded into the existing
  clean/recovered/malformed counters or silently dropped from the report.

### Key Entities

- **Ontology fixture**: the committed YAML file (FR-005) declaring entity/relation types drawn
  from the #217 corpus's actual content, loaded via `--ontology` to drive Open/Strict runs.
- **Mode-matrix report set**: three `lcg-eval` JSON reports (freeform, Open, Strict) over the
  same corpus and backend set, each self-identifying its mode (FR-003), comparable against one
  another.
- **Vocabulary-compliance metric**: the new, distinct-from-structured-output count/rate (FR-007)
  of extracted entities/relations whose type fell outside a `Strict` ontology's declared list.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The same corpus and backends can be run in freeform, `Open`, and `Strict` — i.e.
  the CLI flags and fixture exist and produce three structurally comparable reports when a
  maintainer runs the documented commands. (Producing the actual three reports is the maintainer
  follow-up described in Background/User Story 2, not part of this issue's automated
  deliverable.)
- **SC-002**: Once a maintainer has produced the three reports, any reordering of the model
  ranking between modes is visible in the reports (via FR-003's mode field) and can be written up
  without re-running anything or guessing which report is which.
- **SC-003**: Structured-output reliability (FR-004) is reported per mode per backend.
  Vocabulary-compliance (FR-007) is reported per backend for `Strict`-mode runs only — the mode
  it applies to, per FR-007 — and is absent (not zeroed) on freeform/`Open` reports. The two are
  visibly distinct metrics in the report — never merged into one number.
- **SC-004**: README local-extraction guidance is edited to state that its existing figures
  describe freeform extraction only, pending mode-matrix measurement (per User Story 2, Scenario
  2) — or, if a maintainer has already supplied measured mode-matrix figures by the time this
  ships, the guidance instead states plainly whether the ranking holds across modes.
- **SC-005**: `cargo fmt`, `cargo clippy --all-targets -- -D warnings` (locally) /
  `cargo clippy --release -- -D warnings` (CI), and `cargo test` are green.

## Assumptions

- `Ontology`/`OntologyMode` in `crates/core` are complete and correct as-is; this issue is
  harness plumbing (CLI flags, report fields, a fixture, documentation) plus one new metric
  (FR-007), not new core extraction/ontology behaviour.
- #263 (cassette replay backend, `cassette:path=...` in `--backend`) has already merged, so the
  sequencing concern in the original issue ("landing #263 first materially reduces the cost of a
  three-mode matrix") is already satisfied — a maintainer running the eventual mode matrix can
  replay a previously recorded hosted-reference leg rather than re-paying for it per mode.
- Actually executing a full mode-matrix run (which requires a live, spend-authorized
  `ANTHROPIC_API_KEY` and/or a reachable local model server) is outside Fabrik's sandbox,
  consistent with #248's and #217's precedent — this issue's Fabrik-automated deliverable is
  mechanism plus documented procedure, not the executed run itself.
- The vocabulary-compliance metric (FR-007) is additive to the existing report schema; it does
  not change how `structured_output.{clean,recovered,malformed}` is computed for any existing
  (freeform) run, so no historical report or CI expectation regresses.

## Out of Scope

- Actually running the freeform/Open/Strict mode matrix and populating
  `docs/extraction-quality-evaluation.md`/README with measured figures — a maintainer follow-up,
  same as #248's own deferred execution step.
- Changes to `Ontology`/`OntologyMode`/prompt-injection behavior in `crates/core` — assumed
  complete per the original issue's Assumptions.
- Applying `Strict`-mode vocabulary filtering (dropping non-compliant entities/edges, as
  production ingestion does in `crates/core/src/episode.rs`) to the eval harness's extraction
  path. The harness measures the raw model output as emitted, including vocabulary violations —
  that is precisely what FR-007's metric needs to see; silently dropping violations before
  scoring would hide the failure mode this issue exists to surface.

## Source References

- `crates/eval/src/runner.rs:113` — the hardcoded `ontology: None` this issue removes.
- `crates/core/src/prompts/mod.rs` — `build_entity_types_section`/`build_fact_types_section`,
  the three-regime prompt injection this issue's eval coverage must exercise.
- `crates/core/src/ontology.rs` — `Ontology`, `OntologyMode`, `load_ontology` (currently
  workspace-rooted; FR-001 needs a bare-file-path load).
- `crates/core/src/cassette.rs` — cassette key construction; the Edge Cases verification target.
- `crates/eval/src/{cli.rs,backend.rs,report.rs,runner.rs}` — existing harness surface FR-001
  through FR-004/FR-007 extend.
- `docs/eval-full-corpus-runbook.md` — the existing runbook FR-006 extends with the mode matrix.
- `specs/248-benchmark-run-full-corpus/spec.md` — precedent for scoping a Fabrik issue as
  mechanism-plus-runbook when the actual run requires resources outside the sandbox.
- #248 — the benchmark this gap affects; its results describe freeform only.
- #263 — cassette replay backend; already merged (see Assumptions).
- `docs/history/extraction-eval-2026-04.md` — inherited rankings, also freeform-only.
- ADR-0041 — why the local path coerces structure via `json_object` rather than tool use.
