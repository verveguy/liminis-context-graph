# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 development; see `git log` for history before 0.1.0.

## [Unreleased]

### Added

- **Native MCP-over-stdio transport** (`--mcp-stdio`): the binary can now run as a [Model Context Protocol](https://modelcontextprotocol.io) server over stdin/stdout (via `rmcp`), exposing the `knowledge_*` methods as MCP tools to any client — Claude Code, Claude Desktop, other agents — with no Electron/Node dependency. Per-scope tool gating via `--scope` (`read` / `write` / `cypher` / `admin` / `all`); standalone mode opens the database directly, `--connect <sock>` attaches to an already-running service instead. Long operations bridge to MCP progress notifications. See the README's "MCP-over-stdio transport" section and [ADR-0035](docs/adr/0035-mcp-stdio-transport.md). (#195)

### Changed

- Bump lbug pin from 0.17.0 to 0.18.1 to pick up lbug 18's stability fixes. The Rust wrapper API (`SystemConfig`, `Database`, `Connection`, `Value`, error types, etc.) is unchanged between these versions, so this is a build-plumbing change, not a code change: `[workspace.metadata.dist.dependencies.apt]` now declares `libssl-dev` (already preinstalled on both Linux release runner images), and `.github/build-setup.yml` installs Homebrew's `openssl@3` on macOS release builds — 0.18.1's prebuilt static-link path links httplib against OpenSSL instead of 0.17.0's bundled mbedtls. See `docs/adr/0036-lbug-static-link-openssl-discovery.md`. No WAL or on-disk format changes; existing `.lcg/` workspaces continue to work unmodified.

### Fixed

- Attached-mode MCP calls (`--connect`) now fail with a clean timeout error instead of blocking forever if the remote service stalls mid-call, and the JSON-RPC response id is validated so a late/stale reply can't be misdelivered to the next call (idle-read timeout `LCG_ATTACHED_CALL_TIMEOUT_MS`, default 30s). (#196)
- MCP `tools/call` validates required arguments at the transport layer, so a call missing a required field returns a clean tool error instead of silently reaching the handler with an empty or default value. (#196)

## [0.9.0] - 2026-07-13

Initial public release: a local-first context graph engine combining property-graph storage, HNSW vector search, and full-text search in a single embedded service over LadybugDB, with a git-friendly JSONL write-ahead log as the source of truth and a 34-method JSON-RPC 2.0 surface over a Unix socket. See the [README](README.md) for the full feature set and architecture.

### Added

- Prebuilt binaries for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu` now published as GitHub Release assets via `cargo-dist`. One-line install: `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-context-graph/releases/latest/download/lcg-service-installer.sh | sh`.

### Changed

- Bump lbug pin from 0.16.1 to 0.17.0 (see PR #127 for delta summary; new `SystemConfig` defaults: `throw_on_wal_replay_failure=true`, `enable_checksums=true`; also removes `LBUG_BUILD_FROM_SOURCE` — 0.17.0 prebuilt is a self-contained fat bundle).
