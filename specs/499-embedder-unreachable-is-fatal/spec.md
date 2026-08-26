# Feature Specification: Reconsider fatal embedder-unreachable startup failure for MCP-launched mode

**Feature Branch**: `fabrik/issue-499`
**Created**: 2026-08-25
**Status**: Specified
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

This started as a **design question, not a bug report**: the current behaviour is
deliberate and documented, and the question was whether it is still right once an MCP
client is the thing launching the process. The Specify stage sharpened and costed four
candidate answers against the actual codebase (below); the maintainer has since
**confirmed a decision** — see "Decision" below. This spec now describes what the
Research/Plan/Implement stages should build, not an open menu.

## Options considered

1. **Not chosen.** Leave it. Fail fast is defensible and the message is good. Push the
   fix into documentation (#498) and accept that a first run without an embedder is a
   hard stop. *Cost: zero.* Does not address the MCP-launched failure mode at all.

2. **Not chosen (full scope deferred — see Decision and Out of Scope).** Start
   degraded, unconditionally, serving read-only
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

3. **Chosen.** Retry with backoff before giving up, covering the common race where the client
   launches lcg and the sidecar simultaneously. *Cost: low.* This only delays the
   existing fatal probe by a bounded amount; it does not touch schema, `AppState`, or
   any of the ~15 `.embed()` call sites in `crates/core/src/handlers.rs`. It directly
   targets a plausible, common failure shape: an MCP client starting the sidecar and
   the server as sibling processes with no ordering guarantee.

4. **Chosen, narrowed.** Degrade only when MCP-launched, keeping fail-fast for a
   hand-started service. The original issue text said this "requires the process to
   know how it was started, which is a new concept." **That is incorrect as of this
   codebase's current state** — see "Costing findings" item 4 below: the caller
   already knows, at the exact call site that invokes the shared bootstrap function,
   which launch mode it's in. Passing that through is a one-parameter change, not new
   configuration. Cost otherwise inherits option 2's dimension problem *unless*
   "degraded" is scoped down to "the process starts and names the problem" rather
   than "read-only queries still work" — which is exactly the narrowing the Decision
   below adopts, sidestepping the dimension problem entirely.

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

**(4) Is "how was I launched?" knowable without new configuration?** Yes. The
`--mcp-stdio` flag is parsed in `crates/service/src/cli.rs` (the `mcp_stdio` flag at
`cli.rs:101`, branched into `CliMode::Socket` vs. `CliMode::Mcp` at `cli.rs:194`), and
`main.rs`'s `async_main` matches on that `CliMode` enum *before* calling the shared
`bootstrap_app_state` — the two call sites (`main.rs:1120` for `CliMode::Socket`,
`main.rs:1145` for `CliMode::Mcp { connect: None, .. }`) are already in separate match
arms. Threading that distinction into `bootstrap_app_state` as a parameter is a small,
local, one-function-signature change, not a new concept or new configuration surface.
The original issue's stated cost for option 4 does not hold.

## Decision (confirmed by the maintainer, 2026-08-25)

Given the above, options 2 and 4 as originally described ("bind the socket, serve
read-only and non-embedding operations") are both more expensive and riskier than the
issue assumed, because of the DB-dimension coupling in finding (3) — not because of
process-identity plumbing, which finding (4) shows is cheap. The maintainer confirmed
this inverted their own initial preference for option 2, and adopted the Specify
stage's recommended **combination** of option 3 and a narrowed option 4:

1. **Bounded retry with backoff** before the embedder probe gives up, targeting the
   sidecar/lcg simultaneous-launch race. The retry bound (attempt count, backoff
   shape, total ceiling) is a **Plan-stage decision**, not fixed here — Plan MUST
   choose and explicitly document a ceiling short enough that an MCP client does not
   time out waiting for the server to come up.
2. **When `--mcp-stdio` (standalone) is the launch mode and that retry is exhausted**:
   do not open the database at all. Reuse the *existing* "DB never opened" degraded
   branch verbatim — the same `degraded` / `degraded_reason` / `connected: false` /
   `queryable: false` shape `knowledge_status` already returns for WAL corruption
   today — with a new `degraded_reason` value for "embedder unreachable at startup."
   Gate this on `cli_mode`, **never** on message content. This needs no new
   `AppState`/`Embedder` plumbing and doesn't touch any of the ~15 `.embed()` call
   sites, because none of that code runs while the DB never opened. It delivers
   diagnosability (a client can call `knowledge_status` and learn what's wrong) but
   **not** read-only graph queries — those still require the DB to be open, which
   still requires a committed dimension (see Out of Scope).
3. **The socket-service (hand-started) path keeps today's fail-fast behavior
   unchanged** — no new gating logic beyond the `cli_mode` check already required for
   point 2, satisfying FR-001.
4. **Read-only-while-embedder-degraded is explicitly out of scope**, deferred to a
   separate follow-on (see Out of Scope) — not because it's undesirable, but because
   it requires resolving the dimension-commitment problem across both initial schema
   creation and all three `knowledge_recover` strategies, which is a materially larger
   and riskier change than points 1–3.

**Scope guard for Research/Plan/Implement**: if delivering point 2 turns out to
require touching `AppState.embedder`'s `Arc<dyn Embedder>` non-optionality, or any of
the ~15 `.embed()` call sites in `handlers.rs`, **stop and say so rather than
proceeding**. That would mean the "cheap" version of this option has been
misidentified, and the design needs revisiting — not silently expanding into the
read-only-while-degraded scope this issue explicitly defers.

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
confirm the cause is discoverable from within the MCP session (not only from stderr).

**Acceptance Scenarios**:

1. **Given** no embedder is reachable at the configured transport, **When** the server
   is launched via `--mcp-stdio` and the embedder does not become reachable within the
   Plan-stage-defined retry window, **Then** the process does not exit — it starts in
   a degraded state (DB never opened) and `knowledge_status` reports `degraded: true`
   with a `degraded_reason` naming the embedder as unreachable at startup.
2. **Given** the embedder becomes reachable during the retry window (the race case),
   **When** a retry attempt succeeds, **Then** the server starts normally with no
   degraded state at all.
3. **Given** a hand-started socket-service process with the same misconfiguration,
   **When** it starts, **Then** it retains today's fail-fast behavior unchanged
   (FR-001) — the retry/degrade behavior in scenarios 1–2 applies only to standalone
   `--mcp-stdio` mode.

---

### Edge Cases

- Embedder becomes reachable partway through the retry/backoff window (handled by
  Acceptance Scenario 2 — server starts normally, no degraded state).
- Embedder was reachable at startup but becomes unreachable mid-session (out of scope
  — see Assumptions).
- `LCG_EMBEDDING_DIM` is set: today it already overrides *non-transport* probe
  failures; whether it should also short-circuit the retry/degrade path for a
  transport failure is a Plan-stage detail to resolve, not decided here.
- A degraded MCP-mode server receiving a `knowledge_recover` call before the embedder
  is ever reachable: since the DB never opened, this is the same "no DB to recover"
  shape `knowledge_recover` already has to handle for other degraded-startup causes —
  none of today's three recovery strategies can run without a committed embedding
  dimension, and that remains true here. No new recovery strategy is introduced by
  this issue.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST NOT regress the existing fail-fast startup behavior for
  a hand-started socket-service process (`CliMode::Socket`) — a transport-unreachable
  embedder MUST remain immediately fatal on that path, exactly as it is today.
- **FR-002**: In standalone `--mcp-stdio` mode (`CliMode::Mcp { connect: None, .. }`),
  when the embedder probe reports a transport error, the system MUST retry with
  bounded backoff before giving up. The number of attempts, backoff shape, and total
  ceiling are a Plan-stage decision; the ceiling MUST be short enough that an MCP
  client launching the process does not time out waiting for it to come up, and MUST
  be stated explicitly in the plan.
- **FR-003**: If the retry in FR-002 is exhausted without the embedder becoming
  reachable, the system MUST NOT exit. It MUST start without opening the database,
  reusing the existing "DB never opened" degraded-startup path verbatim (the same
  `degraded` / `connected: false` / `queryable: false` shape `knowledge_status`
  already returns for other startup failures such as WAL corruption), with a new
  `degraded_reason` value that identifies the embedder as unreachable at startup.
- **FR-004**: The gate for FR-002/FR-003 MUST be the launch mode (`CliMode::Socket` vs
  `CliMode::Mcp` with `connect: None`, already resolved in `async_main` per Costing
  finding 4) — never message-content sniffing on the underlying transport error, and
  no new configuration surface for "how was I launched."
- **FR-005**: This issue MUST NOT make embedding-dependent operations available while
  degraded under FR-003 — the DB is not open, so no handler that depends on
  `AppState.db` or `AppState.embedder` can run. Delivering read-only or
  non-embedding operations while embedder-degraded is out of scope (see Out of
  Scope); implementing it is **not** a valid way to satisfy FR-002/FR-003.

### Key Entities

- **AppState.degraded_reason**: Existing `Option<String>` on `AppState`
  (`crates/core/src/app_state.rs:58`) describing why the *database* failed to open;
  currently unrelated to embedder reachability, but reusable per the Decision above
  by adding a new reason string rather than a new field.
- **AppState.embedder**: `Arc<dyn Embedder>` (`crates/core/src/app_state.rs:59`) —
  mandatory, not optional. Per the Decision's scope guard, this issue MUST NOT need to
  change this; if it turns out to, stop and flag it (see Out of Scope).
- **Embedder probe**: The startup check in `bootstrap_app_state`
  (`crates/service/src/main.rs`) that resolves transport and calls
  `probe_embedder.probe()` before schema init / DB open.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user who launches the server via an MCP client with no embedder
  running, and for whom the embedder never becomes reachable within the retry window,
  can determine — via `knowledge_status`, without reading process stderr — that the
  embedder is the problem.
- **SC-002**: A hand-started socket-service process with the same misconfiguration
  continues to fail exactly as it does today: same message, same immediate exit.
- **SC-003**: The retry window is bounded to an explicit, documented ceiling (set in
  the Plan stage) short enough that an MCP client does not time out waiting for the
  server to start.
- **SC-004**: A race where the embedder becomes reachable during the retry window
  results in a normal, non-degraded startup — the retry is not merely cosmetic.

## Assumptions

- Embedder reachability at startup vs. mid-session are treated as separate concerns;
  this issue is scoped to the **startup** probe only (FR-010/FR-011), not to detecting
  or recovering from an embedder that goes away after a successful start.
- The MCP-stdio launch mode (`--mcp-stdio` without `--connect`) is treated as a
  reliable signal of "no one is watching stderr"; a hand-started process that happens
  to redirect its own stderr away is out of scope for special-casing.
- "Start degraded" here means the process starts and names the problem, not that
  read-only or non-embedding operations become available — per the Decision above,
  the embedding-dimension problem identified in Costing finding 3 is deliberately not
  solved by this issue.

## Out of Scope

- Hosted embedder providers reachable without a local sidecar (#497).
- Documentation of embedder configuration (#498).
- Detecting or recovering from an embedder that was reachable at startup but becomes
  unreachable later.
- Standardizing the per-call embedding-error shape across all handlers listed in
  Costing finding 2 — worth doing, but a separate, option-independent cleanup not
  bundled into this issue.
- **Read-only-while-embedder-degraded service** (the full ambition of option 2/4):
  serving graph queries or non-embedding operations while the database is open but the
  embedder is not. This requires resolving the dimension-commitment problem in
  Costing finding 3 across both initial schema creation and all three
  `knowledge_recover` strategies — a materially larger, separate follow-on. Per the
  Decision's scope guard: if Research/Plan/Implement finds that satisfying FR-002/
  FR-003 actually requires this (e.g. touching `AppState.embedder`'s non-optionality
  or any `.embed()` call site), stop and flag it rather than expanding into this
  scope.

## Source References

- `crates/service/src/main.rs` (`bootstrap_app_state`, embedder probe, DB open/schema
  init, `CliMode` dispatch, FR-010/FR-011)
- `crates/service/src/cli.rs` (`--mcp-stdio` flag parsing and `CliMode::Socket` /
  `CliMode::Mcp` branch point)
- `crates/core/src/app_state.rs` (`degraded_reason`, `embedder`)
- `crates/core/src/handlers.rs` (`handle_knowledge_status`, `handle_assert_entity`,
  `handle_create_cross_group_edge`, `handle_find_entities`, `handle_find_relationships`,
  `handle_knowledge_recover`)
- `crates/core/src/embedder.rs` (`Embedder` trait, `is_transport_error`, error message
  shapes)
- README "Two transport surfaces" section (`--mcp-stdio`)
- #497 (hosted embedders), #498 (documentation), #451 (`group_ontology_drift` addition
  to `knowledge_status`, cited as evidence that response is actively maintained)
