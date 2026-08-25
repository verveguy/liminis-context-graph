\
# Feature Specification: MCP client config recipes for pointing the embedder elsewhere

**Feature Branch**: `fabrik/issue-498`
**Created**: 2026-08-25
**Status**: Specified
**Input**: User description: "There is no documented pattern for configuring the embedder when `liminis-context-graph` is launched by an MCP client. The mechanism already exists and needs no code change — MCP client configs pass `command`, `args` and `env`, and the service reads all of the relevant settings from exactly those. What is missing is a page that says so, with working recipes."

## Background

`liminis-context-graph` resolves its embedder transport from CLI flags and environment
variables, and an MCP client config (Claude Desktop, Claude Code, or any other MCP client)
launches the binary by specifying exactly `command`, `args`, `env`, and `cwd` — the same
inputs the resolution logic already reads. No code change is required to point an
MCP-launched instance at a different embedder. What's missing is documentation: a reader who
only knows the MCP client config surface (not the CLI/env reference) has no worked example
showing how to translate "I want to use Ollama" or "I want to use a hosted OpenAI-compatible
endpoint" into a JSON snippet they can paste into their client config.

This gap is made worse by one non-obvious behavior in the resolution order: on Unix, if the
default UDS sidecar socket (`/tmp/liminis-inference.sock`) exists, it is picked up
**silently and takes priority over `LCG_EMBEDDING_URL`**. Someone who adds an `env` block to
point at Ollama, while the macOS sidecar happens to be running, gets the sidecar instead —
with no warning that their `env` setting was ignored. This is already documented in general
terms in `docs/configuration.md`'s "Embedder sidecar" section, but not in the specific context
an MCP-client-config reader needs: "why does my `env` block seem to have no effect?"

**Status update since this issue was filed**: the issue text notes the hosted-provider recipe
is "blocked on #497... until then say explicitly that hosted providers are not reachable."
#497 (embedder API-key support) has since merged — `LCG_EMBEDDING_API_KEY` (with
`GRAPHITI_EMBEDDING_API_KEY` and `OPENAI_API_KEY` fallbacks) is implemented and documented in
`docs/configuration.md`. The hosted-provider recipe is therefore **no longer blocked** and is
in scope for this issue, using the already-documented env var rather than deferring it.

That same merge also already added, to `docs/configuration.md`, most of the general
prose this issue originally asked for: the resolution order, the non-bypassability of
`LCG_EMBEDDING_DIM` against transport/auth failures, and the Bearer-token auth recipe for
`--embedder-http`. This issue's remaining, still-unmet job is narrower than its original
framing: translate that existing general reference into copy-pasteable **MCP client config**
recipes (the `mcpServers`/`command`/`args`/`env`/`cwd` shape already used elsewhere in the
docs — see `docs/getting-started.md` and `docs/ipc-mcp-reference.md`), and add the two pieces
of guidance that are genuinely not documented anywhere yet: the dimension/re-ingest footgun,
and working-directory pinning in the context of these recipes.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Default macOS sidecar, zero embedder config (Priority: P1)

A macOS user has the Swift CoreML sidecar (`native/local-inference/`) running and wants their
MCP client to launch `liminis-context-graph` against it. They read the docs and see that no
`env` block is needed at all — the default UDS socket is auto-discovered — and copy a minimal
`mcpServers` entry with just `command`, `args`, and `cwd`.

**Why this priority**: This is the default, most common configuration (it's the one already
shown, without embedder framing, in `docs/getting-started.md`) and requires the least new
content — but its absence from the embedder-specific documentation is exactly the "no `env`
block needed" fact the issue calls out as easy to miss.

**Independent Test**: A reader who has never seen `docs/configuration.md`'s "Embedder sidecar"
section can find, in one place, a complete MCP client config snippet for the default sidecar
case, and correctly infer that omitting `env` is intentional rather than an oversight.

**Acceptance Scenarios**:

1. **Given** the documentation section for MCP embedder recipes, **When** a reader looks at the
   sidecar recipe, **Then** they see a complete `mcpServers` JSON snippet with `command` and
   `args` for `--mcp-stdio`, a `cwd`, and no `env` block, plus one sentence stating that the
   default UDS socket is auto-discovered so no embedder-related `env` entry is needed.

---

### User Story 2 - Local OpenAI-compatible server (Ollama, LM Studio, TEI, vLLM) (Priority: P2)

A user running a local OpenAI-compatible embedding server (Ollama being the concrete example;
LM Studio, text-embeddings-inference, and vLLM following the identical shape at a different
port) wants their MCP client to point at it instead of the default sidecar.

**Why this priority**: This is the most common non-default case and the one most likely to
collide with the "sidecar silently wins" surprise described in Background, since these are
often run on the same machine as (or instead of) the sidecar.

**Independent Test**: A reader can copy the Ollama recipe, substitute the port shown in the
LM Studio/TEI/vLLM variant, and produce a working `env` block without consulting any other
page.

**Acceptance Scenarios**:

1. **Given** the Ollama recipe, **When** a reader copies it, **Then** they see an `env` block
   setting `LCG_EMBEDDING_URL=http://127.0.0.1:11434/v1/embeddings` and a matching
   `LCG_EMBEDDING_MODEL`.
2. **Given** the Ollama recipe, **When** a reader needs LM Studio, text-embeddings-inference,
   or vLLM instead, **Then** the documentation shows the same recipe shape with the port and
   model name called out as the only things that change, rather than repeating the full
   explanation four times.
3. **Given** any recipe in this story, **When** the reader has the macOS sidecar also running
   on the same machine, **Then** the documentation states plainly that the sidecar will win
   over this `env` block unless the sidecar is stopped or an explicit
   `--embedder-uds`/`--embedder-http` flag is passed (see User Story 4 for the flag form), with
   a link to the fuller resolution-order explanation in `docs/configuration.md`.

---

### User Story 3 - Hosted OpenAI-compatible provider with an API key (Priority: P2)

A user wants their MCP-launched instance to call a hosted OpenAI-compatible embeddings
endpoint (OpenAI's own `/v1/embeddings`, as the verified concrete case) instead of a local
server, authenticating with an API key.

**Why this priority**: Named explicitly in the issue as the highest-value case once
unblocked, and now unblocked (see Background). Equal priority to Story 2 since both are the
two variants of "point `--embedder-http` at something," differing only in whether a credential
is involved.

**Independent Test**: A reader can copy the recipe and produce a working MCP client config
that authenticates against a hosted endpoint, including seeing where the key goes and being
warned it lands in the `env` block of a config file (not a secrets manager).

**Acceptance Scenarios**:

1. **Given** the hosted-provider recipe, **When** a reader copies it, **Then** they see an
   `env` block setting `LCG_EMBEDDING_URL` (or a passed `--embedder-http` arg), an API key
   variable (`LCG_EMBEDDING_API_KEY`, noting the `OPENAI_API_KEY` fallback), and a matching
   `LCG_EMBEDDING_MODEL`.
2. **Given** the hosted-provider recipe, **When** a reader reads the surrounding prose,
   **Then** they see a one-line caution that the MCP client config file is not a secrets
   manager — the key is stored in plaintext wherever that config file lives — consistent with
   how `docs/configuration.md` already frames key handling for the non-MCP case.
3. **Given** the hosted-provider recipe, **When** a reader compares it against the general
   Bearer-token-auth documentation in `docs/configuration.md`, **Then** the two are consistent
   (same env var names, same fallback order) — this recipe is a client-config-shaped
   restatement, not a divergent second source of truth.

---

### User Story 4 - Explicit UDS path for a non-default sidecar location (Priority: P3)

A user runs a sidecar-compatible embedder listening on a Unix domain socket at a path other
than `/tmp/liminis-inference.sock` (or wants to force UDS selection even when a different
sidecar occupies the default path) and needs to pass `--embedder-uds <path>` via the MCP
client's `args` list.

**Why this priority**: A real but narrower case than Stories 1–3 — it's the escape hatch for
"the default auto-discovery picked the wrong thing, or there is no sidecar at the default
path," rather than a distinct backend.

**Independent Test**: A reader can copy the recipe and see the non-default path expressed as
an `args` entry, not an `env` entry — reinforcing that CLI flags, not environment variables,
carry `--embedder-uds`.

**Acceptance Scenarios**:

1. **Given** the explicit-UDS recipe, **When** a reader copies it, **Then** they see
   `--embedder-uds <path>` added to the `args` list (alongside `--mcp-stdio`), with no `env`
   block required, and a matching `LCG_EMBEDDING_MODEL` set via `env` if it differs from the
   default `bge-base-en-v1.5`.

---

### User Story 5 - Avoiding the dimension/re-ingest and working-directory footguns (Priority: P2)

A user who successfully points their MCP client at a different embedder wants to know, before
they do it against an existing workspace, whether that's safe — and where `.lcg/` will end up
given that the MCP client (not a shell they control) decides the process's working directory.

**Why this priority**: Called out explicitly in the issue as "an easy and expensive mistake,"
and not documented anywhere in the repository today (verified: no existing page states that a
dimension change against an existing `.lcg/` requires re-ingest). Equal priority to Stories 2–3
because a reader who follows those recipes without this warning is the exact person who hits
the footgun.

**Independent Test**: A reader who is about to switch an *existing* workspace's embedder (not
a fresh one) finds, without searching, a statement that a dimension change is a re-ingest, not
a live config change, together with the concrete sidecar-vs-`text-embedding-3-small` example.
Separately, a reader following any recipe in Stories 1–4 sees how to pin the working directory
in their client config so `.lcg/` lands where they expect.

**Acceptance Scenarios**:

1. **Given** any recipe that changes the embedder for a workspace with a pre-existing `.lcg/`
   database, **When** the new embedder's output dimension differs from the one the database
   was built with, **Then** the documentation states this is not a config change — it requires
   re-ingest (or a WAL rebuild under the new embedder) — using the sidecar (768-dim,
   BGE-base-en-v1.5) vs. `text-embedding-3-small` (1536-dim) as the concrete example pair.
2. **Given** the same context, **When** the reader looks for whether `LCG_EMBEDDING_DIM` helps,
   **Then** the documentation states plainly that it does not — cross-referencing the existing
   `docs/configuration.md` explanation that `LCG_EMBEDDING_DIM` only overrides a non-transport,
   non-auth probe failure, not a genuine dimension mismatch against stored vectors.
3. **Given** any recipe in Stories 1–4, **When** the reader wants `.lcg/` to land in a specific
   directory, **Then** the documentation shows the `cwd` field in the `mcpServers` entry (as
   already used in `docs/getting-started.md`'s and `docs/ipc-mcp-reference.md`'s existing MCP
   examples) and states that omitting it leaves `.lcg/` wherever the client process's own
   working directory happens to be, which for most MCP clients is not the user's project
   directory.

---

### Edge Cases

- A reader running the sidecar recipe (Story 1) on a non-macOS platform: the documentation
  must not imply the default-UDS auto-discovery tier applies there — on non-Unix platforms it
  does not exist at all (fallback is `http://127.0.0.1:8765/v1/embeddings`), and on Linux/other
  Unix the sidecar itself (a macOS Swift/CoreML binary) is unavailable regardless of whether the
  UDS tier exists.
- A reader who sets an embedder `env` var while genuinely wanting the sidecar to win (not a
  mistake) — the documentation should not frame the silent-precedence behavior as *always* a
  footgun, only note that it is worth knowing about, matching the issue's own "surprising" (not
  "wrong") framing.
- A reader combining an explicit `--embedder-uds`/`--embedder-http` flag (Story 4) with an
  embedder `env` var they forgot they had set: the flag always wins per the existing resolution
  order, so no conflict — but this is worth a one-line callout since it is a plausible source of
  "I set the env var and it's still not being used" confusion distinct from the sidecar-silent-
  win case.
- A reader on a fresh workspace (no pre-existing `.lcg/`) following the hosted-provider or
  Ollama recipe: the dimension/re-ingest warning (Story 5) does not apply — there is nothing to
  re-ingest — and the documentation should make clear the warning is about *switching* an
  existing workspace's embedder, not about first-time setup.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The documentation MUST include copy-pasteable MCP client config JSON recipes, in
  the same `mcpServers`/`command`/`args`/`env`/`cwd` shape already used in
  `docs/getting-started.md` and `docs/ipc-mcp-reference.md`, for each of: the default macOS
  sidecar (no `env` block), a local OpenAI-compatible server (Ollama as the named example),
  the LM Studio/text-embeddings-inference/vLLM variant of the same shape, a hosted
  OpenAI-compatible provider with an API key, and an explicit non-default UDS path via
  `--embedder-uds` in `args`.
- **FR-002**: Each recipe MUST name the `LCG_EMBEDDING_MODEL` value it pairs with, since a
  mismatched model name produces a probe failure rather than a clear error (per the existing
  probe behavior documented in `docs/configuration.md`).
- **FR-003**: The documentation MUST state, in the context of these recipes, that a running
  default-path UDS sidecar is auto-discovered and takes priority over an `LCG_EMBEDDING_URL`
  `env` entry — framed as "why your `env` block might appear to have no effect" — and MUST
  cross-reference (not duplicate in full) the existing general resolution-order explanation in
  `docs/configuration.md`'s "Embedder sidecar" section.
- **FR-004**: The documentation MUST state that changing to an embedder with a different output
  dimension against an existing `.lcg/` database is not a live config change but requires
  re-ingest (or a WAL rebuild), using the sidecar (768-dim) vs. `text-embedding-3-small`
  (1536-dim) as the concrete example, and MUST NOT imply this content already exists elsewhere
  (verified absent from the current docs tree).
- **FR-005**: The documentation MUST state that `LCG_EMBEDDING_DIM` does not resolve a genuine
  dimension mismatch or an unreachable/unauthenticated embedder, cross-referencing the existing
  `docs/configuration.md` explanation of what that variable actually overrides rather than
  restating it in full.
- **FR-006**: The documentation MUST show, using the `cwd` field already present in the
  project's existing MCP config examples, how to pin the working directory so `.lcg/` lands in
  a predictable location, and MUST state what happens if `cwd` is omitted (the client process's
  own default working directory is used, which is typically not the user's project directory).
- **FR-007**: The hosted-provider recipe (FR-001) MUST use the already-merged
  `LCG_EMBEDDING_API_KEY` mechanism (with its `OPENAI_API_KEY` fallback) rather than stating
  that hosted providers are unreachable — the blocking condition in the original issue text
  (dependency on #497) no longer applies.
- **FR-008**: The documentation MUST NOT introduce a startup-time warning or any other code
  change for the silent sidecar-precedence behavior described in FR-003 — this issue is
  documentation-only, per the issue's own framing ("The mechanism already exists and needs no
  code change").

### Key Entities

Not applicable — this is a documentation-only change with no new data model.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A reader who launches `liminis-context-graph` from an MCP client and wants to use
  any of the five configurations in FR-001 can copy one JSON snippet and one `LCG_EMBEDDING_MODEL`
  value, with no need to separately consult the CLI/env reference to fill in the gaps.
- **SC-002**: Every env var name, default value, and precedence claim in the new recipes matches
  the corresponding entry in `docs/configuration.md` exactly — no second, drifted source of
  truth for the same facts.
- **SC-003**: The dimension/re-ingest warning (FR-004) and the working-directory guidance
  (FR-006) each appear in one canonical place that every recipe can link to or sit beside,
  rather than being repeated five times with the risk of drifting apart.

## Assumptions

- The new recipes extend `docs/configuration.md`'s existing "Embedder sidecar" section (which
  already documents the CLI/env-level resolution order, transports, and Bearer-token auth) —
  the natural home for embedder-specific content — rather than creating a new page. The exact
  placement (a new subsection there vs. a cross-linked block in `docs/ipc-mcp-reference.md`'s
  "Example MCP client config" section) is a documentation-structure decision left to the
  Research/Plan stages; this spec constrains content and cross-referencing, not file layout.
- "MCP client config" means the `mcpServers`/`command`/`args`/`env`/`cwd` JSON shape already
  used by Claude Desktop, Claude Code, and other MCP clients, matching the shape already shown
  (without embedder-specific framing) in `docs/getting-started.md` and
  `docs/ipc-mcp-reference.md`.
- The hosted-provider recipe uses OpenAI's `/v1/embeddings` as its concrete example, matching
  the only verified hosted-compatible case already documented in `docs/configuration.md`'s HTTP
  transport section.
- No code changes are in scope. Every mechanism these recipes document (CLI flags, env var
  resolution order, `LCG_EMBEDDING_API_KEY`, `LCG_EMBEDDING_DIM` override rules) already exists
  and already ships; this issue is documentation-only.

## Out of Scope

- Adding a startup-time warning when the default-UDS-sidecar tier silently overrides
  `LCG_EMBEDDING_URL` (raised as "arguably worth doing" in the original issue text, but the
  issue's own scope statement rules out a code change; a warning would be one). If wanted,
  this is a separate, code-changing issue.
- Any change to the embedder resolution order, defaults, or env var names themselves.
- Extractor (LLM) configuration recipes — this issue is embedder-only; the extractor's
  local/hosted precedence is a separate, already-documented mechanism (see
  `docs/configuration.md`'s "Extractor: local or hosted" section) with its own, different
  resolution order (no default-socket auto-discovery tier).
- A machine-readable or automatically-validated way of keeping the recipes' env var names in
  sync with `docs/configuration.md` (SC-002 is a review-time property, not a tooling
  requirement, for this issue).

## Source References

- `crates/service/src/main.rs` (`bootstrap_app_state`) — the embedder transport resolution
  logic these recipes must accurately reflect.
- `docs/configuration.md` — "Embedder sidecar" and "Environment variables" sections; the
  existing general (non-MCP-framed) source of truth this issue cross-references rather than
  duplicates.
- `docs/getting-started.md` and `docs/ipc-mcp-reference.md` — existing `mcpServers` JSON
  examples establishing the config shape these recipes reuse.
- #497 (closed) — added `LCG_EMBEDDING_API_KEY` support and its documentation, unblocking the
  hosted-provider recipe (FR-007).
- ADR-0006, ADR-0016, ADR-0497 — embedder transport and Bearer-auth design decisions referenced
  by the existing `docs/configuration.md` content this issue builds on.
