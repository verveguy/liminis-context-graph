# Feature Specification: Swift sidecar: select embeddings / completions / both, so completions-only costs no model setup

**Feature Branch**: `fabrik/issue-501`
**Created**: 2026-08-25
**Status**: Specified
**Input**: User description: "Swift sidecar: select embeddings / completions / both, so completions-only costs no model setup"

## Background

The Swift sidecar serves two independent capabilities — CoreML embeddings and Apple
Foundation Models chat completions — but they cannot be selected independently. The
embedding model is mandatory at startup regardless of what you intend to serve
(`Sources/LocalInference/main.swift:44`):

```swift
guard FileManager.default.fileExists(atPath: modelPath) else {
    fputs("Error: CoreML embedding model not found at \(modelPath)\n", stderr)
    exit(1)
}
```

So a user who wants **only** chat completions must still run `prepare-embedding-assets.sh`:
a ~400 MB BGE download from HuggingFace, a 2–5 minute CoreML conversion, a first-launch
`.mlmodelc` compile, and a `uv`/Python toolchain — all to serve an endpoint that needs none
of it.

The two modes are asymmetric, and the asymmetry favours this change:

| mode | model download | conversion | first-launch compile | build deps |
|---|---|---|---|---|
| completions-only | **none** | **none** | **none** | none beyond Hummingbird |
| embeddings-only | ~400 MB (BGE) | 2–5 min | yes | swift-transformers + uv/Python |
| both | ~400 MB | 2–5 min | yes | same |

Completions cost nothing to provision because there is **no model to ship**:
`FoundationModelsAdapter` uses `SystemLanguageModel.default` from `import FoundationModels`,
an OS framework on macOS 26. Apple manages the weights; we download and convert nothing.

The two halves also already disagree on failure philosophy. Foundation Models being
unavailable is **non-fatal** — the sidecar starts and `/v1/chat/completions` returns 503
while embeddings keep working (`main.swift:56-65`). A missing embedding model is **fatal**.
That asymmetry is what makes completions-only impossible today.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Run completions-only with no embedding setup (Priority: P1)

A developer who only wants Apple Foundation Models chat completions starts the sidecar
with `LOCAL_INFERENCE_MODE=completions` and never runs `prepare-embedding-assets.sh`,
never downloads the BGE model, and never installs the `uv`/Python toolchain.

**Why this priority**: This is the entire motivation for the issue — it is the cost
asymmetry table above, made real. Without it, completions-only remains impossible.

**Independent Test**: Start the sidecar with `LOCAL_INFERENCE_MODE=completions` in an
environment with **no** `.mlpackage` present anywhere and no embedding-related
environment variables set. Confirm it starts successfully and serves
`/v1/chat/completions`.

**Acceptance Scenarios**:

1. **Given** `LOCAL_INFERENCE_MODE=completions` and no `.mlpackage` present anywhere,
   **When** the sidecar starts, **Then** it starts successfully (no fatal exit) and
   serves `/v1/chat/completions`.
2. **Given** `LOCAL_INFERENCE_MODE=completions`, **When** a client calls the embeddings
   endpoint, **Then** the response indicates the endpoint is disabled by configuration,
   using a response distinguishable from the existing "Apple Intelligence unavailable"
   503 degraded-mode response.
3. **Given** `LOCAL_INFERENCE_MODE=completions`, **When** the sidecar starts, **Then** no
   embedding-actor initialisation occurs: no tokenizer load, no `.mlmodelc` compile, and
   no `LOCAL_INFERENCE_HF_CACHE` requirement.

---

### User Story 2 - Run embeddings-only, preserving today's fail-fast guarantee (Priority: P2)

A developer who only wants embeddings starts the sidecar with
`LOCAL_INFERENCE_MODE=embeddings`. The existing hard-failure behaviour for a missing
embedding model is preserved exactly, and the completions endpoint is not exposed.

**Why this priority**: The issue is explicit that the REQ-09 intent — hard-failing when
embeddings are requested but unavailable, to forbid silent fallback to `NLEmbedding` —
must be preserved exactly. This story is what proves that preservation.

**Independent Test**: Start the sidecar with `LOCAL_INFERENCE_MODE=embeddings` and no
`.mlpackage` present. Confirm it exits non-zero with today's message. Then start it with
the model present and confirm `/v1/chat/completions` is not served.

**Acceptance Scenarios**:

1. **Given** `LOCAL_INFERENCE_MODE=embeddings` and a missing `.mlpackage`, **When** the
   sidecar starts, **Then** it exits non-zero with today's existing error message,
   unchanged.
2. **Given** `LOCAL_INFERENCE_MODE=embeddings`, **When** a client calls
   `/v1/chat/completions`, **Then** the route is not served (not merely degraded).

---

### User Story 3 - Default and `both` behaviour is unchanged (Priority: P1)

An existing deployment that does not set `LOCAL_INFERENCE_MODE` continues to behave
exactly as it does today: both endpoints active, same startup checks, same failure
philosophy for each.

**Why this priority**: Every current deployment relies on this. A regression here
breaks production installs, not just the new completions-only path.

**Independent Test**: Start the sidecar with `LOCAL_INFERENCE_MODE` unset and confirm
behaviour (startup checks, routes registered, error handling) is byte-identical to the
pre-change sidecar.

**Acceptance Scenarios**:

1. **Given** `LOCAL_INFERENCE_MODE` is unset, **When** the sidecar starts, **Then**
   behaviour is identical to today: both routes registered, embedding model required at
   startup, Foundation Models unavailability remains non-fatal.

---

### Edge Cases

- `LOCAL_INFERENCE_MODE` is set to a value other than `embeddings`, `completions`, or
  `both`.
- `LOCAL_INFERENCE_MODE=both` but only one of the two backing capabilities is actually
  available at runtime (e.g. embedding model present, Foundation Models unavailable, or
  vice versa) — existing per-capability failure philosophy applies unchanged.
- A client calls a disabled endpoint (mode does not include that capability) versus a
  client calls an enabled-but-degraded endpoint (mode includes the capability, but it is
  currently unavailable) — these two cases must be distinguishable in the response.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The sidecar MUST read a `LOCAL_INFERENCE_MODE` environment variable with
  accepted values `embeddings`, `completions`, and `both`, defaulting to `both` when
  unset.
- **FR-002**: The embedding-model-presence guard (`main.swift:44`) MUST be enforced only
  when the selected mode includes embeddings (`embeddings` or `both`). The guard's
  existing behaviour — hard-failing with today's message when embeddings are requested
  but the model is missing — MUST be preserved unchanged; only the condition under which
  it runs changes.
- **FR-003**: `AppRouter` MUST register only the routes for capabilities enabled by the
  selected mode. A request to an endpoint disabled by configuration MUST return a
  response distinguishable from the existing degraded-but-enabled response (e.g. the
  current "Apple Intelligence unavailable" 503) — a caller must be able to tell "not
  enabled in this configuration" apart from "enabled but currently unavailable."
- **FR-004**: In `completions`-only mode, the sidecar MUST skip embedding-actor
  initialisation entirely: no tokenizer load, no `.mlmodelc` compile step, and no
  requirement for `LOCAL_INFERENCE_HF_CACHE` to be set.
- **FR-005**: `prepare-embedding-assets.sh` (or equivalent setup guidance) MUST be
  documented as required only for `embeddings` and `both` modes. Documentation MUST
  state plainly that `completions`-only mode requires no setup step at all.
- **FR-006**: With `LOCAL_INFERENCE_MODE` unset, sidecar behaviour MUST be byte-identical
  to current (pre-change) behaviour: both endpoints active, embedding model required at
  startup, Foundation Models unavailability handled as a non-fatal 503.
- **FR-007**: Documentation introduced or updated for `LOCAL_INFERENCE_MODE` (setup docs,
  configuration docs) MUST state explicitly that Foundation-Models-backed completions are
  not a drop-in substitute for the extraction backend, and MUST NOT suggest wiring
  `--extractor-uds` at the completions endpoint. This documents, in the context of the
  new mode flag, an architectural decision that already exists independently of this
  issue (ADR-0041: extraction has no default-socket auto-detection, precisely so a
  sidecar running for one purpose is never silently promoted to another).

### Key Entities

- **`LOCAL_INFERENCE_MODE`**: New environment-variable-driven configuration selecting
  which capability set the sidecar serves (`embeddings` | `completions` | `both`).
- **`AppRouter`**: Registers HTTP routes; must become mode-aware so disabled capabilities
  register no route at all.
- **Embedding actor / `EmbeddingsHandler`**: CoreML-backed embedding capability; its
  initialisation (tokenizer, `.mlmodelc` compile, HF cache) must become conditional on
  mode.
- **`FoundationModelsAdapter`**: Apple Foundation Models-backed completions capability;
  unaffected in its own failure behaviour by this change, only in whether its route is
  registered.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A sidecar started with `LOCAL_INFERENCE_MODE=completions` in an environment
  with no `.mlpackage` present anywhere starts successfully and serves
  `/v1/chat/completions`.
- **SC-002**: A sidecar started with `LOCAL_INFERENCE_MODE=embeddings` and a missing
  `.mlpackage` exits non-zero with today's existing error message, and does not serve
  `/v1/chat/completions`.
- **SC-003**: A sidecar started with `LOCAL_INFERENCE_MODE` unset behaves identically, in
  every observable respect, to the pre-change sidecar.
- **SC-004**: Requests to a disabled-by-configuration endpoint receive a response
  distinguishable (status code and/or message) from requests to an enabled-but-degraded
  endpoint, verified by test coverage.

## Assumptions

- Apple Foundation Models framework availability and its existing non-fatal degraded
  behaviour (503 when Apple Intelligence is off) are unaffected by this change; this
  issue only changes whether the route is registered at all, not how it fails when
  registered.
- The architectural safeguard from ADR-0041 (extraction has no default-socket
  auto-detection tier, so a sidecar is never silently promoted to extraction backend)
  remains in force and is not altered by this issue. This issue changes sidecar startup
  mode selection only; it does not change extraction routing.
- Context-window measurements discussed on this issue (p50/p95/max token estimates
  against Apple Foundation Models' ~4,096-token window, and the ~1,285-token fixed system
  prompt overhead) are documentation-derived motivation for the FR-007 documentation
  requirement above. They do not change this issue's engineering scope, and the
  ~4,096-token figure itself has not been measured against the macOS 26 SDK directly.

## Out of Scope

- Making Foundation Models viable as an extraction backend (would require cutting the
  system prompt by roughly 4x and using materially smaller chunks, with output space
  still tight). No such work is part of this issue.
- Benchmarking Foundation Models extraction quality with a numeric evaluation, and
  folding the token/char statistics discussed on this issue back into
  `docs/extraction-quality-evaluation.md` — deferred to issue #504.
- Adding context-limit handling (token counting, truncation, a distinct error for
  exceeding the window) to `FoundationModelsAdapter`. Today it has none, and an oversized
  prompt fails as whatever generic error Foundation Models raises; fixing that is
  separate work.
- Verifying the ~4,096-token Foundation Models context-window figure against the macOS 26
  SDK directly (currently sourced from documentation only).
- Dropping the `swift-transformers` (`Tokenizers`/`Hub`) dependency for completions-only
  builds. `Tokenizers` and `Hub` are imported only by `EmbeddingsHandler.swift`, so a
  completions-only build could in principle drop the dependency for a smaller, faster
  build — but that is a compile-time SwiftPM-traits concern that should follow this
  runtime flag rather than complicate it.

## Source References

- `Sources/LocalInference/main.swift:44` — the embedding-model-presence guard this issue
  makes conditional.
- `Sources/LocalInference/main.swift:56-65` — existing non-fatal Foundation Models
  degraded-mode handling.
- `docs/extraction-quality-evaluation.md:205` — existing (non-quantitative) assessment
  that Foundation Models is not recommended for extraction.
- `docs/configuration.md:162` — the same finding, stated for users.
- `docs/adr/0041-local-openai-compatible-extraction-adapter.md` — the architectural
  decision (no default-socket auto-detection for extraction) that FR-007 documents in
  the context of the new mode flag.
- Commit `6b0305d` — "require explicit opt-in for local extraction, drop socket
  auto-detection."
