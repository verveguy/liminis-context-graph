# ADR-0042: OaiExtractor UDS Connection Pooling

**Status**: Accepted
**Date**: 2026-07-25
**Context**: Issue #230

## Context

`OaiExtractor` (added by #212 / PR #223) copied its UDS transport (`send_chat_uds`) from
`OaiEmbedder`'s UDS transport during development — at the time, both were unpooled: every call
dialed a fresh `UnixStream`, performed a full HTTP/1.1 handshake, and spawned a detached tokio
task to drive the connection. #229 / PR #231 (ADR-0039) fixed this defect for the embedder by
introducing `UdsPool`, a small, lazily-populated, bounded connection pool with re-dial-once-on-
failure semantics and a `PoisonGuard` for cancellation safety. That fix did not touch
`extractor.rs`, leaving the identical defect in place there — this issue closes that gap.

Extraction runs over whole corpora, with up to four `send_chat` calls funneling through
`send_chat_uds` per episode (entity extraction, edge extraction, and the two batched
classification calls), so the per-call socket/FD/handshake/task churn scales with corpus size in
the same way ADR-0039 already described for embeddings.

## Decision

### Duplicate the pool pattern in `extractor.rs`, not a shared helper

`extractor.rs` gains its own `UdsPool` / `dial_uds` / `UdsAttemptError` / `send_and_read_uds` /
`PoisonGuard`, structurally identical to `embedder.rs`'s versions but adapted to a bare
`serde_json::Value` request/response body instead of the embedder's typed `OaiEmbedRequest`/
`OaiEmbedResponse`. A shared generic module (e.g. `crates/core/src/uds_pool.rs`) was considered
and rejected for this issue: it would require touching `embedder.rs`, which issue #230's spec
explicitly scopes out, for a two-call-site payoff that doesn't yet justify the abstraction. The
accepted cost is that a future fix to the pooling/poison-guard logic (e.g. a `#229`/`#230`-class
defect discovered later) needs to be applied in both `embedder.rs` and `extractor.rs` — this ADR
and ADR-0039 cross-reference each other so a future reader/fixer finds the sibling copy.

### Pool size 4, matching the embedder

`UDS_POOL_SIZE = 4`, reusing the embedder's already-reviewed constant rather than introducing an
unjustified different number for the extractor. Sizing is not derived from sidecar-capacity
measurement in either file; if a real-corpus run shows this value under- or over-loads the
sidecar for extraction specifically, this constant is the place to revisit independently of the
embedder's.

### Response stays a bare `Value`

`send_and_read_uds` returns `Result<Value, UdsAttemptError>` rather than a new typed response
struct. `OaiExtractor` already treats every chat-completion response as `Value` throughout
(`send_chat`'s return type, `parse_oai_entity_response`, `parse_oai_edge_response`,
`oai_message_content`); introducing a typed struct here would be a gratuitous change unrelated to
pooling.

### Same redial-once, poison-guard, and error-string contract as ADR-0039

The pool/redial mechanics are functionally identical to the embedder's: acquire a round-robin
slot's `Mutex`, dial if empty, attempt `send_request` under a `PoisonGuard`, and on a
connection-broken failure clear the slot and retry exactly once against a freshly-dialed
connection before surfacing an error. Existing `Error::Ipc` message prefixes (`"UDS connect"`,
`"UDS HTTP/1.1 handshake"`, `"UDS send request"`, `"UDS extractor returned status ..."`, `"parse
UDS chat completion response: ..."`) are preserved verbatim, even though — unlike the embedder —
nothing in this codebase currently pattern-matches on them (`is_transport_error` has no
extractor-side equivalent; there is no extraction startup probe, per ADR-0041 Decision 2).
Preserving the wording costs nothing and keeps the door open for a future consumer.

## Consequences

- An extraction run issuing N UDS-transport chat-completion calls now opens at most
  `UDS_POOL_SIZE` (4) UDS connections over its lifetime, not N.
- No unbounded, unjoined tokio task is spawned per extraction call; at most 4 connection-driver
  tasks exist at a time, matching the pool's lifetime.
- A sidecar restart mid-run costs at most one failed internal send attempt per affected pool slot
  before that slot transparently re-dials; the caller-visible `extract`/`classify_entities`/
  `classify_relations` calls still succeed (unless the sidecar is genuinely down, in which case
  they fail cleanly after the one re-dial, same as before).
- Cancelling an in-flight extraction call (the `tokio::select!` race against `cancel_token` in
  `episode.rs`) now costs at most one wasted re-dial on that call's pool slot the next time it is
  used, rather than being free as it was under fresh-per-call semantics — the same trade ADR-0039
  accepted for the embedder.
- Two independent copies of the pool/redial/poison-guard logic now exist in this codebase
  (`embedder.rs`, `extractor.rs`). A future bugfix to one must be manually applied to the other.

## Alternatives Considered

- **Shared generic `uds_pool` helper covering both `embedder.rs` and `extractor.rs`**: rejected
  for this issue — `embedder.rs` is explicitly out of scope for #230, and the two request/response
  shapes differ enough (`Value` vs. typed structs) that a generic helper would need a non-trivial
  abstraction over (de)serialization for a two-call-site payoff. Revisit if a third UDS-pooled
  adapter appears.
- **A different pool size for extraction**: rejected absent evidence — extraction calls are fewer
  per episode but longer-running than embed calls, and the spec leaves sizing as an
  implementation detail requiring only "small and bounded." Reusing 4 avoids an unjustified
  divergence from the embedder's reviewed constant.
- All other alternatives (single held connection, `hyper-util` client-legacy pooling, reactive-
  only poisoning, semaphore/queue slot assignment) were already considered and rejected by
  ADR-0039 for the identical mechanism; those rejections apply unchanged here and are not
  re-litigated in this ADR.

## References

- ADR-0039 — the precedent decision this ADR adopts the same pool/redial/poison-guard shape from,
  for the embedder's UDS transport.
- ADR-0041 — documents `OaiExtractor`'s existing transport/error-handling contract (no startup
  probe, `send_chat`/`send_chat_uds` split) that this change preserves.
- Issue #230 — this feature.
- Issue #229 / PR #231 — the embedder-side fix this issue mirrors.
- Issue #212 / PR #223 — introduced `OaiExtractor` and, via copy-paste from the then-unpooled
  embedder, the defect this issue fixes.
