# Feature Specification: Integration Tests Leak Spawned liminis-context-graph Processes

**Feature Branch**: `fabrik/issue-500`
**Created**: 2026-08-25
**Status**: Specified
**Input**: User description: "Integration tests leak spawned liminis-context-graph processes: 11 orphans found, oldest 11 days"

## Background

Integration tests that spawn the `liminis-context-graph` binary leak it. The processes
outlive the test run, are reparented to launchd (`ppid=1`), and keep running indefinitely.

Observed on a developer machine on 2026-08-25 — **11 orphaned processes, 243 MB RSS**, the
oldest running for **11 days 21 hours**:

| PID | age | started | worktree |
|---|---|---|---|
| 95275 | 11d 21h | 2026-08-13 02:55 | `.fabrik/worktrees/.../issue-378` |
| 4726 | 8d 08h | 2026-08-16 16:25 | `liminis-context-graph/target/release` |
| 955 | 5d 15h | 2026-08-19 09:04 | `.fabrik/worktrees/.../issue-437` |
| 89862 | 5d 15h | 2026-08-19 09:23 | `issue-437` |
| 97222 | 5d 14h | 2026-08-19 09:32 | `issue-437` |
| 99274–99279 (6) | 5d 14h | 2026-08-19 09:37:58 | `issue-437` |

All 11 had `ppid=1`. The last six share a start timestamp to the second — a single test run
leaked its whole batch.

Their command lines identify them as test-spawned:

```
--embedder-http http://127.0.0.1:56922/v1/embeddings \
--extractor-http http://127.0.0.1:1/v1/chat/completions
```

Ephemeral high ports for the embedder (per-test stub servers) and `127.0.0.1:1` for the
extractor (the deliberately-unreachable-endpoint pattern). **Nothing was listening on any of
those embedder ports** — the stub servers exited with the test run; only the binaries
survived. Two of the worktrees (`issue-378`, `issue-437`) are for work that has since merged,
so these outlived not just their tests but their branches.

**Impact:**

- Unbounded accumulation of processes and memory on any machine that runs the integration
  suite repeatedly — 243 MB observed here, from perhaps three runs.
- Each leaked process holds open a workspace under a Fabrik worktree, which can block
  worktree cleanup and keep deleted-branch working directories pinned.
- A stale process still bound to a workspace socket can be picked up by a later run, giving
  confusing cross-run interference.

**Root cause is not yet established**, and is deliberately part of this issue's scope rather
than assumed:

- **Harness hypothesis**: a test drops its `Child` handle without killing it, or kills its
  stub embedder/extractor first and never signals the binary. Rust's `std::process::Child`
  does *not* kill on drop, so a test that returns early — including via panic or a failed
  assertion — leaks by default unless something explicitly guards against it.
- **Binary hypothesis**: the binary fails to exit when signalled, or wedges in its graceful
  shutdown path once its embedder/extractor become unreachable.

A note on the second hypothesis: `crates/service/src/main.rs` installs SIGTERM/SIGINT
handlers and bounds the shutdown drain at `LCG_SHUTDOWN_TIMEOUT_MS` (default 5,000 ms). When
these 11 were reaped, `SIGTERM` had not taken effect after 3 seconds and `SIGKILL` was used —
but 3 s is *less than* the 5 s timeout, so that observation does not establish that SIGTERM
was ignored. It needs to be re-measured properly (send SIGTERM, wait longer than the
configured timeout, then observe) rather than assumed either way.

A preliminary look at `crates/service/tests/` (informational, not a substitute for the
Research stage's own investigation) shows the test harness already has one guard: `McpClient`
(`crates/service/tests/common/mod.rs`) implements `Drop` to kill and reap its child
unconditionally. However, several test files spawn the binary via a bare
`std::process::Command`/`Child` without going through `McpClient` — for example
`clean_shutdown.rs`, `migration_binary.rs`, `eager_index_build.rs`, `mcp_progress.rs`,
`mcp_attached.rs`, and `mcp_real_corpus_admin_lifecycle_e2e.rs` each contain at least one raw
spawn. This is consistent with the harness hypothesis but does not rule out the binary
hypothesis, and does not by itself explain the batch of six processes leaked in a single
run — root-cause work in Research should treat this as a starting point, not a conclusion.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Test suite leaves no orphaned processes behind (Priority: P1)

As a contributor (or Fabrik worker) running the integration test suite — including runs
where a test fails or panics — no `liminis-context-graph` process spawned by that run should
still be alive once the suite finishes.

**Why this priority**: This is the actual harm described in the issue: unbounded process/RSS
accumulation, pinned worktrees blocking cleanup, and cross-run interference from stale
sockets. Everything else in this issue exists to achieve this outcome.

**Independent Test**: Run the full integration suite (`cargo test -p lcg-service`, or a
targeted subset), including at least one test that is made to panic or fail an assertion
after spawning the binary, then check `pgrep -f "liminis-context-graph "` for any survivor
with `ppid=1`. None should remain.

**Acceptance Scenarios**:

1. **Given** an integration test that spawns the binary and completes normally (pass or
   fail), **When** the test process exits, **Then** the spawned binary is no longer running.
2. **Given** an integration test that spawns the binary and then panics or fails an assertion
   before it would normally reap the child, **When** the test process exits, **Then** the
   spawned binary is no longer running.
3. **Given** a full suite run with many tests spawning the binary (sequentially or under
   `cargo test`'s default parallelism), **When** the run completes, **Then** `pgrep -f
   "liminis-context-graph "` finds zero processes with `ppid=1` that were spawned by that run.

---

### User Story 2 - Root cause is established with evidence (Priority: P1)

As the engineer fixing this issue, I need to know — with a reproducible measurement, not an
inference from an inconclusive prior observation — whether the leak is caused by the test
harness failing to reap its children, by the binary failing to honor graceful shutdown when
its embedder/extractor are unreachable, or both, so the fix addresses the actual cause(s)
rather than only the one that happens to be cheaper to patch.

**Why this priority**: The issue's own prior observation (SIGKILL after 3 s, against a 5 s
timeout) is explicitly inconclusive. Fixing only the harness side while leaving a genuine
binary-side shutdown wedge unaddressed would still leave production/manual invocations
vulnerable to the same failure mode; fixing only the binary side while the harness still
drops `Child` handles without killing them would not fully close the leak.

**Independent Test**: Reproduce a leaked process from a real test run, send it `SIGTERM`,
wait strictly longer than `LCG_SHUTDOWN_TIMEOUT_MS` (default 5,000 ms), and observe whether
the process has exited.

**Acceptance Scenarios**:

1. **Given** a running `liminis-context-graph` process with an unreachable embedder and/or
   extractor endpoint (mirroring the observed `--embedder-http`/`--extractor-http` leaked
   command lines), **When** it receives `SIGTERM` and more than `LCG_SHUTDOWN_TIMEOUT_MS`
   elapses, **Then** the outcome (exited cleanly within the timeout, exited only after the
   timeout via forced shutdown, or still running) is recorded as evidence.
2. **Given** the harness-side spawn sites identified during investigation, **When** each is
   reviewed, **Then** it is determined for each whether it already guards against leaks (e.g.
   via `McpClient`'s `Drop` impl) or spawns the binary without any such guard.

---

### User Story 3 - Regression is cheap to detect going forward (Priority: P2)

As a contributor, I want a documented or automated way to check for leaked
`liminis-context-graph` processes after a test run, so a future regression in this area is
caught before it silently accumulates over days or weeks the way this one did.

**Why this priority**: The issue's own suggested scope frames this as a guard against
*recurrence*, not the leak fix itself — valuable, but secondary to actually stopping the
leak (User Story 1).

**Independent Test**: Follow the documented check (or run the automated one) after a test
run known to be clean, and confirm it reports zero leaked processes; then after artificially
reintroducing a leak (e.g. temporarily reverting the harness fix), confirm it reports the
leak.

**Acceptance Scenarios**:

1. **Given** the fix from User Story 1 is in place, **When** the documented/automated check
   is run after a full suite run, **Then** it reports zero leaked processes.
2. **Given** the check exists, **When** a contributor unfamiliar with this issue reads the
   contributor docs, **Then** they can find and run it without needing to reconstruct the
   `pgrep` invocation from scratch.

---

### Edge Cases

- A test spawns the binary but the spawn itself fails before a guard/wrapper is constructed
  (no leak possible in this case — nothing was spawned).
- A test spawns more than one binary instance in a single test function (e.g. a multi-stream
  or lifecycle test); all instances must be reaped, not just the first.
- A test's spawned binary has already exited (e.g. it crashed on its own) by the time cleanup
  runs — cleanup must be idempotent against an already-exited child, not error or hang.
- Tests run under `cargo test`'s default thread-parallelism, so multiple binaries may be
  spawned and torn down concurrently; the fix must not introduce cross-test interference
  (e.g. one test's cleanup must not affect another test's still-running process).
- A worktree is deleted (e.g. after a Fabrik issue's branch merges) while a process it spawned
  is still alive and holding files open in that worktree — this is the mechanism by which the
  two already-merged worktrees (`issue-378`, `issue-437`) stayed pinned; the fix should make
  this unreachable going forward rather than needing manual cleanup after the fact.
- If the binary hypothesis is confirmed, the same unreachable-embedder/extractor condition can
  occur outside of tests (e.g. a manually-run or misconfigured production instance) — the fix
  to the shutdown path, if needed, should not be test-only.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A definitive determination of root cause MUST be made by reproducing a leaked
  process and sending it `SIGTERM`, then waiting strictly longer than
  `LCG_SHUTDOWN_TIMEOUT_MS` before observing whether it has exited — not inferred from the
  issue's prior inconclusive 3-second observation.
- **FR-002**: Every integration test in `crates/service/tests/` (including its `common/`
  helpers) that spawns the `liminis-context-graph` binary MUST ensure that binary is
  terminated and reaped before the test binary process exits, regardless of test outcome
  (pass, fail, panic, or early return via `?`/early `return`).
- **FR-003**: The mechanism satisfying FR-002 MUST be applied uniformly across all spawn
  sites, not only the ones that already happen to route through `McpClient` — including any
  spawn site that constructs a `Child` directly.
- **FR-004**: The mechanism satisfying FR-002 MUST be idempotent — safe to invoke on a child
  that has already exited or already been explicitly reaped by other code in the same test.
- **FR-005**: If FR-001's investigation attributes any part of the leak to the binary's own
  shutdown path (i.e. it does not exit within `LCG_SHUTDOWN_TIMEOUT_MS` of receiving
  `SIGTERM` when its embedder/extractor endpoints are unreachable), that shutdown path MUST
  be fixed so an unreachable embedder/extractor cannot prevent process exit within the
  configured timeout.
- **FR-006**: A repeatable way to detect this class of regression (leaked, `ppid=1`
  `liminis-context-graph` processes surviving a test run) MUST be added, either as an
  automated check or as a documented command in the contributor docs.
- **FR-007**: The fix MUST NOT require re-running or re-architecting the currently-passing
  parts of the integration suite beyond what's needed to add the leak-prevention mechanism —
  this is a hygiene fix, not a suite redesign.

### Key Entities

- **Spawned `liminis-context-graph` process**: A child OS process started by an integration
  test via `std::process::Command`, running the built binary under test (with
  `--embedder-http`/`--extractor-http`/`--mcp-stdio` or similar flags pointed at test
  fixtures/stubs).
- **Orphaned process**: A spawned process whose parent (the test harness) has exited while
  the child is still running, causing it to be reparented to `launchd` (`ppid=1`) and
  continue running indefinitely, undetected by the test run that created it.
- **Shutdown path**: The binary's own signal-handling and graceful-drain logic
  (`crates/service/src/main.rs`, SIGTERM/SIGINT handlers, `LCG_SHUTDOWN_TIMEOUT_MS`-bounded
  drain) that determines how quickly it exits once asked to.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After running the full integration suite — including at least one test that is
  made to panic or fail deliberately after spawning the binary — `pgrep -f
  "liminis-context-graph "` finds zero surviving processes with `ppid=1` that trace back to
  that run.
- **SC-002**: The root-cause question (harness bug, binary bug, or both) is answered with
  recorded reproduction evidence: a `SIGTERM` sent to a process with unreachable
  embedder/extractor endpoints, a wait longer than `LCG_SHUTDOWN_TIMEOUT_MS`, and the observed
  outcome.
- **SC-003**: If the binary's shutdown path was found faulty under FR-001/FR-005, re-testing
  after the fix shows it exits within `LCG_SHUTDOWN_TIMEOUT_MS` (plus reasonable OS-signal
  delivery latency) of receiving `SIGTERM`, even with unreachable embedder/extractor
  endpoints.
- **SC-004**: A contributor can run a documented command, or an automated check integrated
  into the test/CI workflow, that reports leaked `liminis-context-graph` processes after a
  test run — and it reports zero after a clean run.
- **SC-005**: No new integration test added after this fix lands can spawn the binary without
  going through whatever mechanism satisfies FR-002/FR-003 — i.e. the fix closes the class of
  bug, not just the specific instances found on 2026-08-25.

## Assumptions

- The 11 processes observed on 2026-08-25 are a sample of an ongoing, unbounded leak, not a
  one-time anomaly — the fix targets the underlying cause, not just those 11 instances.
- Manually cleaning up the 11 already-leaked processes and their pinned worktrees on the
  reporting developer's machine is a one-time operational action, not a deliverable of this
  issue (see Out of Scope).
- `LCG_SHUTDOWN_TIMEOUT_MS`'s existing default (5,000 ms) is an acceptable upper bound on
  graceful-shutdown time for test purposes; this issue does not ask to change that default.
- The harness-side fix (ensuring every spawn site reaps its child) is worth doing regardless
  of what FR-001 finds about the binary, since `std::process::Child` not killing on drop is a
  latent bug independent of whatever the binary does — this issue does not gate FR-002/FR-003
  on FR-001's outcome.
- "Integration tests" in scope means the suites under `crates/service/tests/` that spawn the
  built `liminis-context-graph` binary as a subprocess; unit tests and tests that exercise
  library code in-process (no subprocess) are unaffected and out of scope.

## Out of Scope

- Manually killing or cleaning up the 11 already-leaked processes (or their pinned worktrees)
  found on 2026-08-25 — that is a one-time operational cleanup, not a code change.
- Any general refactor of process/subprocess management in the codebase beyond what's needed
  to fix this leak and (if implicated) the shutdown path.
- Changing `LCG_SHUTDOWN_TIMEOUT_MS`'s default value or making it configurable in new ways.
- Non-test (production/manual-invocation) process supervision — e.g. adding a supervisor,
  systemd unit, or launchd job to auto-restart or auto-kill stray production instances. If
  FR-005 fixes the shutdown path, that fix benefits production use too, but adding new
  production process-lifecycle tooling is not part of this issue.

## Source References

- `crates/service/tests/common/mod.rs` — `McpClient`, including its existing `Drop` impl
  (lines ~317–387, ~538–543).
- `crates/service/tests/common/real_corpus.rs` — `SeededWorkspace::spawn_reader`, an existing
  spawn site that already routes through `McpClient`.
- `crates/service/tests/clean_shutdown.rs`, `mcp_clean_shutdown.rs`, `migration_binary.rs`,
  `eager_index_build.rs`, `mcp_progress.rs`, `mcp_attached.rs`,
  `mcp_real_corpus_admin_lifecycle_e2e.rs` — spawn sites to audit for FR-002/FR-003.
- `crates/service/src/main.rs` — SIGTERM/SIGINT handlers and `LCG_SHUTDOWN_TIMEOUT_MS`-bounded
  shutdown drain relevant to FR-001/FR-005.
