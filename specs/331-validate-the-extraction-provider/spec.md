# Feature Specification: validate the extraction provider on first use, not at startup

**Feature Branch**: `fabrik/issue-331`
**Created**: 2026-08-03
**Status**: Draft
**Input**: User description: "Since 0.11.0, lcg-service refuses to start unless an extraction provider is configured. This is a regression for read-only consumers — in 0.9.0 the reader path started fine with no extraction provider; extraction only failed if you actually called knowledge_process_chunk. Move the check from startup to first use."

## Background

Reported in #330 by a downstream consumer ("orac", a laptop MCP reader) against 0.11.0.

Since 0.11.0, `lcg-service` refuses to **start** unless an extraction provider is configured — `ANTHROPIC_API_KEY`, `--extractor-uds/http`, or `LCG_EXTRACTION_URL`. The socket binds, then the process exits (`crates/service/src/main.rs:399`).

This is a regression for read-only consumers. In 0.9.0 the reader path started fine with no extraction provider; extraction only failed if you actually called `knowledge_process_chunk`.

It arrived with #212 (ADR-0041), which fixed bdueck's #201 — *"extraction requires ANTHROPIC_API_KEY despite docs marking it optional"*. That fix was right, and its explicit-selection principle should be preserved. But ADR-0041's reasoning is about **which** provider gets selected — *"selecting it just because the socket exists … would trade a false 'requires a hosted key' claim for an equally misleading 'fully local extraction just works' one"* — not about **when** a missing provider becomes fatal. Hard-failing at startup over-applies it.

### Why the startup check buys nothing

The reporter's observation settles this: **startup does not probe the extractor.** Their shipped workaround is a deliberately unreachable placeholder — `--extractor-http http://127.0.0.1:1/disabled` — which satisfies the check and works, because nothing ever dials it at boot.

So the check validates *configuration presence*, not *reachability*. It cannot detect a misconfigured, unreachable, or unauthorised provider — precisely the failures an operator would want caught early. It only rejects the honest case of "I don't need one."

`--mcp-stdio --scope read` fails the same way, so there is no read-only escape hatch today even when the advertised tool surface contains nothing that can extract.

### Why read-only matters here

Read-only is a first-class use case: serve `knowledge_find_*` / `knowledge_search_passages` / `knowledge_status`, and hydrate a local database from a published WAL via `knowledge_rebuild_from_wal` — pure Cypher replay, no extraction anywhere in the path. Requiring a provider there forces a real API key (or a fake endpoint) onto a path that never extracts, and undercuts the local-first story for pure consumers.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A read-only consumer starts without an extraction provider (Priority: P1)

A downstream reader (e.g. an MCP client that only queries and rebuilds from a published WAL) starts `lcg-service` with an embedder configured but no extraction provider, and the service comes up and serves reads instead of exiting.

**Why this priority**: This is the regression itself — without it, read-only consumers cannot run 0.11.0+ at all without a fake credential or placeholder endpoint.

**Independent Test**: Start `lcg-service --embedder-http <url>` with no `ANTHROPIC_API_KEY`, `--extractor-uds/http`, or `LCG_EXTRACTION_URL` set. Confirm the process stays up, `knowledge_status` responds, and `knowledge_rebuild_from_wal` completes against a WAL fixture.

**Acceptance Scenarios**:

1. **Given** `lcg-service --embedder-http <url>` with no extraction provider configured, **When** the service starts, **Then** it starts and serves read operations.
2. **Given** that service, **When** `knowledge_rebuild_from_wal` is called, **Then** it replays and rebuilds indices normally — no extraction is involved.
3. **Given** `--mcp-stdio --scope read` with no provider, **When** the server starts, **Then** it starts and advertises its read tools.

---

### User Story 2 - Extraction still fails clearly when unconfigured (Priority: P1)

An operator or client calls an extraction-dependent method (e.g. `knowledge_process_chunk`) on a service that has no provider configured, and gets back a clear, actionable IPC/MCP error rather than a startup failure they never see, a panic, or a silent no-op.

**Why this priority**: Moving the check later is only safe if the failure it used to catch is still caught, just later and per-call — otherwise misconfiguration becomes silent instead of loud.

**Independent Test**: Start the service with no provider configured, call `knowledge_process_chunk`, and confirm the response is a clean error naming what to configure, the process is still running afterward, and read operations still succeed.

**Acceptance Scenarios**:

1. **Given** a service started without a provider, **When** `knowledge_process_chunk` is called, **Then** it returns a clear error naming what to configure — the same guidance the startup error gives today.
2. **Given** the same, **When** any other extraction-dependent method is called, **Then** it fails the same way rather than panicking or silently no-op'ing.

---

### User Story 3 - Configured deployments are unchanged (Priority: P1)

An operator who has already configured a provider (via `ANTHROPIC_API_KEY`, `--extractor-uds/http`, or `LCG_EXTRACTION_URL`) sees no behavioural difference at all — same startup, same selection rules, same extraction behaviour.

**Why this priority**: This is the safety net for the change — moving *when* the check runs must not alter *which* provider gets picked or how a configured deployment behaves, per ADR-0041.

**Independent Test**: Run the existing extraction test suite (provider configured via each supported means) unmodified and confirm it passes with identical results to pre-change behaviour.

**Acceptance Scenarios**:

1. **Given** a provider configured by any supported means, **When** the service starts and ingests, **Then** behaviour is identical to today, including ADR-0041's explicit-selection rules.

---

### Edge Cases

- A provider configured but unreachable must still fail at call time, not startup — this issue must not accidentally introduce a startup probe while moving the check.
- Scope-gated MCP mode: `--scope all` on a service with no provider should still start (FR-001), with extraction tools present but erroring on use — advertising a tool that cannot work is preferable to refusing to boot, but say so deliberately rather than by accident.
- Cassette replay (`LCG_REPLAY_LLM`) is an extraction provider of sorts; confirm it satisfies the check and that a replay-only deployment needs nothing else.
- Degraded-mode startup (ADR-0009) already has a no-database path; make sure the two "start anyway" behaviours compose rather than conflict.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A missing extraction provider MUST NOT prevent startup, in either socket or `--mcp-stdio` mode, at any scope.
- **FR-002**: Extraction-dependent operations MUST fail with an actionable error when no provider is configured. Reuse the current startup message's guidance — it is good; only its timing is wrong.
- **FR-003**: Enumerate every method that requires the extractor and state it in the PR. At minimum `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`. Confirm whether `knowledge_canonicalize_relations` does — its ontology-description fallback embeds, which is a different dependency.
- **FR-004**: ADR-0041's explicit-selection behaviour MUST be preserved exactly: a reachable local sidecar is still never silently preferred over a configured `ANTHROPIC_API_KEY`, and no provider is auto-detected. This issue defers *when the absence becomes fatal*; it does not relax *how one is chosen*.
- **FR-005**: The error MUST remain a clean IPC/MCP error, not a panic or a process exit — a read-only service must survive an errant write call.
- **FR-006**: Update the README and `docs/configuration.md` to state that an extraction provider is required only for extraction operations, and that read-only deployments need none. The current docs describe the startup requirement.
- **FR-007**: Add a regression test that starts the service with no provider, performs a read and a `rebuild_from_wal`, then asserts an extraction call errors cleanly.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `lcg-service --embedder-http <url>` with no provider starts, serves reads, and completes `knowledge_rebuild_from_wal`.
- **SC-002**: `--mcp-stdio --scope read` starts with no provider.
- **SC-003**: `knowledge_process_chunk` with no provider returns an actionable error; the process stays up and continues serving reads afterwards.
- **SC-004**: A deployment with a provider configured shows no behavioural change — verified against the existing extraction tests.
- **SC-005**: The placeholder workaround from #330 (`--extractor-http http://127.0.0.1:1/disabled`) is no longer necessary, and #330's four acceptance boxes all tick.

## Assumptions

- The extraction provider check moves from a startup-time hard error to a call-time check performed by each extraction-dependent method, rather than becoming a lazily-initialized-but-still-fatal-once check — every extraction-dependent call must independently produce the clear error when unconfigured, for as long as the service remains unconfigured.
- The embedder remains a required-at-startup dependency; this issue only changes the extraction provider's timing. `--embedder-http <url>` (or equivalent) is still needed to start, per the existing SC-001 wording.
- "Read-only" in this issue means "does not require the extraction provider" — `knowledge_rebuild_from_wal` and other WAL/index-maintenance operations that don't call the extractor are in scope for "starts and works without a provider," even though they are writes to the local database, not reads in the API sense.
- Cassette replay (`LCG_REPLAY_LLM`) counts as a configured extraction provider for purposes of the startup/call-time check, consistent with its current role as a provider substitute.

## Out of Scope

- Changing which provider is selected when several are configured (ADR-0041).
- Adding a `--read-only` flag (#330's option 3) — option 1 subsumes it. If a flag is later wanted for tool-surface reasons, that is a separate issue.

## Source References

- #330 — the report, with repro and the placeholder-workaround observation
- #212 / ADR-0041 — introduced the startup requirement; its selection rules must survive unchanged
- #201 — bdueck's original report that #212 answered
- `crates/service/src/main.rs:399` — the current startup-time fatal check
