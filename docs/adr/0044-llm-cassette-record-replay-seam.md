# ADR-0044: LLM Cassette Record/Replay Seam

**Status**: Accepted
**Date**: 2026-07-26
**Issues**: #232

## Context

Every test exercising the real extraction pipeline previously had two choices: pay for a live
LLM call, or use `MockExtractor`, which returns a fixed `Alice`/`Acme Corp`/`WORKS_AT` triple
regardless of input. Neither lets prompt changes, response-parsing changes
(`parse_entity_response`/`parse_oai_entity_response` and friends), or the ingest pipeline's real
entity/edge yield be regression-tested without spending money on every run.

`AnthropicExtractor` and `OaiExtractor` share no HTTP/transport abstraction — each owns its own
`reqwest::Client`/UDS pool and independently builds requests and parses responses. The only code
both share is `prompts::*` (rendering) and `crate::types::{ExtractedEntity, ExtractedEdge,
ExtractionResult}` (data shapes). `LlmRouter` routes to `primary`/`fallback: Arc<dyn Extractor>`
with switch-once-then-latch fallback semantics (ADR-0041 Decision 4).

## Decisions

### 1. Trait-level seam, not a sub-trait/transport-level seam

FR-006 requires the decorator to wrap the `Extractor` trait itself — `extract`,
`classify_entities`, `classify_relations` — rather than touching `AnthropicExtractor`'s or
`OaiExtractor`'s internals. This was weighed against a sub-trait seam that would intercept each
extractor's raw send+response, which would let replay re-exercise
`parse_entity_response`/`parse_oai_entity_response`/`extract_json_block` and genuinely
regression-test response-parsing changes on every replay, not just the original recording call.

**We chose the trait-level seam.** It is fully generic (works identically over
`AnthropicExtractor`, `OaiExtractor`, and `LlmRouter` with zero changes to any of them, per
FR-006's literal requirement), and it is what the spec's User Stories 1/2/4 and their acceptance
scenarios actually describe (replay produces identical entities/edges; a cassette miss fails
loudly; it works uniformly including through `LlmRouter`).

**Consequence, stated explicitly so it is never assumed otherwise:** a regression purely in
response parsing is caught only by the one live call that recorded the cassette — never by
replay. Replay deserializes a stored `ExtractionResult`/`Vec<String>` directly; it never
re-invokes `parse_entity_response` or `parse_oai_entity_response`. Anyone changing those
functions should not treat this cassette test suite as a safety net for that change; only a live
re-recording run exercises them.

### 2. The matching hash includes rendered prompts, not raw `ExtractOptions` alone

`ExtractOptions` (`episode_body`, `group_id`, `source_type`, `custom_instructions`,
`reference_time`, `ontology`) does not change when a prompt *template* changes (e.g. editing
`extract_text.txt`) or when the ontology's rendered content changes in a way not visible in the
raw struct. Hashing `ExtractOptions` alone would let a stale cassette entry keep silently
matching after such a change — exactly the failure mode the spec's Edge Cases section requires to
surface as a loud miss.

**We chose to render prompts as part of building the hash input.** `crate::cassette` calls the
same public, provider-agnostic `prompts::entity_system_prompt`, `prompts::entity_user_prompt_for`,
and `prompts::edge_system_prompt` functions both extractors already call internally, and folds
the rendered text into the canonical request value alongside the raw `ExtractOptions` fields.
Editing a prompt template file, or a change in what the ontology renders into the system prompt,
now correctly invalidates old cassette entries.

**Known, accepted gap**: `prompts::edge_user_prompt` cannot be rendered ahead of time by the
decorator, because it needs `entity_names` — which only exists inside the wrapped extractor's own
entity-extraction call, after the trait boundary. A template-only edit to `edge_user_prompt`'s own
text (not touching entity names or episode content) will not invalidate a cassette. This is
inherent to the trait-level seam (Decision 1) and is not considered worth the added invasiveness
of a sub-trait seam to close.

`classify_entities`/`classify_relations` have no equivalent shared, extractable prompt-rendering
function to fold in — each extractor builds its own inline classification prompt text, and the
wording differs between `AnthropicExtractor` and `OaiExtractor`. Their hash covers the raw call
arguments (entities/edges and allowed-types) only. A wording-only change to one extractor's
inline classification prompt will not invalidate a cassette recorded against the other extractor.

### 3. Recording wraps each `LlmRouter` leaf individually; replay is a single flat substitute

User Story 4's second acceptance scenario states the router's calls must replay "without
router-specific replay logic." Re-reading that closely settles an asymmetry: **replay** never
needs to know whether a call was originally served by a primary or a fallback — it matches purely
by request-content hash, so one `ReplayingExtractor` can wholesale replace whatever extractor
tree (bare extractor or `LlmRouter`) originally produced the cassette.

**Recording**, by contrast, does need per-leaf attribution: if `RecordingExtractor` wrapped
`LlmRouter` as a whole, it would only ever see the router's already-routed output and could not
tell whether a given call came from primary or fallback, so a primary→fallback failover could not
be recorded as two distinguishable, correctly-attributed entries. `LlmRouter::new`'s existing
signature already accepts pre-built `Arc<dyn Extractor>` primary/fallback slots, so wrapping each
leaf individually — before constructing the router — requires no change to `llm_router.rs`'s
routing logic. `LlmRouter::from_env_with(sink, wrap)` was added so `main.rs` can apply this
per-leaf wrapping through the existing `LCG_EXTRACTION_LLM` `primary:fallback` parsing path,
rather than duplicating that parsing logic; `from_env` delegates to it with an identity closure,
so its behavior is byte-for-byte unchanged when record/replay env vars are unset.

### 4. `ReplayingExtractor` holds no inner extractor and needs no credentials

`ReplayingExtractor` never calls anything but its in-memory cassette index — there is no
`Arc<dyn Extractor>` field to delegate to. This makes FR-002's "no network access" true by
construction rather than by discipline, and it means `main.rs`'s `bootstrap_app_state` can skip
provider resolution entirely under `LCG_REPLAY_LLM` — no `ANTHROPIC_API_KEY`, `--extractor-uds`,
`--extractor-http`, or `LCG_EXTRACTION_URL` is consulted or required in replay mode.

### 5. Credential exclusion is structural, not a scrubbing pass

FR-008 requires cassette records to never contain API keys or auth header values. Because the
decorator operates at the `Extractor` trait boundary — strictly above HTTP request construction
in both `AnthropicExtractor` and `OaiExtractor` — headers and API keys are never part of
`ExtractOptions` or `ExtractionResult` in the first place. There is no scrubbing step because
there is nothing to scrub: the trait-level seam structurally cannot observe credentials. This is
verified, not just asserted, by `crates/core/tests/cassette_record_replay.rs`'s credential-scrub
test, which confirms a fake API key that demonstrably crossed the wire (captured by the stub
server) never appears in the resulting cassette file.

### 6. `Error::CassetteMiss(String)` is a distinct error variant

FR-003/SC-002 require a replay miss to fail "loudly and identifiably." A new
`Error::CassetteMiss(String)` variant (rather than reusing `Error::Ipc` with a distinguishing
string prefix) gives callers and tests a matchable type (`matches!(err, Error::CassetteMiss(_))`)
instead of string-sniffing an error message.

### 7. Cassette writes always append; duplicate keys replay FIFO

Re-running `LCG_RECORD_LLM` against an existing path appends rather than truncates, matching
`WalWriter`'s convention — this answers the spec's Edge Case about re-running recording
predictably. `ReplayingExtractor::load` builds a `HashMap<String, VecDeque<Value>>` index by
scanning the cassette file once, in order; two calls with identical semantic content within one
ingest run are served the two distinct recorded responses in original order, rather than the
first match being consumed and the second treated as a miss.

## Consequences

- Regression coverage for prompt changes and ingest-pipeline entity/edge yield is real and
  replay-verified; regression coverage for response-*parsing* code specifically depends on the
  live recording call, not on replay (Decision 1).
- The `edge_user_prompt` template-text gap (Decision 2) and the classify-methods'
  call-arguments-only hash scope are both documented, narrow, and accepted — not silently
  incomplete.
- `crates/core/src/cassette.rs`'s module doc is the single authoritative source for exactly what
  is and isn't in the matching hash (FR-005); this ADR summarizes the reasoning, the module doc
  states the current, maintained scope.
- No real golden-corpus cassette ships with this issue. It requires a live, paid `ANTHROPIC_API_KEY`
  run (~$1–2 on Haiku, ~45 min) sequenced after #217's corpus/WAL fixture lands, and is deliberately
  left as a manual maintainer follow-up rather than an unattended paid API call during automated
  implementation. All Success Criteria in specs/232-record-replay-llm-cassette/spec.md are
  verified instead against a stub HTTP server in `crates/core/tests/cassette_record_replay.rs`.

## Related

- ADR-0014: Pass `Option<&Ontology>` as a call-time parameter to `Extractor::extract` — rejected a
  wrapper/adapter pattern for ontology specifically, on grounds of only 3 known implementors and
  no cross-cutting benefit. This issue's `RecordingExtractor`/`ReplayingExtractor` *is* the
  wrapper/adapter pattern ADR-0014 rejected, for a different purpose: record/replay is explicitly
  cross-cutting over every current and future `Extractor` implementor, where ontology's need was
  implementor-agnostic input, not implementor-spanning behavior.
- ADR-0010: `tool_use` structured-output extraction — one of the two response-parsing paths this
  issue's replay does *not* regression-test (Decision 1).
- ADR-0041: generalized `LlmRouter`/`AppState.extractor` to `Arc<dyn Extractor>`, the enabling
  precedent for plugging record/replay decorators in at construction time, and the precedent for
  passing model names as explicit constructor arguments rather than deriving them via a trait
  method.
- `crates/core/src/cassette.rs`: `CassetteRecord`, `CassetteWriter`, `RecordingExtractor`,
  `ReplayingExtractor`, and the module doc's authoritative hash-scope documentation (FR-005).
- `crates/core/src/llm_router.rs`: `LlmRouter::from_env_with`.
- `crates/service/src/main.rs` (`bootstrap_app_state`): `LCG_RECORD_LLM`/`LCG_REPLAY_LLM`
  resolution and per-leaf/flat wrapping.
- `crates/core/tests/cassette_record_replay.rs`: integration coverage for all four user stories.
- README.md's "Record/replay cassettes" section: user-facing documentation of the env vars,
  format, and re-recording procedure.
