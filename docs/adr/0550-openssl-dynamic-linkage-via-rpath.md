# ADR-0550: Link OpenSSL dynamically, resolved through `@rpath` on macOS

**Status:** Accepted — supersedes [ADR-0398](0398-openssl-linkage-for-release-artifacts.md)
**Date:** 2026-09-03
**Issue:** #550

## Context

Since lbug 0.18.0, OpenSSL 3 is no longer inside the prebuilt lbug bundle
(LadybugDB/ladybug#590); lbug links it externally and its `build.rs` emits
`-lssl`/`-lcrypto` as *dylib*.

ADR-0398 responded by forcing a **static** link, so a released binary would be
self-contained. That decision was made without an upstream position and was
never exercised by an actual release: v0.13.4 and everything before it pinned
`lbug = "=0.17.0"`, which still bundled OpenSSL, so the macOS release job never
ran either of ADR-0398's scripts. v0.14.0 is the first release that needs
external OpenSSL, and the static approach failed there repeatedly — the macOS
`dist build` kept producing binaries with absolute Homebrew paths despite
`PKG_CONFIG_PATH`, `OPENSSL_DIR`, and a declared `pkg-config` dependency.

Two things then became clear.

**Upstream has an explicit, reasoned position against bundling OpenSSL**
(LadybugDB/ladybug#681):

> Do not want to bundle openssl3 into ladybug. This is a security sensitive
> library. Any time there is a CVE, we'll have to spin up a new version of
> ladybug. Don't have the resources.
>
> If you have openssl3 installed somewhere else with proper security updates
> (highly recommended), happy to add that path.

**And upstream hit our exact bug and fixed it a different way.** Their macOS
Node addon shipped with `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib` baked
in and failed to load for MacPorts users. LadybugDB/ladybug#682 resolved it by
rewriting install names to `@rpath` and embedding several known OpenSSL roots —
explicitly *not* by static linking or bundling.

## Decision

**Link OpenSSL dynamically on every platform. The user supplies OpenSSL 3.**

- **macOS.** Mach-O records the absolute `install_name` of whatever dylib it
  linked against, so a stock build bakes in the build machine's Homebrew path.
  `scripts/stage-openssl-rpath.sh` copies the keg's dylibs into a staging
  directory, rewrites their own `install_name` to `@rpath/...` with
  `install_name_tool`, and points the build at those copies. Linking against
  them makes our binary record `@rpath/libssl.3.dylib`. Three roots are embedded
  as `LC_RPATH` — Homebrew ARM, Homebrew Intel, MacPorts — matching upstream's
  choice in #682.

  The staged dylibs are a **link-time fixture only**. They are never shipped; at
  runtime the loader resolves `@rpath` against the user's own installation.

  **The staging directory is `.openssl-rpath/` at the workspace root, and
  `PKG_CONFIG_PATH` is set in `.cargo/config.toml`'s `[env]` block with
  `relative = true` — not merely exported by the script.** This is load-bearing.
  `dist build` does not propagate the ambient environment to build scripts the
  way a plain `cargo build` does. On a CI runner the two were run seconds apart
  in a single step, with `PKG_CONFIG_PATH`, `OPENSSL_DIR` and `RUSTFLAGS` all
  verified present and `pkg-config --libs openssl` resolving to the staging
  directory: `cargo build --target aarch64-apple-darwin` produced
  `@rpath/libssl.3.dylib`, and `dist build` immediately after baked in
  `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`. Cargo applies `[env]`
  itself, so `dist` cannot drop it; `relative = true` keeps it machine-agnostic.
  This also forces the staging directory into the workspace, since a relative
  entry cannot reach `$RUNNER_TEMP` — but not under `target/`, which `dist`
  manages and may clear between staging and the link.

  **`force = true` on that entry is equally load-bearing.** Without it an
  `[env]` entry is only a *default*: it applies when the variable is unset and
  loses to any value the caller already exported. Adding the entry alone
  therefore changed nothing. A failure-only probe settled it — in one step
  lbug's `build.rs` emitted
  `cargo:rustc-link-search=native=/opt/homebrew/Cellar/openssl@3/3.6.3/lib`
  while `pkg-config --libs openssl` returned the staged directory; only a
  differing `PKG_CONFIG_PATH` explains both. Reproduced locally, byte-identical
  to CI, by building with a hostile ambient `PKG_CONFIG_PATH` pointing at the keg:

  | `[env]` entry | lbug's build script emits | binary records |
  | --- | --- | --- |
  | without `force` | `-L/opt/homebrew/Cellar/openssl@3/3.6.3/lib` | `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib` |
  | with `force = true` | `-L<workspace>/.openssl-rpath/lib` | `@rpath/libssl.3.dylib` |

  The prefix layout and the forced entry are both necessary and neither is
  sufficient: the prefix gives `openssl-sys` a valid `OPENSSL_DIR`, the forced
  entry gives lbug's `build.rs` the right `PKG_CONFIG_PATH`.

- **Linux.** Nothing to do. ELF records a SONAME (`libssl.so.3`) which `ld.so`
  resolves from the system search path, so the binary is already relocatable and
  distro security updates reach it. The staging script is a no-op there.

- **`scripts/assert-openssl-linkage.sh`** replaces the static guard and runs on
  both platforms and in both CI and release: it fails if any OpenSSL reference
  names a filesystem path rather than `@rpath` (macOS) or a bare SONAME (Linux),
  and on macOS additionally fails an `@rpath` reference with no `LC_RPATH`.

### How lbug's `build.rs` finds OpenSSL, per version

This differs across the versions we have pinned, and the difference decides which
lever actually works:

| | 0.18.1 (current pin) | 0.20.1 |
|---|---|---|
| `OPENSSL_DIR` / `OPENSSL_ROOT_DIR` | **not read** | checked first, returns immediately |
| vcpkg | not used | tried, returns if found |
| `pkg-config --variable=libdir` | used | used, but **falls through** |
| hardcoded macOS probe paths | **none** | `/opt/homebrew/opt/openssl/lib`, `/usr/local/opt/openssl/lib` |
| link kind emitted | `dylib=ssl` / `dylib=crypto` | same |

On **0.18.1** `PKG_CONFIG_PATH` alone is sufficient and `OPENSSL_DIR` is inert:
there is no fallback to pollute the search path, so pointing pkg-config at the
staged directory is enough. `stage-openssl-rpath.sh` still exports `OPENSSL_DIR`,
which is harmless here and correct if the pin moves forward again.

On **0.20.1** the pkg-config branch does *not* return, so the Homebrew keg — with
its `.dylib` files and absolute install names — is added to the search path
alongside the staged directory, and satisfying pkg-config is not enough on its
own. Only the `OPENSSL_DIR` branch returns early. That version is not currently
pinned (see #556: 0.20.1 deadlocks, `ladybug#911`), but the mechanism is recorded
here so the next upgrade does not have to rediscover it.

### Why link time, not post-build

`dist build` builds *and packages* in one step. Running `install_name_tool` on
the finished binary — the obvious reading of upstream's fix — would leave the
tarball holding the un-rewritten binary. Staging dylibs with rewritten IDs and
letting the linker copy them in achieves the same result one step earlier, where
it survives packaging.

## Consequences

**We accept a runtime dependency.** A released binary no longer runs on a Mac
with no OpenSSL 3 at any of the three roots. That is a genuine regression in the
single-binary install story, and the reason we accept it is the CVE argument
above: a statically linked OpenSSL makes *us* responsible for shipping a new lcg
for every OpenSSL advisory, and leaves users who do not upgrade silently
vulnerable. Dynamically linked, `brew upgrade openssl@3` fixes every consumer at
once.

**The failure mode is a dyld error before `main()`**, which lcg cannot catch or
report:

```
dyld[...]: Library not loaded: @rpath/libssl.3.dylib
```

This is especially opaque under an MCP client, which typically shows only
"server failed to start". It is documented verbatim in the installation
troubleshooting so it is searchable. A preflight check in the installer was
considered and rejected: cargo-dist 0.32 exposes no install hook, so it would
mean hand-patching a generated installer — more fragile than the problem.

**macOS with neither Homebrew nor MacPorts is unsupported.** Adding a root is a
one-line change to `RPATH_ROOTS` if that turns out to matter.

**ADR-0398's guarantee is withdrawn.** Any statement that released binaries
"require nothing installed" is now false and was corrected in the README as part
of this change.

## Alternatives considered

- **Keep static linking.** Rejected: it puts us on the hook for OpenSSL CVE
  response indefinitely, contradicts upstream's stated position, and we could
  not make it work through `dist build` regardless.
- **Ship the dylibs inside the tarball** with an `@executable_path` rpath.
  Self-contained *and* dynamic, but we would still be distributing OpenSSL and
  so would inherit exactly the CVE obligation the decision above exists to
  avoid — the worst of both.
- **Static on Linux, `@rpath` on macOS.** Rejected for consistency: two linkage
  models means two failure modes, two guards, and an ADR that has to explain
  both. Linux dynamic linking also works the way the CVE argument intends,
  because distros patch `libssl.so.3`.

## Amendment (2026-09-05): what 0.14.0 actually shipped

`@rpath` is the decision above and is what a local `cargo build` or `dist build`
produces. It is **not** what the release workflow produced on a GitHub runner:
there the link resolves through `liblbug.a`'s own `LC_LINKER_OPTION` records —
`ld` never opens the staged dylib, despite the staged prefix being the only
`-L` any build script emits — and the binary names Homebrew's stable prefix,
`/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`. Nine attempts did not close
that gap; the remaining thread is those `LC_LINKER_OPTION` records. Tracked in
[#550](https://github.com/verveguy/liminis-context-graph/issues/550).

That binary is fine for the documented prerequisite. `brew install openssl@3` on
Apple Silicon puts OpenSSL exactly there, and `/opt/homebrew/opt/openssl@3` is a
symlink Homebrew maintains across patch upgrades, so the CVE-response property
this ADR exists to protect still holds. What it loses is relocatability: MacPorts
users, and anyone with Homebrew at a non-standard prefix, must build from source.

`scripts/assert-openssl-linkage.sh` therefore accepts either `@rpath` or a
package manager's *stable* prefix, and still rejects a versioned Cellar path
such as `/opt/homebrew/Cellar/openssl@3/3.6.3/lib`, which breaks on the next
openssl patch release even on the machine that built it.

**The release was blocked for far longer than the defect warranted, by a guard
stricter than the requirement.** The staging script and its `@rpath` machinery
are kept — they work everywhere except this one path, and they are what #550
will finish — but relocatability across all three prefixes is an enhancement,
not a release blocker, and should not be treated as one again.
