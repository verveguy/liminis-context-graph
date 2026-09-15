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
  `windows.yml` and local builds. It stages `libssl.lib`/`libcrypto.lib` under both the `ssl.lib`/
  `crypto.lib` names lbug 0.18.1 asked for and their own `libssl.lib`/`libcrypto.lib` names —
  which lbug 0.20.x's (0.20.3 included) Windows-specific link-lib branch asks for directly, per issue #561 — onto
  `LIB` (never `RUSTFLAGS`, which would bust cargo's cache and collide with cargo-dist). It detects
  a static install by the absence of OpenSSL DLLs in `<root>/bin`, and then exports `LINK` with the
  Windows system libraries plus `OPENSSL_STATIC=1`. A dynamic install still works for local
  development: the script adds `<root>/bin` to `PATH` instead.
- **Release build:** `release.yml` builds the static triplet on the Windows runner (cached by vcpkg
  commit) before `dist build`.
- **Guard:** `scripts/assert-openssl-linkage.sh` gains a Windows branch that **fails** if a shipped
  binary imports `libssl-3*.dll` or `libcrypto-3*.dll`. Previously it printed "unsupported OS" and
  exited 0, which would have passed a broken artifact.

### Amendment (2026-09-15): the lbug extensions still need OpenSSL DLLs

Static linking covers **only lcg's own executables**. lbug's prebuilt `win_amd64` extension
libraries, `libvector` and `libfts.lbug_extension` (verified for 0.18.1 and 0.20.0), are DLLs that
import `libssl-3-x64.dll` and `libcrypto-3-x64.dll`, plus `msvcp140`/`vcruntime140`/`vcruntime140_1`.
lbug loads each one with a plain `LoadLibraryW(<absolute path>)`, so Windows resolves those imports
from the exe's directory, the system directories and `PATH`, never from the extension's own
directory. On a machine without OpenSSL DLLs on `PATH`, `Db::open` fails with `Failed to load
library: …libvector.lbug_extension … The specified module could not be found.`

This is a property of lbug's Windows extensions, not of lcg's bundling (ADR-0559). An extension
lbug downloads from its CDN has the same dependency and fails the same way. It went unnoticed
because every Windows run happened under Git Bash, whose `PATH` includes Git for Windows'
`mingw64\bin`, which ships both DLLs. The same exe and bundle fail from PowerShell without that
directory and pass with it.

- **Ship the two DLLs in the bundle.** `scripts/stage-openssl-dlls-windows.sh` copies them from
  vcpkg's **dynamic** `x64-windows` OpenSSL port (OpenSSL 3.6.4 at the time of writing, from the
  same vcpkg commit as the static triplet) into `.lbdb/extension/<LBUG_EXTENSION_VERSION>/win_amd64/`.
  `include = [".lbdb"]` already packages that directory per target, so no per-target `include` is
  needed.
- **Put that directory on the DLL search path.**
  `lbug_extension_home::expose_extension_dependencies` calls `SetDllDirectoryW(<that directory>)`
  before `LOAD EXTENSION`. With a plain `LoadLibraryW`, that directory is searched right after the
  exe's directory. The `AddDllDirectory` family has no effect without `LOAD_LIBRARY_SEARCH_*` flags,
  which lbug doesn't pass. The call is process-global, and race-free only under `Db::open`'s
  `OPEN_LOCK`. It works the same for the release archive and an `LCG_LBUG_HOME` bundle.
- **CI runs clean.** `windows.yml` runs its end-to-end test from PowerShell with Git's `mingw64`/`usr`
  directories and any OpenSSL or vcpkg directories removed from `PATH`, and first asserts that neither
  DLL is resolvable there.

Consequences of the amendment:

- **An OpenSSL CVE now means a Windows lcg release for two reasons, not one:** the statically linked
  exe, and the `libssl-3-x64.dll`/`libcrypto-3-x64.dll` lcg now redistributes. Both come from the
  same vcpkg OpenSSL port, so one vcpkg bump updates both.
- The Windows archive is no longer "a single self-contained `.exe`". It is the exe plus the `.lbdb`
  bundle, which now carries two OpenSSL DLLs.
- **The Visual C++ runtime stays a user prerequisite, and the extensions add no new failure mode for
  it.** The exe itself imports `msvcp140.dll`, `vcruntime140.dll` and `vcruntime140_1.dll`, the same
  three the extensions import. The loader resolves those before any lcg code runs, so a machine
  without the redistributable fails at launch with the standard loader error, not inside `Db::open`.
  Any machine where lcg starts can also satisfy the extensions' runtime imports. Bundling the runtime
  app-local would need the DLLs beside the exe at the archive root, which `SetDllDirectoryW` cannot
  help with (the exe's imports resolve first) and which the flat `include` cannot place for one
  target. So the release notes state the requirement instead.
- **What testing shows, and what it doesn't.** Verification ran on a machine with Visual Studio Build
  Tools, so the VC++ runtime was present. It shows the bundled OpenSSL DLLs resolve with no OpenSSL
  DLL on `PATH`. It does not show behaviour on a pristine Windows install without the redistributable.
- Any later DLL load inside lcg inherits the changed search order: the bundle directory is added,
  and the current directory is removed. This is intended for a service process.

## Consequences

- The Windows archive is a single self-contained `.exe` plus the bundled lbug extensions (ADR-0559),
  and it runs on a clean machine with nothing else installed.
- **OpenSSL security fixes on Windows require an lcg release.** This is the cost ADR-0550 avoided
  elsewhere. It is accepted because a bundled DLL would carry the same cost, and because Windows has
  no package manager that removes it. When an OpenSSL CVE lands, a Windows release must pick up a
  newer vcpkg port (bump the runner's vcpkg or pin a newer baseline).
- The binary grows by roughly the size of the OpenSSL objects actually used.
- **The C runtime stays dynamic** (`msvc-crt-static = false` in `[workspace.metadata.dist]`).
  "Static OpenSSL" does not mean a fully static binary. lbug's prebuilt `lbug.lib` is compiled
  `/MD`, and cargo-dist's default `+crt-static` compiles the `cxx` bridge `/MT`. The first Windows
  `dist build` failed to link on exactly that (`LNK2038 RuntimeLibrary mismatch`). So the shipped
  `.exe` needs the Visual C++ runtime (`vcruntime140.dll` and friends). That runtime is present on
  most Windows installs and on any machine with Visual Studio or a VC++ redistributable, but it is
  not guaranteed on a pristine one, and it is **not** bundled. The vcpkg OpenSSL triplet is
  `x64-windows-static-md` for the same reason: `/MD`, matching lbug.
- The Windows linkage guard matches OpenSSL DLL names in the PE import table by string. That is
  deliberately simple (no `dumpbin`, which needs a Visual Studio developer shell), and it can only
  err toward failing a build, never toward passing a dynamic one.
