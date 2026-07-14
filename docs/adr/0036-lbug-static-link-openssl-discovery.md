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

## Decision

Let the crate's own `pkg-config`-based discovery do the work; only ensure OpenSSL is actually present and discoverable on each platform, at the point closest to where the failure would otherwise surface silently:

- **Linux (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`)**: both release runner images (`ubuntu-24.04`, `ubuntu-24.04-arm`) ship `libssl-dev` preinstalled, which provides `openssl.pc`. No `build-setup.yml` change is functionally required, but `libssl-dev = "*"` is declared explicitly in `[workspace.metadata.dist.dependencies.apt]` (`Cargo.toml`) anyway, so the real dependency is self-documenting rather than an incidental property of the current runner images.
- **macOS (`aarch64-apple-darwin`)**: the `macos-14` runner has no discoverable OpenSSL by default (only the system `/usr/bin/openssl` 1.1.1 compat shim, which ships no `.pc` file). `.github/build-setup.yml` runs `brew install openssl@3` before the lbug cache-restore steps, scoped with `if: runner.os == 'macOS'`.

No `OPENSSL_DIR`, manual `PKG_CONFIG_PATH` export, or hardcoded `rustc-link-search` path is used. Empirical verification (this repo's Apple Silicon dev machine, Homebrew): once `openssl@3` is installed, `pkg-config --exists openssl` succeeds with **no `PKG_CONFIG_PATH` set at all** — `pkg-config --debug` shows it resolves via `/opt/homebrew/lib/pkgconfig/openssl-uninstalled.pc`, an "uninstalled" symlink Homebrew places in pkgconf's *default* search path even for the keg-only `openssl@3` formula. A plain `brew install openssl@3` is therefore sufficient; the crate's own `pkg-config` call finds it with zero extra configuration. This was confirmed as SC-002's mandatory local pre-merge gate: `cargo build --release -p lcg-service` links cleanly (`otool -L` shows the binary linked against `/opt/homebrew/opt/openssl@3/lib/{libssl,libcrypto}.3.dylib`) and the binary runs to its embedder-config check.

## Consequences

- **No CI signal for macOS regressions.** The GH-hosted `macos-14` runner's behavior was not independently verified beyond this repo's own dev machine at implementation time — the release workflow's `pull_request`-triggered dry run (`build-local-artifacts`, unpublished) is the only place this gets exercised on the actual runner image. Any future lbug bump that touches the native SSL/TLS backend again must re-verify this mechanism with a real (or dry-run) release build, not just a local macOS build — the local machine's Homebrew state is not guaranteed identical to the runner's.
- **Rejected**: hardcoding `OPENSSL_DIR` or a version-pinned `rustc-link-search` path. This is exactly the anti-pattern that caused the original v0.9.0 failure (an implicit assumption about bundle contents that silently broke on the next native-bundle version). The `pkg-config`-based mechanism auto-adapts to whatever OpenSSL version Homebrew installs.
- **Rejected**: relying solely on Linux runner images shipping `libssl-dev` incidentally, without declaring it in `Cargo.toml`. Declaring it costs nothing (already present) and documents the real dependency for anyone reading `[workspace.metadata.dist.dependencies.apt]`.
- Future lbug bumps: re-run the source diff against the new `build.rs` before assuming this mechanism still applies — a future native bundle could switch TLS backends again, add a new external dependency, or change how it locates OpenSSL.
