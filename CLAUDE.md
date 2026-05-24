# liminis-graph — Claude guidance

## Rust pre-commit checks (MUST run before every commit)

CI runs three commands (see `.github/workflows/ci.yml`); any failure blocks merge. Run them locally first to save a fabrik retry cycle. Use the debug profile locally for faster feedback (CI uses `--release` only where the integration-test linker requires it).

1. `cargo fmt --all` — auto-format. Never commit without running this. Rust treats whitespace as binary pass/fail; even a single misaligned brace fails `cargo fmt --check` in CI.
2. `cargo test` — compiles lib + tests and runs them. (CI runs `cargo test --release` because the 6 integration tests require release-mode linking. Locally, debug is fine for iteration; if your change touches release-only behavior, also run `cargo test --release` before committing.) Common trap: lib builds while tests fail to compile, because tests are a separate compilation unit — adding a field to a struct used in tests silently breaks the test build until every constructor is updated.
3. `cargo clippy -- -D warnings` — CI runs this exact form. Locally, `cargo clippy --all-targets -- -D warnings` is **stricter** (covers tests, benches, and examples in one pass) and is recommended for catching issues that CI's lib-only clippy would still hit via the test build. CI's `-D warnings` means any warning blocks merge. Common traps:
   - `dead_code` on test-only helpers → add `#[allow(dead_code)]`
   - `items_after_test_module` → put any non-test helpers BEFORE `#[cfg(test)] mod tests { }`, never after
   - New clippy lints introduced by a toolchain bump

If any step fails, fix and re-run from step 1 (fmt may have shifted line numbers).

## When adding or modifying a struct field

Grep ALL constructor call sites, including test files:

```
grep -rn "StructName {" --include="*.rs" .
```

Tests live in `liminis-graph-core/tests/*.rs` AND inline `#[cfg(test)] mod tests { }` blocks within source files. Both compile separately from the library and will silently break if you only update the lib sites. This has burned us repeatedly (e.g. #46, #58 CI fix cycles).

## Toolchain

- Install via `rustup`. Ensure `cargo` and `rustc` are on `PATH` — typically `~/.cargo/bin`, or `/opt/homebrew/opt/rustup/bin` on Apple Silicon with Homebrew-managed rustup.
- CI provisions its toolchain via `dtolnay/rust-toolchain@stable` on Ubuntu.
- Clippy lints can change between toolchain versions. If CI introduces a new lint that wasn't there yesterday, check the toolchain delta before assuming the code is wrong.

## Build artifact

The `liminis-graph` binary is consumed by the liminis Electron app via `graphiti_service.py` over a Unix socket. Breaking the IPC protocol (defined in `liminis-graph-core/src/handlers.rs` + the Python-side `service_protocol.py`) breaks the app. When adding or changing a method, keep both sides aligned and update the Tier 1a/1b/1c parity tests in `liminis-graph-core/tests/ipc_parity.rs`.
