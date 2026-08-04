# ADR-0341: Build Release Artifacts Once and Share Across the Test and E2E Jobs

**Status**: Accepted
**Date**: 2026-08-04
**Issues**: #341

## Context

On a code-touching PR, six CI jobs each ran a full `cargo build --release` of the same
workspace at the same commit: `ci.yml`'s `test` job, plus the five jobs in
`real-corpus-e2e.yml` (`real-corpus-e2e`, `mcp-real-corpus-e2e`, `mcp-write-mutation-e2e`,
`mcp-admin-data-e2e`, `mcp-admin-lifecycle-e2e`). ADR-0328 put those five jobs on the PR
path, running in parallel with `test` specifically so they'd add no latency to a PR's
time-to-mergeable — but it also made them five more concurrent readers of the shared
`lbug-cache-*` key that `ci.yml`'s `build-lbug` job writes.

#336 (2026-08-04, `main`) failed `mcp_real_corpus_e2e` with a `duplicate symbol:
antlr4::atn::ATNConfig::ATNConfig(...)` link error — the exact race `ci.yml`'s own header
comment already documented from an earlier incident (two jobs racing on the same cache
key, one linking against a half-written archive set). A re-run of the identical commit
went green. This is the empirical answer to ADR-0328's own FR-005 open question: yes,
adding five more concurrent cache readers aggravated the race, at least once.

Both problems (six redundant builds; five more racers on one cache key) share a root
cause: every one of those six jobs independently invoked `cargo build --release`,
producing near-identical, uncoordinated output.

## Decision

### 1. `build-release` is the workspace's one and only `cargo build --release` per run

A single job (replacing the old `build-lbug`) now performs `cargo build --release`
followed by `cargo test --release --no-run --message-format=json` — compiling every
integration test target, including the five e2e test binaries, without executing them.
This is the sole full-workspace compile in the entire CI run (FR-001/SC-001). It is also
now the *only* writer of the `lbug-cache-*` key (FR-003) — every job downstream of it
only restores build outputs, never builds them.

### 2. Two different hand-off mechanisms, chosen by what each consumer actually needs

- **`test`** needs the *entire* compiled `target/release` tree — clippy, the R-003 bench
  gate, and `cargo fmt` all operate over more than the six e2e binaries. It restores the
  whole tree via a **run-scoped `actions/cache`** key, `target-release-${{
  github.run_id }}`, saved explicitly by `build-release` and restored by `test` with
  `fail-on-cache-miss: true`.
- **The five e2e jobs** need exactly six binaries: their own compiled test executable
  plus the `liminis-context-graph` service binary. They get these via a small
  `actions/upload-artifact`/`download-artifact` pair, scoped to a
  `target/release/e2e-artifacts/` staging directory that `build-release` populates by
  locating each test binary's cargo-hashed path via `cargo test --no-run`'s
  `--message-format=json` stream and copying it to a fixed name.

**Why not one uniform mechanism for both?** Forcing everything through one channel means
either transferring the full ~2.7GB tree to five jobs that need six binaries (violates
FR-004, which explicitly forbids this), or giving `test` only six binaries and making it
recompile everything else (violates FR-001). FR-004's own text names exactly the six
binaries FR-002 lists as what the e2e jobs need — reading it as scoped to those five
jobs, not to `test`'s hand-off, is the only interpretation under which FR-001 and FR-004
are simultaneously satisfiable. `test`'s cache-based hand-off still independently
satisfies FR-003 (no job reads a key another job is concurrently writing): the run-scoped
key makes it a strict single-writer/single-reader relationship regardless of transfer
size.

**Why the run-scoped cache key structurally prevents the #336 race, not just makes it
less likely:** `github.run_id` is unique per workflow run. No job in any other run can
ever observe this key, written or not. Within this run, exactly one job writes it
(`build-release`, once) and exactly one job reads it (`test`, once, after `needs:`
guarantees `build-release` completed). There is no second writer to race against.

**Cross-job mtime normalization, required for the `test` hand-off to actually be a
cache hit.** Cargo's freshness check for path-dependency crates (every crate in this
workspace) is mtime-based, not content-hash-based: a unit is stale if any source file's
mtime differs from what was recorded when its fingerprint was written. `actions/checkout`
does not preserve git commit timestamps — each job's checkout stamps files with that
job's own wall-clock checkout time. Since `test` checks out independently and strictly
*after* `build-release` finishes building, its sources would always read as newer than
the fingerprints recorded during `build-release`'s build, making cargo recompile the
entire workspace on every run regardless of the cache restore — silently defeating
FR-001 and tripping the FR-006 guard every time. Both `build-release` and `test` run an
identical `git ls-files -z | xargs -0 touch -d "@$(git log -1 --format=%ct)"` step right
after checkout, stamping every tracked file to the commit's own timestamp — a value
that's identical and deterministic across both jobs' independent checkouts, so the
mtime comparison lines up and the restored build is recognized as fresh.

### 3. Bodily merge into `ci.yml`, not a `workflow_call` orchestrator

GitHub Actions' `needs:` and job-level artifact/cache dependencies only work between
jobs declared in the same workflow YAML file. A `workflow_call`-based orchestrator can
express `needs:` only at the *call* level — waiting for an entire called workflow, not
one job inside it. An orchestrator calling `ci.yml` and (the former) `real-corpus-e2e.yml`
separately could not let the five e2e jobs depend on just `build-release` without also
waiting on `test`'s full clippy/bench/fmt tail, which would delay the e2e signal well
past its current landing time — reintroducing exactly the regression FR-008 warns
against. Merging the job *definitions* bodily into one file is the only way to get
job-level dependency granularity, so `real-corpus-e2e.yml` is deleted and its five jobs
now live in `ci.yml`.

### 4. The five e2e jobs no longer install a Rust toolchain (FR-006/Story 4)

Each e2e job now only checks out the repo, downloads its two binaries, and runs the test
binary directly (e.g. `./target/release/e2e-artifacts/real_corpus_e2e --ignored`,
equivalent to what `cargo test ... -- --ignored` did before — everything after `--` was
already passed straight through to the same binary). No `cargo`/`rustc` is present, so a
future change that accidentally adds a build step after the artifact download fails
immediately and loudly (there is nothing to invoke it with) rather than merely being
flagged by a heuristic.

`test` still needs `cargo` for clippy/fmt/bench, so it keeps a log-grep guard —
generalized from the original FR-008 check (`grep -q "Compiling lbug "`, written when
`test` did its own `cargo build --release`) to `grep -q "Compiling "` against *any* crate.
Since `test` no longer builds anything itself, any `Compiling` line in its `cargo test
--release` output now means the cache hand-off failed to avoid recompilation for some
crate, not specifically lbug.

`RUST_TEST_THREADS=4` (set via `.cargo/config.toml`'s `[env]`, applied by cargo when it
spawns a process) does not apply when a test binary is invoked directly. Each e2e job
sets it explicitly as a step-level environment variable instead.

### 5. `build-lbug` is absorbed into `build-release`, not run alongside it

`build-lbug`'s only purpose was to populate the lbug cache ahead of `test`. Once
`build-release` runs `cargo build --release` unconditionally, it populates the same
cache as a side effect — running a separate `build-lbug` job concurrently would
reintroduce a second writer to the same key, exactly the race this ADR exists to
eliminate.

### 6. One shared classifier output, not two

`ci.yml`'s `changes` job now always applies the WAL-fixture `extra-deny-pattern` that
used to be `real-corpus-e2e.yml`-only, so a single `code_changed` output gates `test`,
`build-release`, and all five e2e jobs. This is a minor accepted inefficiency (a
WAL-fixture-only PR change now also runs `test`, which it doesn't strictly need) traded
for not threading two independently-computed classifier outputs through six downstream
jobs' `if:` conditions.

### 7. `workflow_dispatch` gains an `e2e_only` input

`real-corpus-e2e.yml` previously supported a standalone `workflow_dispatch` with no
`ci.yml` run to pair with. That capability is preserved via a new boolean
`e2e_only` input on `ci.yml`'s `workflow_dispatch`: when true, `test`'s `if:` adds
`!(github.event_name == 'workflow_dispatch' && inputs.e2e_only)`, so a manual
"e2e only" run still executes `build-release` (the e2e jobs depend on it) but skips
`test`'s own release-verification/clippy/fmt/bench tail.

### 8. `ci-failure-notify.yml`'s listener moves from "Real-Corpus E2E" to "CI"

The five e2e jobs used to live in their own top-level workflow specifically so they had
a distinct `workflow_run` identity independent of `ci.yml`'s required `test` job — a
`workflow_run` listener can't see through the bodily merge decided above. With
"Real-Corpus E2E" gone, the `workflows:` list is updated to `"CI"`. This is a deliberate
broadening, not just a rename: this listener now also fires for a post-merge failure of
`test` itself (release build, clippy, fmt, R-003 bench), which it never did before. A
`test` failure on `main` going unnoticed is arguably worse than the gap this widens, but
it is a real behavior change worth naming explicitly rather than treating as incidental.

## Consequences

- Exactly one full `cargo build --release` (plus one `cargo test --release --no-run`)
  runs per CI run, down from six independent `cargo build --release` invocations
  (FR-001/SC-001).
- The `lbug-cache-*` key has exactly one writer per run (FR-003/SC-002); the specific
  duplicate-symbol failure class from #336 cannot recur for this structural reason.
- The e2e jobs' pass/fail signal continues to land in parallel with `test`, not after it
  — `build-release`'s own compile cost is the same work `test` used to do itself, just
  relocated to run once instead of six times, so it does not add sequential latency
  ahead of the e2e jobs starting.
- `real-corpus-e2e.yml` no longer exists as a file; its five jobs are `ci.yml` jobs now.
  Anyone looking for that workflow by name (Actions UI, `workflow_run` listeners, local
  muscle memory) needs to look at "CI" instead.
- `ci-failure-notify.yml` now also opens/updates a tracking issue for a post-merge
  `test`-job failure on `main`, which it did not do before (see Decision 8).
- The exact measured artifact-transfer size (FR-004), `build-release`'s wall-clock
  duration, and the PR's own before/after time-to-mergeable delta (FR-007/SC-003) are
  recorded in this implementing PR's description from its own CI run, following the
  same pattern ADR-0328 used for its own timing claims.

## Alternatives Considered

- **(A) Make `test` the sole producer; `needs: test` on the five e2e jobs.** Literally
  satisfies "compiled at most once" with the smallest diff, but delays the e2e signal
  until after `test`'s full ~17-20 minute run (build + execution + eval guards + R-003
  bench + clippy + fmt) completes — landing the e2e signal *after* the PR is already
  mergeable instead of ~9 minutes *before*, as it does today. Rejected: violates FR-008's
  explicit instruction not to delay the e2e signal past its current landing time.
- **(B) A dedicated producer job feeding both `test` and the e2e jobs via one mechanism.**
  Runs into the same tension resolved in Decision 2: `test` needs the full tree, the e2e
  jobs need six binaries, and no single transfer mechanism serves both without violating
  either FR-001 or FR-004. Superseded by the two-mechanism split actually implemented.
- **(C) A restore-only `build-e2e-binaries` job compiling only what the e2e tests need,
  running in parallel with `test`'s own unchanged independent full build.** Preserves
  timing and eliminates the write-write race (same restore-only reasoning ADR-0328
  Decision 4 already validated), and would have been a legitimate reading of "compiled
  at most once" if two full-ish compiles were acceptable — but two compiles is not one,
  so it does not achieve FR-001/SC-001 the way the adopted design does. Rejected in
  favor of true single-build once the two-mechanism split (Decision 2) showed FR-001 and
  FR-004 could both be satisfied without it.
- **FR-009's fallback (distinct cache keys per writer, keep all six builds)**: not taken.
  Artifact/cache reuse across the merged workflow turned out feasible without the cost
  FR-009 anticipates paying for — see Decision 1-2. SC-005 (the fallback's success
  criterion) does not apply.

## References

- Issue #341
- #336 — the observed duplicate-symbol CI failure motivating this issue
- #328 / ADR-0328 — added the `pull_request` trigger and the five parallel e2e jobs this
  issue restructures; its FR-005 open question ("verify whether this aggravates the
  cache race") is answered by #336
- #322 / ADR-0322 — the docs-only fast path and shared `classify-changes` composite
  action, reused unchanged by the merged workflow
- ADR-0316 — confirms the R-003 bench step, not the release build, was the historical
  dominant CI cost; this change is not expected to move that number
- #298 / ADR-0298 — the post-merge failure notifier updated in Decision 8
- `.github/workflows/ci.yml` — the merged workflow implementing this decision
