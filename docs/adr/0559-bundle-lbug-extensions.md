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

So pre-placing the two extension files under a directory this project controls, and pointing
`home_directory` at it before the existing `INSTALL` statements run, is sufficient — no change is
needed to those statements themselves.

## Decision

### Resolve an extension home directory, 3-tier precedence

Before issuing `INSTALL vector` / `INSTALL fts`, `Db::open` resolves an "extension home"
directory (`crates/core/src/lbug_extension_home.rs`) in this order, using the first candidate
that resolves to a directory containing both extension files:

1. **`LCG_LBUG_HOME`** — an explicit env var override, for an operator staging extensions at a
   non-standard location (a shared read-only mount, a custom packaging layout).
2. **A directory derived from `std::env::current_exe()`'s parent** — the layout a release
   archive bundles files under: the binary sits at the archive's top level, with a `.lbdb/`
   sibling directory.
3. **Neither resolves** — no new statement is issued, and lbug's existing default (download from
   the CDN) applies unchanged. This is the required non-regression path for existing deployments
   and for running from a `cargo build`/dev binary.

Both tiers 1 and 2 resolve to the *root* directory `home_directory` itself expects — lbug appends
`.lbdb/extension/<version>/<platform>/...` internally; `resolve_extension_home()` only checks
that subtree exists before returning the root.

### Directory-level resolution, with an explicit partial-bundle guard

lbug's own `INSTALL` does not distinguish "never staged here" from "staged incompletely" — it
silently downloads whatever single file is missing from an otherwise-existing `home_directory`
(confirmed empirically against the pinned build). `resolve_extension_home()` therefore adds its
own pre-check: if the versioned platform directory (`.lbdb/extension/<version>/<platform>/`)
doesn't exist at all, this tier doesn't apply and resolution falls through to the next tier
(this is also how **version drift** — the pin's `LBUG_EXTENSION_VERSION` no longer matching a
previously-staged directory's version segment — resolves cleanly, with no special-case
detection). But once that directory exists, a missing file inside it is a hard `Error::Config`,
not a silent fall-through, so an operator's "offline" deployment cannot reach the network
unexpectedly because half of a bundle failed to extract.

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
to prevent, one layer down. A unit test (`extension_version_matches_lbug_crate_version`) asserts
this file's content equals `lbug::VERSION`; it fails loudly if a pin bump doesn't update the
file, but — because the mapping isn't always exact-match — it cannot verify the *new* value is
correct, only that someone looked. That residual gap is inherent and accepted.

### A deliberate, narrow exception to ADR-0024's bound-parameter convention

[ADR-0024](0024-bound-parameter-db-access.md) mandates bound `$name` parameters over Cypher-text
interpolation project-wide, and lists `escape()` as deleted. `CALL home_directory=...` cannot
follow that convention: confirmed empirically, `CALL home_directory=$home` fails to prepare
(`Binder exception: $_0_ has type PARAMETER but LITERAL was expected`), and the function-call form
`CALL home_directory($home)` also fails (`Catalog exception: function home_directory does not
exist`). lbug's `CALL name='literal'` pragma syntax for global settings accepts only a literal,
unlike the table-function `CALL FOO(..., $param)` forms already used elsewhere in `db.rs`.

`escape_cypher_string_literal` (`crates/core/src/db.rs`) reintroduces exactly the backslash
convention [ADR-0022](0022-lbug-cypher-escaping-convention.md) documented before ADR-0024
superseded it (`\` → `\\`, then `'` → `\'`) — narrowly scoped to this one call site, with its own
unit test for a single-quote-containing path (FR-006), and commented as a verified exception
rather than a reversion.

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
