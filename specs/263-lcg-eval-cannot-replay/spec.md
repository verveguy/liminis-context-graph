# Feature Specification: Cassette replay backend for `lcg-eval`

**Feature Branch**: `fabrik/issue-263`
**Created**: 2026-07-27
**Status**: Draft
**Input**: User description: "lcg-eval cannot replay a cassette: ReplayingExtractor exists in core but no --backend spec reaches it"

## Background

`crates/core/src/cassette.rs:281` defines `ReplayingExtractor`, and #232 shipped it with tests and ADR-0044 ("Record/Replay LLM Cassette"). But `lcg-eval`'s backend parser (`crates/eval/src/backend.rs:34-61`) accepts only three kinds — `anthropic`, `oai-http`, `oai-uds` — and `build_extractor` constructs `AnthropicExtractor`/`OaiExtractor` directly, with no factory seam for a replay backend. Searching the tree for the env-var seam mentioned in `llm_router.rs:65` (`LCG_REPLAY_LLM`/`LCG_RECORD_LLM`) finds only `crates/service/src/main.rs`, which is the standalone service's ingest path, not `lcg-eval`.

**Net effect: cassettes are write-only from the only tool that records them.** #232's stated purpose — "re-run our own extraction pipeline against frozen real responses" — is unreachable from `lcg-eval`. #228 built the eval harness, and its `--record-cassette` flag (`crates/eval/src/cli.rs`) wires the recording half via `RecordingExtractor` (`crates/eval/src/main.rs:72-80`), but nothing wires the replay half.

This is not hypothetical. The #248 full-corpus benchmark run (`specs/248-benchmark-run-full-corpus/`) captured 226 real Haiku responses (3.3 MB) for its `baseline` leg, then the run died in the `qwen` leg on an unrelated fault. That capture cannot currently be replayed, so re-running the benchmark re-pays for the `baseline` leg's extraction calls a second time.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Reuse an already-paid-for hosted capture (Priority: P1)

As a maintainer re-running the #248 benchmark after a failure in a later leg, I want to replay the hosted baseline from the cassette that run already captured, so that I don't re-pay for extraction calls whose real responses are sitting on disk.

**Why this priority**: This is the concrete, current blocker — a paid capture exists and cannot be reused, forcing needless re-spend on every retry of a multi-leg benchmark run.

**Independent Test**: With `anthropic-claude-haiku-4-5-20251001.jsonl` (226 recorded `extract` calls) on disk and networking unavailable, run `lcg-eval --backend baseline=cassette:path=<that file> ...` and confirm the baseline leg completes with zero outbound LLM requests, producing the same entities/edges as the original live run.

**Acceptance Scenarios**:

1. **Given** a cassette file containing recorded `extract` calls for every chunk in the corpus subset being run, **When** `lcg-eval --backend baseline=cassette:path=<file> ...` is invoked with networking unavailable, **Then** the baseline leg completes successfully with zero outbound LLM requests.
2. **Given** the same cassette and corpus subset, **When** replay runs, **Then** the entities and edges produced for `baseline` are identical to those from the original live-recorded run.

---

### User Story 2 - Deterministic regression testing of the extraction pipeline (Priority: P2)

As a developer changing prompt templates or response parsing, I want to run the extraction pipeline against frozen real model output, so that a behavioural regression surfaces as a test failure rather than requiring a paid run to detect.

**Why this priority**: Extends #232's regression-testing value proposition to `lcg-eval` specifically, catching prompt/parsing drift without spending on a live run — valuable, but secondary to unblocking the concrete stuck benchmark in User Story 1.

**Independent Test**: With a committed cassette, edit `extract_text.txt` (or any templated prompt input covered by the request hash) and re-run replay; confirm it fails with `Error::CassetteMiss` rather than completing and silently scoring against stale responses.

**Acceptance Scenarios**:

1. **Given** a committed cassette recorded against the current prompt templates, **When** a covered prompt template is edited and replay is re-run, **Then** the run fails with `Error::CassetteMiss` naming the missing key, and produces no scored result.

---

### Edge Cases

- **Replaying both hosted legs would destroy the noise floor.** #248's `baseline` and `candidate` are deliberately two *independent* live samples of the same spec; their disagreement **is** the measurement. Replaying one cassette into both makes them byte-identical and judged F1 becomes 1.000 by construction. The `docs/eval-full-corpus-runbook.md` update (FR-006) must say this explicitly: replay is for `baseline` only, `candidate` always runs live.
- **The captured cassette holds 226 records against a 228-chunk corpus** (`crates/core/tests/fixtures/real_corpus_wal/corpus_prose.jsonl`, per `crates/eval/src/corpus.rs`'s `loads_the_real_217_fixture` test). Cause unresolved. Under replay this becomes permanent: those two chunks will `Error::CassetteMiss` on every future run against that file. That is loud rather than silent (satisfying FR-003), but the 226-vs-228 discrepancy itself should be diagnosed as part of this work and the finding recorded in the runbook update — if it is a legitimate skip (e.g. two chunks that produce no `extract` call), say so; if it is dropped coverage, flag that it undermines any benchmark built on the fixture.
- **Cassette recorded against a different corpus, or a truncated/corrupt file.** `ReplayingExtractor::load` (`crates/core/src/cassette.rs:286-314`) already parses the whole file eagerly and returns `Error::Config` on the first invalid JSON line, so a truncated/corrupt file fails at load time today. A cassette recorded against an unrelated corpus has no distinct "wrong corpus" signal at load time — since matching is by per-call content hash, not corpus identity — but the first mismatched request produces `Error::CassetteMiss` immediately (on the very first extraction call), which is loud and immediate in practice. Building corpus-identity pre-validation into `ReplayingExtractor` is out of scope (see Out of Scope) — `Error::CassetteMiss` on first use satisfies FR-003 without new replay logic.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `lcg-eval`'s backend spec parser MUST accept a `cassette` kind — `--backend <NAME>=cassette:path=<PATH>` — and `build_extractor` MUST construct the existing `ReplayingExtractor` (`crates/core/src/cassette.rs:281`) from it. This issue MUST NOT reimplement replay logic in `crates/eval`; it wires the existing type in.
- **FR-002**: A backend built from a `cassette:` spec MUST make zero outbound LLM requests. This MUST be verifiable with networking unavailable.
- **FR-003**: A cassette miss (a request with no matching recorded entry) MUST fail loudly and identifiably — `Error::CassetteMiss`, naming the missing key — and MUST NOT fall through to a live call.
- **FR-004**: The `model` field on recorded `CassetteRecord`s (`crates/core/src/cassette.rs:132`) MUST be fixed to store the model id (e.g. `claude-haiku-4-5-20251001`) rather than the backend name (e.g. `"baseline"`), which is what `crates/eval/src/main.rs:72-80` currently passes as both the `provider` and `model` arguments to `RecordingExtractor::new`. This is a mislabel for cassettes destined for a public repo as canonical per-model fixtures. The `model` field is descriptive only — `request_key` (`crates/core/src/cassette.rs:59`) hashes semantic call content and explicitly excludes provider/transport specifics — so existing cassette files can be relabelled in place (e.g. by editing the `model` value on each JSONL line) without invalidating any key.
- **FR-005**: `--record-cassette` and a `cassette:` backend spec MUST be rejected together for the same backend name, with a clear error message. Recording a replay is meaningless (there is no live call to capture).
- **FR-006**: `docs/eval-full-corpus-runbook.md` MUST be updated with a "resuming a partial run" section demonstrating replay of an already-captured leg, and MUST state explicitly that replay applies to `baseline` only — never to `candidate` — per the noise-floor edge case above.

### Key Entities

- **`BackendKind::Cassette`**: a new variant (alongside `Anthropic`, `OaiHttp`, `OaiUds`) in `crates/eval/src/backend.rs`'s backend-spec enum, holding the cassette file path.
- **`ReplayingExtractor`**: existing type (`crates/core/src/cassette.rs:281`), reused unchanged as the constructed `Arc<dyn Extractor>` for a `cassette:` backend.
- **`CassetteRecord.model`**: existing field (`crates/core/src/cassette.rs:132`) whose write path (`crates/eval/src/main.rs:72-80`) is corrected by this issue to carry the model id.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Replaying `anthropic-claude-haiku-4-5-20251001.jsonl` as the `baseline` backend completes with zero network calls and produces results identical to the original live run.
- **SC-002**: Editing a prompt template covered by the request hash invalidates the cassette, surfacing as `Error::CassetteMiss` rather than silent divergence.
- **SC-003**: The captured 226-record file works after an in-place `model` relabel, with no re-capture required.
- **SC-004**: `cargo fmt`, `cargo clippy --all-targets -- -D warnings` (locally) / `cargo clippy --release -- -D warnings` (CI), and `cargo test` are all green.

## Assumptions

- `ReplayingExtractor` is functionally complete; this is a wiring and CLI-surface change to `crates/eval`, not new replay logic in `crates/core`.
- Per ADR-0044's documented gaps, the request hash does not cover the edge *user* prompt or the inline classification prompts, so template edits to those specific prompts will not invalidate a cassette. This gap is out of scope here; it is a candidate follow-up issue if it turns out to matter in practice.
- The 226-vs-228 record-count discrepancy in the #248 capture is diagnosed and documented as part of this work, but resolving it (e.g. re-capturing to close the gap) is not required for this issue's completion — `Error::CassetteMiss` on the two uncovered chunks is an acceptable, loud outcome per FR-003.

## Out of Scope

- Reimplementing or extending `ReplayingExtractor`'s matching/replay behavior in `crates/core` (e.g. adding corpus-identity pre-validation at load time). FR-003's existing `Error::CassetteMiss`-on-first-mismatch behavior is sufficient.
- An automated tool or migration script for relabeling already-captured cassette files' `model` field. FR-004 only requires fixing the write path so *future* recordings are labeled correctly; relabeling the existing #248 capture in place is a manual maintainer action enabled by (but not automated by) this issue.
- Closing the ADR-0044 hash-coverage gap for the edge user prompt and inline classification prompts.
- Re-running or completing the #248 benchmark itself — this issue only unblocks the tooling that benchmark needs.

## Source References

- `crates/core/src/cassette.rs` — `ReplayingExtractor`, `RecordingExtractor`, `CassetteRecord`, `request_key`
- `crates/eval/src/backend.rs` — `BackendKind`, `parse_backend_spec`, `build_extractor`
- `crates/eval/src/main.rs:72-80` — where `RecordingExtractor` is currently constructed with the backend name in place of the model id
- `crates/eval/src/cli.rs` — `--record-cassette` flag parsing and existing backend-name cross-validation (lines ~213-216) as a precedent for FR-005's validation
- `docs/adr/0044-llm-cassette-record-replay-seam.md` — ADR for the record/replay seam, including the documented hash-coverage gaps referenced in Assumptions
- `docs/eval-full-corpus-runbook.md` — runbook to be updated per FR-006
- `specs/232-record-replay-llm-cassette/spec.md` — spec that introduced `ReplayingExtractor`
- `specs/228-rust-extraction-quality-eval/spec.md` — spec that introduced the `lcg-eval` harness and `--record-cassette`
- `specs/248-benchmark-run-full-corpus/spec.md` — the benchmark run that surfaced this gap
