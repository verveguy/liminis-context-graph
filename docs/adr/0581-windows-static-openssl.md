# ADR-0581: Link OpenSSL Statically on Windows

**Status:** Accepted
**Date:** 2026-09-15
**Issue:** #581

## Context

lbug links OpenSSL 3 externally since 0.18.0 (LadybugDB/ladybug#590): its `build.rs` emits
`cargo:rustc-link-lib=dylib=ssl` and `=crypto` unconditionally. [ADR-0550](0550-openssl-dynamic-linkage-via-rpath.md)
decided that lcg links OpenSSL **dynamically** on macOS and Linux, resolved through `@rpath` or a
bare SONAME. The reason: the user's package manager (Homebrew, MacPorts, apt, dnf) owns that
OpenSSL, so its security updates reach lcg users without an lcg release per CVE. What must never
ship is a binary naming one machine's OpenSSL path.

#582 made lcg build and run on Windows (named-pipe IPC). Adding `x86_64-pc-windows-msvc` as a
release target forces the same question there, and two facts make the macOS/Linux answer wrong for
Windows.

1. **There is no system OpenSSL for dynamic linking to defer to.** Windows ships none, and there is
   no package manager a user can be assumed to have that keeps one patched. A dynamically linked
   `liminis-context-graph.exe` needs `libssl-3-x64.dll` and `libcrypto-3-x64.dll` beside it or on
   `PATH`. In practice that means *we* ship the DLLs, frozen at the version we built with. That is
   the same update story as static linking, just with more files.
2. **cargo-dist can't put DLLs in the Windows archive alone.** `[workspace.metadata.dist] include`
   is a flat list applied to every target's archive (cargo-dist 0.32, which this repo pins). It has
   no per-target form, and `github-build-setup` steps are not per-target either. Bundling DLLs would
   mean staging placeholder files on macOS and Linux, or post-processing the Windows zip outside
   `dist build`, which builds and packages in one step.

It was verified on a real Windows 11 machine that static linking works:
- **Libraries:** vcpkg's `openssl:x64-windows-static-md` (OpenSSL 3.6.4; `/MD`, so it matches Rust's
  dynamic CRT), staged as `ssl.lib`/`crypto.lib`.
- **System libraries:** `ws2_32 crypt32 user32 advapi32`, supplied through link.exe's `LINK`
  environment variable.
- **Result:** the linked `liminis-context-graph.exe` imports no OpenSSL DLL. With every vcpkg
  directory removed from `PATH`, it served a real-embedding workload (bge-base, 768-d) end to end
  and ranked 4/4 semantic queries correctly.

## Decision

**On Windows, lcg links OpenSSL statically**, from vcpkg's `x64-windows-static-md` triplet. ADR-0550
is unchanged for macOS and Linux.

- **Staging:** `scripts/stage-openssl-windows.sh` is the single mechanism, used by `release.yml`,
  `windows.yml` and local builds. It stages `libssl.lib`/`libcrypto.lib` as the `ssl.lib`/`crypto.lib`
  names lbug asks for, onto `LIB` (never `RUSTFLAGS`, which would bust cargo's cache and collide with
  cargo-dist). It detects a static install by the absence of OpenSSL DLLs in `<root>/bin`, and then
  exports `LINK` with the Windows system libraries plus `OPENSSL_STATIC=1`. A dynamic install still
  works for local development: the script adds `<root>/bin` to `PATH` instead.
- **Release build:** `release.yml` builds the static triplet on the Windows runner (cached by vcpkg
  commit) before `dist build`.
- **Guard:** `scripts/assert-openssl-linkage.sh` gains a Windows branch that **fails** if a shipped
  binary imports `libssl-3*.dll` or `libcrypto-3*.dll`. Previously it printed "unsupported OS" and
  exited 0, which would have passed a broken artifact.

## Consequences

- The Windows archive is a single self-contained `.exe` plus the bundled lbug extensions (ADR-0559),
  and it runs on a clean machine with nothing else installed.
- **OpenSSL security fixes on Windows require an lcg release.** This is the cost ADR-0550 avoided
  elsewhere. It is accepted because a bundled DLL would carry the same cost, and because Windows has
  no package manager that removes it. When an OpenSSL CVE lands, a Windows release must pick up a
  newer vcpkg port (bump the runner's vcpkg or pin a newer baseline).
- The binary grows by roughly the size of the OpenSSL objects actually used.
- The Windows linkage guard matches OpenSSL DLL names in the PE import table by string. That is
  deliberately simple (no `dumpbin`, which needs a Visual Studio developer shell), and it can only
  err toward failing a build, never toward passing a dynamic one.
