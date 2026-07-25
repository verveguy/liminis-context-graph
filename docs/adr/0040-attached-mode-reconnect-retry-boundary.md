# ADR-0040: Attached-Mode Reconnect — Retry Only Write-Time Failures

**Status**: Accepted
**Date**: 2026-07-25
**Issues**: #213 (originally reported in #206 Part B)

## Context

`AttachedBackend` (ADR-0035 Decision 4) forwards every `tools/call` as JSON-RPC over a Unix
socket to an already-running `liminis-context-graph` service, serialized on one persistent
connection guarded by a `tokio::sync::Mutex` so exactly one call is ever in flight. Before this
issue, that connection was dialed exactly once at process startup and never reopened: if the
remote service restarted (deploy, crash-and-supervisor-restart, manual bounce), the next call's
`write_all` failed with a broken-pipe error, was reported to the caller as an ordinary call
failure, and — critically — the same dead stream was left in place and reused for every call
after that. The attached MCP process was permanently wedged with respect to that remote until it
was itself restarted, even though a working socket was available again seconds later.

Adding reconnect logic raises a correctness question that a naive "just retry on any failure"
implementation gets wrong: some of the methods this transport forwards are **not idempotent**
(`knowledge_add_episode`, `knowledge_canonicalize_relations`, `knowledge_reprocess_entity_types`,
and others that mutate the graph). If a connection breaks *after* the request bytes were already
written — the remote may have received it, started executing it, or even completed it — blindly
retrying that call over a new connection risks silently double-applying the write. The transport
has no way to ask the remote "did you already run this?": the wire protocol (out of scope for
this issue) has no idempotency-key or exactly-once semantics.

## Decision

Reconnect logic distinguishes exactly two failure phases, and only one of them is retried:

1. **Write-time failure** (`write_all` and/or `flush` fails on the current connection): the
   request is **provably** incomplete or unsent from this process's perspective. It is safe to
   redial the originally-configured socket path and retry sending that same request **exactly
   once** over the new connection. If the redial itself fails, or the retried write also fails,
   the call fails with a structured error describing the reconnect failure — no further retries,
   no hang.
2. **Read-time failure** (idle-read timeout, EOF, a socket read error, or a malformed response
   line): the request was already fully written before this failure surfaced, so the remote's
   execution status is **unknown** — it may have run, partially run, or not run at all. This
   call is **never** automatically retried; it fails immediately with a descriptive error. The
   connection is unconditionally invalidated (dropped from the client's side) so that the *next*
   call redials fresh instead of reusing a stream whose byte-framing is now suspect, rather than
   waiting for a background health-check or another failed attempt to notice.

A `write_all` that succeeds followed by a `flush` that fails is deliberately **not** split into a
third case. `tokio::net::UnixStream` gives no way to ask "how many of the bytes I wrote actually
reached the peer," so a flush failure after a successful `write_all` is exactly as ambiguous, on
the bytes-delivered question, as a `write_all` failure itself. `write_all` and `flush` are
therefore treated as one atomic write-time unit: either sub-step failing is classified the same
way (safe to redial and retry once), rather than inventing a third, unverifiable classification
between them.

Reconnect (dial, write-retry, connection-invalidation) happens entirely inside the single
`Mutex` guard already held for the whole call — it is never dropped and reacquired mid-reconnect.
This preserves ADR-0035 Decision 4's "one call in flight ⇒ any progress line unambiguously
belongs to the current call" invariant across a reconnect for free, and rules out a race between
a reconnecting call and a subsequent call by construction, without a new synchronization
primitive.

The request-ID counter (`next_id: AtomicU64`) and the stale-response-discard logic (PR #196: a
terminal response whose `id` doesn't match the current call's is a straggler from an earlier,
already-abandoned call and must be discarded rather than misdelivered) are both untouched by
reconnect. A retried write reuses the same call's `id` — it is a new attempt of the same logical
call, not a new one — and continues to work correctly because the discard logic keys off `id`
independent of which physical connection a line arrived on.

## Consequences

- A caller that supplies `_progress_token` (or any other request shape) never needs to implement
  its own retry-on-broken-pipe logic for attached mode — the client handles the safe case
  transparently.
- A caller whose call fails with a "connection lost mid-call" (read-time) error must treat that
  call's outcome as genuinely unknown, exactly as if it had called a non-idempotent method
  directly over a flaky link with no transport-level exactly-once guarantee. This is not a
  regression from pre-#213 behavior (which also never retried and also left the outcome
  ambiguous) — it's the same ambiguity, now paired with an automatically-healed connection for
  the *next* call instead of a permanently wedged one.
- Repeated/flapping remote restarts do not cause unbounded retry: at most one automatic retry
  happens per call (write-time failure only), and a second consecutive failure surfaces to the
  caller rather than looping.
- A future contributor extending `AttachedBackend` must not "fix" the write_all/flush bundling
  into two separate cases without re-deriving this ADR's reasoning — doing so either loses the
  conservative safety margin (if flush-failure is reclassified as non-retryable) or reintroduces
  double-write risk (if it's reclassified as always-retryable without the ambiguity caveat).

## Alternatives Considered

- **Retry on any failure (write or read), always exactly once**: rejected — this is the naive
  approach that risks double-applying non-idempotent writes like `knowledge_add_episode` when
  the failure surfaces after the remote already executed the call.
- **Never retry automatically, always require the caller to reissue**: rejected as unnecessarily
  conservative for the write-time case, where the request provably never left this process — a
  transient dead-connection blip on an otherwise-idle attached session would surface as a caller
  visible failure for no correctness reason.
- **Exponential backoff / multiple retries**: rejected as out of scope (see the issue's Out of
  Scope section) — a caller that needs more resilience than one retry re-issues the call itself;
  building a general retry policy here would also make the "exactly one write-time retry" safety
  boundary harder to reason about.
- **Idempotency keys / exactly-once wire protocol**: rejected as a much larger change to the
  socket wire protocol itself (explicitly out of scope for this issue) — it would let read-time
  failures be safely retried too, but requires the remote to track and dedupe request IDs across
  reconnects, which is a protocol-level feature, not a client-side fix.

## References

- ADR-0035 — established `AttachedBackend`, the single-`Mutex`-guarded-connection design, and
  the "one call in flight" invariant this ADR's reconnect logic preserves
- PR #196 — established the per-read idle timeout and the stale-response-discard logic this
  ADR's read-time-failure path builds on without breaking
- Issue #206 (Part B) — original report of both the false-timeout and no-reconnect defects
- Issue #213 — this feature
