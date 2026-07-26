# Feature Specification: Record/Replay LLM Cassette for Extraction

**Feature Branch**: `fabrik/issue-232`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "Record/replay LLM cassette for extraction — deterministic, zero-cost re-runs of the real extraction path"

## Background

Every test that exercises the extraction pipeline either uses `MockExtractor` — which returns a fixed `Alice` + `Acme Corp` + one `WORKS_AT` edge regardless of input — or requires live, paid LLM calls. There is no way to re-run the real extraction path against real model output deterministically.

That leaves a whole class of change untestable:

- **Prompt changes.** The predecessor project's extraction eval was invalidated wholesale when prompt layout was restructured, because nothing pinned model behaviour to a known input/output pair. This project has the same exposure today.
- **Response-parsing changes.** `AnthropicExtractor` parses `tool_use` blocks; `OaiExtractor` (#212) parses OpenAI `tool_calls`/`content` with defensive fallbacks. Both are parsing real-world model output shapes, and both are currently covered only by synthetic fixtures the author wrote by hand.
- **Regression testing of the ingest pipeline** end-to-end with realistic entity/edge yield, without paying per run.

### Why this cannot be done outside the engine

Two constraints make this an engine feature rather than a scripting one:

1. **LLM calls happen inside the service.** `crates/core/src/episode.rs` calls `state.extractor.extract(...)`. External drivers (e.g. the #217 capture script) speak `knowledge_add_episode` over the Unix socket and never observe the model exchange.
2. **The extraction endpoint is hardcoded.** `crates/core/src/extractor.rs:19` — `const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages"` — with no override on `main`, so a recording proxy cannot be interposed either. (#212 adds `LCG_EXTRACTION_URL`, but only for the OpenAI-compatible path.)

### What a cassette is and is not for

Worth stating precisely, because these were conflated in earlier discussion:

| Artifact | Answers |
|---|---|
| WAL fixture (#217) | rebuild the graph with **no LLM and no embedder** |
| Corpus prose (#217) | feed **identical inputs to a different model** (model comparison, #228) |
| **LLM cassette (this issue)** | **re-run our own extraction pipeline against frozen real responses** |

A cassette recorded from `claude-haiku-4-5` can only replay `claude-haiku-4-5`. It is **not** a model-comparison tool — that's what the corpus is for. Its value is deterministic, free regression testing of prompts, parsing, and downstream ingest against genuine model output.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Deterministic replay of a recorded ingest (Priority: P1)

An engineer runs a real ingest against corpus text with recording enabled, producing a cassette. They later re-run the identical ingest with replay enabled and get the same entities and relationships, with zero outbound LLM calls — even with networking unavailable.

**Why this priority**: This is the core value proposition of the feature — deterministic, zero-cost regression testing of the real extraction path.

**Independent Test**: Enable `LCG_RECORD_LLM` on one real extraction run to produce a cassette, then disable networking, enable `LCG_REPLAY_LLM`, re-run the identical ingest, and confirm the resulting entities/edges are identical with zero observed network calls.

**Acceptance Scenarios**:

1. **Given** a workspace with no existing cassette, **When** `LCG_RECORD_LLM=<path>` is set and a real episode is ingested, **Then** a JSONL cassette file is created at `<path>` containing one record per extraction call.
2. **Given** a previously recorded cassette, **When** `LCG_REPLAY_LLM=<path>` is set (networking unavailable) and the identical episode is ingested again, **Then** the same entities and relationships are produced and no outbound LLM request is made.

---

### User Story 2 - Loud failure on cassette miss (Priority: P2)

An engineer who changed a prompt or a piece of parsing logic replays a cassette and the request doesn't match any recorded entry. The ingest fails immediately and identifiably, instead of silently falling back to a live paid call or producing wrong output.

**Why this priority**: Prevents silent cost and behavior drift; this is the difference between a trustworthy regression harness and one that quietly masks divergence.

**Independent Test**: Under replay mode, issue a request that doesn't match any recorded key and confirm a distinct, identifiable error is raised with no network call.

**Acceptance Scenarios**:

1. **Given** a cassette that does not contain a record matching the current request's key, **When** an extraction is attempted in replay mode, **Then** the call fails with an identifiable "cassette miss" error and no network request occurs.

---

### User Story 3 - Credential-free cassettes (Priority: P2)

A maintainer commits cassette fixtures to the public repository and needs assurance that no API keys, auth headers, or other credentials ever appear in the recorded file.

**Why this priority**: Cassettes are committed to a public repo; a credential leak would be a security incident.

**Independent Test**: Run an automated test that scans a recorded cassette for key-shaped strings and known auth header names, asserting none are present.

**Acceptance Scenarios**:

1. **Given** recording is enabled and a live extraction call is made, **When** the response is written to the cassette, **Then** no Authorization/API-key header value or other key-shaped string appears anywhere in the record.

---

### User Story 4 - Works uniformly across extractors and the router (Priority: P2)

A developer wants record/replay to work identically whether the ingest is configured with a bare `AnthropicExtractor`, a bare `OaiExtractor`, or an `LlmRouter` wrapping primary/fallback extractors, without each extractor implementation needing its own record/replay logic.

**Why this priority**: This is what makes a decorator-based approach worth building instead of a one-off shim on a single extractor.

**Independent Test**: Configure record/replay around an `LlmRouter` with a primary and fallback extractor, run an ingest that exercises fallback, and confirm both primary and fallback calls are captured and later replayed correctly.

**Acceptance Scenarios**:

1. **Given** an `LlmRouter` with a primary and fallback extractor, **When** recording is enabled and the primary fails over to the fallback during a real run, **Then** both the primary and fallback calls are recorded as separate cassette entries.
2. **Given** a cassette recorded through an `LlmRouter`, **When** replay is enabled, **Then** the router's calls are served from the cassette without router-specific replay logic.

---

### Edge Cases

- Multiple episodes ingested in one run produce multiple extraction calls with different content, each of which must match its own distinct cassette record.
- Two requests with identical semantic content within a single ingest run (duplicate calls) must both be served correctly from the cassette, rather than the first match being consumed and the second treated as a miss.
- Re-running recording against a path that already holds a cassette from a prior run (append vs. overwrite behavior) must be handled predictably.
- A prompt or parsing change that alters the semantic request content invalidates the existing cassette for the affected calls; this must surface as a loud cassette-miss failure, never a silent divergence.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When `LCG_RECORD_LLM=<path>` is set, every extraction call MUST append a record to the file at `<path>` capturing the full request (model, messages, tools/tool_choice or equivalent) and the full response body, plus metadata sufficient to interpret it (provider, model, timestamp, prompt version if available).
- **FR-002**: When `LCG_REPLAY_LLM=<path>` is set, extraction calls MUST be served from the cassette at `<path>` with no network access.
- **FR-003**: A replay request that has no matching cassette record MUST fail loudly and identifiably rather than silently falling through to a live call.
- **FR-004**: Cassette record matching MUST use a stable hash of the semantic request content, independent of record order or wall-clock time.
- **FR-005**: The keying scheme MUST document precisely what request content is included in the hash and what is excluded (e.g., timestamps, nonces).
- **FR-006**: Record/replay MUST be implemented as a decorator over the existing `Extractor` trait (e.g., `RecordingExtractor<E: Extractor>` / `ReplayingExtractor<E: Extractor>`) so it applies to `AnthropicExtractor`, `OaiExtractor`, and `LlmRouter` without modifying each implementation.
- **FR-007**: Extractor selection (plain vs. recording vs. replaying) MUST happen at the point the extractor is constructed (`app_state.rs`).
- **FR-008**: Cassette records MUST NOT contain API keys, credentials, or auth header values — such material must be excluded or scrubbed before writing.
- **FR-009**: The cassette format MUST be plain JSONL, uncompressed, one record per line.

### Key Entities *(if applicable)*

- **Cassette**: A JSONL file where each line is one recorded extraction exchange, selected via `LCG_RECORD_LLM` / `LCG_REPLAY_LLM`.
- **Cassette Record**: A single JSONL line capturing one extraction request/response pair plus interpretive metadata (provider, model, timestamp, prompt version) and its deterministic matching key.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A real ingest run with recording enabled produces a cassette; an identical subsequent ingest with replay enabled produces the same entities and relationships with zero outbound LLM requests, verifiable with networking unavailable.
- **SC-002**: A cassette miss during replay always fails loudly and identifiably; it never results in a silent live call.
- **SC-003**: An automated test scanning a recorded cassette for key-shaped material and auth headers finds none.
- **SC-004**: Record/replay functions correctly both through a bare extractor and through an `LlmRouter` (primary/fallback).
- **SC-005**: A cassette produced by this feature is committed as uncompressed JSONL.
- **SC-006**: `cargo fmt`, `cargo clippy --release -- -D warnings`, and `cargo test --release` all pass.

## Assumptions

- A cassette recorded from one model (e.g. `claude-haiku-4-5`) can only replay that same model; it is not a model-comparison tool — that role belongs to the corpus (#217) plus #228.
- The #217 golden-corpus capture run has already completed (228 episodes / 1,506 entities / 2,392 relationships) without a cassette, because this recording hook did not yet exist — the originally hoped-for "free recording" window (piggybacking on #217's final capture) has closed.
- Sequencing going forward: #217 completes first using the WAL already captured, with no further paid extraction; this issue (#232) lands next, adding the record/replay decorator; then a single re-capture pass regenerates the full artifact set (corpus prose, WAL, `expected_results.json`, and cassette) in one consistent run — same revisions, cleanup version, model, and prompts.
- The re-capture cost (~$1–2 on Haiku, ~45 min) is budgeted as part of landing this issue. Its record-then-replay sequence — record during the run, then replay and assert identical entities/edges with zero outbound LLM requests — doubles as the acceptance-criteria verification step for this feature.
- #217 is being restructured with a two-phase stage→ingest flow (corpus staged to disk before any paid extraction) specifically so the eventual re-capture is robust against a network fault mid-run discarding paid work.
- This issue is not blocked on #212 (OpenAI-compatible extractor) — the decorator is generic over the `Extractor` trait and does not require `OaiExtractor` to exist.
- Prompt or parsing changes may legitimately invalidate an existing cassette; when that happens it must surface as a loud cassette-miss failure, not a silent divergence, with a documented re-recording procedure.
- Cassette size is expected to be a few MB of JSONL for the full corpus (~230 calls, each including a full prompt); acceptable to commit uncompressed, but should be measured before committing.

## Out of Scope *(optional)*

- Recording embedder traffic — the embedder is separately mockable and cheap.
- Model comparison across providers/models — that is the role of the corpus (#217) and #228.
- Changing extraction behaviour.

## Source References *(optional)*

- `crates/core/src/episode.rs` — extraction call site (`state.extractor.extract(...)`)
- `crates/core/src/extractor.rs:19` — hardcoded `ANTHROPIC_API_URL`
- `crates/core/src/app_state.rs` — extractor construction / selection point
- #217 — golden real corpus capture run / artifact set
- #228 — eval harness (may replay cassettes for its own regression tests)
- #212 — adds the `LCG_EXTRACTION_URL` seam for the OpenAI-compatible path
