# ADR-0559: Bundle lbug vector/fts extensions so startup never downloads from the CDN

**Status:** Accepted
**Date:** 2026-09-05
**Issue:** #559

## Context

Every `Db::open` (`crates/core/src/db.rs`) runs `INSTALL vector` / `INSTALL fts`. On a machine
that has never run lcg before, lbug resolves those by checking
`<home_directory>/.lbdb/extension/<version>/<platform>/<name>/lib<name>.lbug_extension` and, if
absent, downloading it from `https://extension.ladybugdb.com/`. `home_directory` defaults to the
user's home directory and is settable at runtime with `CALL home_directory='<dir>'`.

That download imposes a third-party CDN dependency this project does not control:

- An air-gapped or egress-restricted deployment cannot start at all.
- A CDN outage or a stale cache becomes an lcg outage — not hypothetical:
  LadybugDB/ladybug#903 was a Cloudflare caching layer serving stale/mismatched extension
  artifacts.
- The download happens inside `Db::open` while holding the process-global `OPEN_LOCK`
  (`crates/core/src/db.rs`), with no timeout, serializing every other concurrent open in the
  process behind it. This was previously mitigated in CI by caching `~/.lbdb/extension` per job
  (issue #555) — a workaround around the download, not a removal of the dependency: a cache miss
  (e.g. on any `Cargo.lock` change) still reached the CDN.

`INSTALL` short-circuits when the file is already present (`src/extension/extension_installer.cpp`
in the vendored lbug C++ source):

```cpp
if (vfs->fileOrPathExists(localLibFilePath) && !info.forceInstall) {
    // The extension has been installed, skip downloading from the repo.
    return false;
}
```

So pre-placing the two extension files under a directory this project controls is sufficient to
avoid the download — the only remaining question is how to tell lbug where to find them.

### A mechanism that was tried and abandoned: `CALL home_directory='<dir>'`

The first implementation pre-placed the files under a directory, pointed `home_directory` at it
with `CALL home_directory='<dir>'`, and let the existing `INSTALL vector`/`LOAD EXTENSION vector`
(bare name) statements resolve locally — exactly the mechanism this issue's own spec proposed.
This was abandoned during the Validate stage: it caused **silent row loss** in an unrelated
`RELATES_TO` dump→rebuild round trip (`mcp_real_corpus_admin_data_e2e`), reproduced
deterministically on both macOS and `linux/amd64` (Docker, matching CI). A methodical A/B
narrowed the cause precisely:

- Removing the redirect (falling back to lbug's own default resolution) always passed, whether
  or not a network download actually occurred.
- Redirecting `home_directory` to *any* value different from the process's real home directory
  always failed — independent of which files were staged there, their exact bytes (byte-for-byte
  identical to a fresh CDN fetch, confirmed via SHA-256), or which filesystem the target
  directory lived on (`/tmp` and a plain subdirectory of the real `$HOME` both failed).
- Redirecting `home_directory` to a value *equal to* the real home directory — i.e., issuing the
  exact same `CALL home_directory=...` statement, exercising the exact same code path — always
  passed.

A full audit of every reference to `home_directory` in the vendored lbug 0.18.1 C++ source (both
the `homeDirectory` field and the `"home_directory"` setting-name string) found only three
consumers — extension-directory resolution, and two `~`-prefix `glob()`/`expandPath()` expansion
sites — none of which explain data loss in an unrelated graph query. The mechanism is real and
reproducible, but its root cause inside lbug was not found by static source review; it was
reported upstream separately. This project does not depend on understanding *why* — it depends on
not depending on `home_directory` at all, which the mechanism below achieves.

## Decision

### Load extensions by absolute path, bypassing `home_directory`/`INSTALL` entirely

Before falling back to `INSTALL vector`/`INSTALL fts`, `Db::open` resolves the absolute paths to
the two extension files (`crates/core/src/lbug_extension_home.rs`) in this order, using the first
candidate whose versioned platform directory contains both files:

1. **`LCG_LBUG_HOME`** — an explicit env var override, for an operator staging extensions at a
   non-standard location (a shared read-only mount, a custom packaging layout).
2. **A directory derived from `std::env::current_exe()`'s parent** — the layout a release
   archive bundles files under: the binary sits at the archive's top level, with a `.lbdb/`
   sibling directory.
3. **Neither resolves** — falls back unchanged to `INSTALL vector`/`LOAD EXTENSION vector`/
   `INSTALL fts`/`LOAD EXTENSION fts` (lbug's own default: home directory, download on demand).
   This is the required non-regression path for existing deployments and for running from a
   `cargo build`/dev binary.

When a candidate resolves, `Db::open` issues `LOAD EXTENSION '<absolute path>'` directly for each
file — no `INSTALL`, no `CALL home_directory=...`. This works because
`ExtensionManager::loadExtension` (vendored lbug C++) only takes the `INSTALL`-oriented,
`home_directory`-dependent path when the given name matches its `OFFICIAL_EXTENSION` table by
exact (case-insensitive) string equality (`"VECTOR"`, `"FTS"`, ...); a filesystem path never
matches that table, so lbug treats it as a `USER` extension, `dlopen`s the exact path given, and
never consults `home_directory` or the CDN. (Trade-off: this also skips
`executeExtensionLoader`'s `_loader.lbug_extension` step, but neither `vector` nor `fts` ships
one — confirmed both by what `INSTALL` itself downloads and by a direct CDN probe.)

### Directory-level resolution, with an explicit partial-bundle guard

Loading by absolute path means a missing file simply fails to load rather than silently
downloading (the `INSTALL`-specific silent-fill behavior no longer applies at all) — but this
project's own resolution is still directory-level, so an incomplete bundle is caught before
either `LOAD EXTENSION` statement runs: if the versioned platform directory
(`.lbdb/extension/<version>/<platform>/`) doesn't exist at all, this tier doesn't apply and
resolution falls through to the next tier (this is also how **version drift** — the pin's
`LBUG_EXTENSION_VERSION` no longer matching a previously-staged directory's version segment —
resolves cleanly, with no special-case detection). But once that directory exists, a missing file
inside it is a hard `Error::Config`, not a silent fall-through, so an operator's "offline"
deployment cannot reach the network unexpectedly because half of a bundle failed to extract.

### `LBUG_EXTENSION_VERSION`: single-file source of truth, not derivable from `lbug::VERSION`

The extension-directory version segment is **not a derivable function of the `lbug` crate's
semver** — it's whatever version string is baked into that specific native build, which usually
but not always matches the crate's patch version. Empirically: the `0.18.1` pin resolves to
directory `0.18.1` (exact match), but a `0.19.1` pin resolved to directory `0.19.0`, and both
`0.20.1` and `0.20.2` resolved to directory `0.20.0`. This must be re-verified by hand
(`INSTALL vector` against the pinned version with an empty `HOME`, or checking
`https://extension.ladybugdb.com/v<N>/...`) every time the `lbug` workspace pin moves.

A single file at the repo root, `LBUG_EXTENSION_VERSION` (one line, no trailing newline), is the
one place this value lives — read by `crates/core/src/lbug_extension_home.rs` via
`include_str!`, and by `scripts/stage-lbug-extensions.sh` via `cat`. Two independently-maintained
copies (one in Rust, one in shell/YAML) would only need to go stale in *one* place to silently
reintroduce the CDN dependency this issue removes — exactly the failure mode this design exists
to prevent, one layer down.

The staleness tripwire does **not** assert `LBUG_EXTENSION_VERSION == lbug::VERSION` — that
equality doesn't always hold (see above), so it would fail forever after the first diverging
pin bump, even once a human had correctly re-verified and updated `LBUG_EXTENSION_VERSION`,
permanently blocking every subsequent lbug upgrade instead of catching only the "nobody looked"
case. Instead, a dedicated marker constant, `LBUG_CRATE_VERSION_VERIFIED_AGAINST` (a plain Rust
literal, deliberately not read from the version file), records the `lbug` crate version the
current `LBUG_EXTENSION_VERSION` was last empirically verified against. The unit test
`extension_version_was_verified_against_current_lbug_pin` asserts that constant equals the live
`lbug::VERSION`; it fails loudly if the crate pin moves without a human re-running the probe and
updating both `LBUG_EXTENSION_VERSION` and `LBUG_CRATE_VERSION_VERIFIED_AGAINST` — but, because
the mapping isn't always exact-match, it cannot verify the *new* value is correct, only that
someone looked. That residual gap is inherent and accepted.

### A deliberate, narrow exception to ADR-0024's bound-parameter convention

[ADR-0024](0024-bound-parameter-db-access.md) mandates bound `$name` parameters over Cypher-text
interpolation project-wide, and lists `escape()` as deleted. `LOAD EXTENSION '<path>'` cannot
follow that convention — it takes a literal path, not a bound parameter, the same constraint the
abandoned `CALL home_directory=...` mechanism had (confirmed empirically: `CALL
home_directory=$home` fails to prepare with `Binder exception: $_0_ has type PARAMETER but
LITERAL was expected`) — unlike the table-function `CALL FOO(..., $param)` forms already used
elsewhere in `db.rs`.

`escape_cypher_string_literal` (`crates/core/src/db.rs`) reintroduces exactly the backslash
convention [ADR-0022](0022-lbug-cypher-escaping-convention.md) documented before ADR-0024
superseded it (`\` → `\\`, then `'` → `\'`) — narrowly scoped to this one call site (now applied
to each resolved extension file path, twice per `Db::open`, rather than once to a directory
root), with its own unit test for a single-quote-containing path (FR-006), and commented as a
verified exception rather than a reversion.

### CI: one touchpoint, not seven

`ci.yml`'s `build-release` job is now the **only** job in the entire CI run permitted to reach
`extension.ladybugdb.com`, and only on a cache miss keyed by `LBUG_EXTENSION_VERSION`'s content.
It stages the `linux_amd64` bundle (every downstream job runs on `ubuntu-latest`) and uploads it
as a same-run artifact; the `test` job and all 6 real-corpus e2e jobs download that artifact and
set `LCG_LBUG_HOME` before running, rather than each independently caching-or-downloading into
`~/.lbdb/extension` (issue #555's original mitigation). This is a strictly stronger guarantee
than before: those 7 jobs could previously reach the CDN on any cache miss; after this change
none of them ever can, regardless of cache state.

Real extension binaries are fetched at release/CI **build** time only (this script's own
network call), never at a user's **startup** time — consistent with the issue's scope, which
treats CDN use as acceptable during this project's own packaging but not during a deployed
binary's first run.

## Consequences

- A release binary extracted intact starts successfully with zero network route to the CDN
  (SC-001), and `LCG_LBUG_HOME` gives operators an escape hatch for non-standard layouts
  (SC-002).
- A pre-existing deployment (no bundle, no override) is unaffected — same download-on-demand
  behavior as before (SC-003).
- `ci.yml`'s required checks no longer depend on `extension.ladybugdb.com` reachability except
  in `build-release`, and only on a cache miss (SC-004).
- A future `lbug` pin bump that forgets to update `LBUG_EXTENSION_VERSION` fails a unit test
  instead of silently reintroducing the CDN dependency (SC-005) — but fixing that failure still
  requires a human to re-run the empirical probe; the test cannot compute the correct value.
- `.github/build-setup.yml`'s staging step is a maintained template, not a live one:
  `allow-dirty = ["ci"]` (Cargo.toml) makes `dist generate` refuse to run at all, so unlike a
  normal cargo-dist project this fragment is never actually inlined into
  `.github/workflows/release.yml`. The identical step is hand-added there, in
  `build-local-artifacts`, the same way `scripts/stage-openssl-rpath.sh`'s step already was. A
  future edit to the staging step must be applied to both files by hand.
- `.github/build-setup.yml`'s target-triple → platform-string mapping must stay in lockstep with
  `lbug_extension_home.rs`'s `platform_string()`; both are commented to cross-reference each
  other, but a 4th release target needs both updated together.
- Windows is unmapped (`platform_string()` returns `None`), matching the fact that no Windows
  release target exists in `Cargo.toml`'s `workspace.metadata.dist.targets`. This degrades
  gracefully — tier 2 simply never resolves — rather than erroring.

## Alternatives considered

- **Fetch extensions from `build.rs`.** Rejected: `build.rs` runs on every `cargo build`, not
  just release builds, so this would reintroduce a network dependency on the common developer
  path this issue exists to remove.
- **Vendor/mirror the extension CDN.** Out of scope per the issue — this is about this project's
  own release packaging and startup resolution, not renegotiating anything with upstream
  LadybugDB.
- **Keep the `~/.lbdb` CI cache instead of a same-run artifact.** Rejected for `test`/e2e: a
  cache is restore-or-download, so a cache miss still reaches the CDN — exactly the gap FR-010
  exists to close. `build-release`'s own cache is different in kind: it still gates a *download*
  (acceptable, build-time-only, per the issue's scope), not the *absence* of a network path for
  the other 7 jobs, which get the bundle via artifact download only.
- **Check real extension binaries into the repo.** Rejected: duplicates per-platform
  release-artifact concerns, adds binary churn on every lbug bump, and the issue's own scope
  already treats a controlled, monitored, build-time CDN fetch as acceptable.
