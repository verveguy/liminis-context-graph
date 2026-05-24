# liminis-graph — Claude guidance

## Rust pre-commit checks (MUST run before every commit)

CI fails on any of these; running them locally first saves a fabrik retry cycle. Run in this exact order — earlier failures usually mask later ones:

1. `cargo fmt --all` — auto-format. Never commit without running this. Rust treats whitespace as binary pass/fail; even a single misaligned brace fails `cargo fmt --check` in CI.
2. `cargo build --tests --release` — compile lib AND tests. The library can build cleanly while tests fail to compile, because tests are a separate compilation unit. Common trap: adding a field to a struct that's also constructed in test files — lib sites get updated, test sites don't, lib build succeeds, test build fails.
3. `cargo clippy --all-targets --release -- -D warnings` — `--all-targets` includes tests, benches, and examples. CI runs with `-D warnings`, so any warning blocks merge. Common traps:
   - `dead_code` on test-only helpers → add `#[allow(dead_code)]`
   - `items_after_test_module` → put any non-test helpers BEFORE `#[cfg(test)] mod tests { }`, never after
   - New clippy lints arriving with a toolchain bump
4. `cargo test --release` — actually run tests.

If any step fails, fix and re-run from step 1 (fmt may have shifted line numbers, etc.).

## When adding or modifying a struct field

Grep ALL constructor call sites, including test files:

```
grep -rn "StructName {" --include="*.rs"
```

Tests live in `liminis-graph-core/tests/*.rs` AND inline `#[cfg(test)] mod tests { }` blocks within source files. Both compile separately from the library and will silently break if you only update the lib sites. This has burned us repeatedly (e.g. #46, #58 CI fix cycles).

## Toolchain

- Cargo and rustc come from `rustup`, installed via Homebrew: `/opt/homebrew/opt/rustup/bin` must be on `PATH`.
- Clippy lints can change between toolchain versions. If CI introduces a new lint that wasn't there yesterday, check the toolchain delta before assuming the code is wrong.

## Build artifact

The `liminis-graph` binary is consumed by the liminis Electron app via `graphiti_service.py` over a Unix socket. Breaking the IPC protocol (defined in `liminis-graph-core/src/handlers.rs` + the Python-side `service_protocol.py`) breaks the app. When adding or changing a method, keep both sides aligned and update the Tier 1a/1b/1c parity tests in `liminis-graph-core/tests/ipc_parity.rs`.
