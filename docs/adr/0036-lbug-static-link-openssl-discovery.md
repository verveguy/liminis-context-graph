# ADR-0036: lbug 0.18.1 Static-Link OpenSSL Discovery

**Status**: Accepted
**Date**: 2026-07-14
**Issue**: #190 (upgrade lbug 0.17.0 → 0.18.1)

## Context

`lbug` is consumed exclusively via its prebuilt static-link path (source builds are broken upstream — [LadybugDB/ladybug-rust#18](https://github.com/LadybugDB/ladybug-rust/issues/18), 7399 duplicate-symbol linker errors; see [ADR-0021](0021-cargo-dist-build-setup-env-injection.md)). The v0.9.0 release build already failed once on this exact class of problem (PR #188): a floating `latest` native bundle skewed ahead of the pinned `lbug` crate, pulling in a 0.18.x bundle whose `httplib` links against OpenSSL (`ssl_st`, `ssl_ctx_st`) instead of 0.17.0's self-contained fat archive, which merges a bundled mbedtls into `liblbug.a` and needs no external SSL. That surfaced as `ld: symbol(s) not found for architecture arm64` on the macOS release runner, with no CI signal beforehand — there is no macOS Rust build in CI, only the release workflow.

Upgrading the crate pin itself to `=0.18.1` (this issue) reintroduces the same OpenSSL requirement deliberately. A full source diff of the `lbug` crate between 0.17.0 and 0.18.1 (`src/` is byte-identical; only `build.rs` changed) shows the static-link path now unconditionally emits, for every non-Windows target:

```rust
if let Ok(output) = Command::new("pkg-config").args(["--variable=libdir", "openssl"]).output() { ... }
println!("cargo:rustc-link-lib=dylib=ssl");
println!("cargo:rustc-link-lib=dylib=crypto");
```

This is not conditioned on macOS — it applies to Linux too. Whether the link succeeds depends entirely on whether `pkg-config` (or the default linker search path) can resolve `openssl.pc` / `libssl`/`libcrypto` at build time.

## The portability defect this must avoid

Simply installing `openssl@3` and letting the crate's `pkg-config` discovery resolve it (the first implementation of this ADR) **builds and runs on the build machine but produces a non-distributable macOS binary**. Homebrew's keg ships both `libssl.a` and `libssl.3.dylib` in the same `libdir`, and `ld` prefers the dylib — so the binary records an **absolute-path** load dependency on `/opt/homebrew/opt/openssl@3/lib/{libssl,libcrypto}.3.dylib`. That path does not exist on an end-user Mac without Homebrew `openssl@3`, so the distributed binary (and any Liminis app bundling it) fails at launch with a dyld "Library not loaded" error. It also **regresses the self-contained property** the 0.17.0 mbedtls fat archive had. "`otool -L` links against the Homebrew dylib and the binary runs *here*" is not the bar — a zero-external-OpenSSL-dependency binary is.

## Decision

Ensure OpenSSL is present on each platform, and on macOS **link it statically** so the released binary is self-contained:

- **Linux (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`)**: both release runner images (`ubuntu-24.04`, `ubuntu-24.04-arm`) ship `libssl-dev` and `pkg-config` preinstalled, providing `openssl.pc` and the tool to resolve it. `libssl-dev = "*"` and `pkg-config = "*"` are declared explicitly in `[workspace.metadata.dist.dependencies.apt]` (`Cargo.toml`) so the real dependencies are self-documenting rather than incidental. (Linux keeps the dynamic `libssl.so.3` link — that library is near-universally present on target Linux systems; static Linux OpenSSL is a possible future hardening, tracked separately.)
- **macOS (`aarch64-apple-darwin`)**: `.github/build-setup.yml` runs `brew install openssl@3 pkg-config` (with `HOMEBREW_NO_AUTO_UPDATE=1` to avoid a multi-minute self-update, per Gemini review on PR #191), then a second step **forces a static link**. `build.rs` hard-codes `-lssl -lcrypto` and feeds `ld` the dir returned by `pkg-config --variable=libdir openssl`; the fix hands it a libdir containing **only** `libssl.a`/`libcrypto.a` (symlinks into a temp dir + a custom `openssl.pc` on `PKG_CONFIG_PATH`), so `-lssl` resolves to the static archive. No source patch to `build.rs`, no `OPENSSL_DIR`, no hardcoded version path — the mechanism auto-adapts to whatever OpenSSL version Homebrew installs.

**Verification (SC-002, this repo's Apple Silicon dev machine)**: a fresh `cargo build --release -p lcg-service` against the static-only libdir produces a binary whose `otool -L` shows **no `ssl`/`crypto`/Homebrew dependency at all**, the binary runs to its embedder-config check, and the **full `lcg-core` suite (117 unit + all integration tests, which exercise real DB opens + `INSTALL`/`LOAD EXTENSION` — the code paths that use OpenSSL) passes**. Binary size grows ~5 MB (baked-in `libcrypto`), as expected.

## Consequences

- **No CI signal for macOS regressions.** There is no macOS Rust build in CI; the release workflow's `build-local-artifacts` job (and a local macOS build) are the only places this is exercised. The local pre-merge gate MUST check self-containment with `otool -L` (zero `ssl`/`crypto` deps) — not merely that the binary builds and runs, which is what the first implementation of this ADR did and why the non-portable dylib link shipped undetected until a real end-user/app test. Any future lbug bump touching the native SSL/TLS backend must re-verify both the link *and* self-containment.
- **Rejected**: hardcoding `OPENSSL_DIR` or a version-pinned `rustc-link-search` path. This is exactly the anti-pattern that caused the original v0.9.0 failure (an implicit assumption about bundle contents that silently broke on the next native-bundle version). The `pkg-config`-based mechanism auto-adapts to whatever OpenSSL version Homebrew installs.
- **Rejected**: relying solely on Linux runner images shipping `libssl-dev` incidentally, without declaring it in `Cargo.toml`. Declaring it costs nothing (already present) and documents the real dependency for anyone reading `[workspace.metadata.dist.dependencies.apt]`.
- Future lbug bumps: re-run the source diff against the new `build.rs` before assuming this mechanism still applies — a future native bundle could switch TLS backends again, add a new external dependency, or change how it locates OpenSSL.
