# Feature Specification: Bounded Timeouts for `OaiEmbedder`'s UDS Transport

**Feature Branch**: `fabrik/issue-541`
**Created**: 2026-09-05
**Status**: Specified
**Input**: User description: "UDS embedder transport has no timeout — the same unbounded hang #510 fixed for HTTP, on the default path"

## Background

ADR-0510 (#510) bounded `OaiEmbedder`'s **HTTP** transport with a whole-request timeout
(`LCG_EMBEDDING_TIMEOUT_MS`) and a connect-phase timeout (`LCG_EMBEDDING_CONNECT_TIMEOUT_MS`),
because an embedder backend that accepts a connection and never responds hangs the calling task
indefinitely. That ADR explicitly scoped out the UDS transport:

> The UDS transport (`EmbedTransport::Uds`) has the identical hazard on its hand-rolled hyper
> `send_request`/read path, but is explicitly out of scope here.

This issue closes that gap.

**UDS is the default path, not the fallback.** `main.rs`'s transport resolution order is:

1. `--embedder-uds` / `--embedder-http` CLI flag
2. the default UDS socket `/tmp/liminis-inference.sock`, **if it exists**
3. `LCG_EMBEDDING_URL`
4. hard error

Anyone running the bundled Swift sidecar — the documented macOS setup in `native/local-inference/`
— resolves to UDS and is **not** covered by #510's fix. The hazard #510 exists to close is still
fully present on the most common deployment.

The consequence is the one #510 describes: an embedder that accepts a connection and then never
responds hangs the calling task indefinitely, and since #444 (PR #487) reordered the assert
handlers' embed-before-existence-check, that hang holds the **instance-wide write lock** for its
duration. #526 makes this worse again by removing the WAL's stored vectors and #440's fallback, so
every WAL replay now recomputes — thousands of embedder calls on a real corpus (4,126 vectors
across 12,482 records on the #217 capture), each one an opportunity to hang.

`send_and_read_uds` (in `crates/core/src/embedder.rs`) uses a hand-rolled hyper connection pool
rather than `reqwest`, so `Client::builder()`'s `timeout`/`connect_timeout` — the mechanism #510
used — cannot reach it. Confirmed by direct inspection: neither `dial_uds` (dials a fresh
`UnixStream`, performs the HTTP/1.1 handshake) nor `send_and_read_uds` (sends the request, reads
the response) nor `do_embed_uds_raw`'s pool-slot acquisition (`pool.slots[idx].lock().await`) has
any bound today. It needs its own mechanism.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Hung UDS backend fails fast instead of hanging forever (Priority: P1)

An operator runs the default macOS setup (bundled Swift sidecar over UDS). The sidecar accepts a
connection but then stops responding — a deadlock, a crash mid-response, a cold-start stall. Today
this hangs the calling task indefinitely and, on the create path, holds the instance-wide write
lock for the duration. With this fix, the call fails within a bounded, configurable time, the lock
is released, and the failure is classified the same way other UDS connectivity failures already
are.

**Why this priority**: This is the core hazard the issue exists to close, and it applies to the
default (not fallback) deployment path.

**Independent Test**: Stand up a stub UDS server that accepts a connection and never writes a
response. Call `embed()` (and separately `probe()`) against it and assert the call returns an
error within the configured bound rather than hanging, that `is_transport_error` classifies the
error as a transport error, and — via an integration test analogous to
`concurrent_rw_integration.rs` — that `state.write_lock` is released within the same bound.

**Acceptance Scenarios**:

1. **Given** a UDS embedder configured with a short whole-request timeout, **When** the backend
   accepts the connection and never responds, **Then** the `embed()`/`probe()`/`embed_batch()`
   call fails within that configured bound instead of hanging.
2. **Given** the failure in Scenario 1 occurs on the assert-entity create path (inside
   `state.write_lock`), **When** the timeout fires, **Then** the write lock is released within the
   same bound — no other write is blocked indefinitely by this one hung call.
3. **Given** the failure in Scenario 1, **When** the resulting error is inspected, **Then**
   `is_transport_error` returns `true` for it, so `main.rs`'s fatal-vs-bypass startup logic and
   #499's `--mcp-stdio` retry loop continue to work unmodified.
4. **Given** a UDS embedder responding normally within the configured bounds, **When** any embed
   call is made, **Then** behavior and results are identical to today — no regression for the
   working case.

---

### User Story 2 - A stuck connection pool fails fast instead of queuing forever (Priority: P2)

The UDS transport uses a small fixed pool of connection slots. If every slot is currently occupied
by a call that is itself hung (Story 1's scenario, in progress), a new caller waiting to acquire a
slot would otherwise queue forever — the same hazard by a different route, since the pool never
yields a usable connection back.

**Why this priority**: Bounding only the request/response phase leaves this second path to the
identical hang; the issue's scope explicitly calls this out as an equal-priority requirement, not
an afterthought.

**Independent Test**: Occupy all pool slots with calls hung against a stub server (Story 1's
server), then issue one more call and assert it fails within a bounded time rather than blocking
on pool-slot acquisition indefinitely.

**Acceptance Scenarios**:

1. **Given** every pool slot is held by a call stuck against a hung backend, **When** a new call
   is issued, **Then** it fails within a bounded, configurable time rather than waiting
   indefinitely for a slot to free up.
2. **Given** dialing a fresh connection (socket connect + HTTP/1.1 handshake) stalls, **When** a
   call needs to populate an empty or cleared pool slot, **Then** that dial also fails within a
   bounded, configurable time rather than hanging.

---

### Edge Cases

- Backend accepts the connection and hangs before sending any response bytes (the primary repro
  scenario for both stories).
- Backend accepts the connection, sends response headers, then hangs mid-body.
- Backend stalls during the HTTP/1.1 handshake itself, after the raw socket connect succeeds but
  before the connection is usable.
- A call that times out mid-request must not leave its pool slot in a state where the *next*
  caller silently reuses a connection with indeterminate framing (partially written request,
  unread stale response) — existing `PoisonGuard` behavior already clears a slot when a future is
  cancelled mid-flight; a timeout needs the same treatment.
- All pool slots simultaneously stuck (Story 2) vs. only one stuck while others remain usable —
  the latter must not be affected at all (round-robin picks a different, healthy slot).
- The existing single-retry-on-broken-connection behavior (a `send_request` failure that indicates
  a dead connection, as opposed to a hang) must remain distinct from the new timeout behavior and
  keep working as it does today.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A UDS embedder call whose `send_request` + response-read sequence exceeds a bounded,
  configurable duration MUST fail with an error instead of hanging indefinitely.
- **FR-002**: The bound in FR-001 MUST be configurable via the same environment variable already
  used for the HTTP transport's whole-request bound, `LCG_EMBEDDING_TIMEOUT_MS` — no new,
  UDS-specific variable name.
- **FR-003**: Acquiring a usable connection for a UDS embedder call — dialing a fresh connection
  (socket connect + HTTP/1.1 handshake) and/or waiting for a pool slot to become available — MUST
  also be bounded by a configurable duration; it must not be possible for pool exhaustion alone to
  hang a caller indefinitely.
- **FR-004**: The bound in FR-003 MUST be configurable via the same environment variable already
  used for the HTTP transport's connect-phase bound, `LCG_EMBEDDING_CONNECT_TIMEOUT_MS` — no new,
  UDS-specific variable name.
- **FR-005**: An error produced by FR-001 or FR-003 firing MUST be classified as a transport error
  by `is_transport_error`, matching how existing UDS connectivity failures (connect failure,
  handshake failure, send-request-to-a-dead-connection failure) are already classified.
- **FR-006**: When a timeout occurs while `state.write_lock` is held (the create-path flow
  described in ADR-0510/#487), the lock MUST be released within the same bound as the timeout
  itself — no unbounded step may follow a timeout before the lock is dropped.
- **FR-007**: A connection on which a timeout fired MUST NOT be silently reused for a subsequent
  call; the pool slot involved must be treated as broken (cleared, requiring a fresh dial next
  use), consistent with how a mid-flight-cancelled call is already handled.
- **FR-008**: A UDS embedder that responds within the configured bounds MUST behave identically to
  today — same results, same existing single-retry-on-broken-connection behavior for a definite
  (non-hang) connection failure.
- **FR-009**: When either `LCG_EMBEDDING_TIMEOUT_MS` or `LCG_EMBEDDING_CONNECT_TIMEOUT_MS` is
  unset, the UDS transport's defaults MUST match the HTTP transport's defaults established in
  #510 (30000ms whole-request / 5000ms connect-phase), so the two transports share one mental
  model.
- **FR-010**: An unparseable, zero, or negative value for either environment variable MUST be
  rejected at embedder construction time with a clear configuration error — mirroring #510's
  strict-validation behavior for the HTTP transport — rather than silently ignored.

### Key Entities

- **UDS connection pool slot**: One of a fixed number of reusable hyper HTTP/1.1 connections over
  a Unix domain socket, dialed lazily and round-robin-selected per call.
- **Timeout bound**: One of two independently configurable durations (whole-request,
  connect/acquisition) shared by name and default value with the HTTP transport.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A UDS embedder that accepts a connection and never responds causes an `embed()`,
  `embed_batch()`, or `probe()` call against it to fail within the configured
  `LCG_EMBEDDING_TIMEOUT_MS` bound, verified by an automated regression test that measures elapsed
  wall time (mirroring #510's `http_transport_hung_backend_times_out_on_embed`).
- **SC-002**: A UDS embedder call blocked solely on pool-slot/connection acquisition (every slot
  occupied by a hung call, or a dial that itself hangs) fails within the configured
  `LCG_EMBEDDING_CONNECT_TIMEOUT_MS`-derived bound, verified by an automated regression test.
- **SC-003**: `state.write_lock` is demonstrably released within the bound after a UDS timeout on
  the create path, verified by an integration test analogous to `concurrent_rw_integration.rs`.
- **SC-004**: The full existing UDS transport test suite (connection reuse, retry-on-broken
  -connection, pool round-robin, response parsing) continues to pass unmodified, demonstrating no
  behavior change for a normally-responding backend.
- **SC-005**: `is_transport_error` returns `true` for every error introduced by this change,
  verified by a unit test alongside the existing `is_transport_error_recognizes_uds_*` tests.

## Assumptions

- `LCG_EMBEDDING_TIMEOUT_MS` bounds the send-request-and-read-response span of a single attempt
  (FR-001); `LCG_EMBEDDING_CONNECT_TIMEOUT_MS` bounds dialing a fresh connection and/or waiting to
  acquire a pool slot (FR-003). The exact mechanism (e.g. one `tokio::time::timeout` per phase vs.
  a combined budget) is a Research/Plan-stage decision; this spec constrains only the observable
  bound, not the implementation shape.
- The existing single-retry-on-broken-connection behavior (a dead connection detected via a
  failed `send_request`) is orthogonal to this change and is assumed to keep its current shape:
  each attempt (initial and retry) gets its own timeout window rather than sharing one budget
  across both.
- A timeout is treated as equivalent to "connection broken" for pool-hygiene purposes (FR-007) —
  the slot is cleared rather than reused — since the connection's framing state after a partial
  write or partial read is indeterminate, the same reasoning already applied to a mid-flight
  cancelled future via `PoisonGuard`.
- Default values and validation behavior for both environment variables are inherited unchanged
  from #510 (ADR-0510) rather than re-derived for UDS specifically, per the issue's explicit intent
  to reuse #510's env-var contract "for consistency."
- This issue covers only `OaiEmbedder`'s UDS transport (`crates/core/src/embedder.rs`). See Out of
  Scope for a structurally similar but distinct UDS transport this issue does not touch.

## Out of Scope

- **The UDS transport used for LLM extraction calls** (`crates/core/src/extractor.rs`'s
  `send_and_read_uds`, serving `/v1/chat/completions`). It has the identical hang hazard on the
  identical hand-rolled hyper pool pattern, but is a separate code path serving a different
  purpose (extraction, not embedding) with its own error-classification shape
  (`UdsAttemptError::HttpStatus`/`Malformed`, from #306). Worth a follow-up issue but not folded
  into this one.
- **Narrowing `state.write_lock`'s critical section.** ADR-0510 already deferred this as a
  separate, structural fix (dropping the lock during the embed call and re-resolving under a
  freshly-acquired guard). This issue only bounds the hold duration for the UDS path, matching
  what #510 did for HTTP.
- **Adding retry-on-timeout behavior.** This issue bounds how long a hang can hold the write lock;
  it does not add a retry for a timed-out (as opposed to a definitely-broken) connection, matching
  #510's equivalent exclusion for HTTP.
- **New environment variables.** No UDS-specific timeout variables are introduced; the existing
  HTTP-transport names are reused per the issue's explicit direction.

## Source References

- ADR-0510 (`docs/adr/0510-oaiembedder-http-timeouts.md`) — the HTTP-side fix this issue mirrors,
  including its explicit "Out of Scope" callout for UDS.
- `crates/core/src/embedder.rs` — `dial_uds`, `send_and_read_uds`, `UdsAttemptError`,
  `PoisonGuard`, `do_embed_uds_raw`, `UdsPool`, `is_transport_error`.
- `crates/core/tests/embedder_transport.rs` — existing UDS transport test suite and #510's HTTP
  timeout tests (`http_transport_hung_backend_times_out_on_embed`,
  `http_transport_connect_timeout_independent_of_request_timeout`) to mirror on the UDS side.
- `crates/core/tests/concurrent_rw_integration.rs` — existing write-lock-release test pattern to
  mirror for SC-003.
- Issue #510 / ADR-0510, #444 / PR #487 (write-lock ordering), #499 (`--mcp-stdio` retry loop,
  `EMBEDDER_RETRY_CEILING`), #526 (WAL replay recomputing every vector).
