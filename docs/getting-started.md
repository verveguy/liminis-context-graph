---
layout: default
title: Getting Started
---

# Getting Started

**Multi-graph, not multi-tenant.** One workspace's `liminis-context-graph` process can hold many
independent graphs, each isolated by its own `group_id` and its own WAL stream — this is a
data-organisation feature for one user's own workspaces and subscriptions, not a security
boundary. There is no authentication, no authorisation, and no per-tenant resource isolation:
treat the process boundary as the trust boundary. See
[IPC & MCP Reference: group_ids semantics](ipc-mcp-reference.md#group_ids-semantics-omitted-vs-empty)
and [Operations](operations.md) for how groups work in practice.

## Install prebuilt binary

No Rust toolchain required:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-context-graph/releases/latest/download/lcg-service-installer.sh | sh
```

Prebuilt binaries are published for **macOS (Apple Silicon)**, **Linux x86_64**, and **Linux ARM64** on every tagged release.

> **macOS Gatekeeper note**: If macOS blocks the downloaded binary, clear the quarantine attribute before running:
> ```sh
> xattr -d com.apple.quarantine ~/.cargo/bin/liminis-context-graph
> ```
> Code signing will be added in a future release.

<!-- -->

> **Embedder required at runtime**: the binary connects to an out-of-process embedding service on startup. See [Embedder sidecar](configuration.md#embedder-sidecar) in the Configuration reference.

## Run it

```sh
# start your embedding service first — see "Embedder sidecar" in the Configuration reference
cd your-workspace/            # the directory whose content you're indexing
liminis-context-graph         # creates .lcg/, binds .lcg/service.sock
```

## Talk to it

The service speaks newline-delimited JSON-RPC 2.0 over the socket — from any language:

```python
import socket, json

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(".lcg/service.sock")
f = s.makefile("r", encoding="utf-8")

def call(method, params, id=1):
    s.sendall((json.dumps({"jsonrpc": "2.0", "id": id, "method": method, "params": params}) + "\n").encode())
    return json.loads(f.readline())["result"]

# ingest a chunk of text
call("knowledge_process_chunk", {
    "chunk_text": "Ada Lovelace wrote the first program for Babbage's Analytical Engine.",
    "chunk_id": "notes-0001",
    "source_file": "notes.md",
})

# hybrid (full-text + vector) entity search
print(call("knowledge_find_entities", {"query": "early computing pioneers", "num_results": 5}, id=2))

# graph + WAL health at a glance
print(call("knowledge_status", {}, id=3))
```

See the [IPC & MCP Reference](ipc-mcp-reference.md) for the full method list.

## Talk to it over MCP

Or skip the socket entirely and run the graph as a native [MCP](https://modelcontextprotocol.io) server for Claude Code, Claude Desktop, or any MCP client — add it to your client's MCP config:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your-workspace"
    }
  }
}
```

The client then sees the `knowledge_*` tools directly — no socket client to write. See
[MCP-over-stdio transport](ipc-mcp-reference.md#mcp-over-stdio-transport) for scopes, attached
mode, and the full flag reference.

## Build from source

Requires [Rust/Cargo](https://rustup.rs/), a C++20 compiler, and OpenSSL 3. The first build downloads a prebuilt lbug bundle (LadybugDB bindings), so the graph engine itself is never compiled — no `cmake` build step and no C++ dependency tree. lbug's `build.rs` does still compile its own small cxx FFI bridge locally at `-std=c++2a`, which is why a C++20 compiler is needed (GCC 13+ / a recent Clang; Ubuntu 22.04's GCC 11 is too old, as it lacks `<format>`). The bundle statically ships its other third-party dependencies, but since lbug 0.18.0 it links OpenSSL externally, so you also need `openssl@3` (macOS: `brew install openssl@3`; Debian/Ubuntu: `apt install libssl-dev`).

This applies to building from source only. Released binaries link OpenSSL statically and require nothing installed — see [ADR-0398](adr/0398-openssl-linkage-for-release-artifacts.md):

```bash
cargo build --release                         # build both crates
cargo test -p lcg-core                        # integration tests (LadybugDB round-trip)
cargo run --example basic_ingest -p lcg-core  # example: ingest 3 docs, search, print
cargo run -p lcg-service                      # run the service binary
```

## Bundling in downstream apps

For consumers (e.g. Electron apps or CI pipelines) that need a pinned binary version without running cargo, use the direct tarball URL from GitHub Releases:

```sh
curl -L https://github.com/verveguy/liminis-context-graph/releases/download/<TAG>/lcg-service-aarch64-apple-darwin.tar.xz \
  -o lcg-service-aarch64-apple-darwin.tar.xz
tar -xJf lcg-service-aarch64-apple-darwin.tar.xz
# binary is at: lcg-service-aarch64-apple-darwin/liminis-context-graph
```

Release artifacts are named after the `lcg-service` package (`lcg-service-<target>.tar.xz`); the binary *inside* is `liminis-context-graph`. Targets: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. The archive layout is set by cargo-dist 0.32.0; if cargo-dist is upgraded, verify the layout before updating consumer scripts. Each release includes a `.sha256` companion file for verification (`shasum -a 256 -c <file>.sha256`). The macOS Gatekeeper note above applies to script-downloaded binaries too.

Discover the latest release tag programmatically:

```sh
curl -s https://api.github.com/repos/verveguy/liminis-context-graph/releases/latest | jq -r '.tag_name'
```

## Next steps

- [Configuration](configuration.md) — environment variables and CLI flags.
- [Ontology](ontology.md) — declare an entity/relation type vocabulary.
- [Operations](operations.md) — WAL administration, recovery, and degraded mode.
