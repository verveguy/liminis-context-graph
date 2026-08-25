# ADR-0500: Unified Pre-Bootstrap Signal Handling Across All `CliMode`s

**Date**: 2026-08-25
**Status**: Accepted
**Issue**: liminis-context-graph#500

## Context

Issue #500 reported 11 orphaned `liminis-context-graph` processes accumulating across Fabrik
worktrees, some outliving the worktree/branch they were spawned from by over a week. Research
established the root cause was a **startup race window**, not a wedged graceful-shutdown drain
(ADR-0013's cancellation token, ADR-0017's runtime-drop-based `Arc<Db>` release, and the
`tokio::time::timeout`-bounded join-set drain all continued to work correctly and fast once
reached):

- `main()` calls `sigterm_diag::register()` (added by #247) before the tokio runtime is even
  built. That handler's entire body records the sending PID into a diagnostic atomic — it does
  not trigger shutdown. Once registered, though, it replaces the OS's default SIGTERM
  disposition (terminate) for the rest of the process's life.
- The *real* handler — the one that actually cancels and drains — was installed separately in
  each of `run_socket_service` and `run_mcp_standalone`, and not at all in `run_mcp_attached`.
  All three of those functions are only reached *after* `bootstrap_app_state` completes: DB
  open, workspace migration, and — critically — the embedder/extractor startup probes, which are
  unbounded network I/O.

Any SIGTERM/SIGINT delivered between `sigterm_diag::register()` and whichever of those three
functions was eventually reached fell into a gap: the diagnostic handler consumed it (logging
nothing about shutdown) and the OS does not requeue or redeliver a signal once a handler has
been registered and returns. The process then ran on indefinitely, reachable only by `SIGKILL`.

**Empirical confirmation (FR-001/SC-002).** A controlled reproduction against socket mode, using
the exact leaked command-line shape (`--embedder-http http://127.0.0.1:<ephemeral>/v1/embeddings
--extractor-http http://127.0.0.1:1/v1/chat/completions`), sent `SIGTERM` while readiness was
gated only on the Unix socket accepting a `connect()` (true as soon as `listen()` has been
called, per ADR-0009 — well before `bootstrap_app_state` completes). No `"received SIGTERM"` log
line ever appeared; the process was still running after 10 s, 2× the default
`LCG_SHUTDOWN_TIMEOUT_MS` (5000 ms), and required `SIGKILL`. Gating readiness on an actual
`knowledge_status` IPC round-trip instead (proving `bootstrap_app_state` had already
finished and the real handler was already installed) produced a clean exit, status 0, in under a
second — confirming the drain logic itself was never the problem. This is fully consistent with
the issue's own inconclusive prior observation ("`SIGKILL` after 3 s, less than the 5 s
timeout"): 3 s wasn't "still draining," it was "no handler was ever listening," so no amount of
extra waiting would have produced a graceful exit.

This also explains the batch of six processes leaked in a single run in the original report (all
sharing a start timestamp to the second): several parallel tests under `cargo test`'s default
thread-parallelism can race the same startup window together under shared machine load.

`run_mcp_attached` (`--mcp-stdio --connect <socket>`) was a more severe instance of the same
architectural gap: it never installed any signal handler at all, so once `sigterm_diag::register`
had fired, an attached-mode process could never be gracefully terminated by SIGTERM for its
entire lifetime, not just during startup. This mode wasn't implicated in the observed leak (the
leaked command lines were socket mode), but leaving it unhandled while fixing the other two paths
would be the same bug left standing in a harder-hitting form.

A companion, independent harness-side fix (`ChildGuard` in
`crates/service/tests/common/mod.rs`, applied to every raw `Command`/`Child` spawn site in
`crates/service/tests/`) closes every *observed* instance of the leak on its own, because
`Child::kill()` sends `SIGKILL`, which is unblockable by any userspace race. That fix is
necessary regardless of this ADR's decision (`std::process::Child` not killing on drop is a
latent bug independent of what the binary does), but it does not address the underlying
binary-side bug: a manually-run or supervised production instance signalled during its own
startup window would hang exactly the same way. This ADR is the fix for that.

## Decision

Install the process's one real SIGTERM/SIGINT handling **once**, at the very top of
`async_main` — before `migrate_workspace`, before `bootstrap_app_state`, before any
mode-specific work of any kind — via a single `tokio_util::sync::CancellationToken` shared by
every downstream consumer:

1. `install_shutdown_signal_handlers(shutdown_ct: CancellationToken)` spawns the SIGTERM
   (`#[cfg(unix)]`) and SIGINT listener tasks that call `shutdown_ct.cancel()`. This replaces the
   two separate, duplicated registration blocks previously inside `run_socket_service` and
   `run_mcp_standalone`, and is the first thing `async_main` does after standing up the
   telemetry sink — strictly before `migrate_workspace` or any `bootstrap_app_state` call.
2. `bootstrap_or_exit_on_signal` races `bootstrap_app_state(...)` against
   `shutdown_ct.cancelled()` via `tokio::select!` (biased toward the cancellation branch). If the
   signal wins, `async_main` logs, drains the telemetry sink, and returns `Ok(())` — the same
   controlled-exit shape the rest of the codebase already uses, never `std::process::exit`. If
   `bootstrap_app_state` wins, its `Arc<AppState>` proceeds exactly as before.
3. `run_socket_service` and `run_mcp_standalone` now take `shutdown_ct: CancellationToken` as a
   parameter instead of registering their own handler. `run_socket_service`'s pre-existing
   `Arc<Notify>`-based accept-loop trigger (also notified directly by `handle_connection` on a
   `knowledge_close` call) is preserved as-is; a small forwarding task calls
   `shutdown_ct.cancelled().await` and then `notify.notify_one()`, so both triggers feed the same
   break condition without duplicating the accept loop's logic.
4. `run_mcp_attached` now also takes `shutdown_ct` and calls `server.serve_with_ct(...)` (the same
   `rmcp::ServiceExt` method `run_mcp_standalone` already used) instead of the bare `.serve(...)`
   it previously called, closing its total absence of signal handling. Attached mode holds no
   DB/WAL state of its own, so there is no cancel/drain/drop tail to run afterward —
   `serve_with_ct` closing the loop is sufficient.

`sigterm_diag::register()` in `main()` is untouched. It is genuinely diagnostic-only (records the
sender PID for FR-008-style stderr assertions in tests) and, per its own doc comment, is designed
to coexist with a later real handler via `signal_hook_registry`'s handler-chaining rather than
clobbering a single `sigaction` slot. It was never the bug — the bug was that nothing else was
registered yet during the startup window it covers.

## Rationale

**Why one shared token instead of three separate reorderings.** The three call sites
(`run_socket_service`, `run_mcp_standalone`, `run_mcp_attached`) previously duplicated (or, in
the attached case, omitted) the same signal-registration block. Installing it once at the top of
`async_main` and threading the resulting token down removes the duplication *and* closes the
race in one place, rather than three separately-reasoned-about patches that could drift out of
sync with each other over time.

**Why race `bootstrap_app_state` explicitly, rather than relying on the token being observed
later.** Once `shutdown_ct` is cancelled, that state is permanent and visible to every later
`.cancelled()` call — so even without an explicit race, a *subsequent* accept loop would see an
already-cancelled token on its first poll and exit promptly. But that alone does not help while
`bootstrap_app_state`'s own `.await` (the embedder/extractor probes in particular) is still
in flight: nothing observes the cancellation until that future resolves on its own. Racing it
directly via `tokio::select!` is what actually bounds worst-case shutdown latency to
`LCG_SHUTDOWN_TIMEOUT_MS` even when an embedder/extractor endpoint is slow or unreachable — the
condition FR-005 specifically requires be handled.

**Why dropping the losing `bootstrap_app_state` future is safe (ADR-0017 compatibility).**
ADR-0017 established that `std::process::exit` must never run ahead of the tokio runtime
draining its blocking pool, because an in-flight `spawn_blocking` task can still hold an
`Arc<Db>` clone whose drop performs the WAL checkpoint. This decision does not introduce a new
early-exit path in that sense: dropping the losing future in `tokio::select!` drops whatever
locals it held (including a partially-opened `Db`/`Arc<Db>`, if `bootstrap_app_state` had reached
that point) through the ordinary Rust `Drop` path — the identical mechanism the existing
`drop(state)` calls in `run_socket_service`'s and `run_mcp_standalone`'s own shutdown tails
already rely on. If cancellation arrives while `bootstrap_app_state`'s startup self-recovery
branch has already `tokio::task::spawn_blocking`'d a WAL-replay task, dropping the `.await` on
that `JoinHandle` does not abort the spawned OS thread — it keeps running detached, exactly as it
already does for a normal shutdown, bounded by `main()`'s own `runtime.shutdown_timeout` call
after `block_on` returns. No new unbounded-wait or corruption path is introduced.

**Why `run_mcp_attached` is in scope, not deferred.** It is architecturally the same bug — no
consumer of the shutdown signal beyond the diagnostic no-op — in a strictly worse form (permanent
for the process's whole life, not just a startup window). It was cheap to close: attached mode
opens no DB and holds no WAL state, so there is no bootstrap race to reason about, and
`serve_with_ct` is a pattern `run_mcp_standalone` already established.

## Consequences

- **No unbounded async I/O may run in `async_main` before `shutdown_ct` is installed and, for
  any step whose duration is not tightly bounded, raced against.** A future contributor adding
  new startup work ahead of `run_socket_service`/`run_mcp_standalone`/`run_mcp_attached` must
  either place it after signal-handler installation (already guaranteed, since installation is
  the first statement in `async_main`) or explicitly race it the same way
  `bootstrap_or_exit_on_signal` races `bootstrap_app_state`, if that work can itself block for an
  unbounded time. Silently adding a new unraced `.await` between handler installation and one of
  those three functions would reintroduce a narrower version of this exact bug for that step.
- **`run_socket_service`/`run_mcp_standalone`/`run_mcp_attached` no longer install their own
  signal handlers.** They accept `shutdown_ct: CancellationToken` as a parameter instead. A
  caller (or future fourth `CliMode` arm) that constructs a fresh `CancellationToken::new()`
  rather than reusing the one from the top of `async_main` would silently reintroduce the gap for
  that path.
- **Harness (test-side) leak prevention remains independently necessary.** This ADR closes the
  binary-side race; the companion `ChildGuard` fix in `crates/service/tests/common/mod.rs`
  closes the harness-side gap (a test panicking before it explicitly reaps its spawned child).
  Neither supersedes the other — see the companion commit for FR-002/FR-003.
- **No change to the documented SIGTERM contract.** The binary still exits within
  `LCG_SHUTDOWN_TIMEOUT_MS` of receiving SIGTERM (default 5000 ms) — that contract is now
  actually honored during the startup window too, not altered.
- **`sigterm_diag`'s diagnostic handler is unaffected** and continues to coexist via
  `signal_hook_registry`'s chaining; no coordination between it and `shutdown_ct` was needed or
  added.
