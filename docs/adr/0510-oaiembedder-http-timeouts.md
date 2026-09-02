# ADR-0510: Bounded Request/Connect Timeouts for `OaiEmbedder`'s HTTP Transport

**Date**: 2026-09-01
**Status**: Accepted

## Context

`OaiEmbedder::new_http` built its `reqwest::Client` via `Client::new()` — reqwest's default
client, which has **no request timeout and no connect timeout**. An embedder backend that
accepts a TCP connection and then never responds (a network partition mid-response, a backend
deadlock, a cold-starting local sidecar that accepts the socket but never replies) hangs the
calling task indefinitely.

This became materially worse after #487 (issue #444): before it, `handle_assert_entity` called
`state.embedder.embed(...)` *before* acquiring `state.write_lock`, so a hung embedder stalled
only that one request. #487 correctly moved the embed call onto the create branch, which sits
**inside** the write-lock critical section — `write_lock` is what stops two concurrent creates of
the same not-yet-existing `(name, group_id)` from both resolving "not found" and both inserting,
and holding it across the embed call was a deliberate, documented trade-off (see the comment at
`handlers.rs`'s `handle_assert_entity`, near the `_guard` acquisition). The trade-off #487
accepted was *throughput* — every create-path call now serializes unrelated writes for the
duration of an embedder round-trip. With no timeout on that round-trip, the held duration was
unbounded: a single hung embedder connection could wedge every write on the instance until the
process was restarted.

This codebase already had two independent, narrower workarounds for exactly this class of hazard
before this ADR:

- `main.rs`'s `bootstrap_app_state` wraps the startup probe in an outer `tokio::time::timeout`
  bounded by `EMBEDDER_RETRY_CEILING`, added per ADR-0499, whose own comment cites this same root
  cause ("neither transport's client has a request timeout configured") as the reason it exists.
- The UDS transport (`EmbedTransport::Uds`) has the identical hazard on its hand-rolled hyper
  `send_request`/read path, but is explicitly out of scope here — see Out of Scope below.

No production hang has been observed; this is a preventative fix reasoned from the code path
(the `Client::new()` construction site plus #487's lock-reordering), not from an incident.

## Decision

### Fix at the one true choke point: `OaiEmbedder::new_http`'s `Client` construction

Every HTTP call this embedder makes — single embed, each batch chunk (issue #445), and the
startup probe — goes through the single `reqwest::Client` built in `new_http`. Adding
`.timeout(...)`/`.connect_timeout(...)` at `Client::builder()` time covers all of them with zero
call-site changes anywhere else (`do_embed_http_raw`, `handlers.rs`, `search.rs`,
`canonicalize.rs`, `db.rs`, `embedding_cache.rs`).

### Two independently configurable durations, generous flat defaults

- `LCG_EMBEDDING_TIMEOUT_MS` (default `30000`) — whole-request bound, applied to every call this
  client makes.
- `LCG_EMBEDDING_CONNECT_TIMEOUT_MS` (default `5000`) — connect-phase bound, independent of the
  above.

A single client-wide timeout pair, not per-call-shape tuning: the same `Client` serves probe,
single-embed, and batch-chunk calls, so a generous flat default that comfortably covers the
largest legitimate batch chunk (`LCG_EMBED_BATCH_SIZE`'s max of 256 texts, issue #445) under
normal backend latency is simpler than scaling the timeout with batch size, and satisfies the
spec's batch-compatibility requirement (FR-004) without added complexity.

### Strict validation, not silent fallback — a deliberate departure from this file's other timeout vars

`resolve_embedding_timeout_ms` rejects an unparseable or zero override with `Error::Config`,
mirroring `resolve_embed_batch_size`'s validation shape for `LCG_EMBED_BATCH_SIZE`. This is a
conscious divergence from the more common convention in this codebase for timeout-flavored env
vars — `LCG_ATTACHED_CALL_TIMEOUT_MS` and `LCG_SHUTDOWN_TIMEOUT_MS` (both in
`crates/service/src/main.rs`) silently fall back to their default on a parse failure. Strict
validation was chosen here because a silently-ignored override of *this specific* variable would
be actively dangerous in a way the other two aren't: if an operator sets
`LCG_EMBEDDING_TIMEOUT_MS=0` intending "make this fail fast for a health check" and it were
silently treated as "keep the 30s default," that operator would have no visible signal that
their intended fast-fail behavior never took effect. Rejecting a negative value falls out for
free — `u64::parse` fails on a leading `-`, so no separate range check is needed the way
`resolve_embed_batch_size` needs one for its upper bound.

### `new_http`/`from_env` become fallible

Since timeout resolution and `Client::builder().build()` are both fallible, `OaiEmbedder::new_http`
and `OaiEmbedder::from_env` now return `Result<Self, Error>` instead of `Self`. This ripples
mechanically through every call site (`main.rs` ×2, the `basic_ingest` example, and every test
constructing an `OaiEmbedder` directly) — each fix is a `?`/`.expect()`/error-branch, not a logic
change. This was preferred over adding a separate infallible constructor plus a fallible
"apply timeouts" step, because a single construction path means invalid timeout configuration
can never be silently skipped by a caller that forgets the second step.

### No changes to `is_transport_error`, the UDS transport, or ADR-0499's retry wrapper

`reqwest::Error::is_timeout()` reports `true` for both a connect-phase timeout and a
whole-request timeout — confirmed directly against the vendored reqwest source
(`connect.rs`'s `with_timeout` helper, `error.rs`'s `is_timeout()`) — so `is_transport_error`
(`Error::Http(re) => re.is_connect() || re.is_timeout() || re.is_request()`) already classifies
both timeout flavors correctly with no change needed. `main.rs`'s fatal-vs-bypass startup logic
and issue #499's `--mcp-stdio` retry loop therefore keep working unmodified (FR-005).

ADR-0499's outer `tokio::time::timeout`/`EMBEDDER_RETRY_CEILING` wrapper around the startup probe
is left in place. It now overlaps with this ADR's client-level timeout for the HTTP-probe case
specifically, but it still does independent work this fix cannot: it bounds the whole
resolve-transport-and-probe sequence, including the UDS existence check, not just one HTTP call.
Whichever bound is tighter fires first; there is no correctness conflict, only intentional
defense-in-depth. Simplifying or removing it was considered and explicitly deferred — it is not
required by this issue and is a judgment call better made once the client-level fix has some
runtime history.

## Out of Scope

- **Narrowing `state.write_lock`'s critical section.** Dropping the lock during the embed call
  and re-resolving under a freshly-acquired guard immediately before insert (falling back to the
  update path if another writer won the race) removes the unbounded-hold hazard structurally
  rather than merely bounding it. This is a real, better fix, but a separate one — this ADR only
  bounds the hold duration, it doesn't eliminate the hold. May be filed as a follow-up issue.
- **UDS transport timeout hardening.** `send_and_read_uds`'s `sender.send_request`/response-read
  path has the same class of hazard but uses a hand-rolled hyper connection pool, not reqwest, so
  `Client::builder()` cannot reach it. Out of scope for this issue; may be filed separately.
- **Retry-on-timeout behavior.** This ADR bounds how long a hang can hold the write lock: it does
  not add retries for a timed-out request.

## Consequences

- A hung-but-connected HTTP embedder backend now fails every call — probe, single embed, and
  each batch chunk — within a bounded time (default 30s whole-request / 5s connect) instead of
  hanging forever, and `state.write_lock` is released accordingly on the create path.
- Existing deployments that don't set either new env var see no behavioral change for a
  normally-responsive embedder: 30s/5s are reasoned defaults with wide margin over realistic
  local-sidecar and hosted-API latency, not measured against a production incident (none has
  occurred).
- An operator who needs a tighter or looser bound than the default sets
  `LCG_EMBEDDING_TIMEOUT_MS`/`LCG_EMBEDDING_CONNECT_TIMEOUT_MS`; an invalid value for either is
  rejected at construction time with a clear error rather than silently ignored.
- `OaiEmbedder::new_http`/`from_env` are now fallible — any future direct caller of either must
  handle the `Result`, matching the pattern already used for every existing call site.
- The UDS transport and the write-lock critical section itself remain exposed to their own
  version of this hazard, tracked as explicit follow-ups rather than folded into this change.
