# ADR-0010: Migrate do_extract to tool_use structured output

**Status**: Accepted
**Date**: 2026-05-24

## Context

`AnthropicExtractor::do_extract` built a free-form JSON-list prompt and parsed Sonnet's response with `serde_json::from_str`. When the response was truncated for any reason (token budget, network cut, model misbehavior), the parse failed and the chunk was silently dropped. Live evidence showed two chunks from one file lost per ingestion run, with errors like:

```
RPC error -32000: JSON error: EOF while parsing a list at line 89 column 5
```

Raising `max_tokens` reduced the frequency but could not eliminate the problem: entity-rich chunks can always exceed any fixed ceiling, and a truncated response is indistinguishable from a malformed one until parse time.

## Decision

Migrate `do_extract` to use Anthropic's `tool_use` structured output with a forced `tool_choice`:

1. **Forced tool call**: Include `"tool_choice": {"type": "tool", "name": "extract"}` in every request. The model cannot respond with free-form text.

2. **Hand-coded input schema**: A `serde_json::json!` literal describes `ExtractionResult` (entities array + edges array) as a JSON Schema object. No `schemars` dependency is added.

3. **Explicit budget overflow detection**: When the API returns `stop_reason: "max_tokens"`, the response is identified as `ToolOutcome::BudgetExhausted` before any content parsing. This is a detectable, structured failure — not a silent truncation.

4. **Single budget-doubling retry**: On `BudgetExhausted`, double `max_tokens` (8192 → 16384) and retry exactly once. If the retry also overflows, return an error.

5. **`ExtractionTruncated` telemetry**: When a retry is triggered, emit `ExtractionTruncated` with `retry_succeeded` reflecting the retry outcome. This gives operators a signal to monitor for undersized budgets.

6. **`parse_tool_response` pure function**: The response decision tree is extracted into a pure function taking a `Value` and returning `ToolOutcome`. This is directly unit-testable without an HTTP mock library.

## Consequences

**Positive**:
- Chunk drops from budget exhaustion are now explicit, retried, and observable in telemetry rather than silent.
- The tool_use path guarantees a schema-valid response or a detectable error — never silently malformed JSON.
- `parse_tool_response` is a pure function, testable without any HTTP infrastructure.

**Negative / follow-up needed**:
- `do_classify_entities` retains the legacy free-form JSON path with the same truncation risk. A follow-up issue should migrate it.
- `extract_json_block` is now only called from `do_classify_entities`. It is not dead code (the compiler won't warn), but it is a candidate for removal once `do_classify_entities` is migrated.
- Tool definitions are sent in the `tools` array, which is not cacheable under `prompt-caching-2024-07-31`. System-text cache hit rates are unaffected.
- The forced `tool_choice` is an irreversible constraint on the Anthropic Messages API request shape for this path. Future contributors adding extraction variants must include `tool_choice` or the model may respond with free-form text.

## Alternatives Considered

- **Raise `max_tokens` ceiling only**: Reduces frequency but does not eliminate the problem. Entity-rich chunks can always exceed any fixed ceiling.
- **Chunk splitting**: Addresses the root cause differently but is the caller's responsibility and orthogonal to this fix.
- **`schemars` derive**: Generates JSON Schema from Rust types automatically. Rejected because the schema is small (6 fields across 2 types) and the added compile-time dependency outweighs the benefit.
- **Mock HTTP library for tests**: `httpmock` or `wiremock` would enable end-to-end HTTP test coverage. Rejected to avoid adding dev-dependencies; the `parse_tool_response` pure-function approach provides equivalent behavioral coverage.
