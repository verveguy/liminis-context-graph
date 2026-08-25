# ADR-0497: Bearer-Token Authentication for the Embedder HTTP Transport

**Date**: 2026-08-25
**Status**: Accepted

## Context

`OaiEmbedder`'s HTTP transport (`do_embed_http_raw` in `crates/core/src/embedder.rs`) sent no
credential of any kind — no `Authorization` header, no API-key environment variable. The README
and `docs/configuration.md` described pointing `liminis-context-graph` at "a different
OpenAI-compatible embedder" as a first-class alternative to the bundled macOS sidecar, but
without auth support that phrase only ever resolved to unauthenticated local servers (Ollama,
llama.cpp, LM Studio, text-embeddings-inference, Infinity, vLLM) — never OpenAI itself or any
other hosted provider, since every one of them requires a bearer token. This left non-macOS
users, or macOS users unwilling to build the Swift sidecar from source, with no "I already have
an API key, just use that" path. See issue #497.

`extractor.rs`'s `AnthropicExtractor` already reads `ANTHROPIC_API_KEY` once at construction and
attaches it via a custom `x-api-key` header on every request — the precedent for "read once at
construction, attach per request" this feature follows for the embedder, using `reqwest`'s
`.bearer_auth()` instead since OpenAI's convention is `Authorization: Bearer <key>`, not a custom
header.

## Decision

### Three-tier key resolution, with one warning-free convenience tier

`resolve_embedding_api_key()` checks, in order: `LCG_EMBEDDING_API_KEY` → deprecated alias
`GRAPHITI_EMBEDDING_API_KEY` (emits the standard `DEPRECATED: env var ...` warning on use) →
`OPENAI_API_KEY` (**no** deprecation warning, since it isn't a legacy spelling of an LCG-specific
variable, it's a distinct, widely-recognized standard name being read as a convenience default).
An empty string at any tier is treated as absent for that tier, so `LCG_EMBEDDING_API_KEY=""`
falls through to the next tier rather than sending a syntactically-present-but-empty Bearer token.

The `OPENAI_API_KEY` tier is not gated on the target endpoint being OpenAI's own API — per FR-002
and Acceptance Scenario 2, it must resolve and be used against *any* configured
`--embedder-http`/`LCG_EMBEDDING_URL` endpoint, since that's the whole point of the convenience
tier (a key exported for other OpenAI tooling reused against a self-hosted or third-party
"OpenAI-compatible" server). That is deliberately a wider blast radius than
`ANTHROPIC_API_KEY`, which only ever talks to a fixed Anthropic endpoint — this key can be sent
to an operator-chosen URL. To keep that non-silent, this tier logs a one-line informational
notice (distinct from the `GRAPHITI_*` deprecation warning — it doesn't ask the operator to
rename anything) naming the redacted endpoint the key is about to be sent to, so an operator who
has `OPENAI_API_KEY` exported for unrelated purposes gets a visible signal rather than a silent
credential reuse.

This is bespoke logic, not a change to the existing `lcg_env_var(new, old)` two-tier helper:
`lcg_env_var` is used at ~15 other call sites, treats `""` as a valid value, and has no way to
add a third, warning-free tier. Changing its semantics for this one feature's need would be a
wide, unrelated blast radius. If a future variable needs the same "N-tier, empty-as-absent, one
tier warning-free" shape, this function's structure — not `lcg_env_var`'s — is the precedent to
follow or deliberately deviate from.

### A 401/403 at startup is always fatal, never overridable by `LCG_EMBEDDING_DIM`

`LCG_EMBEDDING_DIM` exists to bypass a probe that reached the embedder but got an unexpected
response shape — a legitimate "I know what dimension this model produces, trust me" override.
An authentication failure is a different failure class: no dimension value can make a rejected
credential valid. `is_auth_error()` (mirroring the existing `is_transport_error()` pattern)
detects HTTP 401/403 via `reqwest::Error::status()` and is checked in `main.rs`'s
`bootstrap_app_state` *before* the existing dim-override branch, so an auth failure can never be
masked into a false "success."

### URL-userinfo redaction is scoped to Basic-auth `user:pass@host` only

`redact_url_userinfo()` strips the standard Basic-auth userinfo component from a URL before it's
echoed in the startup log line (via `transport_info()`) or in an error message, and returns the
original userinfo substring so `main.rs` can scrub the same text out of a wrapped
`reqwest::Error`'s `Display` output too (which can independently embed the raw URL). Ad hoc "API
key in query string" conventions vary too much by provider to redact generically within this
feature's scope; a provider-specific convention needing redaction is follow-up work, not handled
here. The key itself is never part of the URL for this transport, so this redaction is purely a
defense against an operator embedding credentials in the URL itself, not a second path for the
Bearer key to leak.

### Builder method, not a constructor signature change

`OaiEmbedder::new_http`/`new_uds` are called positionally in 15+ places (tests, examples,
`main.rs`) with no key argument. `with_api_key(self, Option<String>) -> Self` is a builder method
chained only at the two `main.rs` construction sites (the throwaway probe embedder and the final
steady-state embedder — both must resolve to the same key, or the probe could succeed
unauthenticated while every real call 401s) and inside `from_env()`. It is a no-op on the UDS
variant: `new_uds` never accepts a key parameter, so it is structurally impossible for a UDS
request to carry one — `with_api_key`'s no-op behavior is an ergonomic convenience for a shared
call site, not the FR-005 compliance mechanism itself.

## Consequences

- Hosted embedding providers that accept a Bearer token against `OaiEmbedder`'s existing
  `{input, model}` / `{data: [{embedding}]}` wire shape — OpenAI's own `/v1/embeddings` is the
  verified case — are now reachable via `--embedder-http`/`LCG_EMBEDDING_URL` with no local
  inference process required.
- Every existing unauthenticated deployment (the macOS sidecar over UDS, a local unauthenticated
  HTTP server) is unaffected: no key configured means no `Authorization` header, byte-for-byte
  identical to the pre-change request.
- A future fourth auth-relevant tier (e.g. a different hosted provider's own standard env var
  name) should follow this ADR's "N-tier, empty-as-absent, warning only on the deprecated-alias
  tier" shape rather than composing `lcg_env_var`.
- Non-Bearer auth schemes (Azure OpenAI's `api-key` header, query-string API keys, mTLS, OAuth
  token refresh) remain unsupported — this ADR covers Bearer-token auth only, not general auth
  scheme flexibility.
