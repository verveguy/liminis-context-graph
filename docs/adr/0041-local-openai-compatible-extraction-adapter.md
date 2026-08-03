# ADR-0041: Local/OpenAI-Compatible Extraction Adapter

**Status**: Accepted
**Date**: 2026-07-25
**Issues**: #212 (this change); motivated by #201 ("fully local" claim false for extraction);
depends on #210 (resolved)

## Context

The documented "run fully local models" story was not achievable for **entity/relationship
extraction**. The shipped extractor (`AnthropicExtractor`) hard-codes
`https://api.anthropic.com/v1/messages` as its endpoint, and `LlmRouter` — the only thing
`AppState::from_env` ever constructed for `AppState.extractor` — was typed directly to
`AnthropicExtractor`, with no seam for another implementation. Any `knowledge_process_chunk` /
`knowledge_add_episode` call therefore 401s unless `ANTHROPIC_API_KEY` is set, even though the
macOS CoreML sidecar already serves an OpenAI-compatible `/v1/chat/completions` route (backed by
Apple Foundation Models) on the same socket already used for `/v1/embeddings` — nothing in the
engine ever called it for extraction.

The embedder subsystem (`OaiEmbedder`, ADR-0016) already demonstrates the target end-to-end
pattern: HTTP/UDS transport, resolved via `--embedder-uds`/`--embedder-http` CLI flags with a
default-socket ladder, resolved once in `main.rs` and threaded into `AppState::from_env`. This
issue is the local-extraction counterpart.

## Decisions

### 1. `OaiExtractor` uses `response_format: json_object` only — no function-calling mode

The bundled sidecar's `ChatCompletionRequest`/`ChatCompletionResponse` (Swift,
`native/local-inference/Sources/LocalInference/Models.swift`) have no `tools`/`tool_choice`
request fields and no `tool_calls` response field — the only structured-output signal it
understands is `response_format: {"type": "json_object"}`, which appends a "respond with valid
JSON only" instruction server-side and best-effort brace-extracts the model's raw text.

Rather than building a second, untested function-calling code path for hypothetical
`--extractor-http` targets that do support it (vLLM, Ollama, LM Studio, hosted OpenAI-compatible
APIs), `OaiExtractor` implements **one mode** everywhere: `response_format: json_object` plus an
explicit JSON-shape instruction appended to the shared, provider-agnostic system prompts
(`prompts::entity_system_prompt`/`edge_system_prompt` — unchanged, reused verbatim), parsed
defensively via the same `extract_json_block()` helper `AnthropicExtractor`'s
`classify_entities`/`classify_relations` already use for their own non-tool-use text responses.
This is a materially weaker reliability guarantee than the Anthropic path's schema-enforced
`tool_use` (ADR-0010) — no schema validation at the transport layer, only defensive parsing — and
is an explicit, accepted trade-off for this issue's scope, not an oversight.

`classify_entities`/`classify_relations` wrap their array output in a JSON object
(`{"types": [...]}`) rather than a bare array, since `response_format: json_object` requires a
top-level JSON object.

### 2. No live reachability probe gates extraction at startup

Unlike `OaiEmbedder::probe()`, which the embedder startup path always calls (to auto-detect
embedding dimension — there is no analogous shape to detect for extraction), `OaiExtractor`
performs no blocking call at startup. Foundation Models' first real generation call can be slow
(on-device model warm-up), so an eager blocking chat-completion at startup risked a materially
worse latency regression than the embedder's cheap embed-probe, for no benefit — the pre-existing
Anthropic path (`AnthropicExtractor::from_env`) also performs zero startup verification.

Genuine unreachability surfaces at call time through the normal `Extractor` error path (FR-010),
distinct from "no provider configured at all" (FR-011), which was originally a startup-time fatal
error — superseded by [ADR-0331](0331-lazy-extraction-provider-validation.md), which defers that
check to first use as well; the precedence order below is unchanged.

### 3. Endpoint/provider selection precedence (FR-006), resolved once in `main.rs` — no socket
   auto-detection tier

`bootstrap_app_state` resolves one of three outcomes, highest priority first:

1. An explicit `--extractor-uds <path>` or `--extractor-http <url>` CLI flag — always selects
   `OaiExtractor`, regardless of whether `ANTHROPIC_API_KEY` is set. Validated at startup the same
   way the embedder's flags are (UDS: socket file must exist; HTTP: URL must have a valid
   `http(s)://` scheme and non-empty host) — fatal at startup if invalid, no live probe.
2. `ANTHROPIC_API_KEY` set, no explicit local flag — selects `LlmRouter::from_env` (the
   pre-existing Anthropic path), byte-for-byte unchanged. This is the load-bearing
   backward-compatibility guarantee (FR-007): a reachable local sidecar never silently steals
   traffic from an already-configured hosted key.
3. Neither of the above — `LCG_EXTRACTION_URL` env override if set, else no provider is
   configured. **Superseded** by [ADR-0331](0331-lazy-extraction-provider-validation.md): at the
   time this ADR was written, that case was a fatal, actionable startup error identifying the
   missing configuration (FR-011); it is now deferred to the first extraction-dependent call,
   which returns the same actionable message instead. The precedence order in items 1–3 is
   otherwise unchanged.

Deliberately **not** mirrored from the embedder: there is no "does the default UDS socket exist"
auto-detection tier. The embedder's tier-3 ladder auto-selects `/tmp/liminis-inference.sock`
whenever it's present, on the theory that a live sidecar is unambiguously good news for embedding
quality. That reasoning does not transfer to extraction — the same sidecar's
`/v1/chat/completions` route (Apple Foundation Models) was evaluated in prior work and found
inadequate for entity/relationship extraction (see Consequences). A bare socket-existence check
would silently select that specific backend the moment no `ANTHROPIC_API_KEY` is set, for every
user who happens to have the sidecar running for embeddings — trading the "requires a hosted key"
problem this issue set out to fix for an equally misleading "works with no setup" one. Selecting
that same sidecar for extraction remains fully possible — pass `--extractor-uds
/tmp/liminis-inference.sock` explicitly, or set `LCG_EXTRACTION_URL` — the change is only that
tier 3 requires an explicit signal from the operator rather than inferring one from process state.

A `extractor: provider=..., transport=..., endpoint=...` startup log line reports the resolved
choice, mirroring the embedder's existing `embedder: transport=..., endpoint=..., dim=...` line.

### 4. `LlmRouter` and `AppState` generalize to `Arc<dyn Extractor>`

`LlmRouter.primary`/`.fallback` change from concrete `AnthropicExtractor` to
`Arc<dyn Extractor>`/`Option<Arc<dyn Extractor>>`, so a local adapter instance can serve as
`AppState.extractor`'s sole implementation, or (structurally, for future work — not wired up by
this issue) as a `LlmRouter` primary/fallback slot. `LlmRouter::new` takes explicit
`primary_model_name`/`fallback_model_name: String` parameters instead of deriving them via a new
trait method — callers already know the model name at construction time, and not every
`Extractor` implementation (test doubles included) has a meaningful model name to report.
`primary_failed`'s switch-once-then-latch-for-the-session fallback semantics are provider-agnostic
and unchanged.

`AppState::from_env` gains an `extractor: Arc<dyn Extractor>` parameter (mirroring the existing
`embedder` parameter exactly) and no longer constructs `LlmRouter::from_env` internally —
provider/transport selection moves entirely into `bootstrap_app_state` (`main.rs`), matching how
embedder selection already works.

### 5. Cross-provider fallback stays out of scope

`bootstrap_app_state` only ever constructs a same-provider extractor: `LlmRouter::from_env`'s own
`LCG_EXTRACTION_LLM`-parsed `primary:fallback` pair (Anthropic-only, unchanged), or a bare
`OaiExtractor` with no fallback. No combination mixes an Anthropic primary with a local fallback
or vice versa — the `Arc<dyn Extractor>` generalization makes such combinations structurally
possible for future work, but wiring them up is explicitly out of scope here.

## Consequences

- With the CoreML sidecar running and `ANTHROPIC_API_KEY` unset, extraction now works with no
  hosted API key — the "fully local" README claim is restored for extraction, matching embedding.
- Existing hosted-key users see zero behavior change: same endpoint, same model parsing, same
  telemetry, same errors — a running local sidecar never redirects their traffic.
- Local-extraction reliability is bounded by prompt-plus-defensive-parsing, not schema-enforced
  tool use. `SC-005`'s automated coverage is necessarily synthetic-fixture-based (Rust-side parser
  tests only, well-formed/malformed/truncated); genuine end-to-end extraction quality against the
  real sidecar can only be confirmed by manual testing on macOS with Apple Intelligence enabled,
  not by this repo's standard CI.
- The OpenAI-compatible `usage` object (`prompt_tokens`/`completion_tokens`) is mapped to the same
  `TelemetryEvent::TokenUsage` shape as Anthropic's `input_tokens`/`output_tokens`; local model
  names are absent from the Anthropic pricing table, so `cost_for_usage` naturally returns `None`
  with no special-casing (FR-009).
- `finish_reason: "length"` triggers the same budget-doubling-retry-once behavior
  `AnthropicExtractor` already implements for `stop_reason: "max_tokens"` (FR-013). The bundled
  sidecar always returns `finish_reason: "stop"` and ignores `max_tokens` entirely, so this path
  cannot be exercised against the real sidecar today — it exists for real OpenAI-compatible
  servers (vLLM, Ollama, LM Studio, ...) reached via `--extractor-http`, which do honor
  `max_tokens` and do emit `"length"`.
- **Known limitation, and the reason Decision 3 has no socket-auto-detection tier: the bundled
  sidecar's backend (Apple Foundation Models) has prior evidence of inadequate extraction
  quality.** See [`docs/extraction-quality-evaluation.md`](../extraction-quality-evaluation.md) for
  the full evaluation, methodology, and model rankings this finding is based on — including the
  quality-verified local alternative (`qwen3.6-27b`) that does clear the quality bar. This ADR's
  `Extractor` trait implementation and CLI/precedence-selection mechanism are unaffected by that
  finding — the same `OaiExtractor` adapter works against any OpenAI-compatible endpoint, including
  quality-verified local models, via `--extractor-http`/`--extractor-uds`. What the finding changed
  in this design is Decision 3: an operator can still point `--extractor-uds` at the bundled
  sidecar and get exactly the behavior a bare auto-detection tier would have produced, but the
  engine never makes that specific choice on the operator's behalf. Resolving the quality gap
  itself (a stronger bundled default, or swapping in a higher-quality local model) is tracked by
  #228, not this ADR.

## Related

- ADR-0016: OpenAI-compatible embedding contract over UDS; hyper for UDS transport — the pattern
  `OaiExtractor`'s HTTP/UDS transport mirrors directly.
- ADR-0010: Migrate `do_extract` to `tool_use` structured output — the schema-enforced mode this
  issue's local adapter explicitly cannot use against the bundled sidecar (see Decision 1).
- `crates/core/src/extractor.rs`: `OaiExtractor`, `parse_oai_entity_response`/
  `parse_oai_edge_response` and their unit tests (SC-005).
- `crates/core/src/llm_router.rs`: `LlmRouter`'s `Arc<dyn Extractor>` generalization.
- `crates/service/src/cli.rs`: `ExtractorFlag`, `--extractor-uds`/`--extractor-http` parsing and
  mutual-exclusivity validation.
- `crates/service/src/main.rs` (`bootstrap_app_state`): the FR-006 precedence ladder.
