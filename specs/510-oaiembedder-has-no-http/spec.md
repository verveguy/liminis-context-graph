# Feature Specification: OaiEmbedder HTTP transport has no request/connect timeout

**Feature Branch**: `fabrik/issue-510`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "OaiEmbedder has no HTTP timeout: a hung embedder now holds the instance-wide write lock indefinitely"

## Background

`OaiEmbedder`'s HTTP transport is built with `Client::new()` (`crates/core/src/embedder.rs`),
which is reqwest's default client — **no request timeout and no connect timeout**. An embedder
endpoint that accepts a connection and then never responds hangs the calling task indefinitely.

This matters more since #487 than it used to. Before #487 (issue #444), `handle_assert_entity`
called the embedder *before* acquiring `state.write_lock`, so a hung embedder stalled only that
one request. #487 correctly moved the embed call onto the create branch, which sits **inside**
the write-lock critical section — `write_lock` is what stops two concurrent creates of the same
not-yet-existing `(name, group_id)` from both resolving "not found" and both inserting, and that
reordering was deliberate. But the trade-off #487 documented was *throughput* (every create-path
call now serializes unrelated writes for the duration of an embedder round-trip); with no
timeout, that duration is unbounded. A hung embedder now holds the instance-wide write lock
indefinitely, and every write on the instance blocks behind it.

The missing timeout predates #487 — #487 only widened its blast radius from one request to the
whole instance.

Note the interaction with #445 (embedding batch requests): a single HTTP call may now carry many
texts at once (`LCG_EMBED_BATCH_SIZE`, default 64) and is legitimately slower than a single-text
call. A timeout tuned only for single embeds could spuriously fail large batches, so whatever
default timeout is chosen must remain compatible with the batch path.

No hang has been observed in production; this is reasoning from the code, not from an incident.
Severity depends on how reachable a hung-but-connected embedder actually is in practice.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Hung embedder no longer wedges the instance (Priority: P1)

As an operator running the LCG service against an embedder backend that can hang while still
holding its connection open (network partition mid-response, backend deadlock, a cold-starting
local sidecar that accepts the socket but never replies), I want a stuck embed call to fail
within a bounded time instead of blocking forever, so that one bad embedder round-trip cannot
stall every other write on the instance.

**Why this priority**: This is the only scenario in the issue. Without it, `state.write_lock`
can be held indefinitely by a single request, taking down write availability for the whole
instance until the process is restarted.

**Independent Test**: Point `OaiEmbedder`'s HTTP transport at a test listener that accepts the
TCP connection and never writes a response (or never completes the handshake, for the
connect-timeout case). Confirm the call returns an error within the configured bound rather than
hanging, and that a concurrent write on a different entity is not blocked by it once the bound
elapses.

**Acceptance Scenarios**:

1. **Given** an embedder HTTP backend that accepts the connection but never sends a response,
   **When** `handle_assert_entity`'s create path calls `embed()` (or `embed_batch()`),
   **Then** the call fails with a bounded-time error instead of hanging, and `state.write_lock`
   is released for other writers.
2. **Given** an embedder HTTP backend that never completes the TCP/TLS handshake,
   **When** an embed or probe call is made,
   **Then** the call fails within the configured connect-timeout bound, distinct from the
   whole-request bound.
3. **Given** the default `LCG_EMBED_BATCH_SIZE` (64) and a backend responding at normal latency,
   **When** a batch embed call is made,
   **Then** it completes successfully and is not spuriously failed by the default timeout.
4. **Given** an operator sets an override for the request or connect timeout via environment
   variable, **When** the `OaiEmbedder` HTTP client is constructed, **Then** it uses the
   overridden value instead of the built-in default.

---

### Edge Cases

- Backend accepts the connection and never responds at all (the issue's core scenario) — must
  produce a bounded-time, timeout-classified error, not an indefinite hang.
- Backend accepts a TCP connection but stalls before completing the HTTP/TLS handshake — the
  connect timeout must trigger independently of the whole-request timeout.
- A large batch request that legitimately takes longer than a single-item round-trip, but is
  still within normal backend latency, must not be spuriously failed by the default timeout.
- An invalid override value (unparseable, zero, or negative) for either timeout must be rejected
  clearly rather than silently ignored or causing a panic.
- Behavior for `MockEmbedder`, `HashEmbedder`, `NameMapEmbedder`, `CountingEmbedder`, and
  `UnconfiguredEmbedder` is unaffected — none of them perform network I/O.
- The UDS transport (`EmbedTransport::Uds`) has the same class of hazard (an unbounded
  `send_request`/read) but is explicitly out of scope for this issue — see Out of Scope.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `OaiEmbedder`'s HTTP transport MUST enforce a bounded whole-request timeout on
  every HTTP call it makes (single embed, batch embed, and the startup probe), so a backend that
  accepts a connection and never responds cannot block the caller indefinitely.
- **FR-002**: `OaiEmbedder`'s HTTP transport MUST additionally enforce a bounded connect timeout,
  distinct from the whole-request timeout, so a backend that accepts a connection but never
  completes the handshake still fails within a bounded time.
- **FR-003**: Both timeouts MUST be independently configurable via environment variables, with
  built-in defaults used when unset, following this file's existing `LCG_EMBEDDING_*` naming and
  `lcg_env_var` two-tier lookup convention (see `LCG_EMBEDDING_URL`, `LCG_EMBEDDING_MODEL`,
  `LCG_EMBEDDING_DIM`).
- **FR-004**: The default whole-request timeout MUST be generous enough to accommodate a
  default-sized batch call (`LCG_EMBED_BATCH_SIZE`, default 64 texts) under normal backend
  latency without spurious failure — it must not regress #445's batching feature.
- **FR-005**: A call that exceeds either timeout MUST continue to be classified as a
  transport/connectivity failure by `is_transport_error` (reqwest already reports timeout errors
  as `is_timeout()`, which that function already checks), so existing fatal-vs-bypass startup
  logic in `main.rs` and the `--mcp-stdio` retry loop (#499) keep working unmodified.
- **FR-006**: An invalid override value for either timeout (unparseable, zero, or negative) MUST
  be rejected with a clear error, consistent with how other numeric env vars in this file are
  validated (e.g. `resolve_embed_batch_size`'s handling of `LCG_EMBED_BATCH_SIZE`), rather than
  silently falling back to a default or panicking.
- **FR-007**: The UDS transport (`EmbedTransport::Uds`) MUST NOT be modified by this change — its
  behavior is explicitly out of scope (see Out of Scope).
- **FR-008**: Existing deployments that do not set either timeout env var MUST see no behavioral
  change for a normally-responsive embedder — the defaults must not tighten reachable, real-world
  latencies into spurious failures.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test embedder backend that accepts a connection and never responds causes the
  calling write-path request to fail, and `state.write_lock` to be released, within the
  configured timeout bound — not indefinitely.
- **SC-002**: A test embedder backend that never completes the connection handshake causes the
  call to fail within the configured connect-timeout bound, and that bound is independently
  reachable from the whole-request bound (i.e. the two can be configured to different values and
  each is observed to take effect on its own).
- **SC-003**: A default-sized batch embed call (64 texts) against a backend exhibiting normal
  per-item latency completes successfully without hitting the default timeout.
- **SC-004**: Both timeout values are overridable via environment variables and take effect
  without any code change, verified by constructing an `OaiEmbedder` with each override set.
- **SC-005**: All existing `embedder.rs` tests continue to pass unmodified except where they
  directly assert on the previous `Client::new()` construction.

## Assumptions

- The exact default duration values for the whole-request and connect timeouts, and the exact
  environment variable names, are implementation decisions for the Research/Plan stage — this
  spec constrains them functionally (independently configurable, batch-compatible default) but
  does not fix specific numbers or names.
- Env var naming will follow the `LCG_EMBEDDING_*` convention already established in this file,
  and may (at Plan stage's discretion) include a deprecated `GRAPHITI_*`-style alias consistent
  with the existing three-tier lookup pattern used elsewhere in `embedder.rs` — this is not
  required by this spec.
- reqwest's `Client::builder().timeout(...)`/`.connect_timeout(...)` produce errors that
  `reqwest::Error::is_timeout()` reports `true` for, which `is_transport_error` already checks;
  Research stage should confirm this holds for both the whole-request and connect-timeout cases
  before relying on FR-005.
- No production hang has been observed; this issue is preventative, reasoned from the code path
  rather than from an incident.

## Out of Scope

- **Narrowing the write-lock critical section.** The issue names, as a structurally better but
  separate fix, dropping `state.write_lock` during the embed call and re-resolving under a
  freshly-acquired guard immediately before insert (falling back to the update path if another
  writer won the race). That removes the unbounded-hold hazard structurally rather than merely
  bounding it, and is explicitly framed in the issue as a follow-up, not part of this fix. May be
  filed as a separate issue.
- **UDS transport timeout hardening.** `EmbedTransport::Uds`'s `send_request`/read path
  (`send_and_read_uds`) has the same class of hazard (no timeout on `sender.send_request` or on
  reading the response body) but uses a hand-rolled hyper connection pool, not reqwest, and is not
  addressed by this issue's proposed fix (`Client::builder()`). May be filed as a separate issue
  if judged worth the same protection.
- **Retry-on-timeout behavior.** This issue is about bounding how long a hang can hold the write
  lock, not about retrying a timed-out request.
- **Automatic backend-type detection or dynamic timeout tuning.** A static, env-var-configurable
  default is sufficient; per-backend-type auto-tuning (e.g. detecting a CoreML sidecar vs. a
  remote API) is not required.

## Source References

- `crates/core/src/embedder.rs` — `OaiEmbedder::new_http`, `do_embed_http_raw`,
  `resolve_embed_batch_size`, `is_transport_error`, `lcg_env_var`
- PR #487 (issue #444) — moved the embed call inside `state.write_lock`'s critical section on the
  create path
- Issue #445 — added batch embedding (`embed_batch`, `LCG_EMBED_BATCH_SIZE`)
- Issue #499 — `--mcp-stdio` startup retry loop that depends on `is_transport_error`'s
  classification of timeout errors
