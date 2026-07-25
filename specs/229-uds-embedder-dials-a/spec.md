# Feature Specification: UDS embedder dials a new connection + handshake + detached task per embedding call — pool it

**Feature Branch**: `fabrik/issue-229`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "The UDS embedder transport opens a brand-new Unix socket connection, HTTP/1.1 handshake, and detached tokio task for every single embedding call. Hold a persistent (or small pooled) connection for the UDS transport instead of dialing per request, mirroring what the HTTP transport already does with its pooled client."

## Background

`crates/core/src/embedder.rs`'s `do_embed_uds_raw` (~line 165) dials a brand-new `UnixStream`, performs a full HTTP/1.1 handshake, and spawns a detached tokio task to drive the connection — for every single embedding call:

```rust
let stream = UnixStream::connect(path).await?;          // new socket pair, 2 FDs
let (mut sender, conn) = http1::handshake(io).await?;   // full HTTP/1.1 handshake
tokio::spawn(async move { let _ = conn.await; });       // detached task, never joined
```

The `EmbedTransport` enum's two variants are asymmetric: the `Http` variant carries a pooled `reqwest::Client`, while the `Uds` variant carries only a path and dials fresh every time:

```rust
enum EmbedTransport {
    Http { client: Client, url: String },   // reqwest — connection-pooled
    Uds  { path: String },                  // no client, no pooling
}
```

This asymmetry indicates the lack of pooling on the UDS path is unintentional rather than a deliberate design choice. Since UDS is the transport for the "fully local" story and the target machine's own sidecar, every entity name, every episode body, and every retry currently pays a fresh connect/handshake/spawn cost. A single ingest crossing the 1000-entity threshold performs thousands of these cycles.

The cost is threefold: **resource churn** (two kernel socket structures and two file descriptors per call, proportional to embedding volume rather than workload duration), **latency** (a handshake round-trip added to every embedding), and **task churn** (an unbounded stream of detached tokio tasks that nothing joins or bounds).

Real-corpus capture runs (#217) have separately shown the sidecar becoming unreachable partway through several long ingests (`Connection refused`, and once `No such file or directory`) while its process remained alive. This is not proven to be caused by the per-request dial/handshake/spawn pattern — a raw-socket stress test of 600 sequential + 640 concurrent requests exercising the sidecar's accept path did not reproduce it — but that test did not exercise this client's handshake-and-spawn path either, so it neither confirms nor rules out this cause. The churn is worth eliminating regardless, and unblocking the capture is a likely bonus.

An identical per-request dial/handshake/spawn pattern was independently confirmed in `OaiExtractor`'s UDS path (`crates/core/src/extractor.rs`, on branch `fabrik/issue-212`). That copy is tracked separately (see Out of Scope) since `extractor.rs` is currently owned by an in-flight PR.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Connection reuse across a sustained ingest (Priority: P1)

As the engine performs a large ingest producing hundreds or thousands of embeddings, each embedding call reuses an already-established connection to the local sidecar instead of dialing, handshaking, and spawning a task from scratch every time.

**Why this priority**: This is the core defect. Resource churn, handshake latency, and task churn all scale with embedding volume, and sustained ingest is the primary workload this engine targets.

**Independent Test**: Run an ingest that produces many embeddings against the UDS sidecar and confirm the number of accepted connections stays O(1) (or small-pool-bounded) rather than growing with the embedding count.

**Acceptance Scenarios**:

1. **Given** a UDS-configured embedder mid-ingest, **When** many embedding calls are made in sequence, **Then** the sidecar accepts only a small, bounded number of connections, not one per call.
2. **Given** a UDS-configured embedder, **When** embedding calls are made, **Then** no unbounded, unjoined tokio task is spawned per call.

---

### User Story 2 - Transparent reconnect after sidecar restart (Priority: P1)

As an operator running a long ingest, if the sidecar process restarts partway through, embedding calls recover automatically instead of the whole ingest failing outright.

**Why this priority**: Real-corpus capture runs (#217) — the current release gate — have failed partway through long ingests with the sidecar becoming unreachable. Automatic reconnect prevents a single restart from taking down a long-running ingest, and this issue is prioritized as being on the critical path to the release.

**Independent Test**: Kill and restart the sidecar process mid-ingest and confirm at most one embedding call fails, with subsequent calls succeeding without manual intervention.

**Acceptance Scenarios**:

1. **Given** a held UDS connection, **When** the sidecar restarts and the held connection breaks, **Then** the next embedding call transparently re-dials once and succeeds.
2. **Given** a held UDS connection, **When** the sidecar is genuinely unavailable, **Then** the call fails cleanly after one re-dial attempt rather than retrying indefinitely.

---

### User Story 3 - Concurrent embedding throughput preserved (Priority: P2)

As the engine performs embedding calls in parallel, pooling the UDS connection does not make concurrent calls slower than today's per-call-dial behavior.

**Why this priority**: A pooling fix that silently serializes concurrent embedding work would trade one defect for a throughput regression, so this must be measured rather than assumed.

**Independent Test**: Measure parallel embedding throughput before and after the change and confirm no regression.

**Acceptance Scenarios**:

1. **Given** multiple embedding calls issued concurrently, **When** they are processed against the pooled UDS connection (or small pool), **Then** measured throughput is no worse than the current per-request-dial implementation.

---

### Edge Cases

- The sidecar is killed and restarted mid-run: the broken connection is detected and one re-dial is attempted.
- The sidecar is genuinely unavailable: the re-dial also fails, and the call fails cleanly rather than retrying indefinitely.
- High-concurrency embedding bursts are issued against a single held connection or small pool.
- An idle period is long enough that a keep-alive connection may have been closed from the sidecar side before the next call arrives.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The `Uds` transport variant MUST hold a reusable connection — an established `SendRequest` reused across calls over HTTP/1.1 keep-alive, guarded for concurrent access, or a small bounded pool — instead of dialing a new `UnixStream` per embedding call.
- **FR-002**: If the held connection is broken (e.g., the sidecar restarted), the implementation MUST transparently re-dial once and retry the call, rather than failing the call outright or leaking a dead connection.
- **FR-003**: Re-dial on failure MUST be bounded to one re-dial attempt per call; it MUST NOT mask a genuinely dead sidecar behind an unbounded retry loop.
- **FR-004**: Embedding calls made in parallel MUST NOT serialize behind a single connection any worse than the current per-request-dial implementation; a small connection pool MUST be used instead of a single keep-alive connection if measurement shows a single connection would bottleneck concurrent throughput.
- **FR-005**: The implementation MUST NOT spawn an unbounded number of detached tokio tasks per request.
- **FR-006**: The behavior and error surface exposed to callers of the embedder MUST remain unchanged.

### Key Entities *(if applicable)*

- **`EmbedTransport::Uds`**: the enum variant in `crates/core/src/embedder.rs` responsible for embedding calls over a Unix domain socket to the local sidecar; gains a held connection (or small bounded pool) in place of per-call dialing, mirroring the existing pooled `Http` variant.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An ingest producing N embeddings opens O(1) (or small-pool-bounded, not O(N)) UDS connections — asserted by a test that counts connects, or by instrumenting the sidecar's accept count across a multi-embedding run.
- **SC-002**: Killing and restarting the sidecar mid-run causes at most one failed embedding call, after which subsequent calls reconnect and succeed.
- **SC-003**: Parallel embedding throughput is no worse than the current per-request-dial implementation, as measured (not assumed).
- **SC-004**: Existing embedder tests pass, including `embedder_transport.rs`.
- **SC-005**: `cargo fmt`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` all succeed.
- **SC-006**: A sustained ingest of at least 150 episodes against the UDS sidecar completes without the embedder becoming unreachable.

## Assumptions

- The sidecar runs on the same machine as the embedder client, so UDS is the intended low-overhead local transport and warrants the same pooling treatment already applied to the HTTP transport.
- A single shared keep-alive connection may serialize concurrent embedding calls; if measurement confirms this, a small bounded pool is an acceptable alternative per FR-004.
- The sidecar-unreachable fragility observed during real-corpus capture runs (#217) is not proven to be caused by the per-request dial/handshake/spawn pattern this issue fixes, but the churn is worth eliminating on its own merits.

## Out of Scope

- The HTTP transport (`EmbedTransport::Http`) — already connection-pooled via `reqwest`.
- `OaiExtractor`'s UDS path in `crates/core/src/extractor.rs` — confirmed to have the identical per-request dial/handshake/spawn pattern, but tracked separately in issue #230 (blocked on #212, which currently owns `extractor.rs`). A shared UDS-client helper introduced here would make that follow-up a small adoption change, but building that shared helper is not required by this issue.
- The sidecar's server/accept-side implementation.

## Source References

- #217 — real-corpus capture reliability against the UDS sidecar; the release gate this issue is prioritized to help unblock.
- #230 — follow-up to apply the identical fix shape to `OaiExtractor`'s UDS path, blocked on #212.
