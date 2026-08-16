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
| `LCG_EMBEDDING_DIM` | No | Embedding dimension override if probe fails at startup (default: auto-detected via probe) |
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

That's 28 variables as of this page's writing, covering every `env::var`/`lcg_env_var` call site under `crates/*/src`.

**Deprecated `GRAPHITI_*` aliases.** Every `LCG_*` variable above that predates the rename also
accepts its old `GRAPHITI_*` spelling — `GRAPHITI_SOCKET_PATH`, `GRAPHITI_DB_PATH`,
`GRAPHITI_EMBEDDING_URL`, `GRAPHITI_EMBEDDING_MODEL`, `GRAPHITI_EMBEDDING_DIM`,
`GRAPHITI_EXTRACTION_LLM`, `GRAPHITI_DEDUP_LLM`, `GRAPHITI_DEDUP_ADAPTER_URL`, and
`GRAPHITI_WAL_DIR`. Using one logs `DEPRECATED: env var <old> is deprecated; rename to <new>` at
startup. They are honoured for now; prefer the `LCG_*` names.

## Embedder sidecar

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
exits with an error rather than failing silently on the first embed request.

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
start the sidecar first, then start the binary. Extraction does **not** auto-detect this socket
(see [Extractor: local or hosted](#extractor-local-or-hosted)): the sidecar's Foundation Models
backend is not recommended for extraction quality, so using it there requires the explicit
`--extractor-uds /tmp/liminis-inference.sock` flag rather than happening by default.

### HTTP transport (CI / Linux / custom embedders)

For environments without the Swift sidecar, pass `--embedder-http` pointing at any
OpenAI-compatible embedding endpoint (local or remote):

```bash
liminis-context-graph --embedder-http http://127.0.0.1:8765/v1/embeddings
```

See [ADR 0006](adr/0006-embedder-http-contract.md) and
[ADR 0016](adr/0016-oai-embedding-contract-uds-transport.md) for the wire contract
specification and transport decision record.

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
