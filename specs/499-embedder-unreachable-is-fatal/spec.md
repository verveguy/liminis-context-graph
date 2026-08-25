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
thing launching the process.

### Options under consideration

1. **Leave it.** Fail fast is defensible and the message is good. Push the fix into
   documentation (#498) and accept that a first run without an embedder is a hard
   stop.
2. **Start degraded.** Bind the socket, serve read-only and non-embedding operations,
   and report the embedder as unavailable through `knowledge_status` — which already
   carries `connected`, `queryable`, `initializing` and a `degraded_reason` for
   *database*-level degradation, so the vocabulary exists, though an embedder-level
   signal does not yet (see Open Questions). Embedding-dependent calls return a clear
   per-call error naming the missing embedder. The user gets a server that starts,
   plus a tool that explains exactly what is wrong.
3. **Retry with backoff before giving up**, covering the common race where the client
   launches lcg and the sidecar simultaneously.
4. **Degrade only when MCP-launched**, keeping fail-fast for a hand-started service.

### Correction to the original issue framing

The issue text describes option 4 as requiring "the process to know how it was
started, which is a new concept." That is not accurate as of this repository's current
state: `--mcp-stdio` is already a real, documented CLI flag
(`crates/service/src/main.rs`, README's "Two transport surfaces" section), and
standalone MCP mode (`--mcp-stdio` without `--connect`) already shares
`bootstrap_app_state` — and therefore the fatal embedder probe — byte-for-byte with the
socket-service path. The process already knows, from its own argv, whether it was
launched in `--mcp-stdio` mode. This makes option 4 more directly implementable than
the issue assumes, and is a live alternative to a blanket option 2.

Relates to #497 (hosted embedders, which would make first run far more likely to
succeed) and #498 (documenting the config).

## Open Questions

- [ ] **Which option should this issue implement?** (1) leave fail-fast behavior
  unchanged and defer to documentation in #498; (2) start degraded unconditionally,
  regardless of launch mode; (3) retry with bounded backoff before giving up; (4)
  start degraded only in `--mcp-stdio` mode, keeping fail-fast for the socket-service
  path; or some combination (e.g., bounded retry followed by degraded start, gated on
  `--mcp-stdio`). See "Correction to the original issue framing" above — option 4 does
  not require new process-identity plumbing, since `--mcp-stdio` already exists as a
  launch-mode signal.
- [ ] **If a "start degraded" option (2 or 4) is chosen**, what should the
  `knowledge_status` response look like for embedder-level degradation? Today
  `degraded`/`degraded_reason`/`connected`/`queryable` describe *database*-level
  degradation only, surfaced when `state.db` is `None` (see
  `crates/core/src/handlers.rs` `handle_knowledge_status`); the embedder probe
  currently happens *before* `AppState` is constructed at all, so there is no existing
  slot for "DB is healthy but embedder is unreachable." Should this be a new top-level
  field (e.g. `embedder_available` / `embedder_degraded_reason`), and should the
  per-call error returned by embedding-dependent operations reuse that same reason
  string?
- [ ] **If retry-with-backoff (option 3) is chosen** (alone or combined with a
  degraded-start option), what bound (max attempts, total wait, or both) should apply
  before giving up? Should the retry window apply only in `--mcp-stdio` mode or
  universally?
- [ ] Should any change here also apply to the **socket-service path** when hand-started
  (i.e., should fail-fast be preserved there even if MCP mode changes), or should the
  behavior become uniform across both launch modes?

## User Scenarios & Testing *(mandatory — pending option decision)*

The concrete user scenarios depend on which option above is selected; they will be
written out fully once that decision is made. Placeholder framing for the leading
candidate (option 2/4, "start degraded"):

### User Story 1 - MCP client starts the server without an embedder running (Priority: P1)

A user configures an MCP client (e.g., Claude Desktop, an editor's MCP integration) to
launch `liminis-context-graph --mcp-stdio` without first starting the embedder
sidecar. Today the process exits immediately and the client shows only a generic
"server failed to start" — the actionable stderr message is never seen. The desired
outcome is that the user can inspect *why* the tool isn't fully working from within
the MCP session itself (e.g., via `knowledge_status` or a clear per-call error),
rather than the tool appearing entirely broken.

**Why this priority**: This is the exact failure mode reported from smoke-testing the
v0.13.4 release artifact, and it is the primary friction point blocking first-run
success for MCP-launched usage.

**Independent Test**: Launch the server via `--mcp-stdio` with no embedder reachable;
confirm the server starts (or fails) in a way whose cause is discoverable from within
the MCP session, per whichever option is selected.

**Acceptance Scenarios**:

1. **Given** no embedder is reachable at the configured transport, **When** the server
   is launched via `--mcp-stdio`, **Then** the resulting behavior matches the selected
   option (fails fast with a message forwarded to the client if feasible; or starts
   and reports the embedder as unavailable; or retries before falling back).
2. **Given** the server is running in whatever degraded/retry state was selected,
   **When** the embedder becomes reachable, **Then** [behavior TBD by option — e.g.
   does the server recover on next embed call, or does it require a restart?].

---

### Edge Cases

- Embedder becomes reachable partway through a retry/backoff window.
- Embedder was reachable at startup but becomes unreachable mid-session (out of scope
  per Assumptions below, but worth confirming explicitly).
- Hand-started socket-service process launched with the same misconfiguration — should
  not regress today's fail-fast behavior unless the chosen option says otherwise.

## Requirements *(mandatory — pending option decision)*

### Functional Requirements

- **FR-001**: The system MUST NOT regress the existing fail-fast startup behavior for
  a hand-started socket-service process unless the selected option explicitly changes
  it for that path too.
- **FR-002**: Whatever behavior is selected for `--mcp-stdio` mode MUST make the cause
  of an embedder-unreachable condition discoverable by the MCP client or its user
  through the running session (e.g., via `knowledge_status` or a per-call error),
  since stderr is not visible to that audience.
- **FR-003** *(pending option decision)*: [Exact startup/retry/degraded behavior to be
  filled in once an option is selected.]

### Key Entities

- **AppState.degraded_reason**: Existing `Option<String>` on `AppState`
  (`crates/core/src/app_state.rs`) describing *why the database* failed to open;
  currently unrelated to embedder reachability.
- **Embedder probe**: The startup check in `bootstrap_app_state`
  (`crates/service/src/main.rs`) that resolves transport and calls
  `probe_embedder.probe()` before `AppState` is constructed.

## Success Criteria *(mandatory — pending option decision)*

### Measurable Outcomes

- **SC-001**: A user who launches the server via an MCP client with no embedder
  running can determine, without reading process stderr, that the embedder is the
  problem.
- **SC-002** *(pending option decision)*: [Concrete, technology-agnostic success
  measure once the option is chosen — e.g. "the server binds and answers
  `knowledge_status` within N seconds even with no embedder reachable" for option 2/4,
  or "the documented retry window is bounded and predictable" for option 3.]

## Assumptions

- Embedder reachability at startup vs. mid-session are treated as separate concerns;
  this issue is scoped to the **startup** probe only (FR-010/FR-011), not to detecting
  or recovering from an embedder that goes away after a successful start.
- The MCP-stdio launch mode (`--mcp-stdio`) is already a reliable signal of "no one is
  watching stderr"; a hand-started process that happens to redirect its own stderr
  away is out of scope for special-casing.

## Out of Scope

- Hosted embedder providers reachable without a local sidecar (#497).
- Documentation of embedder configuration (#498).
- Detecting or recovering from an embedder that was reachable at startup but becomes
  unreachable later.

## Source References

- `crates/service/src/main.rs` (`bootstrap_app_state`, embedder probe, FR-010/FR-011)
- `crates/core/src/app_state.rs` (`degraded_reason`)
- `crates/core/src/handlers.rs` (`handle_knowledge_status`)
- README "Two transport surfaces" section (`--mcp-stdio`)
- #497 (hosted embedders), #498 (documentation)
