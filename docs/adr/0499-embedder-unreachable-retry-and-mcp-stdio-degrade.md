# ADR-0499: Bounded Embedder-Probe Retry and Degrade-Without-Opening-DB for Standalone `--mcp-stdio`

**Date**: 2026-08-25
**Status**: Accepted

## Context

`bootstrap_app_state` (`crates/service/src/main.rs`) probes the embedder before opening the
database, so a misconfigured embedder fails at startup rather than on the first embed request
(FR-010/FR-011 of the original startup-resolution work). A transport-classified probe failure was
unconditionally fatal, in both launch modes, with no retry.

That is the right call for a hand-started socket-service process: an operator watching the
terminal sees the actionable message, starts the sidecar, and retries. It is the wrong call for
standalone `--mcp-stdio`, where an MCP client (Claude Desktop, an editor's MCP integration) spawns
the process directly: nobody is watching stderr, the client shows only its own generic "server
failed to start," and the embedder story is genuinely hard on first run — the macOS sidecar
requires building a Swift CoreML pipeline from source, and hosted providers were unreachable
before #497. "Embedder not up yet" is close to the expected first-run state for this launch mode,
and it presented as a total, unexplained tool failure. See issue #499 for the full option
analysis (four candidates were costed; this ADR covers the two that were chosen).

Two structural facts, both surfaced during Research, shaped the design:

1. `Db::open` is immediately followed by `conn.init_schema(embedding_dim)`, and
   `Entity.name_embedding`/`RelatesToNode_.fact_embedding` are fixed-size `FLOAT[N]` columns — the
   database cannot be opened at all without first committing to a concrete embedding dimension.
   Today that dimension comes only from a successful probe. This rules out "start degraded but
   still serve read-only graph queries" as a cheap option: it would require either guessing a
   fallback dimension (risking a mismatch against the real embedder once reachable, forcing a
   reindex, and poisoning all three `knowledge_recover` strategies, which read `state.embedder
   .dim()` to rebuild schema) or not opening the DB at all, in which case only diagnosability is
   deliverable, not read-only queries.
2. The launch mode is already known at exactly the call site that would need to thread it through:
   `async_main`'s `match cli_mode` already has separate arms for `CliMode::Socket` and
   `CliMode::Mcp { connect: None, .. }`. Passing that distinction into `bootstrap_app_state` is a
   one-parameter change, not new configuration.

## Decision

### Bounded retry, gated on launch mode, with a fixed 5-second ceiling

`bootstrap_app_state` gained an `allow_embedder_degrade: bool` parameter, threaded from
`async_main`'s two call sites (`CliMode::Socket` → `false`; `CliMode::Mcp { connect: None, .. }` →
`true`) through `bootstrap_or_exit_on_signal`. The embedder's transport-resolution-then-probe
sequence was extracted into `resolve_and_probe_embedder`, returning a three-way outcome:

- `Ready { .. }` — probe succeeded (including the existing `LCG_EMBEDDING_DIM`-override path for a
  non-transport, non-auth probe failure — unchanged).
- `Fatal(String)` — a malformed `--embedder-http` URL, `--embedder-uds` on a non-Unix platform, or
  an authentication failure (401/403). Never retried, in either launch mode.
- `Retryable(String)` — everything that reads as "not reachable yet": an explicit
  `--embedder-uds` path that doesn't exist, the default-UDS-absent-and-no-`LCG_EMBEDDING_URL`
  case, or a transport-classified probe failure (`is_transport_error`).

With `allow_embedder_degrade == false` (socket mode), a `Retryable` outcome returns `Err`
immediately on the first attempt — zero retries, byte-for-byte the pre-#499 behavior (FR-001,
verified by `socket_mode_still_fails_fast_on_unreachable_embedder`). With `allow_embedder_degrade
== true` (standalone `--mcp-stdio`), a `Retryable` outcome is retried with exponential backoff
(250ms initial, doubling, capped at 1s per sleep) until 5 seconds of wall-clock time have elapsed
since the first attempt.

**Why the whole resolve-then-probe sequence is retried, not just the final `.probe()` call.** The
default-UDS/explicit-`--embedder-uds` existence check runs via `Path::exists()` *before* any probe
is even constructed, and is not classified through `is_transport_error` at all. If the sidecar
creates its UDS socket file only once fully up — the common case for `bind()+listen()`, and the
deployment shape the macOS bundled sidecar and this issue's own motivating smoke test used — a
retry loop wrapping only `probe_embedder.probe()` would leave that race entirely unaddressed for
what is plausibly the *more* common real-world instance of the sidecar/lcg simultaneous-launch
race, not the `--embedder-http` case alone.

**Why 5 seconds, 250ms→1s backoff.** Long enough to absorb typical sidecar process-spawn +
socket-bind timing (sub-second to low-single-digit seconds); short enough to stay well under
typical MCP client initialize timeouts (commonly 10s+). This is a reasoned estimate, not measured
against a specific client's own timeout — if real-world reports show it's wrong, it is a single
constant to tune, not a structural change.

### Retry window exhausted ⇒ degrade without ever opening the database

If the retry window elapses without success, `bootstrap_app_state` does not call `Db::open` at
all — the embedding dimension was never established, and per the structural fact above there is no
safe way to construct a database at this point. It constructs `AppState` directly via the same
`AppState::from_env(..., db: None, degraded_reason: Some(reason), ...)` shape the existing
WAL-corruption degraded branch already produces, with a new `degraded_reason` value,
`EMBEDDER_UNREACHABLE_DEGRADED_REASON = "embedder_unreachable_at_startup"` (a `pub const` in
`crates/core/src/embedder.rs`, shared by `main.rs` and `handlers.rs`).

`AppState.embedder: Arc<dyn Embedder>` is non-optional and must still be populated. `main.rs`
constructs `UnconfiguredEmbedder` — a zero-state struct whose `embed()` always errors and whose
`dim()` uses the trait's default (768), mirroring the existing `UnconfiguredExtractor` precedent
(#331: "validate the extraction provider on first use, not at startup"). This satisfies the
type without touching `AppState.embedder`'s non-optionality or any of the ~15 `.embed()` call
sites in `handlers.rs` — none of them are reachable anyway, since `handle`'s blanket degraded-mode
guard (`crates/core/src/handlers.rs`) rejects every method except a fixed exempt list whenever
`state.db.load_full().is_none()`.

This delivers diagnosability — a client can call `knowledge_status` from within the MCP session
and learn the embedder is the problem — but deliberately not read-only graph queries, which still
require an open database and therefore a committed dimension. That fuller scope (serving reads
while the embedder is degraded) is explicitly deferred; see Consequences.

### `knowledge_recover` rejects every strategy for this specific reason, not per-strategy

`knowledge_recover`/`knowledge_recover_full` *are* in the degraded-mode exempt list, and both read
`state.embedder.dim()` before dispatching to a strategy — the one gap the scope guard above didn't
automatically close. Tracing the three strategies: `drop_lbug_wal` and `restore_from_backup`
reopen an *existing* DB file, and `schema::init`'s `CREATE NODE TABLE IF NOT EXISTS` is a no-op
against an already-existing table regardless of the `dim` argument passed — so those two would
actually be harmless even with a placeholder dimension. `rebuild_from_workspace_wal`, however,
deletes the DB file first and creates schema fresh from whatever `dim` it's given; it has no
"must already have valid existing data" precondition unlike the other two, and would silently
commit `UnconfiguredEmbedder`'s fabricated placeholder dimension permanently over what could be a
perfectly good prior workspace — reporting success in a way indistinguishable from a genuine
recovery via `knowledge_status` alone.

Rather than special-case per-strategy safety — which would depend on `schema::init`'s
`IF NOT EXISTS` idempotence as a load-bearing fact, an implementation detail rather than a
contract, and would need re-deriving correctly on every future change to that function — both
`handle_knowledge_recover` and `handle_knowledge_recover_full` reject **every** strategy outright
when `degraded_reason == EMBEDDER_UNREACHABLE_DEGRADED_REASON`, before `state.embedder.dim()` is
ever read. `handle_knowledge_status`'s degraded branch reports `recovery_available: []` for this
specific reason instead of the usual `["drop_lbug_wal", "rebuild_from_workspace_wal"]`, keeping
the advertised list truthful. The only way out of this degraded state is restarting the process
once the embedder is reachable — no new recovery strategy is introduced by this issue.

### `LCG_EMBEDDING_DIM` does not bypass the retry/degrade path

FR-011's existing rule — a dimension override cannot paper over an unreachable embedder — is
deliberate and unchanged. A guessed dimension for an embedder that was never actually reached is
exactly the "guess a fallback dimension" risk this issue's own Decision explicitly rejected for
the fuller read-only-while-degraded scope. This issue adds retry+degrade around the
transport-error case; it does not add a new override path through it.

### `is_transport_error` also recognizes `reqwest`'s request-kind pool errors

While writing the binary-level regression test for the retry-then-succeed race
(`mcp_stdio_recovers_when_embedder_becomes_reachable_mid_retry`), a real classification gap
surfaced: `is_transport_error` checked only `reqwest::Error::is_connect()`/`is_timeout()`, but a
client racing a listener's very first moment of accepting connections can get a
`hyper_util`/`hyper` "connection was not ready" pool-cancellation error — `Kind::Request`, not
`Kind::Connect` — instead of a plain connection-refused error. `is_request()` is a superset of
`is_connect()` (verified directly: every `is_connect()` error is also `is_request()`) and is
structurally disjoint from `Kind::Status` (what `is_auth_error` checks) and `Kind::Decode` (the
"unexpected response shape" catch-all branch) — sending a request and getting back a bad status or
an unparseable body are never `is_request()` errors. Folding `is_request()` into
`is_transport_error`'s `Error::Http` arm was therefore a safe, narrowly-scoped fix for a real (if
narrow) false-fatal-classification gap in exactly the race this issue's retry loop exists to
cover, not merely a workaround for test flakiness.

## Consequences

- A user launching `liminis-context-graph --mcp-stdio` with no embedder yet running, whose
  embedder never becomes reachable within 5 seconds, gets a process that starts and names the
  problem via `knowledge_status` — not a silent, unexplained failure the MCP client can't explain
  (SC-001).
- A hand-started socket-service process with the same misconfiguration is byte-for-byte unchanged:
  same message, same immediate exit (SC-002).
- The sidecar/lcg simultaneous-launch race — the common case this issue was filed against —
  resolves to a normal, non-degraded startup when the embedder comes up within the retry window
  (SC-004), covering both the UDS-socket-existence race and the HTTP-probe race.
- **Read-only-while-embedder-degraded is explicitly out of scope, not merely deferred by
  omission.** Serving graph queries or non-embedding operations while the database is open but the
  embedder is not would require resolving the dimension-commitment problem (structural fact 1
  above) across both initial schema creation and all three `knowledge_recover` strategies — a
  materially larger, separate follow-on. If a future change finds it must touch
  `AppState.embedder`'s non-optionality or any `.embed()` call site to deliver something in this
  area, that is a signal the design needs revisiting, not silently expanding this issue's scope.
- `knowledge_recover` is unavailable for the entire duration of an embedder-unreachable degraded
  session, even for the two strategies (`drop_lbug_wal`, `restore_from_backup`) that would have
  been safe in isolation — a deliberate simplicity-over-permissiveness tradeoff. The only recovery
  path is restarting the process once the embedder is reachable.
- The `is_request()` broadening to `is_transport_error` is a small increase in what counts as
  "retry this instead of failing fast," applying identically to the socket-service path's own
  first-attempt classification (though socket mode never retries, so the practical effect there is
  limited to which error message a `Retryable`-classified failure produces, not whether it's
  fatal).
