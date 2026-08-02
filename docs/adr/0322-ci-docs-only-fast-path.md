# ADR-0322: CI Docs-Only Fast Path via Job-Level Skip

**Status**: Accepted
**Date**: 2026-08-02
**Issues**: #322

## Context

`test (ubuntu-latest)` is the sole required status check on `main`
(`required_status_checks.contexts == ["test (ubuntu-latest)"]`,
`enforce_admins.enabled == true`), and `.github/workflows/ci.yml` triggers it on every
`pull_request` regardless of what changed. A PR touching zero `.rs` and zero `Cargo.*`
files still pays a full release build and test suite — 15–18 minutes — to prove nothing
about itself.

Concrete instance: PR #320 (the documentation site, issue #295) changed 23 files, none
of them Rust. Its two meaningful checks — "Build site and check internal links" (18s)
and "Verify llms-full.txt is up to date" (7s) — were green in under half a minute. It
still could not merge, because `test (ubuntu-latest)` was mid-run.
`gh pr merge --admin` does not help: `enforce_admins=true`, so branch protection applies
to maintainers too.

This is a different lever from #316. That issue asks how to make the suite itself
faster; this one asks not to run it at all when it cannot be affected by the diff.

## Decision

### 1. Skip the job via `if:`, never filter the workflow via `paths`/`paths-ignore`

A new `changes` job runs on every `pull_request` and `push` trigger — the `on:` block
is untouched. It computes `code_changed` and the two expensive jobs (`build-lbug`,
`test`) are gated with `needs: changes` plus `if: needs.changes.outputs.code_changed
== 'true'`.

This is the load-bearing choice in the whole design. If `ci.yml` gained a
`paths-ignore` on its `pull_request` trigger instead, the workflow itself would never
run on a docs-only PR, and the required `test (ubuntu-latest)` check would never post
at all — branch protection would wait for a check that will never exist, converting a
15-minute delay into a permanently unmergeable PR. GitHub Actions marks a job skipped
via `if:` with a "skipped" conclusion, and a required check with conclusion "skipped"
satisfies branch protection; a workflow filtered out by `paths`/`paths-ignore` produces
no check run at all. The two mechanisms look similar and are not — this ADR exists
because a future contributor could "simplify" this back to `paths-ignore` without
realizing the difference is exactly the failure mode this issue was filed to fix.

### 2. Classification lives in one inline bash step, not a third-party action

No existing third-party path-filtering dependency (e.g. `dorny/paths-filter`) is used
in this repo. An inline step keeps the classification logic in one file (no drift
between a job's YAML config and its actual behavior) and gives direct control over the
two properties that matter most here: fail-safe defaulting and rename handling.

### 3. Any classification error defaults to `code_changed=true`, never to a skip

Every early-return in the `changes` job's script — a non-`pull_request` event, a failed
`git fetch`, a failed `git diff` — sets `code_changed=true` before exiting. The only
path to `code_changed=false` is the single success branch at the bottom, where the diff
was actually computed and checked against the classification pattern. A `git
fetch`/`git diff` failure (rare infra hiccup, e.g. GitHub's `uploadpack
.allowReachableSHA1InWant` support behaving unexpectedly for an arbitrary commit SHA)
must never silently satisfy the required check by skipping; the conservative failure
mode is always "run the suite unnecessarily," never "skip it unnoticed."

### 4. Classification is a deny-list keyed on what actually affects the Rust build/test job

| Pattern | Classification | Why |
|---|---|---|
| `\.rs$` | code | source; also covers `build.rs` |
| `Cargo\.toml$`, `Cargo\.lock$` (any depth) | code | manifests/lockfile — covers `crates/*/Cargo.toml` and `spikes/**/Cargo.toml` |
| `^\.cargo/` | code | build config (e.g. the `LBUG_VERSION` pin) |
| `^\.github/workflows/` | code | CI/build files — also satisfies "editing `ci.yml` itself must run the full suite" |
| `^crates/eval/scripts/` | code | `crates/eval/scripts/test-scripts.sh` is invoked directly by the `test` job's "eval script guards" step, so a change here can break CI even though it isn't `.rs` |
| everything else | docs/non-code | default |

Two entries are deliberately asymmetric despite living under similarly-named
directories: `crates/eval/scripts/**` is code (the `test` job executes it directly),
while a top-level `scripts/**` (docs-generation tooling from #295/PR #320) is docs — the
Rust suite never runs it. The distinction is what each path is actually exercised by,
not its directory name.

Unmatched/unrecognized paths default to docs. This is safe only because the overall
decision is a deny-list (any code-pattern match anywhere in the diff forces the full
suite), not an allow-list — a genuinely new code-relevant path just needs a pattern
added here later; it does not silently skip today by being unrecognized in the
dangerous direction. (The dangerous direction — silently skipping on an actual error —
is handled separately by the fail-safe default in Decision 3.)

`git diff --no-renames` is used deliberately: a `.rs` file renamed into `docs/` shows as
a delete of the old path plus an add of the new one, so the old path's `.rs` extension
still trips the code pattern instead of being hidden behind git's rename-detection
heuristics.

### 5. `push` always runs the full suite unconditionally; no fast path for it

`push` is already scoped to `branches: [main]` (see the concurrency-block comment in
`ci.yml` — that scoping exists to avoid two concurrent runs of the same commit racing
the shared lbug build cache). It only fires post-merge, so nobody is blocked waiting on
it, and adding diff-base logic for a second event shape isn't worth the complexity for
zero user-facing benefit. The `changes` job short-circuits to `code_changed=true` for
any non-`pull_request` event.

### 6. `test`'s gate restates the implicit `build-lbug` success dependency

`test`'s `if:` is `needs.changes.outputs.code_changed == 'true' &&
needs.build-lbug.result == 'success'`, not just the first clause. Without the second
clause, adding a custom `if:` to `test` would silently drop the implicit default
behavior GitHub Actions applies when a job has `needs:` but no custom `if:` — "only run
if all needed jobs succeeded." Omitting it would let `test` attempt to run even after a
genuine `build-lbug` failure on the code-touching path, changing today's behavior in
exactly the case this issue promises not to touch (FR-003/User Story 2).

## Consequences

- A docs-only PR's `build-lbug` and `test` jobs report conclusion "Skipped" rather than
  running for 15–18 minutes; the required `test (ubuntu-latest)` check still posts (as
  passing), so branch protection is satisfied — not merely absent a pending check.
- A PR touching `.rs`, `Cargo.toml`, `Cargo.lock`, `.cargo/**`, `build.rs`, or
  `.github/workflows/**` — or a mixed docs+code PR — runs the exact same full suite as
  before this change, with no reduction in coverage or duration.
- The one-run-per-commit / single-lbug-cache-consumer property this repo's `push`
  scoping protects is untouched: this change adds a job inside the existing workflow,
  it does not add a workflow run or change any trigger.
- The classification table lives in exactly one place (`ci.yml`'s `changes` job); a
  future code-relevant path (e.g. a new top-level `scripts/**` directory that the `test`
  job starts invoking) needs a pattern added there, or it defaults to docs and the full
  suite would not run for a change that actually needs it. This is a known, accepted
  gap — not silently safe, but visible and documented here rather than assumed away.
- If the `changes` job itself fails outright (e.g. a checkout infra failure), its
  outputs are unset and both downstream jobs skip via the same
  `needs.changes.outputs.code_changed == 'true'` check evaluating false. This is a
  residual gap — a real infra failure could theoretically satisfy the required check via
  skip — but it is visible: the failed `changes` job shows red on the PR, and a total
  checkout failure would likely fail every other job too. Not addressed further here.

## Alternatives Considered

- **`paths-ignore` on the `pull_request` trigger**: rejected outright — see Decision 1.
  This is the trap the issue was filed specifically to avoid; it produces a
  permanently-unmergeable docs-only PR instead of a fast one.
- **`dorny/paths-filter` or an equivalent third-party action**: rejected in favor of an
  inline bash step — see Decision 2. No such dependency exists in this repo today, and
  the fail-safe-default and `--no-renames` behaviors this design needs are more directly
  controlled inline than through a generic action's YAML pattern config.
- **Defaulting unmatched paths to code (allow-list instead of deny-list)**: rejected —
  it would require enumerating every legitimately-docs path in this repo (and every
  future one) rather than the much shorter list of paths that actually affect the Rust
  build/test job, and would regress toward "docs PRs still run the full suite" for any
  path not yet on the list.
- **Diff-base logic for `push` events too**: rejected — see Decision 5. `push` only
  fires post-merge on `main`; there is no PR blocked waiting on it, so the added
  complexity of a different diff base for that event shape has no user-facing payoff.

## References

- Issue #322
- #316 — the complementary lever: make the suite itself faster (this issue is about not
  running it at all when it cannot be affected by the diff)
- PR #320 / issue #295 — the motivating case: 23 files, zero Rust, blocked 15 minutes on
  a Rust suite that could not have caught anything in that diff
- `.github/workflows/ci.yml` — the `push: branches: [main]` scoping and its
  in-file comment documenting the lbug cache race this ADR's Decision 5 and Consequences
  sections preserve
