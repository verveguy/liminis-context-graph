# ADR-0039: UDS Embedder Connection Pooling

**Status**: Accepted
**Date**: 2026-07-25
**Context**: Issue #229
**Supersedes**: ADR-0016 (UDS transport section only)

## Context

ADR-0016 introduced the UDS transport for `OaiEmbedder` and deliberately chose fresh-per-call
semantics: every embed call dials a new `UnixStream`, performs a full HTTP/1.1 handshake via
`hyper::client::conn::http1::handshake`, and spawns a detached tokio task to drive the
connection, then drops all of it once the response is read. ADR-0016 justified this on
cancellation-safety grounds — "dropping the future closes the stream with no leaked state."

Since then, embedding volume has grown to the point where this per-call cost dominates: a
single ingest crossing the 1000-entity threshold performs thousands of dial/handshake/spawn
cycles, each paying a socket-pair allocation, an HTTP/1.1 handshake round-trip, and an unjoined
tokio task. The `Http` transport variant, by contrast, has always used a pooled `reqwest::Client`
— the asymmetry between the two `EmbedTransport` variants was unintentional, not a deliberate
per-transport design choice.

Real-corpus capture runs (#217, the release gate) have separately shown the sidecar becoming
unreachable partway through several long ingests. This is not proven to be caused by the
per-request dial/handshake/spawn pattern, but the churn is worth eliminating regardless.

## Decision

### Small fixed pool (N=4), not a single held connection

`EmbedTransport::Uds` gains a `UdsPool`: a fixed-size array of 4 slots
(`Vec<tokio::sync::Mutex<Option<SendRequest<Full<Bytes>>>>>`), each holding at most one
established `hyper::client::conn::http1::SendRequest`, selected round-robin via an
`AtomicUsize` cursor. `UDS_POOL_SIZE = 4` is a tunable constant, not a value derived from
sidecar capacity measurement — the Swift/CoreML sidecar's own concurrency model is undocumented
in this repo. If a real-corpus run surfaces evidence that 4 over- or under-loads the sidecar,
this constant is the place to revisit.

A single held connection was rejected outright rather than attempted first: HTTP/1.1 without
pipelining serializes one in-flight request per connection as a matter of protocol, not
implementation quality (this codebase uses `hyper::client::conn::http1`, no pipelining). Since
concurrent embed calls are a real occurrence — concurrent in-flight IPC requests each running
their own `add_episode` — a single connection is known in advance to bottleneck them. Building
that version and only then discovering the regression would waste an implementation cycle.

Slots start empty; connections are dialed lazily on first use of each slot, not eagerly at
pool construction. This preserves `main.rs`'s startup-probe embedder — constructed purely to
make one `probe()` call and then dropped — as cheap as it was before: it dials exactly one of
the four slots and never touches the other three.

`hyper-util`'s `client-legacy` pooling machinery was considered and rejected: it is designed
around `Uri`/TCP-style keying and would need adaptation for UDS path-keyed pooling, for no
benefit over the small amount of pool logic added directly here with primitives already in use
elsewhere in this codebase (`tokio::sync::Mutex`, `AtomicUsize`).

### One bounded re-dial on a broken connection

Per call: acquire the slot's `Mutex`, dial if empty, then attempt `send_request`. A failure
from `send_request` itself (the connection is dead — sidecar restarted, idle-closed the socket,
etc.) clears the slot and retries against a freshly-dialed connection exactly once. If the
retry also fails, the call fails with the same error prefix (`"UDS send request: ..."`) that a
first-attempt failure would have produced. This is a single bounded retry, not an unbounded
retry loop — a genuinely unavailable sidecar still fails the call after one re-dial attempt.

Failures that occur *after* a request was successfully sent over a healthy connection — a
non-2xx status, an unreadable body, unparseable JSON — are not treated as connection failures
and are not retried; re-dialing and re-sending would reproduce the same result, since the
problem is not the connection.

The existing string-prefix contract that `is_transport_error` (and, through it, `main.rs`'s
startup path) depends on — `"UDS connect"`, `"UDS HTTP/1.1 handshake"`, `"UDS send request"` —
is preserved verbatim. The retry logic changes *when* these operations happen, not the error
strings they produce on final failure.

### Cancellation safety: poison-on-drop replaces "always safe"

ADR-0016's cancellation-safety argument no longer holds unmodified: `episode.rs`'s
`tokio::select!` races against a cancellation token routinely drop in-flight `embed()` futures
today, which was safe when every call owned a dedicated, about-to-be-discarded stream. With a
shared, reused connection, dropping the future mid-`send_request` or mid-body-read can leave
the connection in an indeterminate state — a partially written request, or an unread stale
response sitting in the pipe that would corrupt the *next* call's read on that slot.

This is handled with a `PoisonGuard`: a drop guard wrapping the send-request-and-read-body span
for one call, armed by default and disarmed only once that span runs to completion (a full
response was read, successfully or with a definite, already-parsed error). If the guard is
dropped while still armed, it clears the slot back to `None`, so the next use of that slot
re-dials fresh instead of reusing a connection that may be holding corrupted state. Detecting
"was this future dropped rather than completed" proactively (via the guard), rather than only
reacting to the next call's `send_request` error, was chosen because a poisoned-but-not-closed
connection could otherwise return a garbled response instead of a clean, retryable error.

## Consequences

- An ingest producing N embeddings now opens at most `UDS_POOL_SIZE` (4) UDS connections over
  its lifetime, not N — resource churn (socket pairs, file descriptors) and per-call handshake
  latency no longer scale with embedding volume.
- No unbounded, unjoined tokio task is spawned per embed call; at most 4 connection-driver
  tasks exist at a time (one per populated slot), matching the pool's lifetime.
- A sidecar restart mid-ingest costs at most one failed internal send attempt per affected pool
  slot before that slot transparently re-dials; the caller-visible `embed()` call still
  succeeds (unless the sidecar is genuinely down, in which case it fails cleanly after the one
  re-dial, same as before).
- Cancelling an in-flight `embed()` call (via `episode.rs`'s existing cancel-token races) now
  costs at most one wasted re-dial on that call's pool slot the next time it is used, rather
  than being free as it was under fresh-per-call semantics. This is an acceptable trade for
  eliminating per-call dial/handshake/spawn churn on the non-cancelled path, which is the
  overwhelming majority of calls.
- `UDS_POOL_SIZE = 4` is a judgment call made without sidecar-capacity data. It is a named
  constant specifically so it can be tuned without further design work if real-corpus runs
  (#217, or its eventual replacement) show it under- or over-loads the sidecar.
- The `Http` transport variant is unchanged (already pooled via `reqwest`). `OaiExtractor`'s UDS
  path (`crates/core/src/extractor.rs`) had the identical pre-existing per-call dial pattern; this
  was out of scope here and has since been fixed by issue #230 / ADR-0042, which adopts this
  pool design's shape as an independent copy rather than a shared abstraction.

## Alternatives Considered

- **Single held connection, add a pool only if benchmarking shows serialization**: rejected —
  HTTP/1.1 without pipelining serializing one request per connection is not something that
  needs measuring to discover; it's inherent to the protocol as used here. Going straight to a
  small pool avoids implementing and then discarding a version known in advance to regress
  concurrent throughput.
- **`hyper-util`'s `client-legacy` pooling client**: rejected — built around `Uri`/TCP-style
  keying, would need nontrivial adaptation for UDS path-based pooling, and adds a new feature
  flag for no benefit over the pool logic implemented directly with primitives already used
  elsewhere in this codebase.
- **Reactive-only poisoning (detect a corrupted connection only when the next call's
  `send_request` errors, no proactive guard)**: rejected — a connection left in an indeterminate
  state by a dropped future is not guaranteed to error cleanly on next use; it could instead
  return a garbled response that looks superficially valid, which is worse than a clean,
  retryable error.
- **Semaphore/queue-based slot assignment instead of round-robin**: rejected as unnecessary
  complexity — round-robin via an `AtomicUsize` bounds concurrency to `UDS_POOL_SIZE` in-flight
  sends and spreads load adequately for this workload, matching existing atomic-counter idioms
  already used in this codebase (e.g. `episode.rs`'s `ActiveWriteGuard`).

## References

- ADR-0016 — the predecessor decision this ADR supersedes (UDS transport section only); its
  wire-format, transport-selection, and startup-probe sections remain unchanged and valid.
- ADR-0008 — established this project's existing taste for small, named/bounded connection
  strategies over unbounded pools (a different layer — Python client to Rust binary — but the
  same reasoning informs the pool-size choice here).
- Issue #229 — this feature.
- Issue #217 — real-corpus capture reliability against the UDS sidecar; the release gate this
  issue is prioritized to help unblock, though the causal link to this specific defect is
  unconfirmed.
- Issue #230 / ADR-0042 — applies the identical fix shape to `OaiExtractor`'s UDS path in
  `crates/core/src/extractor.rs`.
