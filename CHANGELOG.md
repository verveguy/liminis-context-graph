# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 development; see `git log` for history before 0.1.0.

## [0.9.0] - 2026-07-13

Initial public release: a local-first context graph engine combining property-graph storage, HNSW vector search, and full-text search in a single embedded service over LadybugDB, with a git-friendly JSONL write-ahead log as the source of truth and a 34-method JSON-RPC 2.0 surface over a Unix socket. See the [README](README.md) for the full feature set and architecture.

### Added

- Prebuilt binaries for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu` now published as GitHub Release assets via `cargo-dist`. One-line install: `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-context-graph/releases/latest/download/lcg-service-installer.sh | sh`.

### Changed

- Bump lbug pin from 0.16.1 to 0.17.0 (see PR #127 for delta summary; new `SystemConfig` defaults: `throw_on_wal_replay_failure=true`, `enable_checksums=true`; also removes `LBUG_BUILD_FROM_SOURCE` — 0.17.0 prebuilt is a self-contained fat bundle).
