# liminis-context-graph — Claude guidance

## Specs use Spec Kit (NON-NEGOTIABLE)

All feature specs in this project use Spec Kit format: `## User Scenarios & Testing` with prioritized stories, `## Requirements` with FR-NNN identifiers, `## Success Criteria` with SC-NNN identifiers, `## Edge Cases`, and `## Assumptions`.

**For Fabrik-driven features**: File a GitHub issue (with a free-form description or a Spec Kit–formatted body), label it `fabrik:yolo`, and put it in the Specify column of the project board. Fabrik's Specify stage automatically produces the canonical spec in Spec Kit format and commits it to `specs/<issue_number>-<slug>/spec.md` on the issue's feature branch. The spec ships with the implementation in the same PR and lands on `main` when the PR merges. No manual pre-commit step is needed — do not run the `/speckit-specify` slash command and do not create the spec directory by hand.

- The directory prefix is the **GitHub issue number** (e.g., `specs/29-tier-2-wal-admin/`), not a sequential NNN counter. This guarantees uniqueness across parallel issues.
- Any pre-existing `specs/<NNN>-*` directories (`001-rust-knowledge-graph`, etc.) predate this convention and stay as-is.
- For an **in-flight issue** (PR open, not yet merged), the spec is visible on the PR's feature branch via GitHub's web UI file browser. It appears on `main` only after the PR merges.

## ADR numbers are issue numbers (NON-NEGOTIABLE)

A new ADR is named `docs/adr/<issue_number>-<slug>.md` — the **GitHub issue number** that motivated the decision, zero-padded to four digits. [`docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md`](docs/adr/0283-name-index-scan-fallback-for-endpoint-authority.md) (from issue #283) is the first one written under this rule; it is not `0053`, the next free number in the sequence.

This is the same rule, for the same reason, as `specs/<issue_number>-<slug>/`: a shared sequential counter is claimed at *branch* time, so any two issues in flight claim the same number, and the collision is invisible until merge.

**Why it is worth being strict about.** The collision does not surface as a merge conflict in the ADR files — they have different slugs, so both apply cleanly. It surfaces only as a conflict on the single row each adds to `docs/adr/index.md`, which presents as a generic unmergeable branch. A Fabrik stage that hits it will fail, retry, fail again, exhaust its three attempts and pause, because rebasing and renumbering is outside any stage's scope. #279 and #281 both claimed `0051` and cost exactly that.

- **Existing ADRs keep their numbers.** `0001`–`0052` are sequential and immutable; do not renumber them. The gap between the last sequential ADR and the first issue-numbered one is expected, not missing history.
- **No motivating issue** (a decision recorded during direct collaboration): use the PR number instead, and say so in the ADR's own header.
- **Two ADRs from one issue**: both carry that issue's number and are distinguished by slug. The number identifies the issue, the slug identifies the decision, so cite them as `ADR-0283 (name-index-scan-fallback)` when the number alone is ambiguous.
- **Citing an ADR** below `0042` in old material: check `docs/adr/index.md`'s *Historical numbering* appendix first. Those are pre-consolidation numbers from when ADRs lived in two directories, and the left column decodes them. Never renumber a historical citation to match a current ADR — they refer to different decisions.

**For small bug fixes**: The full Spec Kit workflow is overkill — a focused issue with reproduction steps, expected/actual behavior, and acceptance criteria is fine. The Spec Kit threshold is roughly: if the work is large enough to be a "feature" or to have user-facing acceptance scenarios, file it as a Fabrik issue and let the Specify stage handle the spec. If it's a one-line fix or a clearly-scoped regression, just file the issue directly.

## Where work happens (NON-NEGOTIABLE)

Two rules govern how changes land in this repo:

1. **Major work is driven by Fabrik.** Anything that meets the Spec Kit threshold above is filed as a Spec Kit issue and worked by Fabrik through its stages (Specify → Research → Plan → Implement → Review → Validate). Do not implement major features by hand in a side conversation; let the agent do it from the spec so the artifacts (spec, plan, tasks, PR) stay aligned.
2. **Smaller work is done in a git worktree and pushed as a PR — never edited directly in `main`.** The `main` checkout must always remain valid: clean working tree, all tests passing, ready for Fabrik to fork worktrees from. Even a one-line doc tweak or a focused regression fix goes through a feature branch in a separate worktree.

**Why:** Fabrik runs against `main` and forks per-issue worktrees from it. Uncommitted edits in `main` corrupt that workflow — Fabrik's worktrees won't see in-flight changes, conflict detection breaks, and the meaning of "what's in main" stops being well-defined. The same worktree-and-PR discipline Fabrik applies to itself must apply to direct collaboration too.

**How to work in a worktree:**

```bash
# from the main checkout (the repo root)
git worktree add ../liminis-context-graph-worktrees/<short-slug> -b feat/<short-slug> main
cd ../liminis-context-graph-worktrees/<short-slug>
# ... edit, test, commit ...
cargo fmt --all
cargo test
cargo clippy --all-targets -- -D warnings
git push -u origin feat/<short-slug>
gh pr create --fill
# after merge, return to main checkout to clean up:
cd -
git worktree remove ../liminis-context-graph-worktrees/<short-slug>
git branch -D feat/<short-slug>
```

Worktrees live as siblings to the main checkout under `../liminis-context-graph-worktrees/<short-slug>/`, not inside the repo. Always run `cargo fmt --all && cargo test && cargo clippy --all-targets -- -D warnings` from inside the worktree before pushing — see the Rust pre-commit checks section below for the detailed gate behavior.

The Spec Kit threshold and the worktree threshold are the same: features go through Spec Kit + Fabrik; everything else still gets a worktree + PR.

## Long-running commands (NON-NEGOTIABLE)

This applies in every stage — Implement, Review, Validate, and direct collaboration — not only around pre-commit checks. It applies any time you need a signal from something that won't finish inside a single foreground call, whether that's a local test suite, a GitHub Actions run, or a benchmark workflow.

An agent's foreground shell call is capped at **10 minutes**. When a check you'd otherwise run will exceed that, work through these options **in order**:

0. **If you already ran it, don't run it again — read the output you kept.** Capture expensive
   commands to a file once (`cmd > /tmp/out.log 2>&1`) and grep that file for whatever you need
   next. Re-running an 8-minute suite because you want `grep` instead of `tail` costs another 8
   minutes and produces identical bytes. This is the single most wasteful mistake available in this
   repo, and it has happened: one stage spent 26 minutes on three `cargo test` runs that differed
   only in how they sliced the output.
1. **Don't run it.** If something else already runs the exact check, let it. CI runs `cargo test --release` and `cargo clippy --release -- -D warnings` on every PR (see the Rust pre-commit checks section below) — a local rerun duplicates that work and gains nothing.
2. **Run the in-budget subset instead.** A debug-mode build/test pass, or a narrower command scoped to what changed, often gives a fast local signal without hitting the cap. See the pre-commit gate below for the Rust-specific version of this.
3. **If a long-running result is genuinely required and nothing else provides it (options 1–2 don't apply), background it and poll within the same turn.** Start the command in the background, then check on it with a small number of short, bounded foreground checks with brief pauses between them — staying well under the 10-minute cap for each check — and report the outcome (done, failed, or that it did not finish within the polling budget) before ending the turn. For example: launch a slow local reproduction script or one-off migration in the background redirected to a log file (`long_task.sh > /tmp/task.log 2>&1 & echo $!`), then poll by PID and by tailing that log file (`ps -p <pid>`, `tail /tmp/task.log`) a few minutes apart to see whether it has finished — a raw shell job-control check like `jobs` won't work here, since shell state doesn't persist between separate foreground calls, only the PID and the file on disk do. Report the result either way once you stop polling. (Don't reach for this with `cargo test --release` — that's exactly the case option 1 already covers.)

**Backgrounding a command is fine. Ending your turn while it's running, hoping to be woken by a completion notification, is the fatal step** — in a headless run that notification never arrives, the stage closes incomplete, and after enough retries the issue auto-pauses. This has happened repeatedly: #208, #190, #219, #212, #236, #283, #297.

**Never wait on CI or a benchmark run — this holds in every stage, whether or not that stage is CI-gated.** A stage completes its work and emits `FABRIK_STAGE_COMPLETE` (or the equivalent for the stage you're in) without checking whether CI has finished. Today only Validate is configured with `wait_for_ci: true` (see `.fabrik/stages/validate.yaml`): for Validate specifically, the engine re-polls CI after stage completion, applies the `fabrik:awaiting-ci` label while it's pending, and re-invokes the stage only if CI fails. Implement and Review are not CI-gated by default — they complete and the pipeline advances without the engine waiting on CI at all, so there is even less reason for those stages to poll it themselves. Do not poll `gh run watch`, `gh pr checks`, or schedule a wakeup to check on a GitHub Actions or `bench.yml` run's status in any stage — where CI gating exists, it's the engine's job, not the stage's, and where it doesn't exist, there's nothing to wait for.

## Pre-spec ideas (`ideas/`)

The `ideas/` directory (created on demand) holds pre-spec sketches and design notes that have not crossed the Spec Kit threshold. **Do not implement directly from files there** — they are exploratory by definition and may be wrong, incomplete, or contradicted by later thinking. When an idea matures, file a Fabrik issue (with a Spec Kit–formatted body for features, or a focused bug body for fixes) and the resulting spec lives in `specs/<issue-number>-<short-name>/spec.md` per the convention above.

## Rust pre-commit checks (MUST run before every commit)

CI runs three commands (see `.github/workflows/ci.yml`); any failure blocks merge. Run the **local gate below** before pushing to save a fabrik retry cycle — it is the debug-profile equivalent that fits the time budget on a warm cache (see below for the cold-worktree case). Full release verification is CI's job, not yours.

> **Local verification has a 10-minute budget.** See "Long-running commands" above for the general rule and the casualty list. The specific case here: CI's `test` job — a release build plus the suite plus the R-003 bench correctness gate — used to measure **15–18 minutes** even on a warm cache, so the complete release verification path cannot be run in the foreground; CI already runs it on every PR. Use the debug-mode local gate below instead, which fits inside the budget; let CI own full release verification.
>
> **The local gate's measured cost is ~8.6 minutes on a worktree with a warm debug-profile
> cache**: `cargo fmt --all` (< 1s) + `cargo test` (~8.5 min, the dominant cost) + `cargo clippy
> --all-targets -- -D warnings` (< 10s once `test` has already built the debug artifacts) — see
> #316. Run each of the three as its own foreground call (as the numbered list below does), not
> chained into one command — that keeps every individual call comfortably under the 10-minute
> budget even though their sum approaches it, and `cargo test` is the one to watch since it alone
> can approach a full 10-minute call on a warm cache.
>
> **A genuinely cold worktree (no prior debug build at all) is unmeasured** and pays additional
> first-time dependency-compile cost on top of the warm-cache figure above — #316 did not capture
> this number, since it found the debug dependency graph is not the bottleneck driving the failure
> class this budget note exists to prevent (see below). If `cargo test` alone exceeds a single
> foreground call on a cold worktree, fall back to the "Long-running commands" section's narrower-scope
> option: split the run per crate (`cargo test -p lcg-core`, then `-p lcg-service`, then `-p
> lcg-eval`) rather than chaining a longer combined command.

> **Run the suite once, capture it to a file, then query the file.** `cargo test` is ~8.5 minutes;
> re-running it to look at its output a different way costs another ~8.5 minutes and tells you
> nothing new. Redirect once and grep the artifact as many times as you like:
>
> ```sh
> cargo test > /tmp/test.log 2>&1; tail -5 /tmp/test.log
> grep -E "^(test result|failures:)" /tmp/test.log      # summary
> grep -A 20 "^failures:" /tmp/test.log                 # what failed, and why
> ```
>
> This is not a micro-optimisation. A single Fabrik stage was observed spending **26 minutes on
> three back-to-back `cargo test` runs whose only difference was `tail -10`, then `tail -5`, then
> `grep -E`** — roughly 17 minutes re-deriving output it already had. Re-run the suite only after
> the code changes; never to reformat a result.
>
> The same rule applies to any expensive command in this repo: `cargo clippy`, a release build, a
> corpus e2e binary. Capture once, inspect the capture.

1. `cargo fmt --all` — auto-format. Never commit without running this. Rust treats whitespace as binary pass/fail; even a single misaligned brace fails `cargo fmt --check` in CI.
2. `cargo test` — compiles lib + tests and runs them, in **debug**. CI runs `cargo test --release` because the ~50 integration-test binaries (crates/core: 37, crates/service: 11, crates/eval: 2 — not "six", a stale figure corrected by #316) require release-mode linking; that is CI's job, not yours (see the budget note above). Measured: compile+link for all ~50 binaries is ~4 minutes, execution ~1 minute — this was never actually the dominant CI cost (see below). If your change touches release-only behavior, say so in the PR body so a reviewer knows CI is the first place it gets exercised — do not run the release suite locally to pre-empt it. Common trap: lib builds while tests fail to compile, because tests are a separate compilation unit — adding a field to a struct used in tests silently breaks the test build until every constructor is updated.
3. `cargo clippy --all-targets -- -D warnings` — run this debug-profile local gate. It's **stricter** on targets than CI's own check (covers tests, benches, and examples in one pass), but uses the debug profile, so it will miss release-only warnings (e.g., `dead_code` on `#[cfg(not(debug_assertions))]` paths). CI separately runs `cargo clippy --release -- -D warnings` (release profile, to reuse cached lbug artifacts) — that's CI's job, not yours. The `--release --all-targets` combination mirrors CI most closely but pays a full release build, so leave it to CI unless you are specifically chasing a release-only lint. CI's `-D warnings` means any warning blocks merge. Common traps:
   - `dead_code` on test-only helpers → add `#[allow(dead_code)]`
   - `items_after_test_module` → put any non-test helpers BEFORE `#[cfg(test)] mod tests { }`, never after
   - New clippy lints introduced by a toolchain bump

**lbug is not compiled from C++ — it is a downloaded prebuilt bundle.** Since 0.17.0 the crate ships a "fat bundle": `build.rs`'s `try_download_prebuilt_lbug()` fetches a prebuilt `liblbug.a` from the `LadybugDB/ladybug` releases and caches it under the crate's registry source directory (`.cache/lbug-prebuilt/<key>`). Because that lives in the shared cargo registry rather than in `target/`, it is downloaded **once per machine** and reused by every worktree. **Do not set `LBUG_BUILD_FROM_SOURCE` — the source-build path is broken.** Since the fat bundle already contains every third-party archive, a cmake source build links both the bundle and the individual archives and fails with 7399 duplicate-symbol errors (`link_bundled_deps=true`; upstream `LadybugDB/ladybug-rust#18`). The flag was removed from `ci.yml`, `release.yml`, and `bench.yml` for exactly this reason. The only supported override is an external library via `LBUG_LIBRARY_DIR` + `LBUG_INCLUDE_DIR`.

**The bundle is no longer *fully* self-contained: OpenSSL links externally.** lbug 0.18.0 switched the TLS backend from bundled mbedtls to OpenSSL 3 (`LadybugDB/ladybug#590`). Everything else — antlr4, fastpfor, parquet, zstd, yyjson, and mbedtls itself — is still statically bundled, but `build.rs` now emits `cargo:rustc-link-lib=dylib=ssl` and `=crypto` **unconditionally** and takes its search path from `pkg-config --variable=libdir openssl` and nothing else. `OPENSSL_DIR` and `OPENSSL_ROOT_DIR` are *not* read by the published crate (they belong to `openssl-sys` and to `ladybug-rust`'s `main`, which publishes no tags and is not a safe proxy for a released version) — `PKG_CONFIG_PATH` is the only lever. A `pkg-config` miss does not fail the build script; it just omits the `-L`, deferring the failure to the linker.

Left alone this makes the release binary depend on a dynamic `libssl`/`libcrypto`, and on macOS it bakes in Homebrew's absolute install name (`/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`), so the artifact will not load for anyone without that exact path. `scripts/stage-openssl-static.sh` prevents this by pointing `PKG_CONFIG_PATH` at a directory holding only `libssl.a`/`libcrypto.a`; `scripts/assert-static-openssl.sh` is the CI guard. Both run in `ci.yml`'s `build-release`, and the staging step is *separately* present in `release.yml` because cargo-dist inlines `.github/build-setup.yml` at generate time rather than referencing it at run time. See **ADR-0398**. Local `cargo build` without the script links dynamically, which is fine for development.

**`.cargo/config.toml` pins `LBUG_VERSION = "0.19.1"`, and that pin is load-bearing.** It must always match the `lbug` crate pin in the workspace `Cargo.toml` — the two move together or not at all. Without it `build.rs` downloads the *floating* `latest` ladybug release, building the pinned crate's FFI against a mismatched native bundle. That skew broke the v0.9.0 release: the then-latest 0.18.1 bundle linked httplib against OpenSSL while the pinned 0.17.0 crate expected the bundled mbedtls, and the macOS build failed with `ld: symbol(s) not found`. The same file sets `RUST_TEST_THREADS = "4"` because lbug mmaps an 8 TB virtual region per `Db::open()` and default parallelism exhausts the macOS VM ceiling.

**The 15–18 minutes CI used to spend was not, in fact, dominated by `cargo test --release`.**
Issue `#316` measured it: `cargo build --release` (~2–2.5m) + `cargo test --release` (~5m, of which only
~1m is test *execution* — the rest is compile+link) + the R-003 `dedup_overlap_check` bench step
(**~10.5–11m — 58–60% of the job**) + clippy/fmt (negligible). The bench step was the dominant
cost, caused by a Criterion footgun: `crates/core/benches/search.rs` bundled four
`criterion_group!`s behind one `criterion_main!`, and Criterion's `--` filter only gates the
measurement loop inside `bench_function`, not each group's setup code — so a single filtered
invocation still built+indexed ~143,200 rows across every group before reaching the ~1,000-row
check it was actually asked for. Issue `#316` split this into five separate `[[bench]]` targets (one per
former group), making isolation a target-layout property instead of a filter string — see
**ADR-0316**. The R-003 gate now runs in ~1–2 minutes, and it stays PR-blocking (nothing moved
post-merge). Do not expect further build-cache work on the test-suite side to move the CI job's
total time; that was never where the time was going.

**CI's lbug cache** stores only the Rust-side build-script outputs (`target/release/build/lbug-*`, `target/release/.fingerprint/lbug-*`, `target/release/deps/liblbug-*`), so the build script isn't re-run on every job. Note it does **not** cover the cargo registry source directory where `.cache/lbug-prebuilt/<key>` lives, so a cold runner still downloads the bundle once; the cache saves the build-script re-run, not the fetch. The key includes `runner.os`, the resolved `rustc` version, and a hash of the `lbug` stanza in `Cargo.lock`, so unrelated dep bumps don't invalidate it. To bust a corrupted cache, bump `LBUG_CACHE_BUST` in `.github/workflows/ci.yml`'s top-level `env:` block — that invalidates every lbug cache entry across branches.

**Worktrees do not share a `target/` directory.** Each carries its own (the main checkout is ~19 GB), so every new worktree recompiles the full Rust dependency graph even though the lbug bundle itself is shared. Setting a shared `CARGO_TARGET_DIR` would remove that duplication, but cargo takes an exclusive lock on the target directory — with several Fabrik workers running concurrently they would serialize on it. Leave it unset unless you are working a single worktree at a time and want the rebuild savings.

If any step fails, fix and re-run from step 1 (fmt may have shifted line numbers).

**Docs-only PRs skip the Rust job.** `ci.yml`'s `changes` job classifies the PR diff; if it touches no `.rs`, `Cargo.toml`/`Cargo.lock`, `.cargo/**`, `build.rs`, `.github/workflows/**`, or `crates/eval/scripts/**` file, `build-lbug` and `test` both show conclusion "Skipped" instead of running — that's expected, not a misconfiguration. It's a job-level `if:` skip, not a `paths-ignore` on the workflow trigger, so the required `test (ubuntu-latest)` status check still reports (as passing) rather than never posting. Any PR touching a code-relevant path, or any classification error, runs the full suite unchanged. See [ADR-0322](docs/adr/0322-ci-docs-only-fast-path.md).

## Running performance benchmarks

Performance benchmarks are **not** run on every PR — they run on explicit invocation only. Use:

```bash
gh workflow run bench.yml
```

Triggering the run is all a stage does — do not wait for it to finish; see "Long-running commands" above for why stages never wait on CI or benchmark runs.

Results appear in the Actions tab under the "Perf Benchmarks" workflow. Each run uploads two artifacts (30-day retention):
- **`bench-results-<sha>`** — plain-text criterion output for `1k`, `10k`, and `50k` dedup runs; download the zip from the Actions UI to inspect.
- **`criterion-html-<sha>`** — criterion HTML reports with interactive plots (box plots, violin plots); download locally for detailed comparison.

The `dedup_overlap_check` correctness gate (R-003) still runs automatically on every PR as part of the `test` job — only the *measurement* steps moved to the on-demand workflow. To enable nightly automatic bench runs, uncomment the `schedule:` block in `.github/workflows/bench.yml`.

## When adding or modifying a struct field

Grep ALL constructor call sites, including test files:

```
grep -rn "StructName {" --include="*.rs" .
```

Tests live in `crates/core/tests/*.rs` AND inline `#[cfg(test)] mod tests { }` blocks within source files. Both compile separately from the library and will silently break if you only update the lib sites. This has burned us repeatedly (e.g. #46, #58 CI fix cycles).

## When adding a new `knowledge_*` dispatch method

Adding a new match arm to `handle()` in `crates/core/src/handlers.rs` also requires a new `ToolSpec` entry in `crates/service/src/mcp/tools.rs` (name, description, JSON input schema, scope bucket) — the MCP-over-stdio tool surface (`--mcp-stdio`, see [ADR-0035](docs/adr/0035-mcp-stdio-transport.md)) is a hand-maintained registry, not derived by reflection over the dispatch table. Pick the scope bucket per the table in the README's "MCP-over-stdio transport" section: `read` for queries, `write` for content mutations, `admin` for WAL/lifecycle/index-maintenance operations, or the dedicated `cypher` scope only for a genuine arbitrary-query/mutation escape hatch. `crates/service/src/mcp/tools.rs`'s own tests assert the registry's total count and per-scope bucket sizes — update those counts alongside the new entry.

## Toolchain

- Install via `rustup`. Ensure `cargo` and `rustc` are on `PATH` — typically `~/.cargo/bin`, or `/opt/homebrew/opt/rustup/bin` on Apple Silicon with Homebrew-managed rustup.
- CI provisions its toolchain via `dtolnay/rust-toolchain@stable` on Ubuntu.
- Clippy lints can change between toolchain versions. If CI introduces a new lint that wasn't there yesterday, check the toolchain delta before assuming the code is wrong.

## Build artifact

The `liminis-context-graph` binary (built from the `lcg-service` crate at `crates/service`) is consumed by the liminis Electron app via `graphiti_service.py` over a Unix socket. Breaking the IPC protocol (defined in `crates/core/src/handlers.rs` + the Python-side `service_protocol.py`) breaks the app. When adding or changing a method, keep both sides aligned and update the Tier 1a/1b/1c parity tests in `crates/core/tests/ipc_parity.rs`.

## Schema parity with graphiti

`crates/core/src/schema.rs` must track parity with graphiti's Kuzu driver, `graphiti_core/driver/kuzu_driver.py` — that file is the canonical source of truth for node/rel tables and their column sets (lbug *is* Kuzu, renamed; see `docs.ladybugdb.com`). A missing or mistyped column makes the WAL's `MERGE`/`SET` fail to *prepare*, and under batched replay one `prepare()` failure is attributed to **every** row sharing that template — so a single schema gap can silently drop an entire category of mutations. When touching schema, diff against `kuzu_driver.py` and add the missing columns/stub tables rather than guessing. (History: #128/#130/#133/#136/#144 were all FalkorDB-dialect or schema-parity gaps; note also `VECF32(...)` is FalkorDB-only — Kuzu/lbug embeddings are bare `FLOAT[]` list literals.)

## Debugging a live or degraded service

The running service speaks **newline-delimited JSON-RPC 2.0** over its Unix socket (`<workspace>/.lcg/service.sock`). You can query a live graph directly — useful for inspection, analysis, or driving a recovery by hand:

```python
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(".lcg/service.sock")
s.sendall((json.dumps({"jsonrpc":"2.0","id":1,"method":"knowledge_status","params":{}})+"\n").encode())
print(json.loads(s.makefile("r", encoding="utf-8").readline())["result"])  # entity_count, episode_count, relationship_count, ontology, wal, ...
```

Useful read methods: `knowledge_status`, `knowledge_get_episodes {last_n}`, `knowledge_find_entities {query,num_results}` (FTS+vector; note `num_results`, **not** `limit` — an unknown key is silently ignored and defaults to 10), `knowledge_find_relationships`, `knowledge_get_nodes_by_group` / `knowledge_get_edges_by_group`. Adding `"_progress_token":"..."` to a long op (e.g. `knowledge_rebuild_from_wal`) makes it stream `{"type":"progress",...}` lines before the terminal result.

**WAL-corruption recovery** (corrupt `db.wal` → degraded mode): the service binds its socket before opening the DB, so `knowledge_recover` is reachable even when degraded. The fast path is `drop_lbug_wal` (reopen at last checkpoint) → resume only the WAL tail (using the last episode as the resume cursor) → rebuild indexes — see **ADR-0009** (degraded-mode startup & recovery), **ADR-0025** (auto-heal index build), and **ADR-0026** (episode-cursor WAL resume) for the full model and the validated playbook.
