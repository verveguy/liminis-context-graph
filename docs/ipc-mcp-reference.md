---
layout: default
title: IPC & MCP Reference
---

# IPC & MCP Reference

`liminis-context-graph` serves the same graph over **two transport surfaces**, both routed
through the same core dispatch in
[`crates/core/src/handlers.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/core/src/handlers.rs) —
no graph logic is duplicated between them:

- **JSON-RPC 2.0 over a Unix domain socket** (default). Newline-delimited requests/responses over `.lcg/service.sock`.
- **[Model Context Protocol](https://modelcontextprotocol.io) over stdin/stdout** (`--mcp-stdio`). Any MCP client — Claude Code, Claude Desktop, other agents — can query and mutate the graph directly.

## IPC methods (44)

The socket dispatch handles **44 methods**: 43 `knowledge_*` methods plus `health_check`.
`health_check` is the one method not prefixed `knowledge_*`, and it is the reason the IPC
surface (44) and the MCP tool registry (43, below) differ by exactly one — `health_check` is
not exposed as an MCP tool.

| Category | Methods |
|----------|---------|
| Health | `health_check` |
| Status | `knowledge_status`, `knowledge_rebuild_status` |
| Ingestion | `knowledge_process_chunk`, `knowledge_add_episode` |
| Direct assertion | `knowledge_assert_entity`, `knowledge_assert_relationship` |
| Search | `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_search_passages`, `knowledge_query_cypher` |
| Graph reads | `knowledge_get_episodes`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source` |
| Deletion | `knowledge_delete_episode`, `knowledge_delete_by_source`, `knowledge_delete_chunk_episode`, `knowledge_delete_by_group`, `knowledge_clear_all` |
| Curation | `knowledge_merge_entities`, `knowledge_validate_corrections`, `knowledge_apply_corrections`, `knowledge_reprocess_entity_types` |
| Relation typing | `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types` (deprecated), `knowledge_reprocess_relation_types` |
| Semantic search maintenance | `knowledge_backfill_summary_embeddings` |
| Cross-group pointers | `knowledge_add_cross_group_edge`, `knowledge_rebind_pointers` |
| WAL administration | `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_wal_mark_create`, `knowledge_wal_mark_list`, `knowledge_wal_mark_delete`, `knowledge_rebuild_from_wal`, `knowledge_build_indices` |
| Recovery / lifecycle | `knowledge_recover`, `knowledge_recover_full`, `knowledge_close` |

For request/response shapes and parameter details, the dispatch `match` arms in
[`handlers.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/core/src/handlers.rs)
and their handler functions are the source of truth — this page is the method index, not a
copy of each handler's parameter parsing.

`knowledge_status`'s WAL fields — including `wal.hydration_status` (issue #456), which
distinguishes a genuinely empty group from one whose WAL holds unapplied content — are documented
field-by-field in [Operations: `knowledge_status` health fields](operations.md#knowledge_status-health-fields)
rather than here.

Every long-running method above (the six WAL/recovery/reclassification operations most likely
to run for a while) accepts a `_progress_token` and streams `{"type":"progress",...}` frames
before the terminal result — see [Progress notifications](#progress-notifications) below.

### Readiness

A successful connection to `.lcg/service.sock` is **not** evidence the service is ready. The
socket is bound before the database opens — deliberately, so `health_check` and recovery IPC stay
reachable during degraded-mode recovery ([ADR-0009](adr/0009-degraded-mode-startup-recovery.md))
— and issue #378's WAL-root migration also runs in that same pre-open window, after the bind.
(Legacy `.graphiti/`→`.lcg/` workspace migration runs earlier still, before the socket is even
bound.) The process's own accept loop only starts once startup work has fully resolved, so a
request sent immediately after `connect()` queues in the kernel rather than racing the migration
with stale state — the real risk is a client that treats `connect()` succeeding as readiness by
itself and acts on that assumption (e.g. inspecting on-disk state) without waiting for a
`health_check` round-trip.

The correct readiness signal is a `health_check` round-trip reporting `"healthy"`:
`handle_health_check` only returns `healthy` once `Db::open()` has succeeded, which is after
migration has completed. Poll `health_check` until it reports `healthy` (or `knowledge_status`
until `connected` and `queryable` are both `true` and `initializing` is `false` —
`knowledge_status` has no `healthy` field of its own) before sending real work; see
[Operations: Self-healing and degraded mode](operations.md#self-healing-and-degraded-mode)
for the full rationale. A `degraded` response after startup has otherwise settled is a legitimate
outcome (e.g. unrecovered corruption) — not something to retry indefinitely.

## MCP-over-stdio transport

`liminis-context-graph --mcp-stdio` starts a native [Model Context Protocol](https://modelcontextprotocol.io)
server over stdin/stdout, using the official Rust SDK (`rmcp`). Any MCP client (Claude Code,
Claude Desktop, other agents) can query and mutate the knowledge graph directly — no Electron
app, no Node, no custom JSON-RPC client required. This is an *additional* external-facing
surface; the Unix-socket JSON-RPC protocol above is unchanged.

Every MCP tool is derived from the `knowledge_*` dispatch methods above — tool names match the
IPC method names verbatim, and each `tools/call` is translated into an `IpcRequest` and routed
straight through the same core dispatch the socket service uses. Tool descriptions and JSON
schemas are maintained in the
[`ToolSpec` registry in `crates/service/src/mcp/tools.rs`](https://github.com/verveguy/liminis-context-graph/blob/main/crates/service/src/mcp/tools.rs) —
that file is the canonical source for per-tool descriptions; they are not duplicated here.

### Flags

| Flag | Description |
|------|-------------|
| `--mcp-stdio` | Starts the MCP server over stdin/stdout instead of binding the Unix socket. |
| `--scope=<list>` | Comma-separated list of scopes to advertise in `tools/list` (default `all`). See [Scopes](#scopes) below. |
| `--connect <path>` | Attached mode: forward every `tools/call` as JSON-RPC over the given Unix socket to an already-running service, instead of opening the database directly. |
| `--allow-remote-close` | Attached mode only: advertise and allow `knowledge_close`, forwarding the shutdown to the remote service. No effect in standalone mode (no `--connect`). |

### DB-access modes

- **Standalone (default, no `--connect`)**: the MCP process opens the `.lcg` database directly,
  reusing the same startup and self-recovery path as the socket service ([ADR 0009](adr/0009-degraded-mode-startup-recovery.md)).
  Zero-dependency — works with no other process running.
- **Attached (`--connect <socket-path>`)**: the MCP process never opens the database; it
  forwards each call over the given socket to a service that already has it open. Use this to
  add MCP access to a workspace where another socket-service instance is already running,
  without contending for lbug's single-writer lock.
  - **Idle timeout.** `LCG_ATTACHED_CALL_TIMEOUT_MS` (default 30s) is a **per-read-line** idle
    timeout, not a whole-call timeout: it resets on every line read off the socket, including
    `{"type":"progress"}` lines. A call that keeps emitting progress is never bounded by it, no
    matter how long the call runs in total — only genuine silence (no output at all for the full
    timeout window) trips it. If the remote stops responding mid-call (e.g. it crashes), the
    attached client fails that call with a clean timeout error rather than blocking forever.
  - **Reconnect and retry.** If the connection to the remote breaks, the client transparently
    re-dials the same socket path rather than staying wedged. If the break is detected while
    writing the outgoing request — treated as safe to retry, since the write failing is the
    client's best available signal that the request didn't get through — the client
    automatically retries that request exactly once over the freshly-dialed connection. If the
    break is detected only after the request was fully written — while waiting for or reading
    the response — the call is **not** retried automatically, since the remote's execution
    status is unknown and blind retry could double-apply a non-idempotent write (e.g.
    `knowledge_add_episode`); that call fails with a clear "connection lost mid-call" error, but
    the connection is marked dead so the *next* call reconnects fresh. If a reconnect attempt
    itself fails (no listener at that path), the call fails with a clear, descriptive error —
    never a hang — and a later call will try reconnecting again. See
    [ADR-0040](adr/0040-attached-mode-reconnect-retry-boundary.md) for the full rationale.

### Scopes

Scopes are additive and composable (e.g. `--scope=read,admin`). `tools/list` advertises the
union of all active scopes.

| Scope | Methods |
|-------|---------|
| `read` | `knowledge_status`, `knowledge_find_entities`, `knowledge_find_relationships`, `knowledge_get_episodes`, `knowledge_get_nodes_by_group`, `knowledge_get_edges_by_group`, `knowledge_get_edges_by_uuids`, `knowledge_search_passages`, `knowledge_list_entities`, `knowledge_list_relationships`, `knowledge_get_entity_neighbors`, `knowledge_get_entities_by_source`, `knowledge_rebuild_status`, `knowledge_validate_corrections` |
| `write` | `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_delete_episode`, `knowledge_delete_by_source`, `knowledge_delete_chunk_episode`, `knowledge_clear_all`, `knowledge_apply_corrections`, `knowledge_merge_entities`, `knowledge_reprocess_entity_types`, `knowledge_canonicalize_relations`, `knowledge_backfill_relation_types`, `knowledge_reprocess_relation_types`, `knowledge_add_cross_group_edge`, `knowledge_assert_entity`, `knowledge_assert_relationship` |
| `cypher` | `knowledge_query_cypher` |
| `admin` | `knowledge_dump_wal`, `knowledge_prepare_checkpoint`, `knowledge_wal_mark_create`, `knowledge_wal_mark_list`, `knowledge_wal_mark_delete`, `knowledge_rebuild_from_wal`, `knowledge_recover`, `knowledge_recover_full`, `knowledge_close`, `knowledge_build_indices`, `knowledge_rebind_pointers`, `knowledge_delete_by_group`, `knowledge_backfill_summary_embeddings` |
| `all` | every scope above (default) |

**`cypher` is a power scope, not bundled into anything else.** `knowledge_query_cypher` executes
raw Cypher with no param interpolation or value coercion — despite being a "query" method, it can
perform arbitrary mutations, and it bypasses the WAL-ordering and embedding invariants that the
structured write tools maintain. It is never implicitly included in `read`, `write`, or `admin`;
operators must opt in explicitly (or via `all`).

**`knowledge_close` in attached mode is a footgun without `--allow-remote-close`.** In
**standalone** mode, `knowledge_close` is always advertised under `admin` scope and shuts down
only this MCP process's own DB connection. In **attached** mode, calling it would shut down the
*running remote service*. Without `--allow-remote-close`, `knowledge_close` is omitted from
`tools/list` entirely in attached mode (not merely rejected when called). Pass
`--allow-remote-close` only when you specifically intend this MCP connection to be able to stop
the remote service.

**Recovery and export live under `admin`.** `knowledge_rebuild_from_wal` (rebuild one group's data
from its own WAL directory — `group_id`, default `"liminis"`), `knowledge_dump_wal`
(snapshot/export the graph into a fresh compacted WAL directory), `knowledge_wal_mark_create` /
`_list` / `_delete` (name a retained WAL position within one group's stream, without a full
snapshot — each also takes `group_id`, default `"liminis"`, and `_list` reports only that one
group's marks, never an aggregate across groups), and `knowledge_recover` / `knowledge_recover_full`
are all `admin`-scope tools — an attached client only sees them when launched with `--scope=admin`
(or `all`). If a mutation goes wrong, this is the recovery path. See [Operations](operations.md)
for the recovery model in full. Note the WAL replays **forward-only**, so take periodic
`knowledge_dump_wal` snapshots, or a lighter-weight `knowledge_wal_mark_create` named position, if
you want restore points before large or destructive operations — a mark does not survive
`knowledge_dump_wal`, since dump_wal renumbers sequence numbers and a copied mark's `seq` would be
meaningless against the new numbering.

**`knowledge_rebuild_from_wal` refuses to run against a non-empty group, unless you ask it not
to.** Since issue #378, one instance holds an independent WAL directory and applied position per
`group_id`; `knowledge_rebuild_from_wal {group_id, ...}` targets exactly one of them and never
disturbs another group's data or position. A `from_seq: 0` (default) full rebuild against a group
that already contains data fails fast with an explicit error rather than silently emitting a
duplicate-primary-key failure for every existing `Entity`/`Episodic`/`RelatesToNode_` row in that
group — the native write path uses `CREATE`, not `MERGE`, for those labels. Pass `force_clear:
true` to have the call clear *that group's* data automatically before replaying (the same
group-scoped purge `knowledge_delete_by_group` uses — this does **not** delete or reopen the
database file, unlike the pre-378 whole-database `force_clear` behavior), or clear it yourself
first with `knowledge_delete_by_group {group_ids: [group_id]}`. `dry_run: true` always fails fast
on a non-empty group regardless of `force_clear`, since a dry run must never mutate the database —
this lets a preview surface the problem before you commit to a real rebuild. None of this applies
to an incremental `from_seq > 0` resume, which intentionally targets a group that already has
state.

**`to_seq` bounds replay from the other end: `from_seq <= seq <= to_seq`.** Pass an inclusive
upper bound to exclude a mutation (and everything after it) from a rebuild — e.g. a WAL-recorded
mutation that corrupted the graph. Omit `to_seq` for today's unbounded behavior (replay to the
end of the WAL); it must not be less than `from_seq`, or the call is rejected before any WAL line
is read or the database is touched. A bounded rebuild is **not durable**: WAL entries past
`to_seq` stay on disk, unapplied, not truncated or archived — a later unbounded rebuild, or a
`from_seq` resume that covers the excluded range, reapplies them, including a previously-excluded
bad mutation. `to_seq` bounds an endpoint; it does not add reverse/undo semantics to the
forward-only replay noted above.

### Relation typing (`canonicalize_relations`, `backfill_relation_types`, `reprocess_relation_types`)

Three tools populate an edge's `relation_type`, with different tradeoffs:

**`knowledge_canonicalize_relations`** maps each edge's **existing raw predicate** onto your
ontology's declared `relation_types`. Its behavior has four caveats worth knowing before you
rely on it:

- **`group_id` is required** (issue #447) — candidate selection and the resulting WAL mutations
  are both restricted to that one group; no other group's edges or WAL stream are ever touched. An
  omitted, `null`, or empty `group_id` is rejected before any candidate selection or write, rather
  than falling back to a database-wide rewrite or the default group — there is no supported way to
  canonicalize every group in one call.
- **The primary pass is lexical, over the predicate — not the `fact`.** It matches the edge's
  predicate / current `relation_type` against ontology type names, aliases, and keywords. It does
  **not** read the edge's `fact` sentence, so an edge whose `relation_type` was cleared cannot be
  re-mapped from its fact by this pass.
- **`embedding_threshold` tunes only the fallback promoter** (default `0.7`). The fallback embeds
  each residual edge's `fact` against the ontology types' *descriptions* and force-assigns the single
  nearest type at or above the threshold. Lowering it types more edges, but by nearest-neighbor
  force-fit with **no abstention** — an idiosyncratic fact (e.g. "*X is affiliated with Y*") can land
  on a wrong type (e.g. `HOLDS`).
- **Re-runs are only partly idempotent, and clearing `relation_type` can't be undone by
  canonicalize.** A re-run skips an edge only when it's already at its target — a `Mapped` edge
  already equal to the canonical type, or a residual edge already `UNCLASSIFIED`; an edge whose
  classification *changes* is overwritten (including a previously-assigned type), while
  arrow-named "noise" edges keep any existing predicate. Critically, canonicalize's only input is
  the edge's existing predicate / `relation_type` — if you **null that field to "start clean,"
  canonicalize has nothing to map from and cannot rebuild it.** Snapshot with `knowledge_dump_wal`
  before such an operation.

**`knowledge_backfill_relation_types`** (DEPRECATED) does not classify at all — it mints
uppercased fact-prefix pseudo-types (e.g. `THE_SPECIFICATION_DOCUMENT_DEFINES`) for edges with no
`relation_type`, rather than matching against the ontology. Avoid it for building a typed
taxonomy; prefer `knowledge_reprocess_relation_types` below, which supersedes it for that purpose.
Like `knowledge_canonicalize_relations`, **`group_id` is required** (issue #447): candidate
selection and WAL attribution are both restricted to that one group, and an omitted, `null`, or
empty `group_id` is rejected rather than running database-wide or against the default group.

**`knowledge_reprocess_relation_types`** is the relation-side twin of
`knowledge_reprocess_entity_types`: for each in-scope edge, it sends the edge's `fact` and the
ontology's declared relation types (name + description) to the configured extraction LLM and asks
it to pick exactly one type, or honestly abstain. This is the tool to reach for when you want
genuine fact-based classification instead of lexical matching or pseudo-typing:

- **Always reads the `fact`, never the predicate.** Unlike `canonicalize_relations`, classification
  is grounded in the edge's natural-language fact sentence against the ontology's declared menu —
  not lexical/alias/keyword matching on the existing predicate string.
- **A declared ontology relation-type menu is always required.** Unlike
  `knowledge_reprocess_entity_types` (whose `untyped` scope works with no ontology via open-ended
  classification), every scope value (`untyped`, `off_ontology`, `all`) fails with a structured
  `{success: false, error: ...}` if the ontology declares no relation types — there is no
  open-ended fallback (see [ADR-0037](adr/0037-relation-classification-abstention-writes-unclassified.md)).
- **Abstention is an honest, real write of `UNCLASSIFIED`.** If the LLM cannot map a fact to any
  declared type, the edge's `relation_type` is set to the literal string `UNCLASSIFIED` — never a
  force-assigned nearest match. This differs from `knowledge_reprocess_entity_types`, where an
  unclassifiable entity is simply left unchanged (see ADR-0037).
- **`scope`** controls candidates: `"untyped"` (default) — `relation_type` NULL/empty, the same
  predicate `backfill_relation_types` uses; `"off_ontology"` — untyped edges plus edges whose
  `relation_type` isn't a declared type (this naturally covers prior `UNCLASSIFIED` sentinels and
  `backfill_relation_types`'s fact-prefix pseudo-types with no special-casing); `"all"` — every
  edge in the group.
- **Idempotent.** An edge whose computed verdict already matches its current `relation_type`
  (including an edge already correctly `UNCLASSIFIED`) is left unchanged — no write, no WAL entry.
- **`dry_run: true`** returns `would_reclassify_count`, a `plan` array of per-edge
  `{edge_id, fact, old_type, new_type}` entries, and a `breakdown` object counting edges per
  assigned `new_type` (including an `UNCLASSIFIED` count) — without mutating the graph.
- **`dry_run: false` (apply)** returns `reclassified_count`, `unchanged_count`, and — since
  issue #332 — the same `breakdown` object as the dry-run path, so callers can see the per-type
  classification distribution (including how many candidates abstained to `UNCLASSIFIED`)
  without a separate dry-run call. `plan` and `would_reclassify_count` remain dry-run-only, since
  they describe a proposed mutation rather than one that already happened; `breakdown` is always
  present on a successful apply response, as `{}` when there were zero in-scope candidates.

### Semantic search maintenance (`backfill_summary_embeddings`)

**`knowledge_backfill_summary_embeddings`** (issue #470) computes `summary_embedding` for every
entity in `group_id` that has a non-empty `summary`, so entities created before this capability
existed become semantically retrievable by summary paraphrase — not just by name-vector or
full-text match. `knowledge_find_entities` already fuses a summary-vector match into its hybrid
retrieval for entities embedded going forward (both `knowledge_assert_entity` and the extraction
path embed `summary` on creation); this tool is what makes that retrieval available for entities
that predate the capability.

- **Every candidate is unconditionally re-embedded on each call.** There is no cheap way to tell
  "already has a real embedding" apart from "still carries the schema migration's zero-vector
  placeholder" from a stored `FLOAT[]` value alone, so re-running this backfills the same rows
  again. This is safe (idempotent in effect) but not free (an embedder round-trip per candidate,
  same as `knowledge_backfill_relation_types` and subject to the same no-batching caveat, #445) —
  use `dry_run: true` first to see the candidate count.
- **Holds an exclusive lock for the whole run, blocking all other reads and writes.** Unlike most
  admin operations, the summary-vector HNSW index is dropped for the run's duration and rebuilt
  at the end — this is the only way an indexed embedding column can be refreshed for existing
  rows at all (a plain `SET` on an HNSW-indexed column is rejected once the index exists). Prefer
  running this at a low-traffic time, especially for a large group.
- **`group_id` is required** (matching `knowledge_backfill_relation_types`'s convention):
  candidate selection and WAL attribution are both restricted to that one group; an omitted,
  `null`, or empty `group_id` is rejected rather than running database-wide or against the
  default group.
- **A partially-completed backfill is not an error.** Entities not yet processed simply retrieve
  via existing name/lexical behavior, exactly as before this issue — running the tool again
  covers more of the group each time.
- **`summary_embedding` stays write-once after backfill, same as on creation.** A later
  `knowledge_assert_entity` re-assert that changes an entity's `summary` does not refresh its
  `summary_embedding` — the vector reflects whichever summary was embedded last (at creation, or
  at the most recent backfill run), not necessarily the current `summary` text. Re-run this tool
  to bring a changed summary's vector back in sync.

### Cross-group pointers (`add_cross_group_edge`, `rebind_pointers`)

`knowledge_add_cross_group_edge` creates an edge whose endpoint(s) may live in a `group_id`
other than the edge's own — the hub/layer-graph topology introduced by issue #369 (see
[ADR-0369](adr/0369-resolvable-cross-group-pointers.md)). Every intra-group edge write
(`knowledge_add_episode`, `knowledge_process_chunk`, and every other existing write path) is
completely unaffected: pointers only ever exist on edges created through this tool.

- **Each endpoint is either a bare UUID or a name to resolve.** `{"uuid": "..."}` names an
  entity already known to live in the edge's own `group_id` — no resolution, no pointer. `
  {"source_group_id": "...", "endpoint_name": "..."}` names a *foreign* endpoint: it is resolved
  by case-insensitive name lookup against that group (the same authority
  `get_entity_by_name_ci_with_scan_fallback` uses for extraction-time endpoint resolution, per
  [ADR-0283](adr/0283-name-index-scan-fallback-for-endpoint-authority.md)), and the edge carries
  a `cross_group_pointers.{src,dst}` object recording the assertion (`source_group_id`,
  `endpoint_name`) and the resolution cache (`resolved_uuid`, `bound_at_seq`, `binding_state`).
- **A foreign endpoint that doesn't currently resolve is `unbound`, not dropped.** Unlike
  ordinary extraction (which hard-drops an unresolvable endpoint at commit — see
  [ADR-0051](adr/0051-edge-endpoint-salvage-and-deferred-drop.md)), the edge is still created;
  only the hop to that side is missing until a later `knowledge_rebind_pointers` call resolves
  it. A `binding_state` of `ambiguous` means more than one entity currently matches the name —
  also retained, also missing that hop, never a silently-guessed winner.
- **A bare-UUID endpoint that turns out to belong to a different group than the edge is
  rejected** before any write happens — this is what keeps a cross-group edge from silently
  losing its pointer fields the first time a caller passes a UUID instead of a name.
- **`knowledge_rebind_pointers`** (`{"source_group_id": "..."}`, required) re-resolves every
  pointer whose `source_group_id` matches, after that source group's own hydration, incremental
  replay, or refresh cycle — including an ordinary `knowledge_rebuild_from_wal` targeting that one
  group, not only a full purge-and-rehydrate (issue #378). A pointer currently `bound` is skipped
  once its `bound_at_seq` is already at or past `source_group_id`'s **own** applied WAL position —
  never any other group's, even when the edge carrying the pointer lives in a third, different
  group — this staleness gate is what makes a second call with no intervening change to
  `source_group_id`'s stream a true no-op for pointers that are already correct. A pointer
  currently `unbound` or `ambiguous` is always re-resolved regardless of `bound_at_seq` (issue
  #392): a known-broken pointer is repaired unconditionally, since the position comparison alone
  cannot tell "nothing changed" apart from "the source group was purged and then restored to the
  same position the pointer was originally bound at." A resolution that would create a self-loop
  or duplicate an existing directed edge invalidates the edge instead of writing a broken or
  redundant one, reusing `knowledge_merge_entities`'s own self-loop/dedup handling rather than a
  new policy. Returns `{checked, bound, unbound, ambiguous, invalidated_self_loop,
  invalidated_duplicate, staleness_skipped}` — `staleness_skipped` counts pointers skipped by the
  gate above, distinct from `checked` (pointers actually re-resolved), so a `checked: 0` result is
  never ambiguous about whether anything was examined.
- **Unbound and ambiguous edges are excluded from normal reads.** Every existing two-hop
  traversal, search, and MCP read path requires both hops to exist — a pointer that hasn't
  resolved is invisible the same way any other incomplete edge would be, no special-casing
  needed. Aggregate counts are visible via `knowledge_status`'s `cross_group_pointers: {bound,
  unbound, ambiguous}` field, so a refresh in progress is observable without a dedicated
  inspection endpoint.

### Ingestion results (`process_chunk`)

`knowledge_process_chunk` reports what it could not write, not only how much. Two additive
result fields exist for that, both introduced in 0.13.2.

- **`dropped_edges` reports the edges behind `edges_dropped_unresolvable`.** An extracted edge
  whose source or target endpoint resolves to no entity — neither in the current extraction batch
  nor in the persisted graph — is dropped rather than written
  ([ADR-0051](adr/0051-edge-endpoint-salvage-and-deferred-drop.md)). `edges_dropped_unresolvable`
  counts those drops; `dropped_edges` describes them, with one entry per counted edge, in
  extraction order:

  ```json
  {
    "edges_dropped_unresolvable": 1,
    "dropped_edges": [
      {
        "source_name": "Ada Lovelace",
        "target_name": "Analytical Engine",
        "relation_type": "WROTE_NOTES_ON",
        "fact": "Ada Lovelace wrote the first published algorithm for the Analytical Engine.",
        "unresolved_endpoint": "target"
      }
    ]
  }
  ```

  `unresolved_endpoint` is `"source"`, `"target"`, or `"both"`. `relation_type` may be `null`,
  mirroring the fact that it is already optional on an extracted edge before resolution is
  attempted. The dropped edge's content is not persisted anywhere, so this result is the only
  place it appears — a consumer that wants to tell a user which fact was lost must read it here
  rather than recover it later. **`dropped_edges` is always present**, an empty list when nothing
  was dropped, so it can be iterated unconditionally. `edges_dropped_unresolvable`'s existing
  meaning is unchanged, and a caller reading only the count is unaffected (issue #411).

- **`warning` reports oversized input.** A `chunk_text` longer than the advisory threshold
  (`LCG_CHUNK_TEXT_ADVISORY_MAX_CHARS`, default 8,000 characters — see
  [Configuration](configuration.md)) adds a `warning` field naming the actual and recommended
  character counts, and emits a `chunk_text_oversized` telemetry event. This is visibility only:
  nothing is truncated, split, or rejected, and the call succeeds exactly as it did before.
  Splitting oversized input is the caller's responsibility (issue #407).

### Progress notifications

The six long-running operations — `knowledge_rebuild_from_wal`, `knowledge_canonicalize_relations`,
`knowledge_backfill_relation_types`, `knowledge_backfill_summary_embeddings`,
`knowledge_reprocess_relation_types`, and `knowledge_reprocess_entity_types` — bridge to MCP
progress notifications when the client
attaches a progress token to the `tools/call` request (`_meta.progressToken`), in both
standalone and attached mode. Without a progress token, these calls simply block until they
complete, same as over the socket protocol. In attached mode, each progress notification also
re-arms `LCG_ATTACHED_CALL_TIMEOUT_MS`'s per-read-line idle timer (see
[DB-access modes](#db-access-modes) above), so a progress-tracked call isn't falsely reported as
timed out just because it runs longer than that timeout in total.

### Example MCP client config

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--scope=read,write"],
      "cwd": "/path/to/your/workspace"
    }
  }
}
```

To attach to an already-running socket service instead of opening the DB directly:

```json
{
  "mcpServers": {
    "liminis-context-graph": {
      "command": "liminis-context-graph",
      "args": ["--mcp-stdio", "--connect", "/path/to/your/workspace/.lcg/service.sock", "--scope=read"]
    }
  }
}
```

See [ADR 0035](adr/0035-mcp-stdio-transport.md) for the transport's internal architecture.
