# Feature Specification: OaiExtractor UDS path — adopt pooled connection (same defect as #229)

**Feature Branch**: `fabrik/issue-230`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "`OaiExtractor`'s UDS transport (`crates/core/src/extractor.rs`) dials a new Unix socket, performs a full HTTP/1.1 handshake, and spawns a detached tokio task per request. This is the same defect as #229 (the embedder's UDS transport), propagated by copy-paste into the local extraction adapter added in #212. Per request: two kernel socket structures, two file descriptors, a handshake round-trip, and an unbounded detached task — for a daemon on the same machine. Adopt the pooled/reusable UDS connection introduced by #229 in `OaiExtractor`, removing the second copy of the per-request dial."

## Background

`OaiExtractor` (added by #212 / PR #223, now merged to `main`) is the local/OpenAI-compatible entity-and-relationship extraction adapter — it lets the whole extraction pipeline run against an on-machine sidecar (e.g. the bundled macOS CoreML sidecar's Foundation Models route) instead of a hosted LLM, over either HTTP or a Unix domain socket (UDS).

Its UDS path (`ExtractTransport::Uds` / `OaiExtractor::send_chat_uds` in `crates/core/src/extractor.rs`) currently dials a fresh `UnixStream`, performs a full HTTP/1.1 handshake, and spawns a detached `tokio::spawn` task to drive the connection — **on every single extraction call**. This is the identical defect fixed in the embedder's UDS transport by #229 (PR #231), which replaced its own per-call dial with a small, lazily-populated, bounded connection pool (`UdsPool` in `crates/core/src/embedder.rs`) plus re-dial-once-on-failure semantics.

The defect reached `OaiExtractor` because its UDS transport code was copy-pasted from the (at-the-time also unpooled) embedder during #212's development. Since extraction runs over whole corpora — potentially many calls per episode (entity extraction, edge extraction, and batched entity/relation classification) — the per-call socket/FD/handshake/task churn is proportionally worse here than for embeddings, even though individual extraction payloads are larger and slower (making the relative overhead of the *handshake itself* smaller per call).

This issue is the direct sibling of #229: same defect, same fix shape, different file. #229 has already merged and its pattern (`UdsPool`, `dial_uds`, `send_and_read_uds`, re-dial-once-on-failure via a poison-guarded pool slot) is the reference implementation to adopt here.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Extraction run over a corpus does not exhaust sockets/FDs (Priority: P1)

An operator runs extraction over a corpus of many episodes using the local UDS-transport extractor (`--extractor-uds <path>`). Each episode triggers multiple extraction calls (entity extraction, edge extraction, batched classification). Today, each of those calls opens and tears down its own kernel socket pair and spawns its own detached driver task; over a large corpus this multiplies into unbounded socket/FD/task churn against the local sidecar process.

**Why this priority**: This is the core defect — the whole point of the issue is bounding resource usage per extraction run, matching what #229 already delivered for the embedder.

**Independent Test**: Run an extraction workload issuing many sequential (or concurrent) UDS-transport chat-completion calls against a stub sidecar and count accepted socket connections at the stub; the count must stay small and bounded rather than growing with the number of calls.

**Acceptance Scenarios**:

1. **Given** an `OaiExtractor` constructed with `new_uds`, **When** it issues N chat-completion calls against a reachable UDS sidecar, **Then** the number of underlying Unix domain socket connections opened is O(1) (bounded by a small fixed pool), not O(N).
2. **Given** an `OaiExtractor` with an already-established pooled UDS connection, **When** it issues a new call, **Then** no new `tokio::spawn`'d connection-driver task is created for that call — the existing pooled connection (and its already-running driver task) is reused.

---

### User Story 2 - Sidecar restart mid-run degrades gracefully (Priority: P1)

The local inference sidecar process is restarted (e.g. by the user, or by macOS OS-level app lifecycle) while an extraction run is in progress. Before this fix, an in-flight per-call dial either fails outright or succeeds against a stale socket depending on timing; after this fix, a held pooled connection can go stale across a restart and must be detected and recovered without operator intervention.

**Why this priority**: Matches the resilience guarantee #229 established for the embedder — extraction runs are long enough that sidecar restarts during a run are a realistic occurrence, not an edge case.

**Independent Test**: Simulate a sidecar restart against a stub UDS listener (sever/replace the accepted connection) mid-run and confirm exactly one extraction call fails, after which subsequent calls succeed via automatic reconnect.

**Acceptance Scenarios**:

1. **Given** an `OaiExtractor` holding a pooled UDS connection to a sidecar that has since restarted (severed the socket), **When** the next extraction call is issued, **Then** the send against the stale connection fails, the extractor re-dials once automatically, and the retried call succeeds against the fresh connection.
2. **Given** the re-dial-and-retry in the above scenario also fails (sidecar still not accepting connections), **When** the caller receives the resulting error, **Then** the error is surfaced to the caller in the same form/shape as before this change (no new error variant or caller-visible contract change), and the pool does not retain a known-bad connection for the next unrelated call to trip over.

---

### Edge Cases

- **Sidecar not yet reachable at construction time**: `OaiExtractor::new_uds` must not eagerly dial — matching current behavior (construction succeeds even before the sidecar is listening; the existing `main.rs` preflight only checks that the socket *path* exists, not that it's currently accepting connections). The first actual call dials lazily.
- **Concurrent extraction calls**: Entity extraction, edge extraction, and classification calls may be in flight concurrently for the same `OaiExtractor` instance (e.g. across episodes processed concurrently). The pooling scheme must not serialize all concurrent calls behind a single held connection in a way that changes today's effective concurrency behavior for the worse.
- **Two consecutive failures on the same call**: initial send fails AND the redial-and-retry also fails. Per #229's established contract, only one re-dial-and-retry is attempted; a second failure is surfaced to the caller as an error (no further automatic retries).
- **Non-Unix platforms**: The UDS transport is already `#[cfg(unix)]`-gated; this change does not alter that gating — non-Unix builds are unaffected.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `OaiExtractor`'s UDS transport MUST reuse a held, previously-established connection across calls rather than dialing a new `UnixStream` and performing a new HTTP/1.1 handshake per call.
- **FR-002**: Connections MUST be established lazily (no eager dial at `OaiExtractor::new_uds` construction time), matching current construction behavior.
- **FR-003**: On a request-send failure against a held connection (indicating the connection is dead — sidecar restarted, idle-closed the socket, etc.), the extractor MUST re-dial exactly once and retry the same request against the fresh connection before surfacing an error to the caller.
- **FR-004**: The UDS transport MUST NOT spawn an unbounded number of detached `tokio::spawn` connection-driver tasks — task creation must be bounded to (at most) one per held/pooled connection, created once when that connection is (re-)dialed, not once per request.
- **FR-005**: Caller-visible behavior MUST be unchanged: `OaiExtractor`'s public constructors (`new_uds`, `new_http`, `from_env`), `transport_info`, and the `Extractor` trait methods keep their existing signatures and success/error semantics. Callers observe the same request/response contract and the same categories of errors on failure — only the internal connection-management strategy changes.
- **FR-006**: All existing extractor tests (HTTP-transport tests and any inline unit tests in `extractor.rs`) MUST continue to pass unmodified.
- **FR-007**: New test coverage MUST demonstrate that a workload issuing many UDS-transport extraction calls against a stub sidecar opens a small, bounded number of underlying socket connections — not one per call — mirroring the equivalent coverage already established for the embedder in `crates/core/tests/embedder_transport.rs`.
- **FR-008**: New test coverage MUST demonstrate the sidecar-restart-mid-run recovery behavior: simulating a severed/stale held connection results in exactly one failed call followed by successful automatic reconnection on the next call.
- **FR-009**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` MUST all pass on the resulting change.

### Key Entities

- **UDS connection pool**: The reusable-connection construct held by `OaiExtractor` for its UDS transport, conceptually equivalent to the embedder's `UdsPool` (a small set of lazily-populated, independently-lockable slots, each holding at most one live HTTP/1.1 sender paired with its background driver task).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An extraction run issuing N UDS-transport chat-completion calls opens a bounded, small number of underlying Unix domain socket connections that does not scale linearly with N.
- **SC-002**: Restarting the local inference sidecar mid-extraction-run costs at most one failed extraction call; all subsequent calls succeed via automatic reconnection with no operator intervention.
- **SC-003**: No functional regression — all pre-existing extractor tests, and the project's full test suite, pass.
- **SC-004**: Tokio task count attributable to the UDS extraction transport stays bounded regardless of call volume (no per-request task leak).

## Assumptions

- **Shared-helper preference is resolved by current repo state, not left open**: the issue text says to "prefer adopting a shared UDS-client helper if #229 introduces one." #229 (PR #231) did **not** extract a shared/reusable module — its pool/dial/send/poison-guard logic lives inline in `crates/core/src/embedder.rs`, specific to the embed request/response shapes. Whether to now factor out a shared helper (which would touch `embedder.rs`, nominally listed as **Out of Scope** below) or to adapt the same pattern independently within `extractor.rs` is a design decision for the Research/Plan stage, not this spec — this spec only requires the functional pooling/resilience behavior (FR-001–FR-004), not a specific code-sharing structure.
- **Pool sizing is an implementation detail**: the embedder's pool uses a fixed size of 4 slots for its own concurrency-vs-resource-use tradeoff. This spec does not mandate a specific pool size for the extractor; it only requires the connection count to be bounded and independent of call volume (FR-001, SC-001).
- **No existing extractor-UDS test coverage exists today** (`crates/core/tests/` has no extractor-transport integration test file analogous to `embedder_transport.rs`) — FR-007/FR-008 will require new test coverage to be added, not just extended.

## Out of Scope

- The embedder's UDS transport (already resolved by #229 / PR #231) — no changes to `crates/core/src/embedder.rs`'s pooling behavior are required by this issue, though the Plan stage may choose to touch it if it decides a shared helper is the right structure (see Assumptions above).
- `OaiExtractor`'s HTTP transport (already uses a pooled `reqwest::Client`; unaffected).
- Extraction-provider selection semantics (`--extractor-uds` / `--extractor-http` / auto-detection / provider fallback in `main.rs`).
- Introducing retry behavior beyond the single re-dial-and-retry-once semantics already established by #229 (e.g. no exponential backoff, no multi-attempt retry loops).

## Source References

- `crates/core/src/extractor.rs` — `OaiExtractor`, `ExtractTransport::Uds`, `send_chat_uds` (current unpooled implementation to be replaced).
- `crates/core/src/embedder.rs` — `UdsPool`, `dial_uds`, `send_and_read_uds`, `PoisonGuard`, `is_transport_error` (the reference pattern established by #229 / PR #231).
- `crates/core/tests/embedder_transport.rs` — existing bounded-connection-count test pattern to mirror for the extractor.
- #229 / PR #231 — "Pool UDS embedder connections instead of dialing per call" (the precedent this issue follows).
- #212 / PR #223 — "feat: add local/OpenAI-compatible extraction adapter (OaiExtractor)" (where the defect was introduced by copy-paste).
