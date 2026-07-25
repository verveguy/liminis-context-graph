# Feature Specification: Deprecate `knowledge_backfill_relation_types` In Place — Stop Implying It Classifies

**Feature Branch**: `fabrik/issue-211`
**Created**: 2026-07-25
**Status**: Draft
**Input**: User description: "`knowledge_backfill_relation_types` is documented as 'Backfill missing relation_type values on existing edges' — implying classification against the ontology — but it does no classification. For each null-typed edge, `derive_relation_type` (`backfill.rs:82-90`) takes the first ~4 whitespace words of the edge's natural-language `fact` and uppercases/underscore-joins them into singleton pseudo-types like `THE_SPECIFICATION_DOCUMENT_DEFINES`. This is worse than leaving edges `UNCLASSIFIED`: it pollutes the `relation_type` space with near-unique labels, breaks typed queries, and is only reversible by re-nulling. With `knowledge_reprocess_relation_types` (#204) providing genuine fact-based classification, the backfill MCP tool has no correct use and actively misleads."

## Background

`knowledge_backfill_relation_types` was added (#144-era work, see `crates/core/src/backfill.rs`) as a way to fill in `relation_type` on edges left NULL/empty by earlier extraction. Its tool description in the MCP registry (`crates/service/src/mcp/tools.rs`) reads "Backfill missing relation_type values on existing edges" — language that implies real classification against the workspace ontology, the same way `knowledge_canonicalize_relations` and `knowledge_reprocess_entity_types` do.

In reality, `derive_relation_type` (`crates/core/src/backfill.rs:82-90`) never consults the ontology. It takes the edge's `name` if it's a plain (non-arrow) predicate, or otherwise falls back to `derive_relation_type_from_fact`, which uppercases and underscore-joins the first four whitespace-delimited words of the edge's `fact` sentence. On a real corpus (#205) this minted values like `THE_SPECIFICATION_DOCUMENT_DEFINES` and `THE_WORKFLOW_GRAPH_CONTAINS_NODES_AND` — near-unique singleton labels, not a taxonomy. Running the tool as documented silently corrupts the `relation_type` space: typed traversals (`WHERE relation_type = 'X'`) become unreliable, and the only way back is to null the field and start over.

This gap has now been closed on the classification side: `knowledge_reprocess_relation_types` (added via #210, merged to `main` in PR #222) is a genuine fact-based LLM classifier — for each in-scope edge it reads the `fact` against the ontology's declared relation types and writes either a declared type or an honest `UNCLASSIFIED` abstention, never a pseudo-type. Its `scope=untyped` mode is a direct, higher-quality replacement for `backfill_relation_types`'s use case (both use the same `relation_type IS NULL OR = ''` candidate predicate).

**Resolution**: `knowledge_backfill_relation_types` shipped in the public v0.10.0 MCP surface. Removing it outright would turn an existing caller's invocation into a cryptic "unknown tool" error with no explanation of what to do instead — a tool that stays present and is *honestly described* is a better guardrail than one that vanishes, since the description is exactly what steers a human's or an agent's tool choice from `tools/list`. This issue therefore keeps `knowledge_backfill_relation_types` registered with its runtime behavior unchanged, and rewrites its description to state plainly what it does, mark it deprecated, warn about the consequences of running it, and point callers to `knowledge_reprocess_relation_types {scope: "untyped"}` as the correct replacement. Actual removal is deferred to a future issue, to be done with notice once callers have had time to migrate.

**Explicitly not touched by this issue**: the underlying `derive_relation_type_from_fact` function stays, unchanged, and keeps powering the extractor's own going-forward fallback (`crates/core/src/extractor.rs:357`, used when the extraction LLM omits a `relation_type` for a newly-created edge) — a different, legitimate use case from a bulk backfill pass over an existing graph.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - MCP tool caller can no longer be misled into building a taxonomy from backfill (Priority: P1)

A user or agent inspecting the MCP tool registry (e.g., via `tools/list`) sees `knowledge_backfill_relation_types` still present, but its description now plainly states that it derives a pseudo-type label from the first few words of each edge's `fact` (not the ontology), marks the tool deprecated, warns that using it pollutes the `relation_type` space and is only reversible by re-nulling the field, and explicitly points to `knowledge_reprocess_relation_types {scope: "untyped"}` as the tool that performs genuine fact-based classification against the ontology.

**Why this priority**: This is the entire point of the issue — closing the misleading-documentation gap reported in #205 before it corrupts another graph's `relation_type` space, without breaking any existing caller.

**Independent Test**: Inspect the `ToolSpec` registry entry for `knowledge_backfill_relation_types` in a unit test and assert its `description` contains the deprecation marker, a plain statement that it derives labels from the `fact` prefix rather than classifying against the ontology, and a reference to `knowledge_reprocess_relation_types`.

**Acceptance Scenarios**:

1. **Given** the MCP tool registry, **When** a caller lists available tools, **Then** no tool's description implies that `knowledge_backfill_relation_types` performs ontology-based classification.
2. **Given** a caller reads `knowledge_backfill_relation_types`'s description, **When** they parse it, **Then** it states plainly that the tool derives a label from the first few words of each edge's `fact` (not the ontology), is marked deprecated, warns that running it pollutes the `relation_type` space and is only reversible by re-nulling, and directs the caller to `knowledge_reprocess_relation_types {scope: "untyped"}` for real classification.
3. **Given** `knowledge_backfill_relation_types` is called with the same parameters as before this change, **When** it runs, **Then** its behavior (candidate selection, derivation algorithm, write semantics, progress notifications) is byte-for-byte unchanged from before this change — only the registry `description` string differs.

---

### User Story 2 - The extractor's forward fallback keeps working (Priority: P1)

A developer running the extraction pipeline on new episodes, where the extraction LLM omits `relation_type` for a newly-produced edge, still gets a non-empty fallback value from `derive_relation_type_from_fact` — this per-edge, going-forward behavior is unrelated to the bulk `knowledge_backfill_relation_types` MCP tool and must not regress.

**Why this priority**: The issue is explicit that `derive_relation_type_from_fact` must be retained and this call site (`extractor.rs:357`) must keep exercising it. Breaking it would silently reintroduce empty `relation_type` values on new edges.

**Independent Test**: Existing extractor tests exercising the omitted-`relation_type` fallback path (`crates/core/src/extractor.rs` `#[cfg(test)] mod tests`) continue to pass unmodified after this change.

**Acceptance Scenarios**:

1. **Given** an extraction LLM response that omits `relation_type` for an edge, **When** the extractor processes that edge, **Then** `derive_relation_type_from_fact` is invoked and the edge receives a non-empty fallback `relation_type`.

---

### Edge Cases

- **Existing pseudo-typed edges in a graph that already ran `knowledge_backfill_relation_types` before this change.** Out of scope for this issue — no migration or cleanup of already-minted pseudo-types is required here. (A caller can run `knowledge_reprocess_relation_types {scope: "off_ontology"}` to fix them, since that scope already treats any non-declared `relation_type` — including pseudo-types — as a candidate.)
- **A caller invokes `knowledge_backfill_relation_types` after this change ships.** It still runs exactly as before (same candidate selection, same derivation, same writes) — the only difference is that the tool's advertised description no longer misrepresents what it does.
- **Registry/scope-bucket count assertions** (`registry_has_34_unique_tools`, `scope_bucket_sizes_match_fr_004_table` in `crates/service/src/mcp/tools.rs`) are unaffected by this change (the tool stays registered, so the total count and `Scope::Write` bucket count are unchanged) — no update to those specific assertions is expected, but a new assertion covering the description content (per FR-002) is added.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_backfill_relation_types` MUST remain registered in the `ToolSpec` registry (`crates/service/src/mcp/tools.rs`) — it MUST NOT be removed from `tools/list` or `tools/call` by this issue.
- **FR-002**: `knowledge_backfill_relation_types`'s `description` field MUST be rewritten to: (a) state plainly that it derives a label from the first few words of each edge's `fact` sentence and does not classify against the ontology; (b) mark the tool as deprecated; (c) warn that running it pollutes the `relation_type` space with near-unique pseudo-types and is only reversible by re-nulling the field; and (d) direct callers to `knowledge_reprocess_relation_types` with `scope: "untyped"` as the tool that performs genuine fact-based ontology classification. A new unit test MUST assert the description contains these elements (e.g., a deprecation marker and a reference to `knowledge_reprocess_relation_types`).
- **FR-003**: `knowledge_backfill_relation_types`'s runtime behavior — candidate selection (`relation_type IS NULL OR = ''`), the derivation algorithm (`derive_relation_type` / `derive_relation_type_from_fact`), write semantics, and MCP progress-notification support — MUST remain unchanged by this issue. Only the registry `description` string changes.
- **FR-004**: `derive_relation_type_from_fact` (`crates/core/src/backfill.rs`) MUST remain in the codebase and MUST continue to be invoked by the extractor's per-edge, going-forward fallback path (`crates/core/src/extractor.rs:357`) exactly as it is today.
- **FR-005**: `canonicalize_relations` and `reprocess_relation_types` MUST NOT be modified by this issue — this issue's scope is limited to the `backfill` MCP tool surface and its description.
- **FR-006**: The `ToolSpec` registry's total tool count and per-scope-bucket count assertions (`registry_has_34_unique_tools`-style test and `scope_bucket_sizes_match_fr_004_table`-style test in `crates/service/src/mcp/tools.rs`) MUST continue to pass unchanged, since no tool is added or removed by this issue.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A caller listing MCP tools (`tools/list`) can no longer form the belief that `knowledge_backfill_relation_types` performs ontology-based classification — its description explicitly says otherwise, marks it deprecated, and names `knowledge_reprocess_relation_types {scope: "untyped"}` as the correct tool.
- **SC-002**: Calling `knowledge_backfill_relation_types` with the same parameters before and after this change produces identical results (same candidate edges, same derived labels, same writes) — confirming the change is description-only.
- **SC-003**: `derive_relation_type_from_fact` continues to be exercised by the extractor's forward-fallback test coverage with zero regressions.
- **SC-004**: `cargo fmt --all`, `cargo clippy --release --all-targets -- -D warnings`, and `cargo test --release` are all green.

## Assumptions

- `knowledge_reprocess_relation_types` (from #210/PR #222) is present on `main` — it merged 2026-07-25 (`9587076`, PR #222) and is registered in `crates/service/src/mcp/tools.rs` with `scope=untyped` covering the same candidate edges `backfill_relation_types` targets.
- This issue's worktree branch was forked from a `main` commit that predates PR #222; rebasing onto current `main` before implementing is expected and is a Research/Implement-stage concern, not addressed further here.
- Full removal of `knowledge_backfill_relation_types` is deliberately deferred to a future issue, to be done with advance notice once existing callers (including any liminis-app usage this repo cannot inspect) have had a chance to migrate to `knowledge_reprocess_relation_types`.

## Out of Scope

- Any change to `derive_relation_type_from_fact`, `canonicalize_relations`, or `reprocess_relation_types` (#204 territory).
- Migrating or re-classifying edges that already carry a fact-prefix pseudo-type from a prior `backfill_relation_types` run — that is what `knowledge_reprocess_relation_types {scope: "off_ontology"}` is for, and is a separate, already-available operation.
- Actual removal of `knowledge_backfill_relation_types` from the tool registry — deferred to a future issue per the Resolution above.
- Any change to `service_protocol.py` / the liminis-app Python IPC surface.

## Source References

- `crates/core/src/backfill.rs:82-90` — `derive_relation_type`, the non-ontology-aware derivation this issue's description fix is about.
- `crates/core/src/backfill.rs:60-72` — `derive_relation_type_from_fact`, retained per FR-004.
- `crates/core/src/extractor.rs:357` — the forward-fallback call site that must keep working.
- `crates/service/src/mcp/tools.rs` — `ToolSpec` registry entry for `knowledge_backfill_relation_types` (description to rewrite per FR-002); `registry_has_34_unique_tools` / `scope_bucket_sizes_match_fr_004_table` tests (unaffected, per FR-006).
- `crates/core/src/handlers.rs:2238` (`handle_backfill_relation_types`), `:2253` (`handle_reprocess_relation_types`) — MCP dispatch (unchanged).
- Issue #205 — original bug report of the pseudo-type minting behavior.
- Issue #204 / #210 (PR #222) — added `knowledge_reprocess_relation_types`, the genuine classifier this issue's description points callers to.
