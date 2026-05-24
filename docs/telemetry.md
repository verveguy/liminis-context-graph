# Telemetry

The `liminis-graph` service emits structured JSON Lines (JSONL) telemetry events that give operators per-call timing, token usage with estimated cost, and WAL throughput counters.

## Transport

**Default**: Events are written to **stderr**, one JSON object per line.

```
{"type":"ipc_call","ts_ms":1716100000000,"method":"knowledge_find_entities","request_id":1,"duration_ms":42,"success":true}
```

**Future**: Set `LIMINIS_TELEMETRY_SOCKET` to a UNIX socket path to stream events there instead of (or in addition to) stderr. This transport is not yet implemented.

To capture events from the default transport:

```sh
./liminis-graph 2> telemetry.jsonl
```

## Event Types

All events share two common fields:

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Discriminant identifying the event kind (see table below) |
| `ts_ms` | u64 | Unix epoch timestamp in milliseconds when the event was emitted |

### `ipc_call`

Emitted after every IPC request completes, from `handlers::dispatch()`. This is a **hot-path event** — it fires on every request.

| Field | Type | Description |
|-------|------|-------------|
| `method` | string | JSON-RPC method name (e.g. `knowledge_add_episode`) |
| `request_id` | any | JSON-RPC request `id` value as-is |
| `duration_ms` | u64 | Wall-clock time from request receipt to response, in milliseconds |
| `success` | bool | `true` if the handler returned `Ok`, `false` for any error |

Example:
```json
{"type":"ipc_call","ts_ms":1716100000000,"method":"knowledge_add_episode","request_id":1,"duration_ms":42,"success":true}
```

### `token_usage`

Emitted after every successful Anthropic API call from `extractor.rs`. This is a **hot-path event** for `knowledge_add_episode` calls.

| Field | Type | Description |
|-------|------|-------------|
| `role` | string | Which LLM use-case produced these tokens (`"extraction"`, future: `"dedup"`) |
| `model` | string | Anthropic model identifier (e.g. `claude-haiku-4-5-20251001`) |
| `input_tokens` | u64 | Input tokens billed by the API |
| `output_tokens` | u64 | Output tokens billed by the API |
| `cache_read_tokens` | u64 | Tokens served from the prompt cache (cheaper rate) |
| `cache_creation_tokens` | u64 | Tokens written into the prompt cache |
| `estimated_cost_usd` | f64 or null | Estimated cost in USD, or `null` if the model is not in the pricing table |

Example:
```json
{"type":"token_usage","ts_ms":1716100000001,"role":"extraction","model":"claude-haiku-4-5-20251001","input_tokens":512,"output_tokens":128,"cache_read_tokens":384,"cache_creation_tokens":0,"estimated_cost_usd":0.000512}
```

### `llm_fallback`

Emitted when the primary LLM is unavailable and extraction falls back to a secondary model. **Not yet emitted** — pending FR-009 (primary→fallback chain implementation).

| Field | Type | Description |
|-------|------|-------------|
| `role` | string | Which LLM use-case triggered the fallback |
| `primary_model` | string | Model that failed |
| `fallback_model` | string | Model being used instead |
| `error_reason` | string | Reason the primary model was unavailable (e.g. `"rate_limit_exceeded"`) |

Example:
```json
{"type":"llm_fallback","ts_ms":1716100000002,"role":"extraction","primary_model":"claude-sonnet-4-6","fallback_model":"claude-haiku-4-5-20251001","error_reason":"rate_limit_exceeded"}
```

### `wal_append`

Emitted after each WAL entry is written. **Not yet emitted** — pending issue #3 (WAL implementation).

| Field | Type | Description |
|-------|------|-------------|
| `duration_us` | u64 | Time to append the WAL entry, in microseconds |
| `bytes` | integer | Size of the appended WAL entry in bytes |

Example:
```json
{"type":"wal_append","ts_ms":1716100000003,"duration_us":180,"bytes":1024}
```

### `service_state`

Emitted when the daemon enters or exits a degraded state. Produced on startup if the lbug DB fails to open with a recoverable error, and again after successful in-process recovery.

| Field | Type | Description |
|-------|------|-------------|
| `state` | string | `"degraded"` or `"healthy"` |
| `reason` | string or absent | Machine-readable reason code (e.g. `"lbug_wal_corrupt"`). Present when `state = "degraded"`. |
| `detail` | string or absent | Human-readable detail, typically the lbug error string. Present when `state = "degraded"` |

Degraded example (emitted at startup when lbug WAL is corrupt):
```json
{"type":"service_state","ts_ms":1716523200000,"state":"degraded","reason":"lbug_wal_corrupt","detail":"database error: Lbug(Runtime exception: Corrupted wal file. Read out invalid WAL record type.)"}
```

Healthy example (emitted after successful `knowledge_recover`):
```json
{"type":"service_state","ts_ms":1716523260000,"state":"healthy"}
```

The renderer uses this event to update the recovery UI state without polling `knowledge_status`.

### `wal_replay_complete`

Emitted once when WAL replay finishes at startup. **Not yet emitted** — pending issue #3 (WAL implementation).

| Field | Type | Description |
|-------|------|-------------|
| `episodes_replayed` | u64 | Number of episodes replayed from the WAL |
| `duration_ms` | u64 | Total replay wall-clock time in milliseconds |
| `throughput_eps` | f64 | Episodes replayed per second |

Example:
```json
{"type":"wal_replay_complete","ts_ms":1716100000004,"episodes_replayed":42,"duration_ms":380,"throughput_eps":110.5}
```

---

## Sample Output

A complete session ingesting one episode and running one search:

```jsonl
{"type":"ipc_call","ts_ms":1716100000000,"method":"knowledge_build_indices","request_id":1,"duration_ms":12,"success":true}
{"type":"token_usage","ts_ms":1716100000100,"role":"extraction","model":"claude-haiku-4-5-20251001","input_tokens":512,"output_tokens":128,"cache_read_tokens":384,"cache_creation_tokens":0,"estimated_cost_usd":0.000512}
{"type":"ipc_call","ts_ms":1716100000150,"method":"knowledge_add_episode","request_id":2,"duration_ms":320,"success":true}
{"type":"ipc_call","ts_ms":1716100000500,"method":"knowledge_find_entities","request_id":3,"duration_ms":18,"success":true}
```

---

## Pricing Table

Token cost estimates use the compiled-in pricing table at `assets/llm_pricing.json`. To override at runtime without recompiling:

```sh
LIMINIS_LLM_COST_TABLE_PATH=/path/to/my_pricing.json ./liminis-graph
```

The JSON schema matches the built-in table:

```json
{
  "claude-haiku-4-5-20251001": {
    "input_per_mtok": 0.80,
    "output_per_mtok": 4.00,
    "cache_read_per_mtok": 0.08,
    "cache_creation_per_mtok": 1.00
  }
}
```

Rates are in USD per million tokens. Models not present in the table produce `"estimated_cost_usd": null`.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LIMINIS_LLM_COST_TABLE_PATH` | *(built-in)* | Path to a JSON pricing table; overrides the compiled-in defaults |
| `LIMINIS_TELEMETRY_SOCKET` | *(unset)* | UNIX socket path for telemetry output (**not yet implemented**) |
