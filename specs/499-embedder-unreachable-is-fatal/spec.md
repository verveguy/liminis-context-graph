# Feature Specification: Reconsider fatal embedder-unreachable startup failure for MCP-launched mode

**Feature Branch**: `fabrik/issue-499`
**Created**: 2026-08-25
**Status**: Draft
**Input**: User description: "design: embedder-unreachable is fatal at startup (FR-011) — reconsider for MCP-launched mode"

## Background

`crates/service/src/main.rs` probes the embedder before opening the database, so a
misconfigured embedder fails at startup rather than on the first embed request
(FR-010/FR-011 of the original startup-resolution work). A transport failure is
**always fatal**, and the code says so explicitly:

```rust
Err(e) if is_transport_error(&e) => {
    // FR-011: transport/connectivity failures are always fatal at startup.
    // LCG_EMBEDDING_DIM cannot override an unreachable embedder.
    return Err(format!(
        "embedder unreachable at startup: {e}. \
         Ensure the embedder sidecar is running before starting liminis-context-graph."
    ).into());
}
```

The reasoning is sound for a hand-started service: an operator sees the message on
their terminal, starts the sidecar, and retries. Failing fast beats discovering the
problem mid-ingest.

**Why the MCP case is different.** When an MCP client spawns the server, nobody is
watching stderr. The user sees the client's own generic failure — "server failed to
start", or a red dot — and the actionable message this code takes care to write is
discarded by the harness. The failure is identical in appearance to a bad binary path,
a permissions problem, or a crash.

The obstacle is also more likely to be present in that context than in a hand-started
one, because the embedder story is genuinely hard today: on macOS it means building
the Swift sidecar in `native/local-inference/` from source with a ~400 MB CoreML model
conversion, and on every other platform it means running your own inference server.
Hosted providers are not reachable at all (#497). So "embedder not up yet" is close to
the *expected* first-run state, and it currently presents as a total, unexplained
failure of the tool.

Observed directly while smoke-testing the v0.13.4 release artifact: with no embedder
running the process exits before binding, and `LCG_EMBEDDING_DIM` does not rescue it —
as the comment says it will not.

This is a **design question, not a bug report**: the current behaviour is deliberate
and documented; the question is whether it is still right once an MCP client is the
thing launching the process. This round of work sharpens and costs the four candidate
answers against the actual codebase, and makes a recommendation. **No behavior
changes in this stage** — FR-011 is untouched. The maintainer picks a direction before
Research begins.

## Options under consideration

1. **Leave it.** Fail fast is defensible and the message is good. Push the fix into
   documentation (#498) and accept that a first run without an embedder is a hard
   stop. *Cost: zero.* Does not address the MCP-launched failure mode at all.

2. **Start degraded, unconditionally.** Bind the socket (or stdio), serve read-only
   and non-embedding operations, and report the embedder as unavailable through
   `knowledge_status`. *Cost: materially higher than the original issue text assumed
   — see "Costing findings" below.* The blocking structural fact: `Db::open` immediately
   calls `conn.init_schema(embedding_dim)`, and `Entity.name_embedding` /
   `RelatesToNode_.fact_embedding` are fixed-size `FLOAT[N]` columns — the database
   cannot be opened at all without first committing to a concrete embedding dimension.
   Today that dimension comes only from a successful probe (or `LCG_EMBEDDING_DIM`,
   which FR-011 explicitly excludes from the transport-failure case). So "start
   degraded but still serve read-only graph queries" requires either guessing a
   fallback dimension (risk: mismatch against the real embedder once it comes up,
   forcing a full reindex — and the mismatch would also poison the three existing
   `knowledge_recover` strategies, all of which call `state.embedder.dim()` to rebuild
   schema) or not opening the DB at all — in which case "read-only operations" isn't
   actually deliverable, only diagnosability is.

3. **Retry with backoff before giving up**, covering the common race where the client
   launches lcg and the sidecar simultaneously. *Cost: low.* This only delays the
   existing fatal probe by a bounded amount; it does not touch schema, `AppState`, or
   any of the ~15 `.embed()` call sites in `crates/core/src/handlers.rs`. It directly
   targets a plausible, common failure shape: an MCP client starting the sidecar and
   the server as sibling processes with no ordering guarantee.

4. **Degrade only when MCP-launched**, keeping fail-fast for a hand-started service.
   The original issue text said this "requires the process to know how it was
   started, which is a new concept." **That is incorrect as of this codebase's
   current state** — see "Costing findings" item 4 below: the caller already knows,
   at the exact call site that invokes the shared bootstrap function, which launch
   mode it's in. Passing that through is a one-parameter change, not new
   configuration. Cost otherwise inherits option 2's dimension problem *unless*
   "degraded" is scoped down to "the process starts and names the problem" rather
   than "read-only queries still work" — see the Recommendation below for a cheap
   version of this option that sidesteps the dimension problem entirely.

## Costing findings

Answers to the four questions raised for this round, from reading the current code
(not from the original issue's assumptions):

**(1) Does `AppState.degraded_reason` already flow to `knowledge_status` in a
client-actionable form, or does it need a new field?** It flows today, but only for
*database*-level degradation. `handle_knowledge_status`
(`crates/core/src/handlers.rs:290`) checks `state.db.load_full().is_none()` first; in
that branch it returns `degraded: true`, `reason: <degraded_reason>`, `connected:
false`, `queryable: false`, `recovery_available: [...]` — a fixed, already-shipped
shape (most recently extended by #451's `group_ontology_drift`, confirming this
response is actively maintained, not frozen). But this path only exists because
`state.db` is an `Option`; `AppState.embedder` (`crates/core/src/app_state.rs:59`) is
`Arc<dyn Embedder>` — **not optional** — so there is currently no slot in `AppState`
for "DB is fine but the embedder isn't," and no branch in `knowledge_status` would
ever report it. Whether a *new* field is needed therefore depends on which option is
chosen: the cheap version of option 4 (below) needs no new field, because it reuses
the DB-never-opened branch verbatim with a new `degraded_reason` string; the full
option 2/4 (DB open with a guessed dimension, embedder marked separately unavailable)
would need a new field, since a healthy DB and an unhealthy embedder would then be
simultaneously true.

**(2) Which handlers are genuinely embedding-dependent, and what do they return
today?** Mixed, and inconsistent:
- `handle_assert_entity` (name/summary embeddings, `handlers.rs:3601-3627`) and the
  RelatesTo-creation handler (fact embedding, `handlers.rs:3769-3778`) **already**
  catch an embed failure per-call and fall back to a same-dimension zero vector plus a
  warning string surfaced in the response — they do not error at all today, transport
  failure or not. This pattern already exists and already ships.
- `handle_create_cross_group_edge` (`handlers.rs:3475`) does **not** have that
  fallback — `state.embedder.embed(&fact).await?` propagates directly, surfacing as a
  generic JSON-RPC `-32000` whose message text is whatever the transport error's
  `Display` produced (e.g. `"UDS connect to /tmp/liminis-inference.sock: ..."` — see
  `crates/core/src/embedder.rs:89`). That text does name the transport/socket, but
  there's no dedicated error code or field a client could pattern-match on reliably,
  and it's inconsistent with the two handlers above.
- `handle_find_entities` / `handle_find_relationships` (hybrid semantic+FTS search,
  `handlers.rs:978-1037`) also propagate embed failures via `Err(e) => return Err(e)`
  with the same generic `-32000` shape. Whether hybrid search *could* degrade to
  FTS-only on embed failure instead of erroring is an implementation question for
  Research, not answered here.
- So: "a per-call error that names the missing embedder" is not yet the case
  uniformly — some paths already silently degrade (with a warning field), some
  already error generically. Any option that leans on per-call errors needs to decide
  whether to standardize this shape across all embedding-dependent handlers, which is
  a wider change than it first appears.

**(3) Does the probe's fail-fast-before-DB-open ordering still hold if startup no
longer aborts?** Yes, and it's stronger than "ordering" — it's a hard dependency, not
a stylistic choice. `Db::open(&db_path)?` is immediately followed by
`conn.init_schema(embedding_dim)?` (`main.rs:567-570`), and schema init needs a
concrete `usize` dimension because the embedding columns are fixed-size `ARRAY`
columns, not variable-length `LIST`s. The `Embedder` trait's `dim()` method
(`crates/core/src/embedder.rs:198-202`) is synchronous and returns a committed value —
there's no way to construct *any* `Embedder` implementation, including a hypothetical
degraded/placeholder one, without deciding a dimension up front. This same
`state.embedder.dim()` call is also load-bearing in all three `knowledge_recover`
strategies (`handlers.rs:4367`, `drop_lbug_wal` / `rebuild_from_workspace_wal` /
`restore_from_backup`), so a wrong guessed dimension wouldn't just affect the initial
open — it would propagate into every recovery path too.

**(4) Is "how was I launched?" knowable without new configuration?** Yes. `main.rs`'s
`async_main` already matches on a `CliMode` enum with `Socket` and `Mcp` variants
*before* calling the shared `bootstrap_app_state` — the two call sites
(`main.rs:1120` for `CliMode::Socket`, `main.rs:1145` for `CliMode::Mcp { connect:
None, .. }`) are already in separate match arms. Threading that distinction into
`bootstrap_app_state` as a parameter is a small, local, one-function-signature change,
not a new concept or new configuration surface. The original issue's stated cost for
option 4 does not hold.

## Recommendation

Given the above, options 2 and 4 as originally described ("bind the socket, serve
read-only and non-embedding operations") are both more expensive and riskier than the
issue assumed, because of the DB-dimension coupling in finding (3) — not because of
process-identity plumbing, which finding (4) shows is cheap.

The cheapest path that still solves the actual reported problem — a diagnosis nobody
reads (SC-001-shaped) — is a **combination**:

- **Option 3** (bounded retry before giving up): directly targets the sidecar/server
  startup race, costs nothing structurally, and reduces false fatal failures for the
  most common MCP-launch case without touching schema or `AppState`.
- **A narrowed version of option 4**: when `--mcp-stdio` (standalone) is the launch
  mode and the retry above is exhausted, do not open the database at all — reuse the
  *existing* "DB never opened" degraded branch verbatim (same `degraded` /
  `degraded_reason` / `connected: false` / `queryable: false` shape `knowledge_status`
  already returns for WAL corruption today) with a new `degraded_reason` value for
  "embedder unreachable at startup," gated on `cli_mode` rather than on message
  content. This needs no new `AppState`/`Embedder` plumbing and doesn't touch any of
  the ~15 `.embed()` call sites, because none of that code runs while the DB never
  opened. It delivers diagnosability (a client can call `knowledge_status` and learn
  what's wrong) but **not** read-only graph queries — those still require the DB to be
  open, which still requires a committed dimension.
- The socket-service path (hand-started) keeps today's fail-fast behavior unchanged,
  satisfying FR-001 below without any new gating logic beyond the `cli_mode` check
  already required for the point above.

True "read-only-while-embedder-degraded" service (the full ambition of option 2) is a
materially larger, separate follow-on that Research/Plan should scope on its own if
wanted, since it requires resolving the dimension-commitment problem across both
initial schema creation and all three recovery strategies.

This is a recommendation, not a decision — **the maintainer picks the direction**
(see Open Questions) before this moves to Research.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - MCP client starts the server without an embedder running (Priority: P1)

A user configures an MCP client (e.g., Claude Desktop, an editor's MCP integration) to
launch `liminis-context-graph --mcp-stdio` without first starting the embedder
sidecar. Today the process exits immediately and the client shows only a generic
"server failed to start" — the actionable stderr message is never seen. The desired
outcome is that the user can determine *why* the tool isn't fully working from within
the MCP session itself (e.g., via `knowledge_status` or a clear per-call error),
rather than the tool appearing entirely broken.

**Why this priority**: This is the exact failure mode reported from smoke-testing the
v0.13.4 release artifact, and it is the primary friction point blocking first-run
success for MCP-launched usage.

**Independent Test**: Launch the server via `--mcp-stdio` with no embedder reachable;
confirm the cause is discoverable from within the MCP session (not only from stderr),
per whichever option is selected.

**Acceptance Scenarios**:

1. **Given** no embedder is reachable at the configured transport, **When** the server
   is launched via `--mcp-stdio` and the sidecar does not become reachable within the
   selected retry window (if any), **Then** the process does not exit silently from
   the client's point of view — the MCP session starts and `knowledge_status` (or an
   equivalent) reports that the embedder is unavailable and why.
2. **Given** the embedder becomes reachable shortly after the server starts (the
   race case), **When** retry (if selected) succeeds within its window, **Then** the
   server starts normally with no degraded state at all.
3. **Given** a hand-started socket-service process with the same misconfiguration,
   **When** it starts, **Then** it retains today's fail-fast behavior (FR-001) unless
   the maintainer's chosen option explicitly says otherwise.

---

### Edge Cases

- Embedder becomes reachable partway through a retry/backoff window.
- Embedder was reachable at startup but becomes unreachable mid-session (out of scope
  — see Assumptions).
- `LCG_EMBEDDING_DIM` is set: today it already overrides *non-transport* probe
  failures; whether it should also participate in the chosen option's transport-error
  handling needs to be decided alongside the option itself.
- A degraded MCP-mode server (if that option is chosen) receiving a
  `knowledge_recover` call before the embedder is ever reachable — none of today's
  three recovery strategies can run without a committed embedding dimension.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST NOT regress the existing fail-fast startup behavior for
  a hand-started socket-service process unless the maintainer's selected option
  explicitly changes it for that path too.
- **FR-002**: Whatever behavior is selected for `--mcp-stdio` mode MUST make the cause
  of an embedder-unreachable condition discoverable by the MCP client or its user
  through the running session (e.g., via `knowledge_status` or a per-call error),
  since stderr is not visible to that audience.
- **FR-003**: The launch-mode distinction used to gate any MCP-specific behavior MUST
  use the existing `CliMode::Socket` / `CliMode::Mcp` split already resolved in
  `async_main` (see Costing finding 4) — no new configuration surface for "how was I
  launched."
- **FR-004** *(pending maintainer decision)*: The exact startup/retry/degraded
  behavior for `--mcp-stdio` mode — to be filled in once an option is selected.

### Key Entities

- **AppState.degraded_reason**: Existing `Option<String>` on `AppState`
  (`crates/core/src/app_state.rs:58`) describing why the *database* failed to open;
  currently unrelated to embedder reachability, but reusable per the Recommendation
  above by adding a new reason string rather than a new field.
- **AppState.embedder**: `Arc<dyn Embedder>` (`crates/core/src/app_state.rs:59`) —
  mandatory, not optional. Any option that keeps the DB open while the embedder is
  degraded would need to change this.
- **Embedder probe**: The startup check in `bootstrap_app_state`
  (`crates/service/src/main.rs`) that resolves transport and calls
  `probe_embedder.probe()` before schema init / DB open.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user who launches the server via an MCP client with no embedder
  running can determine, without reading process stderr, that the embedder is the
  problem.
- **SC-002**: A hand-started socket-service process with the same misconfiguration
  continues to fail exactly as it does today (message and exit behavior unchanged),
  unless the maintainer explicitly decides otherwise.
- **SC-003** *(pending maintainer decision)*: Concrete, technology-agnostic success
  measure for the selected option — e.g. "the retry window is bounded to N seconds and
  documented" for option 3, or "the server binds and answers `knowledge_status` within
  N seconds even with no embedder reachable" for a degraded-start option.

## Assumptions

- Embedder reachability at startup vs. mid-session are treated as separate concerns;
  this issue is scoped to the **startup** probe only (FR-010/FR-011), not to detecting
  or recovering from an embedder that goes away after a successful start.
- The MCP-stdio launch mode (`--mcp-stdio` without `--connect`) is treated as a
  reliable signal of "no one is watching stderr"; a hand-started process that happens
  to redirect its own stderr away is out of scope for special-casing.
- "Start degraded" (if selected) does not imply solving the embedding-dimension
  problem identified in Costing finding 3 unless the maintainer explicitly picks the
  full option 2/4 scope over the narrowed recommendation.

## Out of Scope

- Hosted embedder providers reachable without a local sidecar (#497).
- Documentation of embedder configuration (#498).
- Detecting or recovering from an embedder that was reachable at startup but becomes
  unreachable later.
- Standardizing the per-call embedding-error shape across all handlers listed in
  Costing finding 2 — worth doing, but a separate, option-independent cleanup that
  Research can scope if the maintainer wants it bundled in.

## Source References

- `crates/service/src/main.rs` (`bootstrap_app_state`, embedder probe, DB open/schema
  init, `CliMode` dispatch, FR-010/FR-011)
- `crates/core/src/app_state.rs` (`degraded_reason`, `embedder`)
- `crates/core/src/handlers.rs` (`handle_knowledge_status`, `handle_assert_entity`,
  `handle_create_cross_group_edge`, `handle_find_entities`, `handle_find_relationships`,
  `handle_knowledge_recover`)
- `crates/core/src/embedder.rs` (`Embedder` trait, `is_transport_error`, error message
  shapes)
- README "Two transport surfaces" section (`--mcp-stdio`)
- #497 (hosted embedders), #498 (documentation), #451 (`group_ontology_drift` addition
  to `knowledge_status`, cited as evidence that response is actively maintained)
