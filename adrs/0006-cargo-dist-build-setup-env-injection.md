# ADR 0006 — Inject `LBUG_BUILD_FROM_SOURCE` via cargo-dist `github-build-setup`

**Status**: Accepted
**Date**: 2026-06-01
**Issue**: #124 (prebuilt binaries via cargo-dist)

## Context

`cargo-dist` manages the release workflow (`.github/workflows/release.yml`) and regenerates it from `[workspace.metadata.dist]` config in `Cargo.toml`. The lbug crate requires `LBUG_BUILD_FROM_SOURCE=1` to be set during every build — without it, lbug's `build.rs` downloads a prebuilt `liblbug.a` that omits companion third-party static archives (fastpfor, brotli, lz4, zstd, yyjson), causing link failures.

The problem: cargo-dist has no native `build-env` config key for injecting env vars into build jobs (upstream issue [cargo-dist#1571](https://github.com/axodotdev/cargo-dist/issues/1571)).

The CI workflow (`.github/workflows/ci.yml`) sets `LBUG_BUILD_FROM_SOURCE: "1"` as a job-level env var on all build jobs. The same must apply to the release workflow.

## Decision

Use cargo-dist's `github-build-setup` feature (available since v0.20.0, stable in v0.32.0) to splice custom steps into each build-matrix job before `dist build` runs.

Configuration in `Cargo.toml`:
```toml
[workspace.metadata.dist]
github-build-setup = "build-setup.yml"
```

The file `.github/workflows/build-setup.yml` is a YAML sequence of GHA steps. cargo-dist reads this file and inlines the steps into each `build-local-artifacts` matrix job before the `dist build` step. Steps in this file also restore the lbug build cache so Linux x86_64 release jobs share the cache populated by CI runs.

## How It Works

1. `build-setup.yml` sets `LBUG_BUILD_FROM_SOURCE=1` via `$GITHUB_ENV` — this persists for all subsequent steps in the job, including `dist build`.
2. The same file computes the lbug cache key and restores the lbug build artifacts, matching the cache key used in `ci.yml`. Linux x86_64 release jobs get cache hits from prior CI runs; macOS and ARM64 build cold (no separate cache infra yet — follow-up issue).
3. `cargo dist generate` splices these steps automatically when `github-build-setup` is set.

## Fallback

If `github-build-setup` behaves unexpectedly in a future cargo-dist version, add the env var directly to each build job in the checked-in `release.yml`:
```yaml
env:
  LBUG_BUILD_FROM_SOURCE: "1"
```

The `release.yml` is committed and editable; a manual addition survives `dist generate` only if the stanza is outside the generated section. In practice, if `github-build-setup` is removed from config, `dist generate` will drop the injected steps — revert to the direct env var approach in that case.

## YAML Serialization Issue and `allow-dirty`

When cargo-dist inlines the `build-setup.yml` steps into `release.yml`, it re-serializes multi-line YAML scalars (like the `path:` value for `actions/cache`) with continuation lines at column 0 — which is syntactically invalid YAML in GitHub Actions' YAML parser. The generated `release.yml` was manually patched to use a literal block scalar (`|`) for the multi-line `path` value.

To prevent `dist plan` from rejecting `release.yml` as "out of date", the config sets `allow-dirty = ["ci"]` in `[workspace.metadata.dist]`. This suppresses the stale-check for CI files while preserving the manual YAML fix.

If `cargo dist generate` is run in the future and overwrites `release.yml`, the `path` value in the lbug cache-restore step must be manually re-patched to use `|` block scalar format — or the malformed YAML must be replaced with a single quoted path string. Track this in any future cargo-dist version upgrades.

## Consequences

- Future env vars needed by the build (e.g. for additional native deps) should be added to `.github/workflows/build-setup.yml` using `echo "VAR=value" >> $GITHUB_ENV`.
- The `build-setup.yml` file must exist before running `cargo dist generate` — the tool validates it at generation time.
- The lbug cache restore in `build-setup.yml` does not save the cache (read-only restore). The CI workflow's `build-lbug` job owns the cache-save step for Linux x86_64. macOS and ARM64 platforms have no cache infra today.
- **Cache bust sync**: `build-setup.yml` hardcodes the cache-bust date (`2026-05-29`) in the cache key because `build-setup.yml` steps run outside the `release.yml` workflow-level `env:` context and cannot reference `${{ env.LBUG_CACHE_BUST }}`. When bumping `LBUG_CACHE_BUST` in `ci.yml`, **also update the hardcoded date in `build-setup.yml`** — otherwise release-workflow jobs will miss the CI-populated lbug cache and build from scratch (~30–45 min instead of ~15 min). This is a performance miss, not a build failure.
- `allow-dirty = ["ci"]` means `dist generate` will not overwrite `release.yml`. To upgrade cargo-dist, update `cargo-dist-version` in `Cargo.toml`, run `dist generate`, and re-apply the YAML fix to the cache-restore step.
