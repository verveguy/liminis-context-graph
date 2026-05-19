# Issue #6 Spec: Telemetry and Operator Visibility

**Issue**: #6  
**Created**: 2026-05-19  
**Status**: Specified  
**Maps to**: User Story 6 (Telemetry and operator visibility, P3)  
**Blocked by**: #2 (IPC parity), #3 (WAL parity)

---

## Goal

Make the Rust service self-describing at runtime: emit a structured stream of events covering per-call timing, per-role LLM token usage, fallback transitions, and WAL throughput so the desktop app (and any other consumer) can surface operational metrics without ad-hoc instrumentation. The event schema is first-class library API, not binary-only scaffolding.

---

## User Stories

### Story 1 — Operator can observe per-call timing for every IPC method (P1)

Every IPC method (ingest, search, retrieve, admin) produces a timing event capturing how long the call took, whether it succeeded, and which workspace/group it touched. A monitoring script or the desktop app can consume these events from stderr (or a dedicated socket) without modifying the service binary.

**Why P1**: Timing visibility is the most widely-needed metric and the simplest to emit. It gates all other observability.

**Independent test**: Drive a known workload (5 ingest + 5 search calls) against the service; confirm 10 `ipc_call` events appear on the telemetry stream with correct `method`, `duration_ms`, and `success` fields.

**Acceptance scenarios**:

1. **Given** a running service, **When** a client calls any IPC method, **Then** an `ipc_call` event appears on the telemetry stream with `method`, `duration_ms` (wall-clock), `success` (bool), `group_id`, and an RFC 3339 `timestamp`.
2. **Given** an IPC call that returns an error, **When** the error is returned to the caller, **Then** the `ipc_call` event has `"success": false` and an `error_kind` field with a short error category string.
3. **Given** a workload of N ingest + M search calls, **When** the service runs, **Then** the telemetry stream contains exactly N+M `ipc_call` events (no events are dropped or duplicated).

---

### Story 2 — Operator can observe per-role token usage and estimated cost (P1)

Every LLM call (extraction, dedup) emits a token-usage event with raw token counts per category (input, output, cache-read, cache-write) and a best-effort USD cost estimate. This makes the 2026-04-30 caching audit replayable without ad-hoc instrumentation.

**Why P1**: Token cost is the primary operating cost signal; it is what the caching audit was measuring. Without it the service is financially opaque.

**Independent test**: Ingest one episode with a known extraction model; verify a `token_usage` event appears with `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, and `estimated_cost_usd` all present and non-negative.

**Acceptance scenarios**:

1. **Given** an extraction call, **When** the Anthropic API returns a response, **Then** a `token_usage` event appears with `role: "extraction"`, `model`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, and `estimated_cost_usd`.
2. **Given** a dedup call, **When** the dedup model returns a response, **Then** a `token_usage` event appears with `role: "dedup"` and the appropriate token counts (fields that are not applicable to the model are omitted or zero).
3. **Given** the 2026-04-30 caching audit replayed against the Rust service, **When** the audit script runs, **Then** `cache_read_tokens` and `cache_creation_tokens` from `token_usage` events are sufficient to reproduce the cache-hit-rate calculations the audit performed.

---

### Story 3 — Operator can observe LLM fallback transitions (P2)

When the primary LLM for a role fails and the service falls back to the secondary, a `llm_fallback` event is emitted describing which model was primary, which model took over, and why the primary was rejected. This gives operators a signal that their primary configuration is degraded.

**Why P2**: Fallback masking a persistent primary failure is a silent cost/quality risk. Visibility into when and why fallbacks fire is essential for diagnosing misconfiguration.

**Independent test**: Configure the primary extraction model with an invalid API key; ingest one episode; verify a `llm_fallback` event appears with `primary_model`, `fallback_model`, and `error_kind`.

**Acceptance scenarios**:

1. **Given** a misconfigured primary extraction model, **When** the first extraction call fails, **Then** a `llm_fallback` event appears with `role: "extraction"`, `primary_model`, `fallback_model`, `error_kind`, and `error_message`.
2. **Given** a fallback event, **When** it is inspected, **Then** the subsequent `token_usage` event for the same call references the fallback model, not the primary.
3. **Given** the primary model recovers (next call succeeds), **When** a call succeeds on the primary, **Then** no `llm_fallback` event is emitted for that call.

---

### Story 4 — Operator can observe WAL append and replay throughput (P2)

WAL operations emit timing events so cold-boot replay performance and steady-state append throughput are measurable without a profiler. Replay emits a summary event at completion; append emits a per-operation event.

**Why P2**: WAL throughput is a constitution-level invariant (SC-003: replay ≥ 3× Python baseline). First-class telemetry is how that invariant gets monitored in production.

**Independent test**: Boot the service against a WAL with ≥ 100 lines; verify a `wal_replay_complete` event appears with `total_lines`, `replayed_lines`, `skipped_lines`, `duration_ms`, and `throughput_lines_per_sec`.

**Acceptance scenarios**:

1. **Given** a cold boot against an existing WAL, **When** replay completes, **Then** a `wal_replay_complete` event appears with `total_lines`, `replayed_lines`, `skipped_lines`, `error_lines`, `duration_ms`, and `throughput_lines_per_sec`.
2. **Given** a write transaction, **When** the WAL line is appended, **Then** a `wal_append` event appears with `op_type`, `bytes`, and `duration_ms`.
3. **Given** WAL replay that encounters an unknown op type, **When** the line is skipped with a warning, **Then** the `wal_replay_complete` event reflects the skip in `skipped_lines` and `error_lines` counts.

---

### Story 5 — Telemetry is documented in `docs/telemetry.md` (P2)

Every event type, every field name, every field type, and every allowed enum value is documented in `docs/telemetry.md`. A consumer can implement a parser from the doc alone, without reading the service source.

**Why P2**: Without schema documentation, consumers must reverse-engineer the binary, making the event format an implicit ABI.

**Independent test**: A reviewer can cross-reference every field observed in a live telemetry stream against a definition in `docs/telemetry.md` without finding an undocumented field.

**Acceptance scenarios**:

1. **Given** the telemetry stream produced by acceptance scenario 1 in Story 1, **When** every event field is looked up in `docs/telemetry.md`, **Then** every field has a name, type, and description entry.
2. **Given** `docs/telemetry.md`, **When** it is inspected, **Then** it includes a section per event type (at minimum: `ipc_call`, `token_usage`, `llm_fallback`, `wal_append`, `wal_replay_complete`) and a transport configuration section.

---

## Event Schema

The following event types constitute the minimum telemetry surface. All events are emitted as JSONL (one compact JSON object per line).

> **Implementation note**: the plan stage chose Unix-epoch milliseconds (`ts_ms`, u64) over RFC 3339 strings for the timestamp field to avoid a `chrono`/`time` dependency on the hot path. The discriminant tag serialises as `type` (matching `#[serde(tag = "type")]`), not `event`. `docs/telemetry.md` is the authoritative consumer-facing reference.

### `ipc_call`

| Field | Type | Description |
|---|---|---|
| `type` | `"ipc_call"` | Discriminant |
| `ts_ms` | u64 | Unix epoch timestamp in milliseconds at call completion |
| `method` | string | IPC method name (e.g. `"knowledge_add_episode"`) |
| `request_id` | any | JSON-RPC request `id` value as-is |
| `duration_ms` | u64 | Wall-clock duration of the call in milliseconds |
| `success` | bool | `true` if the call returned without error |

### `token_usage`

| Field | Type | Description |
|---|---|---|
| `type` | `"token_usage"` | Discriminant |
| `ts_ms` | u64 | Unix epoch timestamp in milliseconds at response receipt |
| `role` | string | Which LLM use-case produced these tokens (`"extraction"`, future: `"dedup"`) |
| `model` | string | Model identifier as reported by the provider |
| `input_tokens` | u64 | Prompt tokens billed |
| `output_tokens` | u64 | Completion tokens billed |
| `cache_read_tokens` | u64 | Tokens read from the prompt cache (0 if not applicable) |
| `cache_creation_tokens` | u64 | Tokens written to the prompt cache (0 if not applicable) |
| `estimated_cost_usd` | f64 or null | Best-effort USD cost estimate; null if model not in pricing table |

### `llm_fallback`

| Field | Type | Description |
|---|---|---|
| `type` | `"llm_fallback"` | Discriminant |
| `ts_ms` | u64 | Unix epoch timestamp in milliseconds at transition |
| `role` | string | Which LLM role fell back |
| `primary_model` | string | Model that failed |
| `fallback_model` | string | Model that will handle the request |
| `error_reason` | string | Reason the primary was unavailable (pending FR-009) |

### `wal_append`

| Field | Type | Description |
|---|---|---|
| `type` | `"wal_append"` | Discriminant |
| `ts_ms` | u64 | Unix epoch timestamp in milliseconds at append completion |
| `duration_us` | u64 | Time to append the WAL entry, in microseconds |
| `bytes` | integer | Bytes written (including newline) |

### `wal_replay_complete`

| Field | Type | Description |
|---|---|---|
| `type` | `"wal_replay_complete"` | Discriminant |
| `ts_ms` | u64 | Unix epoch timestamp in milliseconds at replay completion |
| `episodes_replayed` | u64 | Episodes replayed from the WAL |
| `duration_ms` | u64 | Total replay wall-clock duration in milliseconds |
| `throughput_eps` | f64 | Episodes replayed per second |

---

## Scope

### In scope

- A `TelemetrySink` trait in `liminis-graph-core` (library API per Principle II); the binary wires it to stderr or a UNIX socket.
- Emit all five event types defined above.
- Transport: stderr JSONL by default; opt-in dedicated UNIX socket via `LIMINIS_TELEMETRY_SOCKET` env var (path to socket file the service will create).
- `docs/telemetry.md` documenting every event type, field, type, and transport configuration.
- Pricing table for known Anthropic models (Sonnet, Haiku) hardcoded with a `LIMINIS_LLM_COST_TABLE_PATH` override for operator-supplied JSON.
- `[HOT]` tag applies to `wal_append` emission (per-IPC-call on WAL write path) and `ipc_call` emission.

### Out of scope

- OpenTelemetry export (explicitly excluded by constitution and issue).
- Hosted dashboards.
- Sampling / filtering (100% emission only in v1).
- Metrics aggregation beyond the `wal_replay_complete` summary (no rolling histograms in-process).
- Embedding call telemetry (embedding is out-of-process; adapter telemetry is adapter-internal).

---

## Requirements

### Functional Requirements

- **FR-001**: `liminis-graph-core` MUST expose a `TelemetrySink` trait (or equivalent) that the library calls to emit events; the binary MUST be the component that wires the sink to a transport.
- **FR-002**: The service MUST emit an `ipc_call` event for every IPC method call, regardless of success or failure.
- **FR-003**: The service MUST emit a `token_usage` event for every completed LLM call (extraction and dedup roles).
- **FR-004**: The service MUST emit a `llm_fallback` event whenever the primary model for any role is bypassed in favour of the fallback.
- **FR-005**: The service MUST emit a `wal_append` event for every WAL line written.
- **FR-006**: The service MUST emit a `wal_replay_complete` event at the end of every WAL replay operation.
- **FR-007**: All events MUST be serialised as compact JSON objects, one per line (JSONL), with no trailing whitespace other than the newline terminator.
- **FR-008**: By default, the service MUST write telemetry to stderr; when `LIMINIS_TELEMETRY_SOCKET` is set to a path, the service MUST create a UNIX socket at that path and write telemetry exclusively there instead.
- **FR-009**: `docs/telemetry.md` MUST be committed alongside the implementation and MUST document every event type, field, type, allowed values, and transport configuration option.
- **FR-010**: Telemetry emission MUST NOT regress the performance budgets defined in the constitution (p95 search ≤ 500 ms, WAL replay ≥ 3× Python baseline). `[HOT]` tasks cover the `ipc_call` and `wal_append` hot paths.
- **FR-011**: The `estimated_cost_usd` field MUST be computed for known Anthropic models (Sonnet, Haiku); for unknown models it MUST be emitted as `0` with a one-time startup warning logged to stderr.
- **FR-012**: An operator MUST be able to supply a JSON pricing override file via `LIMINIS_LLM_COST_TABLE_PATH`; if the file is absent or malformed the service MUST fall back to hardcoded prices and log a warning.

### Key Entities

- **`TelemetrySink`**: A trait in `liminis-graph-core` with a method `emit(event: &TelemetryEvent)`. The default no-op implementation allows library consumers to opt out. The binary provides a stderr or socket implementation.
- **`TelemetryEvent`**: An enum (one variant per event type) serialisable to the JSONL wire format defined in the Event Schema section above.
- **Pricing table**: A JSON map of `{ "model-id": { "input_per_mtok": float, "output_per_mtok": float, "cache_read_per_mtok": float, "cache_write_per_mtok": float } }`.

---

## Success Criteria

- **SC-001**: Given a known workload of N ingest + M search calls, the telemetry stream contains exactly N+M `ipc_call` events with correct field shapes.
- **SC-002**: Given a workload triggering a primary→fallback transition, a `llm_fallback` event appears with `primary_model`, `fallback_model`, and `error_kind` all populated.
- **SC-003**: Given a cold-boot WAL replay of ≥ 100 lines, a `wal_replay_complete` event appears with `throughput_lines_per_sec` computable from its fields.
- **SC-004**: Given the 2026-04-30 caching audit replayed against the Rust service, all cache metrics (`cache_read_tokens`, `cache_creation_tokens` per call) are available as `token_usage` fields; the audit script requires zero ad-hoc instrumentation.
- **SC-005**: Telemetry emission does not push the p95 search latency above 500 ms or WAL replay throughput below the 3× Python baseline, as measured by the CI bench suite.
- **SC-006**: `docs/telemetry.md` is merged alongside the implementation and passes a completeness check (every field in every emitted event has a corresponding entry in the doc).

---

## Assumptions

- The Anthropic API response includes `usage.cache_creation_input_tokens` and `usage.cache_read_input_tokens` fields when prompt caching is active; the Rust HTTP client surfaces these in the `token_usage` event.
- Dedup model token counts may not be available if the adapter is a subprocess shim; in that case `token_usage` events for dedup are omitted and a one-time startup note is logged.
- 100% event emission is acceptable for v1 (no sampling); if benchmarks show telemetry overhead is non-trivial, a sampling rate config can be added in a follow-up.
- stderr is the default transport because it requires zero socket lifecycle management; the socket transport exists for consumers (desktop app) that prefer structured capture over log tailing.
- The embedding adapter is out-of-process; embedding call telemetry is the adapter's responsibility and is out of scope for this issue.

---

## Out of Scope

- OpenTelemetry export.
- Hosted or cloud dashboards.
- Embedding-layer telemetry.
- In-process rolling histograms or percentile aggregation.
- Metrics filtering or sampling.
- Event schema versioning beyond a `schema_version` field reserved for future use (not required in v1 events).
