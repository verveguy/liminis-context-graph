# ADR-0398: Link OpenSSL Statically So Release Artifacts Stay Self-Contained

> **Superseded by [ADR-0550](0550-openssl-dynamic-linkage-via-rpath.md) (2026-09-03).**
> The decision below — force a *static* OpenSSL link so released binaries are
> self-contained — was reversed. It was never exercised by a release (every tag
> through v0.13.4 pinned lbug 0.17.0, which still bundled OpenSSL), it could not
> be made to work through `dist build` on macOS, and upstream has since stated an
> explicit position against bundling OpenSSL on CVE-maintenance grounds
> (LadybugDB/ladybug#681). lcg now links OpenSSL dynamically and resolves it
> through `@rpath` on macOS. Retained for the history and for its analysis of
> lbug's `build.rs` resolution order, which remains accurate.


**Status**: Accepted
**Date**: 2026-08-15
**Issues**: #398

## Context

Upgrading `lbug` from `0.17.0` to `0.19.1` changed how the graph engine gets its TLS
implementation, and that change reaches all the way out to what a user has to have installed
in order to run `liminis-context-graph`.

Under `0.17.0`, the prebuilt native bundle was genuinely self-contained. `build.rs` merged every
third-party static archive — antlr4, fastpfor, parquet, zstd, yyjson, **and mbedtls** — into a
single `liblbug.a` via `BundleStaticLibrary.cmake`. A grep of the unpacked `0.17.0` crate's
`build.rs` finds no reference to OpenSSL at all. The shipped binary linked nothing beyond system
frameworks and libc.

`0.18.0` changed the TLS backend to OpenSSL 3 (upstream `LadybugDB/ladybug#590`, "Link against
OpenSSL3", plus `#579`/`#581` on precompiled-binary OpenSSL dependencies). Everything else stays
statically bundled — mbedtls is still in the archive — but OpenSSL moved out. The published
`0.19.1` crate's `build.rs` handles it like this, and this is its *entire* OpenSSL handling:

```rust
if let Ok(output) = std::process::Command::new("pkg-config")
    .args(["--variable=libdir", "openssl"]).output()
{ /* ...emit rustc-link-search=native={lib_dir} if it succeeded... */ }
println!("cargo:rustc-link-lib=dylib=ssl");
println!("cargo:rustc-link-lib=dylib=crypto");
```

Three properties of that snippet drive this decision:

1. **`-lssl` and `-lcrypto` are emitted unconditionally**, whether or not `pkg-config` succeeded.
   A `pkg-config` miss does not fail the build script; it just omits the `-L` search path and
   defers the failure to the linker's default search.
2. **`PKG_CONFIG_PATH` is the only lever.** Grepping the published `0.19.1` `build.rs` for
   `OPENSSL_DIR`, `OPENSSL_ROOT_DIR`, `vcpkg`, and `homebrew` returns nothing. Those environment
   variables are read by the `openssl-sys` crate and by `ladybug-rust`'s `main` branch, but *not*
   by the crate we actually depend on. Setting them here would silently do nothing.
3. **The link kind is `dylib`**, so the default outcome is a dynamic dependency.

We measured the default outcome on macOS arm64 before deciding anything. A plain `cargo build`
at `0.19.1` produces:

```
$ otool -L target/debug/liminis-context-graph
    /opt/homebrew/opt/openssl@3/lib/libssl.3.dylib
    /opt/homebrew/opt/openssl@3/lib/libcrypto.3.dylib
    ...
```

against a `0.17.0` baseline of the same command that lists neither. That is not merely "the
binary now needs OpenSSL" — the binary has baked in Homebrew's *absolute install name*. Any user
without `openssl@3` at exactly `/opt/homebrew/opt/openssl@3` cannot start the service, including
every MacPorts user, every Intel-Homebrew user (`/usr/local`), and anyone who installed via the
`curl … | sh` shell installer, which is the documented install path and ships no OpenSSL.
Upstream hit precisely this on their Node binding (`LadybugDB/ladybug#682`); `ladybug-rust` ships
no equivalent rpath or install-name fixup.

This is a regression against the self-contained single-binary property that the fat-bundle model
exists to provide, and it is the kind of regression that surfaces on a user's machine rather than
in CI.

Two structural constraints shaped the response:

- **CI cannot prove macOS.** All eight `runs-on:` entries in `ci.yml` are `ubuntu-latest`.
  `release.yml` does trigger on `pull_request`, but its `build-local-artifacts` job is gated on
  `publishing == 'true' || pr_run_mode == 'upload'`, and no `pr-run-mode` is set, so a PR runs
  only the `plan` job and builds nothing. `aarch64-apple-darwin` is therefore linked for the
  first time *by the release itself* — which is exactly how the v0.9.0 `ld: symbol(s) not found`
  failure reached a tag.
- **`release.yml` is cargo-dist–generated but hand-maintained in places.** It inlines the
  `.github/build-setup.yml` fragment at generate time rather than referencing it at run time, and
  `allow-dirty = ["ci"]` preserves hand edits.

## Decision

**Link OpenSSL statically into the release artifacts**, by staging a directory that contains only
`libssl.a` and `libcrypto.a` plus a synthesized `openssl.pc` whose `libdir` points at that
directory, and putting it first on `PKG_CONFIG_PATH`.

`lbug`'s `build.rs` then emits its `rustc-link-search` at the staging directory, and the linker —
handed `-lssl`/`-lcrypto` with no `.dylib` or `.so` anywhere on that search path — resolves both
to the archives. No patching of `lbug`, no `install_name_tool` pass, and it uses the one lever the
crate actually reads.

Three pieces implement it:

- **`scripts/stage-openssl-static.sh`** resolves an OpenSSL libdir (`brew --prefix openssl@3` on
  macOS, `pkg-config --variable=libdir openssl` on Linux, `LCG_OPENSSL_LIBDIR` to override),
  asserts both archives exist, stages them, writes the `.pc`, and exports `PKG_CONFIG_PATH` —
  appending to `$GITHUB_ENV` when set, printing an `export` line otherwise so it is usable
  locally.
- **`scripts/assert-static-openssl.sh`** fails if `otool -L`/`ldd` on a built binary mentions
  `libssl` or `libcrypto`. It also fails when the inspection tool cannot produce a dependency
  listing at all — a missing tool, an unreadable file, or a damaged binary that makes `otool`
  print a diagnostic and still exit 0 — because "no listing" greps clean and would otherwise
  report OK on a binary nobody looked at.

  It runs in **two** places, over **every** binary that ships:

  | where | what it inspects |
  |---|---|
  | `ci.yml`'s `build-release` | `target/release/{liminis-context-graph,lcg-eval}` — a pre-tag signal, Linux only |
  | `release.yml`, after `Build artifacts` and before upload | every executable directly under `target/*/dist/`, i.e. what `dist build` actually produced, on all three targets |

  **Two packages ship, not one.** `dist plan` publishes `lcg-service` (whose binary is
  `liminis-context-graph`) *and* `lcg-eval`, each with tarballs on all three targets and its own
  shell installer. `lcg-eval` reaches lbug through `lcg-core`, so it links OpenSSL on identical
  terms and its installer breaks for users in exactly the same way. The `release.yml` guard
  therefore matches by **position** (`-path '*/dist/*' ! -path '*/dist/*/*'`) rather than by
  binary name, which also covers a future third binary without editing the step. The
  `! -path` clause is load-bearing: `target/<triple>/dist/` is a full cargo profile directory,
  so without it `deps/` and `build/` drag in scores of test and build-script executables. A
  name-filtered guard inspected one of the two and let the other ship on the assumption that
  one job's `PKG_CONFIG_PATH` covers both; the point of an automated check is that
  self-containment does not rest on that incidental coupling. The step additionally asserts
  both known binaries are among those found, so a layout change cannot quietly reduce the set
  to a subset — or to nothing — and still pass.
- **The OpenSSL dev package is declared on both platforms**, for the same determinism reason
  `cmake` and `ninja-build` are declared rather than assumed, and because it is what supplies
  the `.a` archives the script requires: `libssl-dev` in
  `[workspace.metadata.dist.dependencies.apt]`, and `openssl@3` in
  `[workspace.metadata.dist.dependencies.homebrew]`. `dist plan` on cargo-dist 0.32.0 resolves
  these into each runner's `packages_install`:

  | target | runner | `packages_install` |
  |---|---|---|
  | `aarch64-apple-darwin` | `macos-14` (cargo-dist default — no `github-custom-runners` entry) | `brew bundle install` of a Brewfile containing `openssl@3` |
  | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `apt-get install cmake libssl-dev ninja-build` |
  | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `apt-get install cmake libssl-dev ninja-build` |

  Without the `homebrew` entry, the macOS runner's `brew --prefix openssl@3` — the path the
  script resolves its libdir from on Darwin — would depend entirely on what the `macos-14`
  image happens to ship.

The staging step is wired into `ci.yml`'s `build-release` job and into `release.yml`, and
deliberately **not** into `.github/build-setup.yml`. Two facts about cargo-dist force that
placement:

- `build-setup.yml` is inlined at `dist generate` time rather than referenced at run time, so a
  release built from the current `release.yml` only runs what `release.yml` itself contains.
- cargo-dist inlines the fragment *above* its own `Install dependencies` step — the step that
  applies `[workspace.metadata.dist.dependencies.apt]`. Since `libssl-dev` is what supplies the
  `.a` archives the script requires on the Linux runners, a staging step inlined from
  `build-setup.yml` would run before its own dependency was installed and die with
  `libssl.a not found` on any runner that does not happen to preinstall it.

In `release.yml` the step therefore sits between `Install dependencies` and `Build artifacts`,
where `libssl-dev` is present and the `PKG_CONFIG_PATH` it writes to `$GITHUB_ENV` is still
inherited by the build. `allow-dirty = ["ci"]` is what keeps it there.

## Alternatives Considered

**Ship the dylibs and rewrite install names (rpath fixup).** Requires bundling
`libssl.3.dylib`/`libcrypto.3.dylib` into the archive and running `install_name_tool` to rewrite
the absolute Homebrew paths to `@executable_path`-relative ones. More moving parts, a per-platform
implementation, and it inherits the exact failure mode (`LadybugDB/ladybug#682`) that upstream is
still working through. It also grows the artifact without buying anything a static link doesn't.

**Document OpenSSL 3 as a system requirement.** Cheapest to implement and the worst outcome for
users. It breaks the "no toolchain required" promise of the shell installer, fails outright on any
Linux still shipping OpenSSL 1.1, and on macOS would require users to install Homebrew and place
`openssl@3` at one specific absolute path. Rejected unless static linkage proves infeasible on a
target, in which case the fallback is documented per-target rather than left implicit.

**`RUSTFLAGS="-L native=…"` instead of a synthesized `.pc`.** Would depend on `rustc`'s `-L`
ordering relative to build-script-emitted search paths — an undocumented detail. Going through
`PKG_CONFIG_PATH` means the path lands in `lbug`'s own link line with the ordering the crate
intends, which is what we want to bet a release on.

**Inline the staging logic in YAML instead of a committed script.** Rejected because the release
build path is the one that has never been exercised before a tag. A committed script is what lets
a developer run the *identical* thing locally under `dist build --artifacts=local` on an arm64
macOS machine — the only pre-tag signal macOS gets.

## Consequences

**Good.** Release artifacts keep the self-contained single-binary property, so the shell installer
keeps working with no new system requirement. The `otool -L` output of a `0.19.1` build now matches
the `0.17.0` baseline exactly. Linux gains a permanent pre-tag regression guard.

**Bad.** The build now depends on static OpenSSL archives being present, which is a real
requirement on any machine that runs the release path — hence declaring `libssl-dev` rather than
hoping. A statically linked OpenSSL also means a CVE in OpenSSL requires rebuilding and
re-releasing rather than the user updating a shared library; that is the standard trade for static
linking, and it matches how every other third-party dependency in the bundle already behaves.

**The staging step is load-bearing and its absence is silent at build time.** Removing it produces
a binary that builds and tests fine and then fails on a user's machine. That is why
`assert-static-openssl.sh` exists and runs in CI. If you are reading this because you are about to
delete the "Stage static OpenSSL" step: the assertion will go red, and that is the point.

**`lbug` does not watch `PKG_CONFIG_PATH`, so a warm `target/` can defeat the staging script.**
`lbug` 0.19.1's `build.rs` declares `rerun-if-env-changed` for seven `LBUG_*` variables and not
for `PKG_CONFIG_PATH`. Two consequences, both **fail-closed** — `assert-static-openssl.sh` catches
each, so neither can ship a bad artifact:

- Running `stage-openssl-static.sh` and then `dist build` on a tree that was already built
  *without* staging reuses the cached build-script output and links dynamically anyway. Run
  `cargo clean -p lbug` first; otherwise the local verification recipe above reports a confusing
  FAIL on a warm tree.
- Editing the staging path *without* also bumping the `lbug` version restores a cached
  build-script output pointing at the old directory and does not re-run it. `ci.yml`'s lbug cache
  key hashes the `lbug` stanza of `Cargo.lock` and nothing derived from `PKG_CONFIG_PATH` or the
  staging script, so a cache hit survives an edit to either.

Cold release runners build in the correct order, so the release path itself is unaffected.

**macOS remains unproven by CI.** This ADR does not fix that structural gap — it only makes the
release path reproducible locally so it can be checked by hand before tagging. Closing the gap
properly means adding a macOS job or enabling cargo-dist's `pr-run-mode = "upload"`, which is
separate work.

## Amendment (2026-09-01, issue #529)

The `lbug` pin moved from `0.19.1` to `0.20.1`. One claim in Context above is now historical
rather than current: "Grepping the published `0.19.1` `build.rs` for `OPENSSL_DIR`,
`OPENSSL_ROOT_DIR`, `vcpkg`, and `homebrew` returns nothing" — read that as a statement about
`0.19.1`, which was current when this ADR was written. As of `0.20.1`, `build.rs` checks
`OPENSSL_DIR`/`OPENSSL_ROOT_DIR` *first* and, if either is set, uses that directory directly
without consulting `pkg-config` at all. It also gained a `vcpkg::find_package("openssl")` call
(Windows-oriented, a no-op on our targets) and, after `pkg-config`, hardcoded macOS fallback probe
paths (`/opt/homebrew/opt/openssl/lib`, `/usr/local/opt/openssl/lib` — note: unversioned, no `@3`
suffix, a keg-alias path that typically doesn't exist since `openssl@3` is keg-only).

**This does not change the decision.** This repo's build never sets `OPENSSL_DIR` or
`OPENSSL_ROOT_DIR`, so the new first-checked branch is inert here and `PKG_CONFIG_PATH` remains
the effective lever `stage-openssl-static.sh` relies on — the same lever, the same script, the
same staging step placement. `-lssl`/`-lcrypto` are still emitted unconditionally as `dylib`, so
the static-link strategy (an archives-only directory staged onto `PKG_CONFIG_PATH`) is
unaffected. `#529` re-ran the full hand verification this ADR specifies (`stage-openssl-static.sh`
→ release build → `assert-static-openssl.sh` → `otool -L`) against the `0.20.1` build rather than
assuming the `0.19.1` result still held, in part because upstream `LadybugDB/ladybug#830`
namespace-wraps vendored mbedtls to avoid a symbol clash with statically-linked duckdb — a
C++-internal change invisible to `build.rs`, but exactly the kind of vendoring change this ADR's
"macOS remains unproven by CI" risk exists to catch by hand rather than assume away.

## References

- Issue #398 — the `0.17.0` → `0.19.1` upgrade
- Issue #529 — the `0.19.1` → `0.20.1` upgrade (see Amendment above)
- `LadybugDB/ladybug#590` — "Link against OpenSSL3" (0.18.0), the change that started this
- `LadybugDB/ladybug#579`, `#581` — precompiled-binary OpenSSL dependencies
- `LadybugDB/ladybug#682` — upstream's own absolute-install-name failure on the Node binding
- `LadybugDB/ladybug-rust#18` — why `LBUG_BUILD_FROM_SOURCE` remains unusable
- [ADR-0341](0341-build-release-artifacts-once.md) — the `build-release` job the assertion runs in
- `scripts/stage-openssl-static.sh`, `scripts/assert-static-openssl.sh`
