# liminis-context-graph — Claude guidance

## Specs use Spec Kit (NON-NEGOTIABLE)

All feature specs in this project use Spec Kit format: `## User Scenarios & Testing` with prioritized stories, `## Requirements` with FR-NNN identifiers, `## Success Criteria` with SC-NNN identifiers, `## Edge Cases`, and `## Assumptions`.

**For Fabrik-driven features**: File a GitHub issue (with a free-form description or a Spec Kit–formatted body), label it `fabrik:yolo`, and put it in the Specify column of the project board. Fabrik's Specify stage automatically produces the canonical spec in Spec Kit format and commits it to `specs/<issue_number>-<slug>/spec.md` on the issue's feature branch. The spec ships with the implementation in the same PR and lands on `main` when the PR merges. No manual pre-commit step is needed — do not run the `/speckit-specify` slash command and do not create the spec directory by hand.

- The directory prefix is the **GitHub issue number** (e.g., `specs/29-tier-2-wal-admin/`), not a sequential NNN counter. This guarantees uniqueness across parallel issues.
- Any pre-existing `specs/<NNN>-*` directories (`001-rust-knowledge-graph`, etc.) predate this convention and stay as-is.
- For an **in-flight issue** (PR open, not yet merged), the spec is visible on the PR's feature branch via GitHub's web UI file browser. It appears on `main` only after the PR merges.

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

## Pre-spec ideas (`ideas/`)

The `ideas/` directory (created on demand) holds pre-spec sketches and design notes that have not crossed the Spec Kit threshold. **Do not implement directly from files there** — they are exploratory by definition and may be wrong, incomplete, or contradicted by later thinking. When an idea matures, file a Fabrik issue (with a Spec Kit–formatted body for features, or a focused bug body for fixes) and the resulting spec lives in `specs/<issue-number>-<short-name>/spec.md` per the convention above.

## Rust pre-commit checks (MUST run before every commit)

CI runs three commands (see `.github/workflows/ci.yml`); any failure blocks merge. Run the **local gate below** before pushing to save a fabrik retry cycle — it is the debug-profile equivalent that fits the time budget. Full release verification is CI's job, not yours.

> **Local verification has a 10-minute budget. Read this before running anything.**
>
> An agent's foreground shell call is capped at **10 minutes**. CI's `test` job — a release build plus the suite — measures **15–18 minutes** even on a warm cache. So `cargo test --release` **cannot be run in the foreground**, and reaching for a background run instead is how this repo's agent sessions stall: the worker backgrounds the command, ends its turn to wait for a completion notification, and in a headless run that notification never arrives. The stage closes incomplete, three times, and the issue auto-pauses. This has happened on #208, #190, #219, #212, and #236.
>
> **So: do not run `cargo test --release` locally, and never end a turn waiting on a backgrounded command.** Use the local gate below, which fits inside the budget, and let CI own full release verification. That is what CI is for — a local `--release` run duplicates work CI already does, and blocks the agent for longer than a foreground call is allowed to last.

1. `cargo fmt --all` — auto-format. Never commit without running this. Rust treats whitespace as binary pass/fail; even a single misaligned brace fails `cargo fmt --check` in CI.
2. `cargo test` — compiles lib + tests and runs them, in **debug**. CI runs `cargo test --release` because the 6 integration tests require release-mode linking; that is CI's job, not yours (see the budget note above). If your change touches release-only behavior, say so in the PR body so a reviewer knows CI is the first place it gets exercised — do not run the release suite locally to pre-empt it. Common trap: lib builds while tests fail to compile, because tests are a separate compilation unit — adding a field to a struct used in tests silently breaks the test build until every constructor is updated.
3. `cargo clippy --release -- -D warnings` — CI runs this exact form (release profile, to reuse cached lbug artifacts). Locally, `cargo clippy --all-targets -- -D warnings` is **stricter** on targets (covers tests, benches, and examples in one pass) but uses the debug profile, so it will miss release-only warnings (e.g., `dead_code` on `#[cfg(not(debug_assertions))]` paths). The `--release --all-targets` combination mirrors CI most closely but pays a full release build, so leave it to CI unless you are specifically chasing a release-only lint. CI's `-D warnings` means any warning blocks merge. Common traps:
   - `dead_code` on test-only helpers → add `#[allow(dead_code)]`
   - `items_after_test_module` → put any non-test helpers BEFORE `#[cfg(test)] mod tests { }`, never after
   - New clippy lints introduced by a toolchain bump

**lbug is not compiled from C++ — it is a downloaded prebuilt bundle.** Since 0.17.0 the crate ships a self-contained "fat bundle": `build.rs`'s `try_download_prebuilt_lbug()` fetches a prebuilt `liblbug.a` from the `LadybugDB/ladybug` releases and caches it under the crate's registry source directory (`.cache/lbug-prebuilt/<key>`). Because that lives in the shared cargo registry rather than in `target/`, it is downloaded **once per machine** and reused by every worktree. **Do not set `LBUG_BUILD_FROM_SOURCE` — the source-build path is broken.** Since the fat bundle already contains every third-party archive, a cmake source build links both the bundle and the individual archives and fails with 7399 duplicate-symbol errors (`link_bundled_deps=true`; upstream `LadybugDB/ladybug-rust#18`). The flag was removed from `ci.yml`, `release.yml`, and `bench.yml` for exactly this reason. The only supported override is an external library via `LBUG_LIBRARY_DIR` + `LBUG_INCLUDE_DIR`.

**`.cargo/config.toml` pins `LBUG_VERSION = "0.17.0"`, and that pin is load-bearing.** Without it `build.rs` downloads the *floating* `latest` ladybug release, building the 0.17.0 crate's FFI against a newer native bundle. That skew broke the v0.9.0 release: 0.18.1 links httplib against OpenSSL rather than the bundled mbedtls, and the macOS build failed with `ld: symbol(s) not found`. The same file sets `RUST_TEST_THREADS = "4"` because lbug mmaps an 8 TB virtual region per `Db::open()` and default parallelism exhausts the macOS VM ceiling — which is also part of why the suite is slow enough to matter for the budget above.

So the 15–18 minutes CI spends is **Rust compilation and the test suite**, not C++ — do not expect build-cache work to move that number.

**CI's lbug cache** stores only the Rust-side build-script outputs (`target/release/build/lbug-*`, `target/release/.fingerprint/lbug-*`, `target/release/deps/liblbug-*`), so the build script isn't re-run on every job. Note it does **not** cover the cargo registry source directory where `.cache/lbug-prebuilt/<key>` lives, so a cold runner still downloads the bundle once; the cache saves the build-script re-run, not the fetch. The key includes `runner.os`, the resolved `rustc` version, and a hash of the `lbug` stanza in `Cargo.lock`, so unrelated dep bumps don't invalidate it. To bust a corrupted cache, bump `LBUG_CACHE_BUST` in `.github/workflows/ci.yml`'s top-level `env:` block — that invalidates every lbug cache entry across branches.

**Worktrees do not share a `target/` directory.** Each carries its own (the main checkout is ~19 GB), so every new worktree recompiles the full Rust dependency graph even though the lbug bundle itself is shared. Setting a shared `CARGO_TARGET_DIR` would remove that duplication, but cargo takes an exclusive lock on the target directory — with several Fabrik workers running concurrently they would serialize on it. Leave it unset unless you are working a single worktree at a time and want the rebuild savings.

If any step fails, fix and re-run from step 1 (fmt may have shifted line numbers).

## Running performance benchmarks

Performance benchmarks are **not** run on every PR — they run on explicit invocation only. Use:

```bash
gh workflow run bench.yml
```

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
