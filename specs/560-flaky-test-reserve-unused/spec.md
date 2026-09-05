# Feature Specification: Fix TOCTOU port race in `embedder_degraded_mcp.rs` test helpers

**Feature Branch**: `fabrik/issue-560`
**Created**: 2026-09-05
**Status**: Specified
**Input**: User description: "`crates/service/tests/embedder_degraded_mcp.rs` reserves a port by binding, reading the assigned port, then dropping the listener. The listener is closed when it goes out of scope, so the port number it returns is only a hint. Between that close and whatever happens next, anything else on the machine can take the port — another test in the same parallel run, another cargo job on a shared CI runner, or an unrelated process. This is a textbook time-of-check-to-time-of-use race. It has already bitten: an AddrInUse panic at crates/service/tests/embedder_degraded_mcp.rs:80."

## Background

`crates/service/tests/embedder_degraded_mcp.rs` provides binary-level coverage for issue #499's bounded embedder-probe retry/degrade behavior. Several of its tests need a TCP port with a controlled listening state (nothing listening, or something listening after a delay, or something listening immediately). All of them currently go through the same helper:

```rust
fn reserve_unused_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
```

Binding to port `0` asks the OS to assign a free ephemeral port; reading `local_addr()` reveals which one it picked. But the listener is dropped immediately after, releasing the port back to the OS. Everything from that point on is only ever acting on a *hint* — nothing guarantees the port is still free by the time it's used. This is a time-of-check-to-time-of-use (TOCTOU) race, and it has two distinct failure shapes depending on how the caller uses the hint:

1. **Loud but flaky** — `spawn_stub_embedder_that_hangs` (line 79-80) calls `reserve_unused_port()` and immediately rebinds the same port number. The race window is small but non-zero; losing it panics the test with `AddrInUse`. This is the failure that was actually observed in CI.
2. **Silent, wide window** — `spawn_stub_embedder_after_delay` calls `reserve_unused_port()` and rebinds only after a deliberate delay, so the race window is as wide as that delay. Worse, a lost race is currently swallowed by a `let ... else { return; }`: the stub thread just stops, the embedder stays unreachable, and the test proceeds as if the *product* were failing to distinguish "unreachable" from "reachable" — the exact condition these tests exist to verify. A port race becomes indistinguishable from a real product regression.
3. **Lower severity, but not sound either** — three "never listens" tests (`mcp_stdio_degrades_when_embedder_never_reachable`, `socket_mode_still_fails_fast_on_unreachable_embedder`, `knowledge_recover_rejected_while_embedder_unreachable_degraded`) rely on nothing ever listening on the reserved port so every connection is refused promptly. If something else binds that port in the interim, the test would get a live peer instead of "connection refused" and could assert on the wrong behavior.

Occurrence is rare and load-dependent — this is a CI-reliability issue, not a correctness bug in product code — but failure mode 2 can masquerade as a product failure, which is what makes it worth fixing rather than simply retrying the test on failure.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Close the TOCTOU window for immediate rebind (Priority: P1)

A developer or CI run executes `embedder_degraded_mcp.rs`'s `mcp_stdio_degrades_when_embedder_accepts_but_never_responds` test (which depends on `spawn_stub_embedder_that_hangs`). Under load — parallel test execution or a busy shared CI runner — another process should never be able to steal the port between reservation and the stub's own bind, because the stub no longer releases and rebinds a port number at all.

**Why this priority**: This is the failure mode that has already caused an observed `AddrInUse` panic in CI. It's the most acute and most easily fixed of the three.

**Independent Test**: Read `spawn_stub_embedder_that_hangs` and confirm there is no code path where a `TcpListener` is bound, dropped, and then a *new* `TcpListener` is bound to the same port number. Run the test repeatedly (e.g. under `cargo test ... -- --test-threads=N` with other port-hungry tests running concurrently) and confirm no `AddrInUse` panics occur.

**Acceptance Scenarios**:

1. **Given** `spawn_stub_embedder_that_hangs` is invoked, **When** it acquires its listening socket, **Then** it does so via a single `bind` whose resulting live `TcpListener` is itself moved into the spawned stub thread — never released and reacquired by port number.
2. **Given** another process binds an arbitrary ephemeral port at the same moment a test in this file calls the helper that backs `spawn_stub_embedder_that_hangs`, **When** the test's own bind executes, **Then** it cannot collide with that other process's port, because the test's bind already happened before the port number was ever exposed to calling code.

---

### User Story 2 - Make a lost race in the delayed-bind stub diagnosable (Priority: P1)

A developer or CI run executes `mcp_stdio_recovers_when_embedder_becomes_reachable_mid_retry` (which depends on `spawn_stub_embedder_after_delay`). If, on rare occasions, something else takes the reserved port during the delay window, the test must fail in a way that clearly identifies a port-acquisition problem — not fail (or hang, or produce a misleading passing/failing assertion) as if the product under test were behaving incorrectly.

**Why this priority**: This is the failure mode the issue calls out as most dangerous — a lost race here doesn't look like a port problem, it looks like a product bug, defeating the purpose of the test.

**Independent Test**: Read `spawn_stub_embedder_after_delay` and confirm the `let Ok(listener) = ... else { return; }` silent-abort path is gone, replaced with a loud failure (panic/`expect`/equivalent) on bind failure. The test's existing "connection refused before delay, successful response after" semantics (documented in its own doc comment) must be unchanged — confirm by re-running `mcp_stdio_recovers_when_embedder_becomes_reachable_mid_retry` and observing it still passes with its existing assertions intact.

**Acceptance Scenarios**:

1. **Given** the reserved port is still free when the delayed bind executes, **When** `spawn_stub_embedder_after_delay`'s stub thread wakes up after `delay`, **Then** it binds successfully and behaves exactly as it does today (serves one stub HTTP response per connection).
2. **Given** something else has taken the reserved port by the time the delayed bind executes, **When** the bind fails, **Then** the stub thread fails loudly (panics or otherwise surfaces a clear error) instead of silently returning — so the resulting test failure is attributable to a port race, not to embedder-reachability logic.
3. **Given** the delay has not yet elapsed, **When** a client connects to the reserved port, **Then** the connection attempt is refused (unchanged from current behavior) — the fix must not change this into "connection accepted then stalled."

---

### User Story 3 - Name the remaining unsound-by-design pattern distinctly (Priority: P2)

A future maintainer reading `embedder_degraded_mcp.rs` needs to be able to tell, from naming and/or doc comments alone, which port-acquisition helper gives a genuinely race-free guarantee (safe to copy for a new test that needs one) and which one is knowingly still racy because the test's own semantics require it (nothing listening → prompt "connection refused").

**Why this priority**: Lower severity than Stories 1-2 — the three "never listens" callers' residual raciness is small and accepted, per the issue. But leaving one identically-named helper serving both a sound and an unsound use case invites a future test to copy the unsound pattern by accident, believing it to be safe.

**Independent Test**: Read the helper(s) used by `mcp_stdio_degrades_when_embedder_never_reachable`, `socket_mode_still_fails_fast_on_unreachable_embedder`, and `knowledge_recover_rejected_while_embedder_unreachable_degraded`, and confirm their name and/or doc comment makes clear — without reading the full issue history — that they return a port number only as a hint, with an inherent (accepted) race, as distinct from any helper that returns a live, held `TcpListener`.

**Acceptance Scenarios**:

1. **Given** the split described in the issue's suggested fix, **When** a reader looks at the helper backing the three "never listens" tests, **Then** its name or doc comment states plainly that it releases the port and that a race is possible and accepted, without needing to consult this issue.
2. **Given** the same reader looks at the helper backing `spawn_stub_embedder_that_hangs`, **When** they compare it to the "never listens" helper, **Then** the naming/documentation makes the two easily distinguishable as different guarantees, not interchangeable options.

---

### Edge Cases

- Another process binds the reserved port in the (now narrower, and explicitly accepted) window used by the three "never listens" tests — this residual risk is retained by design per Story 3 and is not eliminated by this issue; it must simply be clearly documented as accepted.
- A delayed bind in `spawn_stub_embedder_after_delay` that loses its race must produce a failure a reader can attribute to the port race on inspection (e.g. via the panic message or a distinguishing test failure), not one that looks identical to an embedder-unreachable assertion failure.
- Fixing Story 1 or Story 2 must not alter any of the five existing tests' observable pass/fail outcomes or their timing assumptions (e.g. the `client.initialize()` timeout margins documented inline).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `spawn_stub_embedder_that_hangs` MUST acquire its listening socket via a single bind whose live `TcpListener` is moved directly into the spawned stub thread, rather than reserving a port number, dropping the listener, and rebinding that same number later.
- **FR-002**: The helper(s) backing `spawn_stub_embedder_that_hangs` MUST expose a way to obtain both a bound, live `TcpListener` and its assigned port number together, so a caller that needs to actually hold the socket never has to release and reacquire it by number.
- **FR-003**: `spawn_stub_embedder_after_delay` MUST continue to produce "connection refused" for every connection attempt before `delay` elapses, and a successful stub HTTP response for connections after the delayed bind succeeds — unchanged from current behavior.
- **FR-004**: `spawn_stub_embedder_after_delay`'s delayed bind, on failure (e.g. because another process took the port during the race window), MUST fail loudly (panic, `.expect(...)`, or equivalent) rather than silently returning from the stub thread, so a lost race is diagnosable as a port problem rather than masquerading as embedder-unreachable behavior.
- **FR-005**: The three "never listens" tests (`mcp_stdio_degrades_when_embedder_never_reachable`, `socket_mode_still_fails_fast_on_unreachable_embedder`, `knowledge_recover_rejected_while_embedder_unreachable_degraded`) MAY continue to use a helper that reserves a port by binding and then releasing it, since holding a live listener without ever accepting would change "connection refused" into "connection accepted then stalled," altering the behavior under test.
- **FR-006**: The helper(s) in this file MUST be organized and named so that the "bind then release, port number is only a hint" pattern (used by FR-005's callers) is clearly distinguishable — by name and/or doc comment, without needing outside context — from the "bind once, hold the live listener" pattern (used by FR-001/FR-002, and by `spawn_stub_embedder_after_delay`'s eventual bind per FR-003/FR-004).
- **FR-007**: This issue's changes MUST be confined to test code — no product code (anything outside `crates/service/tests/`) changes.
- **FR-008**: All five existing tests in `embedder_degraded_mcp.rs` (`mcp_stdio_degrades_when_embedder_never_reachable`, `mcp_stdio_recovers_when_embedder_becomes_reachable_mid_retry`, `socket_mode_still_fails_fast_on_unreachable_embedder`, `mcp_stdio_degrades_when_embedder_accepts_but_never_responds`, `knowledge_recover_rejected_while_embedder_unreachable_degraded`) MUST continue to pass with their existing observable assertions and timing margins unchanged.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `spawn_stub_embedder_that_hangs`'s port acquisition contains no code path that binds, drops, and rebinds a `TcpListener` to the same port number — verifiable by inspection of the diff.
- **SC-002**: A lost race in `spawn_stub_embedder_after_delay`'s delayed bind produces a panic or other loud, attributable failure — verifiable by inspection of the diff (no `let ... else { return; }` silently discarding a bind error remains on that path).
- **SC-003**: `cargo test -p lcg-service --test embedder_degraded_mcp` passes, exercising all five existing tests with unchanged assertions.
- **SC-004**: A reader of the file can, from names and/or doc comments alone, correctly identify which port-acquisition helper is race-free-by-construction and which one carries an accepted, documented residual race — without reading this issue or its GitHub history.

## Assumptions

- "Fix" is scoped to `crates/service/tests/embedder_degraded_mcp.rs` (and, if the Plan/Implement stage judges a shared helper is more natural, `crates/service/tests/common/`) — no production code changes are in scope, consistent with the issue's own framing ("Test-only; no product code is affected").
- The three "never listens" tests retain their current, small, already-existing residual race by design; this issue does not attempt to make them fully race-proof, only to name that residual risk clearly (Story 3 / FR-005 / FR-006).
- "Fail loudly" for the delayed-bind failure (FR-004) can be satisfied by any of `.unwrap()`, `.expect(...)`, or an explicit `panic!` — this spec does not mandate a specific mechanism, leaving the choice to the Plan/Implement stage.
- Exact function names, signatures, and file organization for the split described in FR-006 are an implementation decision for the Plan stage; this spec only requires that the distinction be legible to a reader.

## Out of Scope

- Making the three "never listens" tests fully race-proof (would require some other synchronization primitive or a different mocking approach, changing what's actually under test).
- Any production code changes.
- General test-infrastructure refactors beyond this file's port-handling helpers.

## Source References

- `crates/service/tests/embedder_degraded_mcp.rs` (lines 22-94 for the helpers; lines 126-295 for the five tests that use them)
- Issue #499 (bounded embedder-probe retry/degrade for standalone `--mcp-stdio`), whose acceptance scenarios this test file covers and which this issue's fix must not regress
