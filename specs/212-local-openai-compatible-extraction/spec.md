# Feature Specification: Local/OpenAI-Compatible Extraction Adapter — Make the "Fully Local" Promise True

**Feature Branch**: `fabrik/issue-212`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "Local/OpenAI-compatible extraction adapter — make the 'fully local' promise true. The documented 'run fully local models' story is not achievable for extraction. In v0.10.0 the extractor is Anthropic-only... Add an OpenAI-compatible Extractor implementation and a provider-selection seam mirroring the embedder, so extraction can target a local /v1/chat/completions endpoint (defaulting to the running sidecar when present), making fully-local ingestion real."

## Background

The documented "run fully local models" story is not achievable for **extraction**. In the current shipped binary the extractor is Anthropic-only:

- `app_state.rs` unconditionally builds `LlmRouter::from_env`, which constructs a concrete `AnthropicExtractor` for both primary and fallback.
- `extractor.rs` hard-codes `https://api.anthropic.com/v1/messages` as the extraction endpoint.
- `LCG_EXTRACTION_LLM` only selects the Claude *model name*; there is no way to redirect the transport.
- The only URL override (`AnthropicExtractor::with_url`) exists solely for pointing tests at an unreachable address.

As a result, any `knowledge_process_chunk` / `knowledge_add_episode` call 401s unless `ANTHROPIC_API_KEY` is set — a hard requirement, not the optional one the docs describe (reported in #201).

The macOS CoreML sidecar already serves an OpenAI-compatible `/v1/chat/completions` route backed by Apple Foundation Models — it is live and reachable, but **nothing in the engine ever calls it**. It's exercised today only for embeddings (`/v1/embeddings`), never for extraction.

The seam to fix this already exists in shape: `Extractor` is a trait (`extractor.rs`) with multiple implementations (`AnthropicExtractor`, `MockExtractor`, `ConfigurableExtractor`), and the embedder subsystem already demonstrates the target end-to-end pattern — transport resolved from `--embedder-uds` / `--embedder-http` CLI flags, with a default-socket ladder (CLI flag → default UDS socket if present → env URL → error) resolved once in `main.rs` and threaded into `AppState::from_env`.

This issue is the local-extraction counterpart to that embedder work. It does not change the docs directly — README was already corrected to be truthful in the interim by a separate small PR tied to #201. This issue makes local extraction *real*, so that truthful claim can be restored to its original, stronger form once the capability actually exists.

### Why this matters

- **The "fully local" pitch is currently false for the feature that matters most.** Embedding-only local operation still requires a hosted key for every ingest call, which defeats the "no API key, no network" promise for anyone who can't or won't use Anthropic's hosted API.
- **The sidecar capability is already paid for and idle.** The CoreML sidecar's chat-completions route exists and works for other consumers; extraction simply never calls it.
- **Backward compatibility is non-negotiable.** Existing deployments that configure `ANTHROPIC_API_KEY` must see zero behavior change — this is an additive path, not a replacement.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Fully local ingestion with no hosted API key (Priority: P1)

A user runs the liminis-context-graph engine on macOS with the CoreML sidecar running and does **not** set `ANTHROPIC_API_KEY`. They call `knowledge_process_chunk` (or `knowledge_add_episode`) with ordinary text. Today this call fails with a 401 from Anthropic's API. After this change, the call succeeds, using the local sidecar's `/v1/chat/completions` endpoint for extraction, and returns typed entities and relationships just as the Anthropic path would.

**Why this priority**: This is the entire point of the issue — restoring the "fully local" capability that the README already claims. Without this, nothing else in the issue matters.

**Independent Test**: Start the sidecar, unset `ANTHROPIC_API_KEY`, start the engine with no extractor CLI flags, and call `knowledge_process_chunk` with a short passage containing at least one clear entity and relationship (e.g., "Alice works at Acme Corp"). Assert the call succeeds and the response contains at least one entity and one edge.

**Acceptance Scenarios**:

1. **Given** the sidecar is running and `ANTHROPIC_API_KEY` is unset, **When** `knowledge_process_chunk` is called with text describing an entity and a relationship, **Then** the call succeeds and returns typed entities and edges (not a 401 or connection error).
2. **Given** the sidecar is running and `ANTHROPIC_API_KEY` is unset, **When** the engine starts with no `--extractor-uds` / `--extractor-http` flag and no extraction-endpoint env var set, **Then** the engine auto-detects and uses the sidecar's default socket for extraction, mirroring how the embedder auto-detects it today.

---

### User Story 2 - Anthropic path is unaffected for existing hosted-key users (Priority: P1)

A user who already has `ANTHROPIC_API_KEY` configured (the existing, pre-change setup) upgrades to this version. Their extraction behavior — which model is used, what requests are sent, what telemetry is emitted, what errors look like — must be identical to before. The presence of a running local sidecar (which many users will have anyway, for embeddings) must not silently redirect their extraction traffic away from the Anthropic key they configured.

**Why this priority**: This is a strict backward-compatibility requirement called out explicitly in the issue's acceptance criteria. Regressing existing hosted-key users while fixing the local story would trade one broken promise for another.

**Independent Test**: With `ANTHROPIC_API_KEY` set and the sidecar also running (simulating a typical existing user who has both), call `knowledge_process_chunk` and confirm (via telemetry/logs) that the request went to Anthropic's endpoint, not the local sidecar.

**Acceptance Scenarios**:

1. **Given** `ANTHROPIC_API_KEY` is set and no explicit `--extractor-uds`/`--extractor-http` flag is passed, **When** the sidecar is also running, **Then** extraction still uses the Anthropic API (auto-detection of a local endpoint does not override an already-configured Anthropic key).
2. **Given** `ANTHROPIC_API_KEY` is set, **When** `knowledge_process_chunk` succeeds, **Then** the extraction telemetry event, model name, and cost accounting match pre-change behavior exactly.

---

### User Story 3 - Explicit local-endpoint selection (Priority: P2)

An operator wants extraction to target a specific local endpoint regardless of whether `ANTHROPIC_API_KEY` happens to be set — for example, to test the local path deliberately, or to run against a non-default sidecar socket/port. They pass `--extractor-uds <path>` or `--extractor-http <url>` at startup.

**Why this priority**: Needed for testability and for operators who want deterministic control rather than relying on auto-detection, but it is a refinement of Story 1/2's default behavior rather than the core value delivery.

**Independent Test**: Start the engine with `--extractor-http <url-of-a-mock-openai-compatible-server>` while `ANTHROPIC_API_KEY` is also set, and confirm extraction requests go to the local URL, not Anthropic.

**Acceptance Scenarios**:

1. **Given** `ANTHROPIC_API_KEY` is set, **When** the engine is started with `--extractor-uds <path>` or `--extractor-http <url>`, **Then** extraction uses the explicitly-selected local endpoint, overriding the key-present default.
2. **Given** both `--extractor-uds` and `--extractor-http` are passed simultaneously, **When** the engine starts, **Then** startup fails fast with a clear "mutually exclusive" error, mirroring the embedder's equivalent flags.

---

### Edge Cases

- **No provider reachable at all**: neither `ANTHROPIC_API_KEY` nor a reachable local endpoint (no sidecar socket, no `--extractor-uds`/`--extractor-http`, no env URL override) is available. The system must surface one clear, actionable error identifying that no extraction provider is configured — not a raw connection-refused error or an unhandled panic.
- **Local endpoint configured/detected but unreachable at call time**: the sidecar was up at startup detection but has since gone down (or a `--extractor-http` URL stops responding). Extraction calls must fail with a clear, typed error through the same error path the Anthropic adapter already uses — not a silent empty result.
- **Local model produces malformed or non-structured output**: the sidecar's Foundation-Models-backed endpoint returns a response that isn't valid function-calling output or valid JSON matching the expected schema. This must be treated as a parse error, consistent with how the Anthropic adapter handles a missing `tool_use` block, not silently coerced into an empty extraction result.
- **Local model truncates output**: the OpenAI-compatible equivalent of Anthropic's `stop_reason: "max_tokens"` (typically `finish_reason: "length"`) must be handled with the same budget-doubling retry-once behavior already implemented for Anthropic, so long inputs don't silently produce partial/empty results.
- **Both `--extractor-uds` and `--extractor-http` passed together**: rejected at startup with a clear mutual-exclusivity error, matching the embedder's existing validation.
- **`classify_entities()` with `allowed_types` constraint against the local provider**: the local adapter must enforce the same server-side "convert out-of-set responses to empty string" guard already applied to the Anthropic path, since a local model is no more trustworthy about honoring prompt constraints than a hosted one.
- **Relation-type normalization**: edges produced via the local adapter must flow through the same `normalize_relation_type` / `derive_relation_type_from_fact` fallback logic already applied to Anthropic-sourced edges, since both paths feed the same downstream pipeline and callers must not see provider-dependent relation-type formatting.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide a new `Extractor` implementation that communicates with an OpenAI-compatible `/v1/chat/completions` endpoint, covering both entity extraction and relationship-edge extraction (the two calls currently made against Anthropic's Messages API).
- **FR-002**: The local adapter's `extract()` implementation MUST map the existing structured-output contract (currently Anthropic `tools` / `tool_choice` / `tool_use`) onto an OpenAI-compatible equivalent (function-calling `tools`/`tool_choice`, or `response_format: json_object`), parsing results from `choices[].message.tool_calls` and/or `choices[].message.content` as appropriate to the mode used.
- **FR-003**: The local adapter's `classify_entities()` implementation MUST preserve the existing contract: one type label per input entity, in input order, with an empty string for unclassifiable entities, and MUST enforce the allowed-types constraint server-side exactly as the Anthropic path does today.
- **FR-004**: The engine MUST support selecting the local extraction endpoint via `--extractor-uds <path>` (Unix domain socket) and `--extractor-http <url>` CLI flags. These two flags MUST be mutually exclusive, mirroring `--embedder-uds` / `--embedder-http`.
- **FR-005**: Provider/endpoint selection MUST be resolved once in `main.rs` and passed into `AppState::from_env`, mirroring how embedder transport resolution already works.
- **FR-006**: Endpoint selection precedence, from highest to lowest, MUST be:
  1. An explicit `--extractor-uds` or `--extractor-http` CLI flag — always selects the local adapter, regardless of whether `ANTHROPIC_API_KEY` is set.
  2. `ANTHROPIC_API_KEY` set, with no explicit local flag — selects the Anthropic path, unchanged from today's behavior.
  3. No explicit local flag and no `ANTHROPIC_API_KEY` — auto-detect a local endpoint via a default-socket ladder (default UDS socket if the sidecar is up, else an extraction-endpoint env var override, else fail with the FR-011 error).
- **FR-007**: Auto-detection of a running local sidecar MUST NOT override or bypass a configured `ANTHROPIC_API_KEY` when no explicit local-endpoint flag is given (see FR-006, tier 2) — this is the backward-compatibility guarantee for existing users.
- **FR-008**: `LlmRouter`'s primary/fallback fields MUST be generalized from the concrete `AnthropicExtractor` type to an abstraction (e.g. `Arc<dyn Extractor>`) so that a local adapter instance can serve as primary or fallback, without changing the router's existing fallback-on-failure behavior (switch-once-then-latch-for-the-session semantics).
- **FR-009**: Telemetry/cost accounting MUST handle the local provider's `usage` object shape when present in its response, and MUST no-op gracefully (no error, no panic) when the local provider's response omits usage data entirely.
- **FR-010**: Extraction failures against the local endpoint (unreachable transport, non-2xx response, malformed or unparseable structured output) MUST surface as a typed error through the same `Extractor` error path already used by the Anthropic adapter.
- **FR-011**: When no extraction provider is available at all (no `ANTHROPIC_API_KEY`, no reachable local endpoint), the engine MUST produce one clear, actionable error message identifying the missing configuration, rather than a raw transport error.
- **FR-012**: Edges produced by the local adapter MUST pass through the same relation-type normalization (`normalize_relation_type`) and fact-derived fallback (`derive_relation_type_from_fact`) logic already applied to Anthropic-sourced edges.
- **FR-013**: The local adapter's output-truncation handling MUST mirror the Anthropic adapter's existing behavior: on detecting a truncated response (OpenAI-compatible `finish_reason: "length"`), double the token budget and retry once before giving up and reporting truncation via telemetry.
- **FR-014**: The system MUST restore the README's "fully local" language for extraction (the Principle 3 introduction and/or the macOS sidecar description) to state truthfully that extraction, like embedding, can run with no hosted API key when a local endpoint is reachable. This restoration is coordinated with, but distinct from, the #201 docs-correction PR — that PR made the docs truthful *now* (by softening/removing the claim); this issue makes the claim true again by shipping the capability, and should update the same README passages accordingly.
- **FR-015**: The existing `AnthropicExtractor` behavior (URL, headers, prompt-caching for Sonnet models, retry-on-429/529, budget-doubling on truncation, `LCG_EXTRACTION_LLM` model-name parsing) MUST remain unchanged for callers who continue to use it.

### Key Entities

Not applicable — this feature adds a new transport/provider implementation and a selection seam; it does not introduce new persisted data types. `ExtractedEntity`, `ExtractedEdge`, and `ExtractionResult` (already defined in `types.rs`) are unchanged — the local adapter produces the same shapes the Anthropic adapter does today.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With the local sidecar running and `ANTHROPIC_API_KEY` unset, `knowledge_process_chunk` on a chunk describing at least one entity and relationship succeeds and returns at least one typed entity and one typed edge.
- **SC-002**: With `ANTHROPIC_API_KEY` set and no explicit local-endpoint flag, ingestion behavior (success/failure outcome, entities/edges produced, model name, telemetry events emitted) for a fixed input is unchanged from pre-change behavior, even when a local sidecar is simultaneously reachable.
- **SC-003**: With neither `ANTHROPIC_API_KEY` nor a reachable local endpoint configured, the engine surfaces a single clear, actionable diagnostic message (not a bare connection error or panic).
- **SC-004**: An operator can direct extraction at a specific local endpoint via `--extractor-uds` or `--extractor-http`, verifiable via a startup log line reporting the resolved transport and endpoint (mirroring the embedder's existing `embedder: transport=..., endpoint=...` log line).
- **SC-005**: The OpenAI-compatible adapter's response parsing (`extract()` and `classify_entities()`) has automated test coverage for: a well-formed success response, a malformed/missing-structured-output response, and a truncated (`finish_reason: "length"`) response.
- **SC-006**: `cargo fmt --all --check`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all pass.
- **SC-007**: The README no longer implies extraction requires a hosted API key when a local endpoint is reachable — the "fully local" claim is true for both embedding and extraction.

## Assumptions

- The local extraction endpoint targets the same running CoreML sidecar process already used for embeddings (same default UDS socket path family), reusing its existing `/v1/chat/completions` route rather than requiring a second sidecar process.
- The exact environment-variable name(s) for a local extraction URL override (the tier-3 fallback in FR-006) and for a local extraction model name are implementation details left to the Research/Plan stages; this spec only requires that such an override exists and follows the same `lcg_env_var`-style dual-name convention used elsewhere in this codebase.
- Apple Foundation Models, reached via the sidecar's OpenAI-compatible route, can be prompted to produce structured output (function-calling or constrained JSON) reliable enough for entity/edge extraction. If empirical reliability turns out to be poor, that is a finding for the Research stage to surface, not something resolved by this spec.
- Cross-provider fallback beyond the two straightforward modes in FR-006 (e.g., Anthropic as primary with local as an automatic failover, or the reverse) is not required by this issue. The `LlmRouter` generalization (FR-008) makes such combinations structurally possible for future work, but wiring up that behavior is out of scope here.
- This issue targets macOS (where the CoreML sidecar exists); on other platforms, the local-endpoint path remains available via `--extractor-http` against any OpenAI-compatible server, but there is no local sidecar to auto-detect.

## Out of Scope

- Non-OpenAI-compatible local extraction protocols (e.g., raw llama.cpp APIs, Ollama's native API) — only the OpenAI-compatible `/v1/chat/completions` shape is covered.
- Any changes to the embedder subsystem — it already supports local operation and is unaffected by this issue.
- The #201 docs-correction PR itself (softening the README's premature "fully local" claim to be truthful in the interim) — that PR is separate and precedes this issue's restoration of the claim (FR-014).
- Building or shipping a new local-inference sidecar — this issue only adds a client (`Extractor` implementation) for the sidecar's existing `/v1/chat/completions` route.
- Automatic cross-provider failover (Anthropic ↔ local) beyond the explicit selection precedence in FR-006 (see Assumptions).

## Source References

- `crates/core/src/extractor.rs` — `Extractor` trait, `AnthropicExtractor` implementation, hard-coded Anthropic URL.
- `crates/core/src/llm_router.rs` — `LlmRouter`, currently typed to concrete `AnthropicExtractor` primary/fallback.
- `crates/core/src/app_state.rs` — `AppState::from_env`, currently unconditionally builds `LlmRouter::from_env`.
- `crates/core/src/embedder.rs` — `OaiEmbedder`, the target pattern for an OpenAI-compatible adapter with HTTP/UDS transports.
- `crates/service/src/main.rs` (`bootstrap_app_state`) and `crates/service/src/cli.rs` — the embedder's CLI-flag / default-socket-ladder resolution seam to mirror.
- Issue #201 — original report of the "fully local" claim being false for extraction.
- Issue #210 — prior dependency issue (resolved) that this issue was blocked on.
