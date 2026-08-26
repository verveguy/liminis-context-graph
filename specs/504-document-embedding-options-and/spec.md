# Feature Specification: Document embedding options and conventions: one capability matrix, local and remote

**Feature Branch**: `fabrik/issue-504`
**Created**: 2026-08-26
**Status**: Specified
**Input**: User description: "Document embedding options and conventions: one capability matrix, local and remote"

## Background

There is no single place that tells a user what their embedding options are. The information
exists, but it is scattered across `docs/configuration.md` (flags and env vars),
`native/local-inference/README.md` (the macOS sidecar), ADR-0006 and ADR-0016 (the wire
contract), and `docs/spikes/native-embedder-2026-05.md` (why there is no bundled
cross-platform option). None of them answers the actual question a new user has: *what should
I run, on my platform, and what will it cost me?*

The original issue also referenced `docs/embedding-sidecar-status.md` as a fifth existing
source, describing it as recording "the current state" but not being user-facing guidance.
That file does not exist anywhere in this repository's history (verified via `git log --all`
across every local and remote branch) — the reference is stale, most likely left over from an
earlier draft of the issue or an internal note that was never committed. It is dropped from the
source list below rather than treated as an input to consolidate.

**Known drift to fix.** `docs/configuration.md` describes the macOS default as
auto-detecting `/tmp/liminis-inference.sock`. That path is real — it is the
`liminis-context-graph` binary's own default UDS auto-discovery path — but the Electron
`liminis` app does not use it: the app passes an explicit, per-workspace socket at
`<workspaceRoot>/.liminis/local-inference.sock` when it launches the sidecar. A reader of the
current text would reasonably conclude the app and a bare binary share a socket. They do not,
and this has caused real confusion.

**Dependency status.** The original issue noted that Swift sidecar mode selection
(embeddings-only / completions-only / both) would change what the macOS row of the matrix
says, and that this doc should land after that work. That work — issues #501 (mode selection),
#502 (re-enabling sidecar CI), and #503 (making this repo the sole source of truth for the
sidecar, ADR-0503) — is already merged to `main` as of this spec. `native/local-inference/README.md`
already documents the `LOCAL_INFERENCE_MODE` table. No further waiting is required.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Choose an embedder in one read (Priority: P1)

A user setting up `liminis-context-graph` for the first time — on macOS, Linux, or Windows —
wants to know what embedding backend to run without piecing the answer together from four
separate documents. They land on one page, scan a single capability-matrix table, and pick the
row matching their platform and budget.

**Why this priority**: This is the entire reason the issue exists. Without it, the feature
delivers no value — everything else in this spec exists to make this page trustworthy and
correctly scoped.

**Independent Test**: Hand the page to someone unfamiliar with the project, with a fresh
checkout and no other context. They should be able to name a working embedding option for
their platform and the exact CLI flag to select it, using only this page.

**Acceptance Scenarios**:

1. **Given** a macOS 26+ user with no prior `liminis-context-graph` experience, **When** they
   read the new page, **Then** they can identify the Swift CoreML sidecar as their local,
   zero-per-call-cost default option and the `--embedder-uds` flag (or no flag, since it is
   auto-detected) needed to select it.
2. **Given** a Linux or Windows user, **When** they read the new page, **Then** they see
   plainly that no bundled option exists for their platform, that they must supply an
   OpenAI-compatible endpoint themselves, and at least one concrete example (Python
   sentence-transformers, Ollama, or vLLM/TEI) with the `--embedder-http` flag used to select
   it.
3. **Given** any user reading the page, **When** they check the dimension column for their
   chosen option, **Then** they understand that switching to a different-dimension embedder
   against an existing graph is not a drop-in swap, and can find where the remedy is
   documented.

---

### User Story 2 - Trust the macOS socket description (Priority: P2)

A macOS user running the Electron `liminis` app reads the existing `docs/configuration.md`
text and assumes the app and a manually-started `liminis-context-graph` binary share one
socket at `/tmp/liminis-inference.sock`. They hit unexpected behavior — for example, starting
a second, redundant sidecar for a manual CLI session because they believe the app's sidecar is
reachable at the well-known path — because the app actually binds a separate, per-workspace
socket.

**Why this priority**: This is the specific, named drift the issue calls out as needing
correction. It is a documented source of real confusion, not a hypothetical one, but it is
narrower in scope than the core matrix (P1).

**Independent Test**: Read the corrected passage in isolation. Confirm it names both socket
paths explicitly and states plainly that they are not the same and are not shared, with no
sentence that can be read to imply otherwise.

**Acceptance Scenarios**:

1. **Given** the corrected documentation, **When** a reader looks for "the macOS default
   socket path," **Then** they find both paths named explicitly — the bare binary's own
   `/tmp/liminis-inference.sock` auto-discovery default, and the Electron app's
   `<workspaceRoot>/.liminis/local-inference.sock` — with an explicit statement that these are
   two different sockets serving two different processes' sidecars, not one shared socket.

---

### User Story 3 - Don't conflate embedding and extraction (Priority: P3)

A user configuring extraction (`--extractor-uds` / `--extractor-http` / `ANTHROPIC_API_KEY`)
reads the embedding capability matrix and mistakenly assumes it also governs extraction,
since both axes use similarly-shaped flags and, on macOS, the same sidecar process can serve
both endpoints.

**Why this priority**: The issue explicitly flags this as a known point of confusion, but it
is a secondary clarity concern relative to the core matrix (P1) and the socket drift (P2) —
extraction already has its own documented section in `docs/configuration.md`.

**Independent Test**: Read the page's extraction callout in isolation. Confirm a reader can
state that embedding and extraction are configured with separate, independent flags, and knows
where to find full extraction configuration guidance.

**Acceptance Scenarios**:

1. **Given** a reader who has just read the embedding matrix, **When** they reach the
   extraction callout, **Then** they can state that `--extractor-uds`/`--extractor-http`/
   `ANTHROPIC_API_KEY` are unrelated to the `--embedder-uds`/`--embedder-http` flags above, and
   they know where to find full extraction configuration guidance.

---

### Edge Cases

- A Linux or Windows user must be able to tell, from the matrix alone, that there is currently
  no bundled/zero-setup option for their platform — the page must not bury this behind a
  platform-specific row that reads like "just works" without the caveat.
- A user who conflates the `--extractor-*` flags with the `--embedder-*` flags must be
  redirected by the page's explicit separation note (User Story 3) rather than left to
  discover the distinction by trial and error.
- A user switching from one embedder to a different-dimension one on an existing workspace
  must be pointed to the documented remedy (full re-ingest, or `knowledge_recover` with
  `rebuild_from_workspace_wal`) rather than left to discover `embeddings_recompute_failed` on
  their own.
- A hosted-provider row must not overstate verification: `docs/configuration.md` currently
  calls out OpenAI's own `/v1/embeddings` specifically as "the concrete, verified case," while
  other self-described OpenAI-compatible providers are explicitly not verified beyond the
  unauthenticated local-server case. The new page must preserve that distinction rather than
  presenting all hosted options as equally verified.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The new page MUST contain a single capability-matrix table, one row per
  embedding option, covering at minimum: the Swift CoreML sidecar (macOS), Python
  sentence-transformers, Ollama, vLLM/TEI, and a hosted OpenAI-compatible provider.
- **FR-002**: Each row MUST state: supported platform(s), what must be installed, expected
  setup/model cost (download size, time, or per-call pricing, as applicable), the embedding
  dimension (or "varies" where the model is user-chosen), the CLI flag used to select it
  (`--embedder-uds` vs `--embedder-http`), whether it runs fully offline, whether the binary
  auto-probes its dimension at startup, and what happens if it is unreachable at startup
  (matching the fail-fast vs. retry-and-degrade behavior already documented in
  `docs/configuration.md`'s "Embedder sidecar" section for the relevant run mode).
- **FR-003**: The page MUST state that any OpenAI-compatible `POST /v1/embeddings` endpoint is
  a valid `--embedder-http` target, framing this as the actual extension point rather than an
  exhaustive list of named, individually-supported vendors.
- **FR-004**: The page MUST state that the embedding dimension is fixed into the graph's
  schema (`FLOAT[dim]`) at database-creation time, that the binary auto-probes it at startup,
  that `LCG_EMBEDDING_DIM` can override a probe failure but not a genuine dimension mismatch,
  and that switching to a different-dimension embedder against an existing database is not a
  drop-in swap — linking to `docs/configuration.md`'s "Switching an existing workspace's
  embedder" section for the remedy rather than restating it.
- **FR-005**: The page MUST state that changing the embedding model — even to one with the
  same dimension — invalidates previously stored vectors, and MUST cross-reference issue #440's
  model-identity stamping and mismatch detection rather than re-explaining that mechanism.
- **FR-006**: The page MUST state plainly that no bundled cross-platform embedder exists, that
  non-macOS users must supply an external endpoint, and MUST link
  `docs/spikes/native-embedder-2026-05.md` for why (candle NO-GO, ort GO-with-caveats, neither
  ever built into production).
- **FR-007**: The page MUST include a clearly separated callout — not a merged row or column
  of the embedding matrix — stating that extraction is configured independently via
  `--extractor-uds`/`--extractor-http`/`ANTHROPIC_API_KEY`, is a distinct axis from embedding
  selection, and linking to `docs/configuration.md`'s "Extractor: local or hosted" section for
  full detail.
- **FR-008**: The page MUST correct the socket drift described in Background: it MUST name
  both the `liminis-context-graph` binary's own default UDS auto-discovery path
  (`/tmp/liminis-inference.sock`) and the Electron `liminis` app's actual per-workspace socket
  path (`<workspaceRoot>/.liminis/local-inference.sock`), and state explicitly that these are
  not shared.
- **FR-009**: `docs/configuration.md`'s "Embedder sidecar" section MUST be corrected to
  reflect the same distinction as FR-008, wherever it currently implies or could be read to
  imply that a bare binary and the Electron app share one socket.
- **FR-010**: The new page MUST be linked from `docs/configuration.md`'s "Embedder sidecar"
  section, from `README.md`, and added to `docs/index.md`'s "Reference pages" list.
- **FR-011**: Every row of the capability matrix MUST be verified working before publication —
  either by citing an existing verification already in the repository (a test, an ADR, or
  documented evaluation such as the "concrete, verified case" language already in
  `docs/configuration.md`) or by a verification step performed as part of implementing this
  issue. An option that cannot be verified MUST be labeled as such (e.g. "not independently
  verified — relies on the generic OpenAI-compatible contract") rather than presented as a
  plain, unqualified recommendation, or MUST be omitted from the matrix.
- **FR-012**: The page MUST NOT cite `docs/embedding-sidecar-status.md` as a source or link
  target, since that file does not exist in this repository. The consolidated source set is
  `docs/configuration.md`, `native/local-inference/README.md`, `docs/adr/0006-embedder-http-contract.md`,
  `docs/adr/0016-oai-embedding-contract-uds-transport.md`, and
  `docs/spikes/native-embedder-2026-05.md`.

### Key Entities

- **Embedding option (matrix row)**: A named way to produce embedding vectors for
  `liminis-context-graph` — e.g. the Swift CoreML sidecar, a Python sentence-transformers
  server, Ollama, vLLM/TEI, or a hosted OpenAI-compatible provider. Characterized by platform
  support, install/setup cost, dimension, selection flag, offline capability, dimension
  auto-probe support, and unreachable-at-startup behavior.
- **Extraction option**: A separately-configured way to produce entity/relationship
  extractions (`--extractor-uds`/`--extractor-http`/`ANTHROPIC_API_KEY`) — referenced but not
  enumerated in the embedding matrix; documented in full elsewhere.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A reader unfamiliar with the project can identify, from the new page alone,
  which embedding option to run on their platform and what it costs to set up, without opening
  `docs/configuration.md`, `native/local-inference/README.md`, or either ADR.
- **SC-002**: The `/tmp/liminis-inference.sock` vs. per-workspace-socket distinction is stated
  correctly everywhere the default macOS socket path is documented across
  `docs/configuration.md` and the new page — zero remaining passages implying the bare binary
  and the Electron app share a socket.
- **SC-003**: Every row of the capability matrix is traceable to a citation (an existing doc,
  ADR, test, or evaluation) confirming it works, with no unverified option presented without a
  caveat.
- **SC-004**: The new page is reachable within one click from `README.md`,
  `docs/configuration.md`, and `docs/index.md`.

## Assumptions

- The new page is a standalone file (proposed: `docs/embedding-options.md`, a peer of
  `docs/configuration.md`), not a new subsection appended to `docs/configuration.md` — the
  issue's acceptance criterion ("one page ... linked from `configuration.md`") implies a
  separate destination. The exact filename is not load-bearing to this spec and may be
  adjusted at Plan/Research time if a better fit is found, provided it remains one dedicated
  page.
- "Verified to work" (FR-011) reuses whatever verification already exists in the repository
  (tests, ADRs, or documented evaluation) rather than requiring new end-to-end testing of every
  provider as part of this issue. Research is expected to inventory what verification already
  exists per row before the page is written.
- The dependency noted in the original issue (mode-selection work landing first) is already
  satisfied by the merge of #501, #502, and #503 — no further sequencing is required before
  this work proceeds.
- This is a documentation-only change. No CLI flag, environment variable, or runtime
  resolution-order behavior changes as part of this issue.

## Out of Scope

- Building, integrating, or independently verifying a new embedder option that does not
  already exist in the codebase's supported set.
- Changing any runtime behavior, CLI flag, or environment-variable resolution order.
- Rewriting `docs/configuration.md`'s detailed reference sections beyond the specific drift
  correction (FR-009) and the new cross-link (FR-010).
- Building a second, equally detailed capability matrix for the extraction axis — FR-007's
  brief, separated callout with a link is sufficient; a full extraction matrix is a candidate
  for a future issue if requested.

## Source References

- `docs/configuration.md` — "Embedder sidecar" and "Extractor: local or hosted" sections
- `native/local-inference/README.md`
- `docs/adr/0006-embedder-http-contract.md`
- `docs/adr/0016-oai-embedding-contract-uds-transport.md`
- `docs/adr/0503-swift-sidecar-source-of-truth.md`
- `docs/spikes/native-embedder-2026-05.md`
- Issue #440 — model-identity stamping and mismatch detection on WAL replay
- Issues #501, #502, #503 — Swift sidecar mode selection, CI re-enable, and source-of-truth
  consolidation (all merged to `main`)
