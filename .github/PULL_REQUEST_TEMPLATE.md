> **External contributors**: see [CONTRIBUTING.md](../CONTRIBUTING.md) — the Constitution gates below are maintainer-internal and may be skipped.

## Summary

<!-- What does this PR do? 1–3 bullet points. -->

## Constitution gates

<!-- For each Principle in `.specify/memory/constitution.md` that applies, confirm PASS or N/A. -->

| Principle | Status |
|-----------|--------|
| I — IPC Parity | |
| II — Library and Binary Are Peers | |
| III — LadybugDB Only | |
| IV — WAL Is Authoritative | |
| V — LLM and Embedding Adapters Stay Out-of-Process | |

## Testing

<!-- How did you verify this works? What did you run? -->

- [ ] `cargo build --release`
- [ ] `cargo test`
- [ ] `cargo clippy -- -D warnings`
- [ ] `cargo fmt --check`

## Checklist

- [ ] ADR written or updated (if this changes architecture)
- [ ] No `tch`, `candle`, or `onnxruntime` added to `cargo tree`
- [ ] LadybugDB driver version unchanged or explicitly bumped with rationale

Closes #
