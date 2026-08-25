# Feature Specification: Bearer-Token Authentication for the Embedder HTTP Transport

**Feature Branch**: `fabrik/issue-497`
**Created**: 2026-08-25
**Status**: Specified
**Input**: User description: "`OaiEmbedder` sends no authentication of any kind. `do_embed_http_raw` builds the request with `client.post(url).json(&body)` — no `bearer_auth`, no `Authorization` header, no API-key environment variable. [...] The README and `main.rs`'s error messages present a non-sidecar path as a first-class option: 'point `liminis-context-graph` at a different OpenAI-compatible embedder.' In practice that phrase only covers unauthenticated, local servers [...] It does not cover OpenAI itself, Voyage, Together, Cohere, or any hosted provider, because every one of them requires a bearer token. [...] Add an API key to the HTTP transport, following the existing `lcg_env_var` two-name convention used everywhere else in this codebase: `LCG_EMBEDDING_API_KEY`, falling back to `GRAPHITI_EMBEDDING_API_KEY`, then to `OPENAI_API_KEY` for the common case. When set, apply `.bearer_auth(key)` on the HTTP path in `do_embed_http_raw`. When unset, behave exactly as today [...]"

## Background

`crates/core/src/embedder.rs`'s `OaiEmbedder` talks to an "OpenAI-compatible" `POST /v1/embeddings` endpoint over either a Unix domain socket or HTTP, but the HTTP path (`do_embed_http_raw`) sends no credential of any kind — no `Authorization` header, no API-key environment variable, nothing. This is asymmetric with the rest of the crate: `crates/core/src/extractor.rs` (the LLM extraction path) already has full auth support (`ANTHROPIC_API_KEY`, `x-api-key` header).

The README and `main.rs`'s startup error messages describe pointing `liminis-context-graph` at "a different OpenAI-compatible embedder" as a first-class alternative to the bundled macOS sidecar. In practice, because there is no way to authenticate, that phrase only ever resolves to unauthenticated local servers (Ollama, llama.cpp, LM Studio, text-embeddings-inference, Infinity, vLLM). It does not cover OpenAI itself or any other hosted embedding provider, since every hosted provider requires a bearer token. For a service that refuses to start without a reachable embedder, this leaves non-macOS users, or macOS users who don't want to build the Swift sidecar from source, with no "I already have an API key, just use that" path — they must either build `native/local-inference/` from source (Xcode + a ~400 MB CoreML model download) or stand up and operate their own local inference server.

This issue closes that gap by adding optional Bearer-token authentication to the HTTP embedder transport, without changing behavior for anyone who doesn't set a key.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Start against a hosted, authenticated embedding endpoint (Priority: P1)

A user who already has an OpenAI API key sets `LCG_EMBEDDING_API_KEY` (or reuses their existing `OPENAI_API_KEY`) and points `liminis-context-graph` at `https://api.openai.com/v1/embeddings` via `--embedder-http` or `LCG_EMBEDDING_URL`. The service authenticates successfully, completes its startup probe, and serves ingestion end to end — with no need to build or run any local inference process.

**Why this priority**: This is the entire point of the issue — it is the only scenario that turns "point it at an OpenAI-compatible endpoint" from a misleading claim into an accurate one. Without it, hosted providers remain completely unreachable regardless of everything else in this feature.

**Independent Test**: Start the binary with `LCG_EMBEDDING_API_KEY` set and `--embedder-http` pointing at a stub HTTP server that requires a matching `Authorization: Bearer <key>` header to return a 200. Confirm startup succeeds and an ingest call (`knowledge_add_episode` or equivalent) completes.

**Acceptance Scenarios**:

1. **Given** `LCG_EMBEDDING_API_KEY` is set to a valid key and `--embedder-http` points at an endpoint that requires that exact key as a Bearer token, **When** the binary starts, **Then** the startup probe succeeds and the service becomes ready to serve ingest requests.
2. **Given** `LCG_EMBEDDING_API_KEY` is unset but `OPENAI_API_KEY` is set to a valid key, **When** the binary starts against an endpoint that accepts that key, **Then** the startup probe succeeds (the fallback resolves and is used).
3. **Given** a key is configured, **When** any HTTP embed request is sent (probe or regular `embed()` call), **Then** the outgoing request carries `Authorization: Bearer <key>`.

---

### User Story 2 - Existing unauthenticated local setups are unaffected (Priority: P1)

A user running a local unauthenticated embedder (e.g., the bundled macOS sidecar over UDS, or a local Ollama/llama.cpp server over HTTP) upgrades to this version and sets no new configuration. Behavior is unchanged: no credential is sent, no new failure mode is introduced.

**Why this priority**: This is the safety rail on User Story 1 — every existing deployment must keep working with zero configuration change, since local unauthenticated servers are still the primary supported path (especially the macOS sidecar).

**Independent Test**: Run the existing embedder test suite (and the existing local-sidecar / stub-server integration tests) with none of the three key env vars set. Confirm the outgoing HTTP request is byte-for-byte identical to its pre-change form (no `Authorization` header present at all), and that UDS transport is untouched.

**Acceptance Scenarios**:

1. **Given** none of `LCG_EMBEDDING_API_KEY`, `GRAPHITI_EMBEDDING_API_KEY`, or `OPENAI_API_KEY` is set, **When** an HTTP embed request is sent, **Then** no `Authorization` header is present on the request, matching current (pre-change) behavior exactly.
2. **Given** any or all of the three key env vars are set, **When** the transport in use is UDS (not HTTP), **Then** no credential lookup or header attachment occurs — UDS requests are identical to today's.

---

### User Story 3 - The key never leaks (Priority: P1)

An operator running with a real hosted-provider key wants assurance that the key cannot end up somewhere it shouldn't: process logs, telemetry, or recorded test artifacts.

**Why this priority**: A credential-leak regression is worse than the feature not existing at all. This is explicitly called out in the issue as a hard requirement, not a nice-to-have.

**Independent Test**: With a fake key configured and proven (via a stub server) to actually be sent on the wire, assert the key value never appears in: the `embedder: transport=…, endpoint=…, dim=…` startup log line, any other stderr/stdout output, any telemetry event, or any recorded test artifact (existing or new) covering the embedder path. Extend `crates/core/tests/cassette_record_replay.rs`'s existing "no credential material" pattern (currently extraction-only, see `recorded_cassette_contains_no_credential_material` at line 471) to cover the embedder.

**Acceptance Scenarios**:

1. **Given** a key is configured and used successfully, **When** the startup log line is emitted, **Then** it contains the transport, endpoint, and dim as today, and does not contain the key value.
2. **Given** the configured `--embedder-http`/`LCG_EMBEDDING_URL` endpoint URL itself embeds a credential (HTTP Basic-auth-style `user:pass@host` userinfo), **When** that endpoint is echoed in the startup log line or any error message, **Then** the userinfo component is redacted rather than printed verbatim.
3. **Given** any test in the suite exercises the authenticated HTTP embedder path, **When** its output/artifacts are inspected, **Then** none contain the literal key value or the `Authorization` header name paired with a real value.

---

### User Story 4 - Clear failure on bad credentials (Priority: P2)

A user misconfigures or omits their key against a provider that requires one. Instead of a confusing raw error dump, they get a message that tells them what's wrong and what env var to check.

**Why this priority**: Important for the "onboarding obstacle" framing in the issue, but the feature is usable without it (the raw error still surfaces the HTTP status). Ranked below the P1s because it's a UX polish requirement on top of a working auth mechanism.

**Independent Test**: Point the startup probe at a stub server that returns HTTP 401 for a missing/incorrect Bearer token. Confirm the startup failure message identifies this as an authentication problem and names the relevant env var, rather than being indistinguishable from an unrelated probe failure (e.g., an unexpected response shape).

**Acceptance Scenarios**:

1. **Given** the configured HTTP embedder endpoint returns HTTP 401 or 403 to the startup probe, **When** the binary evaluates the probe result, **Then** it fails startup with a message that identifies the failure as an authentication problem (e.g., references checking `LCG_EMBEDDING_API_KEY` / the provider's key) rather than a generic/raw error dump.
2. **Given** an authentication failure (401/403) at startup, **When** `LCG_EMBEDDING_DIM` is also set, **Then** startup still fails — the dim override (which exists to bypass "reachable but unexpected shape" failures) does not mask an authentication failure, since setting a dimension cannot fix a rejected credential.

---

### Edge Cases

- **Empty-string env var**: `LCG_EMBEDDING_API_KEY=""` (set but empty) is treated the same as unset — the lookup falls through to the next tier, and if all three resolve to empty/unset, no `Authorization` header is sent. This avoids sending a syntactically-present-but-empty Bearer token to a server that might reject it in a confusing way.
- **Multiple tiers set at once**: `LCG_EMBEDDING_API_KEY` wins over `GRAPHITI_EMBEDDING_API_KEY`, which wins over `OPENAI_API_KEY` — first non-empty value in that order is used, matching the existing `lcg_env_var` precedence convention plus one additional fallback tier.
- **`GRAPHITI_EMBEDDING_API_KEY` fallback usage**: logs the standard deprecation warning (per `lcg_env_var`'s existing behavior for every other `GRAPHITI_*` alias). `OPENAI_API_KEY` usage does **not** log a deprecation warning — it isn't a deprecated spelling of an LCG-specific variable, it's a distinct, widely-recognized standard variable name being read as a convenience default.
- **Key set, but transport is UDS**: no header, no lookup — UDS is a local socket and, per the issue, needs no credential regardless of what's configured.
- **Probe vs. steady-state embed calls**: both go through `do_embed_http_raw`, so both carry the header identically — there is no separate "authenticated probe" vs "unauthenticated embed" state.
- **Non-auth HTTP failures** (e.g., 500, malformed response body) must remain distinguishable from auth failures — the actionable "authentication failed" message (User Story 4) must not fire for unrelated failure modes.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The HTTP embedder transport (`do_embed_http_raw`, used by both `probe()` and regular `embed()` calls) MUST attach a resolved API key as `Authorization: Bearer <key>` on every outgoing request.
- **FR-002**: The key MUST be resolved via a three-tier lookup, in order: `LCG_EMBEDDING_API_KEY` → `GRAPHITI_EMBEDDING_API_KEY` → `OPENAI_API_KEY`. The first tier with a non-empty value wins. An empty string at any tier is treated as absent for that tier.
- **FR-003**: Using the `GRAPHITI_EMBEDDING_API_KEY` fallback tier MUST emit the same deprecation warning convention used by every other `GRAPHITI_*`/`LCG_*` pair in this codebase. Using the `OPENAI_API_KEY` fallback tier MUST NOT emit a deprecation warning.
- **FR-004**: When none of the three env vars resolves to a non-empty value, the HTTP request MUST be sent with no `Authorization` header, and MUST otherwise be identical to its pre-change form (byte-for-byte unchanged request).
- **FR-005**: The UDS transport (`do_embed_uds_raw`) MUST NOT perform any key lookup or attach any credential, regardless of whether the key env vars are set.
- **FR-006**: The key value MUST NOT appear in the startup log line (`embedder: transport=…, endpoint=…, dim=…`), in any other log output, or in any telemetry event.
- **FR-007**: If the configured embedder endpoint URL itself contains embedded Basic-auth-style userinfo (`user:pass@host`), that userinfo component MUST be redacted before the endpoint is echoed in the startup log line or in any error message.
- **FR-008**: An HTTP 401 or 403 response to the startup probe MUST cause startup to fail with a message that identifies the failure as an authentication problem (distinct wording from a generic/other probe failure), and this failure MUST NOT be bypassable via `LCG_EMBEDDING_DIM` (unlike the existing non-transport probe-failure override for unexpected response shapes).
- **FR-009**: Test coverage equivalent to `crates/core/tests/cassette_record_replay.rs`'s existing "no credential material" assertions for the extraction path MUST be added for the embedder's HTTP path — proving the key is actually sent over the wire (so the "never leaks" assertion isn't vacuous) and then asserting it appears in no log/output/artifact produced by the test.
- **FR-010**: The README and `docs/configuration.md` MUST be updated to (a) document the new `LCG_EMBEDDING_API_KEY` / `GRAPHITI_EMBEDDING_API_KEY` / `OPENAI_API_KEY` env vars in the existing environment-variables table and variable count, and (b) stop describing "OpenAI-compatible endpoint" as an unqualified catch-all — state plainly that hosted providers are now reachable if they accept a Bearer token against the same `{input, model}` request / `{data: [{embedding}]}` response wire shape `OaiEmbedder` already speaks (OpenAI's own `/v1/embeddings` is the concrete, verified case), and that a provider whose request/response shape differs is not made compatible by this change alone.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With `LCG_EMBEDDING_API_KEY` set to a valid OpenAI API key and the embedder pointed at `https://api.openai.com/v1/embeddings`, `liminis-context-graph` starts successfully and completes an end-to-end ingest (episode add through to a stored, embedded entity).
- **SC-002**: With all three key env vars unset, request bytes sent to a local unauthenticated HTTP embedder are identical to pre-change behavior — no test in the existing embedder suite changes its expected request assertions.
- **SC-003**: No test, log line, telemetry event, or recorded test artifact in the repository contains a real or fake API key value used in any test, verified by an automated test assertion (not manual inspection).
- **SC-004**: A startup attempt against an endpoint that rejects the configured (or missing) credential with HTTP 401/403 produces a distinguishable, actionable error message, verified by an automated test.
- **SC-005**: `docs/configuration.md`'s environment-variable table and count, and the README's embedder description, both reflect the new capability and the new env vars.

## Assumptions

- The empty-string-treated-as-unset rule (Edge Cases, FR-002) is a reasonable default consistent with how a misconfigured/blank env var should behave, though it is not explicitly stated in the source issue; Research/Plan may revisit if there's a reason to treat an explicitly-set empty value differently.
- Treating an authentication failure (401/403) as always-fatal at startup — not overridable by `LCG_EMBEDDING_DIM` — is a product-level judgment call made here rather than left open: the dim override exists for "reachable but unexpected response shape," and a rejected credential is a different failure class that a dimension override cannot meaningfully paper over.
- URL-credential redaction (FR-007) is scoped to the standard Basic-auth userinfo component (`user:pass@host`) only. Ad hoc "API key in query string" conventions vary too much by provider to redact generically within this issue's scope; if a specific provider's URL convention needs redaction, that's follow-up work.
- Documentation (FR-010) intentionally avoids naming specific third-party providers (Voyage, Together, Cohere, etc.) as confirmed-compatible, since their exact request/response wire shape has not been verified against `OaiEmbedder`'s hardcoded `{input, model}` / `{data: [{embedding}]}` contract — only Bearer-token auth is being added, not adapter flexibility for divergent wire formats. OpenAI itself is used as the one concrete, verified example.
- This feature only adds a static, pre-resolved Bearer token read once at embedder construction time (mirroring how `ANTHROPIC_API_KEY` is read once in `extractor.rs`) — not per-request key rotation or refresh.

## Out of Scope

- Non-Bearer auth schemes (custom headers, query-string API keys, mTLS, OAuth token refresh) used by some providers (e.g., Azure OpenAI's `api-key` header convention).
- Any change to the UDS transport.
- Verifying or documenting wire-format compatibility with specific named third-party providers beyond OpenAI itself.
- Secrets-manager / keychain integration for storing the key — it remains a plain environment variable, consistent with `ANTHROPIC_API_KEY`'s existing handling.
- Any change to `crates/core/src/extractor.rs`'s existing, already-implemented auth path.

## Source References

- `crates/core/src/embedder.rs` — `OaiEmbedder`, `do_embed_http_raw`, `do_embed_uds_raw`, `from_env`, `transport_info`, `probe`
- `crates/core/src/extractor.rs` — existing `ANTHROPIC_API_KEY` / `x-api-key` auth precedent (lines ~130, ~200, ~680, ~788, ~2257)
- `crates/core/src/env.rs` — `lcg_env_var` two-name convention
- `crates/service/src/main.rs` (~lines 170-310) — embedder transport resolution, startup probe, `is_transport_error` usage, startup log line
- `crates/core/tests/cassette_record_replay.rs:471-518` — `recorded_cassette_contains_no_credential_material`, the pattern to extend for the embedder
- `docs/configuration.md` — environment-variable table, "Embedder sidecar" section
- `README.md` — "OpenAI-compatible" embedder description
