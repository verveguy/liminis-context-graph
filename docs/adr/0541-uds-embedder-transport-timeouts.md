# ADR-0541: Bounded Timeouts for `OaiEmbedder`'s UDS Transport

**Date**: 2026-09-05
**Status**: Accepted

## Context

ADR-0510 (#510) bounded `OaiEmbedder`'s **HTTP** transport with a whole-request timeout
(`LCG_EMBEDDING_TIMEOUT_MS`) and a connect-phase timeout (`LCG_EMBEDDING_CONNECT_TIMEOUT_MS`),
because an embedder backend that accepts a connection and never responds hangs the calling task
indefinitely. That ADR explicitly scoped out the UDS transport:

> The UDS transport (`EmbedTransport::Uds`) has the identical hazard on its hand-rolled hyper
> `send_request`/read path, but is explicitly out of scope here.

UDS is not a fallback — it is `main.rs`'s **default** transport whenever the bundled Swift
sidecar's socket (`/tmp/liminis-inference.sock`) exists, which is the documented macOS setup.
`send_and_read_uds` (`crates/core/src/embedder.rs`) uses a hand-rolled hyper connection pool
rather than `reqwest`, so `Client::builder()`'s `timeout`/`connect_timeout` — the mechanism #510
used — cannot reach it. Neither `dial_uds` (socket connect + HTTP/1.1 handshake) nor
`send_and_read_uds` (send/read) nor `do_embed_uds_raw`'s pool-slot acquisition
(`pool.slots[idx].lock().await`) had any bound before this issue.

The consequence is the one #510 describes, on the more common deployment path: a hung UDS
embedder holds `state.write_lock` for the duration of the hang on the create path (#487), wedging
every other write on the instance. #526 makes this worse by removing the WAL's stored vectors, so
every WAL replay now recomputes — thousands of embedder calls on a real corpus, each an
opportunity to hang.

## Decision

### Mirror #510, plus two UDS-specific hang points

Every HTTP call goes through one `reqwest::Client`, so #510 fixed it at one choke point. UDS has
no equivalent single client; instead it has three independent hang points: pool-slot mutex
acquisition, dialing (socket connect + HTTP/1.1 handshake), and the send/read of one attempt. Two
new helpers cover all three, reusing #510's env vars and defaults:

- **`acquire_uds_slot`** wraps *both* "wait for the pool slot's mutex" and "dial a fresh
  connection if the slot is empty" in one combined `tokio::time::timeout(connect_timeout, ...)`.
- **`attempt_uds_send`** wraps the existing `PoisonGuard`-guarded `send_and_read_uds` call in
  `tokio::time::timeout(request_timeout, ...)`.

Both are used for the initial attempt *and* the existing single-retry-on-`ConnectionBroken` path
(`do_embed_uds_raw`), so the retry keeps its current shape but every phase of it is now bounded —
including the retry's own redial, wrapped in its own `tokio::time::timeout(connect_timeout, ...)`.

### Combined single budget for acquire+dial, not two stacked timeouts

`acquire_uds_slot` bounds "lock the slot, dial if empty" to `connect_timeout`, not
`2×connect_timeout`. This matches FR-003's wording, which groups "dialing...and/or waiting for a
pool slot" as one phase, and gives a predictable worst-case acquisition latency per attempt.
Diagnosing which of the two sub-phases stalled is a marginal loss against that predictability.

### No retry on timeout, only on definite `ConnectionBroken`

A request-timeout produces `UdsAttemptError::Other`, not `ConnectionBroken`, so it never triggers
the existing single-retry-on-broken-connection path. This is a spec requirement (Out of Scope:
"Adding retry-on-timeout behavior"), not an oversight — a hang and a definite dead-connection
signal are different failure shapes and stay on different paths.

### Reuse existing `is_transport_error` prefixes — no code change to that function

New timeout error messages are worded to start with the same prefixes `is_transport_error`
already recognizes: `"UDS connect: ..."` for both slot-acquisition and dial/redial timeouts,
`"UDS send request: ..."` for request timeouts. This satisfies FR-005 (every new error classifies
as a transport error) with zero changes to a function three other subsystems depend on
(`main.rs`'s fatal-vs-bypass startup logic, #499's `--mcp-stdio` retry loop), at the cost of
slightly less granular error-source text — mitigated by naming the phase and duration in the
message body. Pinning unit tests cover the three new message shapes.

### No new poison-guard mechanism

`PoisonGuard`'s existing `Drop` impl already clears a pool slot when the guarded future is
dropped mid-flight rather than completing — which is exactly what `tokio::time::timeout` does to
its inner future when it elapses. Tracing the exact `.await` points:

- If `acquire_uds_slot` times out, the cancelled future is dropped before `*slot` is ever
  reassigned to `Some(...)` — the slot is left exactly as it was (`None`, or an untouched
  existing connection), which is already safe to reuse or redial.
- If `attempt_uds_send` times out, the cancelled future drops the in-scope `PoisonGuard` before
  `disarm()` runs, so the slot is cleared via the existing mechanism.

FR-007 (a timed-out connection must not be silently reused) is satisfied with zero new types.

### `new_uds` becomes fallible

Resolving and validating both timeout env vars at construction time (mirroring #510's `new_http`)
means invalid configuration is rejected once, at one call site, rather than possibly being
silently skipped by a caller. This ripples mechanically through every call site (`main.rs` ×2,
`real_corpus_replay_perf.rs`, and the UDS test suite in `embedder_transport.rs`) — each fix is a
`?`/`.unwrap()`/error-branch, not a logic change.

## Out of Scope

- **Narrowing `state.write_lock`'s critical section** — deferred by #510 for the same reason;
  this issue only bounds the hold duration for the UDS path too.
- **Adding retry-on-timeout behavior** — see above; this issue bounds how long a hang can hold the
  write lock, it does not add a retry for a timed-out (as opposed to definitely-broken) connection.
- **New environment variables** — the existing HTTP-transport names (`LCG_EMBEDDING_TIMEOUT_MS`,
  `LCG_EMBEDDING_CONNECT_TIMEOUT_MS`) are reused verbatim, per the issue's explicit direction.
- **The UDS transport used for LLM extraction calls** (`crates/core/src/extractor.rs`). It has
  the identical hazard on a structurally identical hand-rolled hyper pool, but serves a different
  purpose with its own error-classification shape (`UdsAttemptError::HttpStatus`/`Malformed`, from
  #306) and is not folded into this issue.

## Consequences

- A hung-but-connected UDS embedder backend — the default deployment path — now fails every call
  (probe, single embed, each batch chunk) within a bounded time (default 30s whole-request / 5s
  connect-or-acquire) instead of hanging forever, and `state.write_lock` is released accordingly
  on the create path.
- A UDS connection pool with every slot occupied by a hung call now fails a new caller within the
  connect-timeout bound instead of queuing indefinitely.
- Existing deployments that don't set either env var see no behavioral change for a
  normally-responsive embedder: the same 30s/5s defaults #510 chose for HTTP apply here, with the
  same wide margin over realistic local-sidecar latency.
- `OaiEmbedder::new_uds` is now fallible — any future direct caller must handle the `Result`,
  matching the pattern `new_http` already established.
- The extraction-side UDS transport remains exposed to its own version of this hazard, tracked as
  a possible follow-up rather than folded into this change.
