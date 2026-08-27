---
layout: default
title: Configuration
---

# Configuration

## CLI flags

| Flag | Description |
|------|-------------|
| `--embedder-uds <path>` | Unix domain socket for the embedder sidecar (default on macOS: `/tmp/liminis-inference.sock`, auto-detected). |
| `--embedder-http <url>` | HTTP URL for an OpenAI-compatible embedding endpoint. Mutually exclusive with `--embedder-uds`. |
| `--extractor-uds <path>` | Unix domain socket for a local OpenAI-compatible extraction endpoint. |
| `--extractor-http <url>` | HTTP URL for a local OpenAI-compatible extraction endpoint. Mutually exclusive with `--extractor-uds`. |
| `--mcp-stdio` | Starts a native [Model Context Protocol](https://modelcontextprotocol.io) server over stdin/stdout instead of binding the Unix socket. See [MCP-over-stdio transport](ipc-mcp-reference.md#mcp-over-stdio-transport). |
| `--scope=<list>` | MCP-stdio only. Comma-separated list of scopes to advertise in `tools/list` (default `all`): `read`, `write`, `cypher`, `admin`. |
| `--connect <path>` | MCP-stdio only. Attached mode: forward every `tools/call` as JSON-RPC over the given Unix socket to an already-running service, instead of opening the database directly. |
| `--allow-remote-close` | MCP-stdio attached mode only. Advertise and allow `knowledge_close`, forwarding the shutdown to the remote service. No effect in standalone mode. |
| `--help` | Print usage and exit. |
| `--version` | Print the binary's version and exit. |

`--embedder-uds`/`--embedder-http` are for the embedding sidecar; `--extractor-uds`/`--extractor-http` are for the extraction LLM. See [Embedder sidecar](#embedder-sidecar) and [Extractor: local or hosted](#extractor-local-or-hosted) below.

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `LCG_SOCKET_PATH` | No | Unix socket path the IPC daemon listens on (default `.lcg/service.sock`) |
| `LCG_DB_PATH` | No | Path to the LadybugDB database file (default `.lcg/db/liminis.db`) |
| `LCG_EMBEDDING_URL` | No | Fallback HTTP URL used when neither `--embedder-uds` nor `--embedder-http` is passed and the default UDS socket (`/tmp/liminis-inference.sock`) is absent. On Unix, if this var is also unset, the binary exits with an error. On non-Unix, defaults to `http://127.0.0.1:8765/v1/embeddings`. |
| `LCG_EMBEDDING_MODEL` | No | Embedding model name sent in requests (default `bge-base-en-v1.5`) |
| `LCG_EMBEDDING_DIM` | No | Embedding dimension override if probe fails at startup (default: auto-detected via probe). Does **not** override an authentication failure (HTTP 401/403) — see [Embedder sidecar](#embedder-sidecar) below. |
| `LCG_EMBEDDING_API_KEY` | No | Bearer token sent as `Authorization: Bearer <key>` on the HTTP embedder transport (`--embedder-http`/`LCG_EMBEDDING_URL`). Falls back to `GRAPHITI_EMBEDDING_API_KEY`, then to `OPENAI_API_KEY`. Unset (or empty) sends no `Authorization` header, unchanged from prior versions. Never applies to the UDS transport (`--embedder-uds`). |
| `OPENAI_API_KEY` | No | Convenience fallback for `LCG_EMBEDDING_API_KEY` (see above) — reuses an already-exported OpenAI key. Not a deprecated alias, so using it logs no *deprecation* warning — but since it sends whatever key is exported for OpenAI tooling generally to whatever `--embedder-http`/`LCG_EMBEDDING_URL` endpoint is configured (not necessarily OpenAI's own API), using this tier does log a one-line informational notice naming the endpoint the key will be sent to. |
| `LCG_EMBED_BATCH_SIZE` | No | Number of texts per array-valued `/v1/embeddings` request when `OaiEmbedder::embed_batch` chunks a larger batch (default `64`, valid range `1`–`256`). The embedding sidecar's real per-request limit is unmeasured; lower this if a batch call fails due to a request-size limit. See [ADR-0445](adr/0445-embedder-batch-api.md). |
| `LCG_EXTRACTION_LLM` | No | Anthropic model for entity extraction, optional `primary:fallback` format. Only consulted on the Anthropic path (see `ANTHROPIC_API_KEY` below); ignored when a local extraction endpoint is selected. |
| `LCG_EXTRACTION_URL` | No | Fallback HTTP URL used when no `--extractor-uds`/`--extractor-http` flag is passed and `ANTHROPIC_API_KEY` is unset. If this var is also unset in that situation, no extraction provider is configured: the binary still starts, but every extraction-dependent call (`knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`) fails with an error identifying the missing configuration — extraction has no default-socket auto-detection (unlike the embedder), so a running sidecar alone is not enough. |
| `LCG_EXTRACTION_MODEL` | No | Model name sent in local extraction requests (default `local`) — decorative against the bundled sidecar (which ignores the request's `model` field), but meaningful for real OpenAI-compatible servers reached via `--extractor-http`. |
| `LCG_EXTRACTION_MAX_TOKENS_CEILING` | No | Uniform ceiling (in tokens) on the per-call `max_tokens` budget for entity/edge extraction, across both the hosted Anthropic path and self-hosted/OAI-compatible models (default `32768`). The per-call initial budget scales with input chunk size up to this ceiling; it exists to stop genuine non-termination (a model that never stops generating), not to optimize spend, so it is intentionally generous. An invalid value (non-numeric, or below the compiled-in 4096-token floor) logs a warning to stderr and falls back to the default. Setting it near the floor is valid but self-defeating: since the per-call budget is `clamp(chunk_len_bytes * ratio, 4096, ceiling)`, a ceiling close to 4096 collapses that range and every call effectively gets the same floor-sized budget regardless of chunk size — silently disabling proportional scaling, not just narrowing the runaway guard. |
| `LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS` | No | Advisory threshold, in **characters** (not bytes), above which `knowledge_process_chunk`'s `chunk_text` is considered oversized (default `8000`). Ingestion is unaffected — no rejection, truncation, or internal splitting — but a call whose `chunk_text` exceeds this threshold gains a `warning` field in its result naming the actual character count and the recommended maximum, and emits a `chunk_text_oversized` telemetry event (see [Telemetry](telemetry.md)). Extraction quality degrades well before any context-window limit is reached, so splitting oversized input into multiple `knowledge_process_chunk` calls is the caller's responsibility. An invalid value (non-numeric, zero, or negative) logs a warning to stderr and falls back to the default. |
| `LCG_RECORD_LLM` | No | Path to an LLM cassette (JSONL). If set, every extraction call is recorded to this file in addition to running live — see [Record/replay cassettes](testing-and-evaluation.md#recordreplay-cassettes). Mutually exclusive with `LCG_REPLAY_LLM`. |
| `LCG_REPLAY_LLM` | No | Path to a previously recorded LLM cassette (JSONL). If set, extraction is served entirely from the cassette — no extractor provider is resolved, no credentials are required, and no network call is ever made. Mutually exclusive with `LCG_RECORD_LLM`. See [Record/replay cassettes](testing-and-evaluation.md#recordreplay-cassettes). |
| `LCG_DEDUP_LLM` | No | If set, enables local dedup adapter |
| `LCG_DEDUP_ADAPTER_URL` | No | URL for the local dedup HTTP adapter (default `http://127.0.0.1:8767`) |
| `LCG_WAL_DIR` | No | WAL **root** directory (default `.lcg/wal`) — holds one subdirectory per `group_id` (issue #378), e.g. `<LCG_WAL_DIR>/liminis/` for the default group. A pre-378 single-stream directory (loose `*.jsonl`/`.checkpoints/`/`.wal-bounds.json` with no `liminis/` subdirectory) is migrated into this layout automatically on first boot under the upgraded binary — see [Operations](operations.md#on-disk-layout) and [ADR-0378](adr/0378-multi-stream-wal-per-group-directory.md). |
| `LCG_WAL_MAX_BYTES_PER_FILE` | No | Per-file byte-size rotation threshold for the WAL (default `5242880` = 5 MB); set to `0` to disable byte-size rotation and rely on event count only |
| `LCG_WAL_MAX_EVENTS_PER_FILE` | No | Per-file event-count rotation threshold for the WAL (default `10000`); rotation fires when either this threshold or `LCG_WAL_MAX_BYTES_PER_FILE` is reached |
| `LCG_REPLAY_LOG_INTERVAL_SECS` | No | Throttle interval in seconds between `[WAL PROGRESS]` log lines written to stderr during WAL replay (default `30`). Set to `0` to emit a line on every progress event. |
| `ANTHROPIC_API_KEY` | No | API key for Anthropic entity/relationship extraction. When set (and no explicit `--extractor-uds`/`--extractor-http` flag is passed), extraction uses the hosted Anthropic API for ingestion (`knowledge_process_chunk` / `knowledge_add_episode`) and entity/relation re-classification (`knowledge_reprocess_entity_types`, `knowledge_reprocess_relation_types`). When unset, extraction requires an explicit `--extractor-uds`/`--extractor-http` flag or `LCG_EXTRACTION_URL` pointing at a local OpenAI-compatible endpoint — it is not auto-detected — see [Extractor: local or hosted](#extractor-local-or-hosted) below. Not needed for read-only, embedding-only, or non-LLM tools. |
| `LIMINIS_WORKSPACE_ROOT` | No* | Absolute path to the workspace root. **Required** for the three corrections IPC methods (`knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types`). If unset, those methods return a `-32000` error. The corrections file is read from `{LIMINIS_WORKSPACE_ROOT}/.liminis/knowledge-corrections.yaml`. |
| `LCG_REPLAY_BATCH_SIZE` | No | Rows per batch during WAL replay (default `64`, valid range `1`–`256`). Lower values reduce peak memory on a large rebuild; higher values replay faster. |
| `LCG_REPLAY_FAILURE_SAMPLES` | No | How many distinct failing lines to retain and report per WAL replay (default `10`). Samples are deduplicated by failure shape, so one bad template cannot crowd out the rest. |
| `LCG_REPLAY_FIDELITY_THRESHOLD` | No | Float `0.0`–`1.0`. Replay warns when the fraction of successfully applied mutations falls below this, i.e. the rebuilt graph is not a faithful reconstruction of the WAL. |
| `LCG_MIGRATION_KEEP_BACKUP` | No | When set, a workspace-layout migration retains its pre-migration backup instead of removing it after a successful migration. |
| `LCG_SHUTDOWN_TIMEOUT_MS` | No | Grace period in milliseconds for in-flight requests to finish on `SIGTERM` before the service exits. |
| `LCG_ATTACHED_CALL_TIMEOUT_MS` | No | Idle-read timeout in milliseconds for MCP attached mode (`--connect`, default `30000`). See [MCP-over-stdio transport](ipc-mcp-reference.md#mcp-over-stdio-transport). |
| `LIMINIS_DEDUP_HYBRID_THRESHOLD` | No | Entity count per `group_id` above which dedup switches from brute-force cosine to the hybrid FTS + vector path. |
| `LIMINIS_LLM_COST_TABLE_PATH` | No | Path to a JSON model-pricing table used to populate `estimated_cost_usd` in `token_usage` telemetry. See [Telemetry](telemetry.md). |

That's 31 variables as of this page's writing, covering every `env::var`/`lcg_env_var` call site under `crates/*/src`.

**Deprecated `GRAPHITI_*` aliases.** Every `LCG_*` variable above that predates the rename also
accepts its old `GRAPHITI_*` spelling — `GRAPHITI_SOCKET_PATH`, `GRAPHITI_DB_PATH`,
`GRAPHITI_EMBEDDING_URL`, `GRAPHITI_EMBEDDING_MODEL`, `GRAPHITI_EMBEDDING_DIM`,
`GRAPHITI_EMBEDDING_API_KEY`, `GRAPHITI_EXTRACTION_LLM`, `GRAPHITI_DEDUP_LLM`,
`GRAPHITI_DEDUP_ADAPTER_URL`, and `GRAPHITI_WAL_DIR`. Using one logs `DEPRECATED: env var <old> is
deprecated; rename to <new>` at startup. They are honoured for now; prefer the `LCG_*` names.
`OPENAI_API_KEY` is not in this list — it's a convenience fallback for `LCG_EMBEDDING_API_KEY`,
not a legacy spelling of an LCG-specific variable, so using it triggers no deprecation warning.

## Embedder sidecar

> For a one-page comparison of every supported embedding option (platform, install, cost,
> dimension, and verification status), see [Embedding Options](embedding-options.md).

`OaiEmbedder` delegates embedding to an external service over the OpenAI-compatible
`POST /v1/embeddings` contract. The binary supports two transports, selected via CLI flags:

```sh
liminis-context-graph --embedder-uds /tmp/liminis-inference.sock            # Unix domain socket (default on macOS)
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings   # HTTP
```

**Default behaviour** (no flags): the binary looks for the Swift CoreML sidecar socket at
`/tmp/liminis-inference.sock`. If absent, it falls back to `LCG_EMBEDDING_URL` (HTTP). If
neither exists, it exits with a clear error.

The binary probes the embedder at startup to confirm it is reachable and auto-detect the
embedding dimension. If the probe fails and `LCG_EMBEDDING_DIM` is not set, the process
exits with an error rather than failing silently on the first embed request. This is the
behaviour for the default Unix-socket service (`liminis-context-graph` with no
`--mcp-stdio` flag), and it is unchanged regardless of how the embedder failed to respond.

**Standalone `--mcp-stdio` mode is different.** When an MCP client (Claude Desktop, an
editor's MCP integration, etc.) launches `liminis-context-graph --mcp-stdio` directly,
nobody is watching the process's stderr — the client just shows a generic "server failed
to start," and the exit-with-an-error message above is never seen. To cover the common
case where the client starts the embedder sidecar and `liminis-context-graph` as sibling
processes with no ordering guarantee, standalone `--mcp-stdio` mode retries an
unreachable embedder with bounded backoff (up to 5 seconds total) before giving up. If the
embedder still isn't reachable once that window elapses, the process does **not** exit —
it starts in a degraded state without opening the database. Call `knowledge_status` from
within the MCP session to discover this: it reports `degraded: true` with
`reason: "embedder_unreachable_at_startup"` and an empty `recovery_available` list (no
`knowledge_recover` strategy can safely run in this state, since no embedding dimension
was ever established — restart the process once the embedder is reachable). If the
embedder becomes reachable during the retry window, startup proceeds normally with no
degraded state at all. A hand-started socket-service process keeps the fail-fast behaviour
above unchanged either way — this retry-and-degrade behaviour applies only to standalone
`--mcp-stdio`.

Start the embedder sidecar **before** starting the `liminis-context-graph` binary.
Without it, the embedding-dependent IPC methods fail immediately with an embedding error:
`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`,
`knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_reprocess_entity_types`, and
`knowledge_canonicalize_relations` (its ontology-description fallback embeds each residual edge's
`fact`). Read-only methods that do not call the embedder (`health_check`, `knowledge_status`,
`knowledge_list_entities`, `knowledge_get_episodes`) work without the sidecar.

### macOS: Swift CoreML sidecar (default)

The repository ships a Swift CoreML sidecar at
[`native/local-inference/`](https://github.com/verveguy/liminis-context-graph/tree/main/native/local-inference)
that serves OpenAI-compatible `/v1/embeddings` (BGE-base-en-v1.5) and `/v1/chat/completions`
(Apple Foundation Models) over UDS at `/tmp/liminis-inference.sock` — fully local inference for
embedding, and a fully local option for extraction: no API key, no network. macOS 26+ and Xcode
command-line tools are required. See
[`native/local-inference/README.md`](https://github.com/verveguy/liminis-context-graph/blob/main/native/local-inference/README.md)
for build and run instructions.

`liminis-context-graph` discovers the sidecar's default UDS socket automatically for embedding —
start the sidecar first, then start the binary. **This default path is not shared with the
Electron `liminis` app.** The app does not use `/tmp/liminis-inference.sock` at all — it
launches its own sidecar process bound to a separate, per-workspace socket at
`<workspaceRoot>/.liminis/local-inference.sock`. A bare `liminis-context-graph` binary started
by hand cannot reach the app's sidecar (or vice versa); they are two different sockets serving
two different processes. Extraction does **not** auto-detect this socket
(see [Extractor: local or hosted](#extractor-local-or-hosted)): the sidecar's Foundation Models
backend is not recommended for extraction quality, so using it there requires the explicit
`--extractor-uds /tmp/liminis-inference.sock` flag rather than happening by default.

The sidecar's own `LOCAL_INFERENCE_MODE` environment variable (`embeddings` | `completions` |
`both`, default `both`) selects which of its own two endpoints it serves — see
[`native/local-inference/README.md`](https://github.com/verveguy/liminis-context-graph/blob/main/native/local-inference/README.md#selecting-a-mode)
for the full table. Running `LOCAL_INFERENCE_MODE=completions` needs no CoreML model setup at
all, since it serves only `/v1/chat/completions`. This mode selection is independent of, and does
**not** change, the extraction-routing behavior above: Foundation-Models-backed completions are
still not a drop-in substitute for the extraction backend, and nothing should route
`--extractor-uds` at this sidecar on the assumption that enabling completions mode makes it one.

### HTTP transport (CI / Linux / custom embedders / hosted providers)

For environments without the Swift sidecar, pass `--embedder-http` pointing at an endpoint
speaking the same `{input, model}` request / `{data: [{embedding}]}` response wire shape
`OaiEmbedder` sends and expects. OpenAI's own `/v1/embeddings` is the concrete, verified case:

```bash
liminis-context-graph --embedder-http https://api.openai.com/v1/embeddings
```

A local unauthenticated server (Ollama, llama.cpp, LM Studio, text-embeddings-inference,
Infinity, vLLM) works exactly as before — set no key and none is sent. This does **not** make
every self-described "OpenAI-compatible" provider a drop-in target: only Bearer-token auth
against this specific wire shape is supported, not divergent request/response formats, and no
other third-party provider's compatibility is verified here.

See [ADR 0006](adr/0006-embedder-http-contract.md) and
[ADR 0016](adr/0016-oai-embedding-contract-uds-transport.md) for the wire contract
specification and transport decision record.

**Authenticating against a hosted endpoint.** Set `LCG_EMBEDDING_API_KEY` (or reuse an
already-exported `OPENAI_API_KEY`) and the binary sends `Authorization: Bearer <key>` on every
HTTP embedder request — both the startup probe and steady-state `embed()` calls:

```bash
export LCG_EMBEDDING_API_KEY=sk-...
liminis-context-graph --embedder-http https://api.openai.com/v1/embeddings
```

If the endpoint rejects the credential (HTTP 401/403), startup fails with a message identifying
it as an authentication problem and naming the relevant env vars — this is always fatal and,
unlike a generic probe failure, is **not** bypassable via `LCG_EMBEDDING_DIM`: a dimension
override cannot fix a rejected credential. The key itself never appears in the startup log line,
any other log output, or telemetry; if the configured URL embeds Basic-auth-style userinfo
(`user:pass@host`), that component is redacted before being echoed anywhere. See
[ADR 0497](adr/0497-embedder-http-bearer-auth.md).

### MCP client config recipes

An MCP client (Claude Desktop, Claude Code, or any other MCP client) launches
`liminis-context-graph` as a subprocess by specifying exactly `command`, `args`, `env`, and
`cwd` in an `mcpServers` entry — the same `mcpServers` JSON shape already used in
[Getting Started](getting-started.md#talk-to-it-over-mcp) and the
[IPC & MCP Reference](ipc-mcp-reference.md#example-mcp-client-config). Those CLI flags and env
vars are read by exactly the same resolution logic described above, regardless of what
launched the process — there is no MCP-specific configuration surface. The five recipes below
translate that resolution order into copy-pasteable client config for each embedder backend.

**Default macOS sidecar — no `env` block needed.** The default UDS socket
(`/tmp/liminis-inference.sock`) is auto-discovered, so an `mcpServers` entry for the bundled
Swift CoreML sidecar needs no embedder-related `env` entry at all:

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

This applies to macOS only: the default-UDS auto-discovery tier doesn't exist on non-Unix
platforms at all (see the environment variable table above), and the sidecar binary itself
(Swift/CoreML) only runs on macOS regardless.

**Local OpenAI-compatible server (Ollama, LM Studio, text-embeddings-inference, vLLM).** Point
`LCG_EMBEDDING_URL` at the local server and set a matching `LCG_EMBEDDING_MODEL` — a mismatched
model name produces a probe failure at startup rather than a clear error, so get it right:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your-workspace",
      "env": {
        "LCG_EMBEDDING_URL": "http://127.0.0.1:11434/v1/embeddings",
        "LCG_EMBEDDING_MODEL": "nomic-embed-text"
      }
    }
  }
}
```

LM Studio, text-embeddings-inference, and vLLM follow the identical shape — only the port and
`LCG_EMBEDDING_MODEL` change:

| Server | Typical `LCG_EMBEDDING_URL` | Example `LCG_EMBEDDING_MODEL` |
|---|---|---|
| Ollama | `http://127.0.0.1:11434/v1/embeddings` | `nomic-embed-text` |
| LM Studio | `http://127.0.0.1:1234/v1/embeddings` | whatever model is currently loaded in LM Studio |
| text-embeddings-inference | `http://127.0.0.1:8080/v1/embeddings` | the model the server was launched with (`--model-id`) |
| vLLM | `http://127.0.0.1:8000/v1/embeddings` | the model vLLM was launched with (`--model`) |

Worth knowing (not necessarily a mistake): if the macOS default-UDS sidecar is also running on
the same machine, it silently wins over this `env` block per the resolution order described
above under [Embedder sidecar](#embedder-sidecar) — stop the sidecar, or pass an explicit
`--embedder-uds`/`--embedder-http` flag (see the next recipe), to make this `env` block take
effect.

**Hosted OpenAI-compatible provider with an API key.** Set `LCG_EMBEDDING_URL` (or pass
`--embedder-http` in `args`), `LCG_EMBEDDING_API_KEY` (falls back to `OPENAI_API_KEY` if
unset — see the environment variable table above), and a matching `LCG_EMBEDDING_MODEL`:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your-workspace",
      "env": {
        "LCG_EMBEDDING_URL": "https://api.openai.com/v1/embeddings",
        "LCG_EMBEDDING_API_KEY": "sk-...",
        "LCG_EMBEDDING_MODEL": "text-embedding-3-small"
      }
    }
  }
}
```

Caution: an MCP client config file is not a secrets manager — the key above is stored in
plaintext wherever that config file lives on disk, the same as any other credential placed in
an `env` block. Treat the config file itself as sensitive.

**Explicit non-default UDS path.** If your sidecar-compatible embedder listens on a Unix
domain socket at a path other than `/tmp/liminis-inference.sock` — or you want to force UDS
selection even when a different sidecar occupies the default path — pass `--embedder-uds` as a
CLI flag in `args`, not as an `env` var:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write", "--embedder-uds", "/tmp/my-embedder.sock"],
      "cwd": "/path/to/your-workspace",
      "env": {
        "LCG_EMBEDDING_MODEL": "bge-base-en-v1.5"
      }
    }
  }
}
```

No `env` block is required unless `LCG_EMBEDDING_MODEL` needs to differ from the default
`bge-base-en-v1.5`. An explicit `--embedder-uds`/`--embedder-http` flag always wins over an
embedder `env` var per the resolution order — worth a callout since it's a plausible source of
"I set the env var and it's still not being used" confusion, distinct from the sidecar-silent-
win case above: here it's *your own* flag taking precedence, not the sidecar's.

#### Switching an existing workspace's embedder

The following applies only when *switching* the embedder for a workspace that already has a
`.lcg/` database with content in it. A fresh workspace with no pre-existing `.lcg/` has nothing
to re-ingest, so none of this applies to first-time setup.

**A dimension change is not a live config change.** `embedding_dim` is baked into the LadybugDB
schema as a fixed-width vector column at database-creation time. Switching, say, from the
macOS sidecar (768-dim, BGE-base-en-v1.5) to `text-embedding-3-small` (1536-dim) against an
existing `.lcg/` database does not resize those columns. `knowledge_rebuild_from_wal` alone is
**not** sufficient here: it recomputes vectors against the already-created schema and, when a
recomputed vector's length doesn't match the already-stored one, it keeps the old
wrong-dimension vector and counts the row under `embeddings_recompute_failed` rather than
fixing it (see [Operations](operations.md#knowledge_status-health-fields)). The correct remedy is a
full re-ingest, or `knowledge_recover` with `{"strategy": "rebuild_from_workspace_wal"}`, which
drops and recreates the database (including schema, at the new embedder's dimension) before
replaying every group's WAL back in.

**`LCG_EMBEDDING_DIM` does not help here.** As described above under
[Embedder sidecar](#embedder-sidecar), that variable only overrides a non-transport,
non-auth *probe* failure at startup — it cannot resolve a genuine dimension mismatch against
already-stored vectors, nor an unreachable or unauthenticated embedder.

**Pin `cwd` to control where `.lcg/` lands.** Every recipe above includes a `cwd` field for
this reason: an MCP client (not a shell you control) decides the launched process's working
directory, and `.lcg/` is created relative to it. Omitting `cwd` leaves `.lcg/` wherever the
client process's own default working directory happens to be — for most MCP clients, that is
not your project directory.

## Extractor: local or hosted

**An extraction provider is required only for extraction operations, not for startup.** A
deployment with no `ANTHROPIC_API_KEY`, `--extractor-uds`/`--extractor-http`, or
`LCG_EXTRACTION_URL` configured starts normally and serves every read-only method
(`knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`,
`knowledge_status`, etc.) as well as `knowledge_rebuild_from_wal` — none of these touch the
extractor. Only `knowledge_process_chunk`, `knowledge_add_episode`,
`knowledge_reprocess_entity_types`, and `knowledge_reprocess_relation_types` require a
configured provider; each returns a clear, actionable error naming what to configure if called
without one, and the process keeps running and serving reads afterward (see
[ADR 0331](adr/0331-lazy-extraction-provider-validation.md)). This makes read-only deployments —
an MCP client that only queries and rebuilds from a published WAL, for example — a fully
supported configuration with no fake credential or placeholder endpoint required.

Entity/relationship extraction runs against one of two providers, selected with the following
precedence (highest first):

1. **Explicit CLI flag** — `--extractor-uds <path>` or `--extractor-http <url>` always selects the
   local OpenAI-compatible adapter, regardless of whether `ANTHROPIC_API_KEY` is set:

   ```bash
   liminis-context-graph --extractor-uds /tmp/liminis-inference.sock              # Unix domain socket
   liminis-context-graph --extractor-http http://127.0.0.1:8765/v1/chat/completions  # HTTP
   ```

   (`--extractor-uds` and `--extractor-http` are mutually exclusive.)
2. **`ANTHROPIC_API_KEY` set, no explicit flag** — extraction uses the hosted Anthropic API,
   unchanged from prior versions. A reachable local sidecar never silently redirects this traffic.
3. **Neither of the above** — `LCG_EXTRACTION_URL` (HTTP), if set, else no extraction provider is
   configured: the binary still starts, and extraction-dependent calls fail with a clear error
   identifying the missing configuration when actually invoked (see above).

Unlike the embedder, extraction has **no default-socket auto-detection tier**: a running sidecar
alone never selects it for extraction, even with no `ANTHROPIC_API_KEY` set. This is deliberate,
not an oversight. Extraction requires an explicit signal — a CLI flag or `LCG_EXTRACTION_URL` —
before it will use a local endpoint at all.

> **The bundled sidecar's model is not recommended for extraction quality.** Prior evaluation
> found Apple Foundation Models' context window and capability insufficient for reliable
> entity/relationship extraction (see
> [Extraction-Quality Evaluation](extraction-quality-evaluation.md) for the full evaluation,
> methodology, and model rankings). All figures there describe **freeform extraction only** — the same
> corpus/backends run under an ontology (`Open`/`Strict`) are not measured there; see
> [Testing & Evaluation](testing-and-evaluation.md#running-under-an-ontology-openstrict) if you
> want to produce those figures yourself. For local extraction that meets a reasonable quality
> bar, run a model such as `qwen3.6-27b` behind an OpenAI-compatible server (e.g. `mlx_lm.server`)
> and point `--extractor-http`/`--extractor-uds` at it, or set `ANTHROPIC_API_KEY` to use the
> hosted baseline. The bundled sidecar's `/v1/chat/completions` route is still reachable for
> extraction if you want it anyway — pass `--extractor-uds /tmp/liminis-inference.sock`
> explicitly — the engine just never picks it for you.

The resolved choice is reported in a startup log line: `extractor: provider=...,
transport=..., endpoint=...` — `provider` is `anthropic` or `local` depending on which path was
selected. Unlike the embedder, extraction performs no live reachability probe at startup —
Foundation Models' on-device warm-up can be slow, and there is no response shape to auto-detect —
so an unreachable local endpoint surfaces as an error on the first extraction call rather than at
startup. See [ADR 0041](adr/0041-local-openai-compatible-extraction-adapter.md) for the full
design, including why the local adapter uses `response_format: json_object` rather than
function-calling (the bundled sidecar has no `tools`/`tool_choice` support).
