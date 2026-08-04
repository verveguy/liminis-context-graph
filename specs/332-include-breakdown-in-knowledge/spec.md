# Feature Specification: Include `breakdown` in `knowledge_reprocess_relation_types`' apply response

**Feature Branch**: `fabrik/issue-332`
**Created**: 2026-08-03
**Status**: Draft
**Input**: User description: "Include breakdown in knowledge_reprocess_relation_types' apply response — reported in #305 while migrating the Liminis desktop app off the deprecated knowledge_backfill_relation_types (verveguy/liminis#934)."

## Background

`knowledge_reprocess_relation_types` returns its per-type classification distribution (`breakdown`) only on the dry-run path. After a real apply, the caller gets `reclassified_count` and `unchanged_count` and nothing else — the number of relations left `UNCLASSIFIED` is not recoverable from the response.

Abstention is the headline behavior of this method. [ADR-0037](../../docs/adr/0037-relation-classification-abstention-writes-unclassified.md) introduced it precisely so a fact matching no declared relation type gets an honest `UNCLASSIFIED` sentinel rather than an invented pseudo-type — the defect that made `knowledge_backfill_relation_types` unsafe (#205/#211). Hiding the abstention count after apply hides the signal the feature exists to produce.

Verified on `main`, `crates/core/src/reprocess_relations.rs`:

| line | what |
|---|---|
| 156–167 | zero-candidate early return: dry-run branch returns `would_reclassify_count`, `plan`, `breakdown` (all empty/zero); apply branch returns `reclassified_count`, `unchanged_count` only — no `breakdown` |
| 219 | `breakdown` declared as a `BTreeMap<String, usize>` |
| 239 | `*breakdown.entry(new_type).or_insert(0) += 1` — populated once, in the path shared by both dry-run and apply, so the data is already correct for apply |
| 244–248 | dry-run response returns `would_reclassify_count`, `plan`, `breakdown` |
| 296–301 | apply response returns `success`, `reclassified_count`, `unchanged_count`, `group_id` only |

The value is already computed on the apply path and then discarded before the final response is built. This is a response-shape omission, not missing logic — no new classification pass or data collection is required.

### Why it matters

These two outcomes are indistinguishable in today's apply response:

- 500 relations confidently classified into declared types
- 500 relations all abstained to `UNCLASSIFIED`

Both report `reclassified_count: 500`, because under ADR-0037 an abstention **is** a real write — unlike entity classification, where abstain means skip (see `knowledge_reprocess_entity_types` in `crates/core/src/handlers.rs`). A high abstention rate is the primary signal that an ontology's relation-type menu does not fit the corpus, and it is exactly what a UI wants to show immediately after the user applies.

The workaround — dry-run first, read `breakdown`, then apply — costs a second full LLM classification pass over the same candidates, and the two passes are not guaranteed to agree (nondeterministic LLM output, and the underlying data may have changed between calls).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A caller can see classification quality after applying (Priority: P1)

A client (e.g. the Liminis desktop app) calls `knowledge_reprocess_relation_types` with `dry_run: false`. Today it receives only aggregate counts of "changed" vs. "unchanged" relations. It needs to know how many of the changed relations landed on `UNCLASSIFIED` vs. a real declared type, without a second call.

**Why this priority**: This is the entire point of the issue — the missing signal is the reason ADR-0037's abstention behavior exists. Without it, callers cannot distinguish a healthy apply from one where the ontology doesn't fit the corpus.

**Independent Test**: Call `knowledge_reprocess_relation_types` with `dry_run: false` against a group with a mix of classifiable and unclassifiable relations. Assert the response contains a `breakdown` object whose per-type counts sum to `reclassified_count` and include an `UNCLASSIFIED` entry when at least one candidate abstained.

**Acceptance Scenarios**:

1. **Given** an apply run that classifies relations, **When** it returns, **Then** the response includes a per-type `breakdown` covering every type written by this run, including `UNCLASSIFIED` where applicable.
2. **Given** an apply run where every candidate abstained, **When** it returns, **Then** the caller can distinguish that from an all-confident run (via `breakdown`) without issuing a second pass.
3. **Given** an apply run with zero in-scope candidates, **When** it returns, **Then** the response includes `breakdown: {}` — an empty map, not a missing key.

---

### User Story 2 - Existing clients are unaffected (Priority: P1)

A client that only reads `reclassified_count` / `unchanged_count` from the apply response (the current contract) continues to work unmodified after this change ships.

**Why this priority**: This is an additive field. Breaking existing callers of a widely-used IPC method to fix a different caller's gap would be a regression, not a fix.

**Independent Test**: Existing IPC parity tests that assert the apply response shape continue to pass with only an added field; no existing assertion on `reclassified_count` / `unchanged_count` name, type, or value changes.

**Acceptance Scenarios**:

1. **Given** a client reading only `reclassified_count` / `unchanged_count`, **When** it runs against the new response, **Then** those fields are unchanged in name, type, and meaning.

---

### Edge Cases

- **Zero-candidate early return** (`reprocess_relations.rs:156–167`): the apply branch of this early return must also gain `breakdown`, as an empty map (`{}`), mirroring the dry-run branch's already-correct empty-map behavior at the same early return.
- **LLM classification failure** (the `Err(e) => return Ok(json!({"success": false, ...}))` branch, `reprocess_relations.rs` around line 213): this response is returned *before* any `breakdown` data exists — no candidates have been classified yet at the point of failure. This response shape is unchanged by this feature; it is out of scope (see below).
- **All-abstain apply**: every candidate maps to `UNCLASSIFIED`. `breakdown` must show `{"UNCLASSIFIED": N}` where `N` equals `reclassified_count`, and this exact scenario needs an explicit test (see FR-006) since it's the case the reporter identified as indistinguishable today.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The apply-path response (`dry_run: false`) of `knowledge_reprocess_relation_types` MUST include a `breakdown` field, with the same shape and semantics as the dry-run path's `breakdown`: a map of relation-type name (including the literal `UNCLASSIFIED`) to the count of relations written with that type during this run.
- **FR-002**: `reclassified_count` and `unchanged_count` in the apply response MUST be unchanged in name, type, and meaning. This change is purely additive to the apply response shape.
- **FR-003**: **All** apply-path return sites in `reprocess_relation_types` (`crates/core/src/reprocess_relations.rs`) MUST include the `breakdown` field, including the zero-candidate early return (currently `reprocess_relations.rs:164–167`), which MUST return `breakdown: {}` — mirroring the dry-run early return's existing `breakdown: {}` behavior at `reprocess_relations.rs:158–162`. A client must be able to rely on `breakdown` always being present on a successful (`success: true`) apply response, rather than testing for its absence.
- **FR-004**: Update the MCP tool description for `knowledge_reprocess_relation_types` in `crates/service/src/mcp/tools.rs`, and the IPC/MCP reference documentation (`docs/ipc-mcp-reference.md`, published at https://v3rv.com/liminis-context-graph/ipc-mcp-reference), to document that the apply-path response now includes `breakdown` with the same shape as the dry-run path. This is user-facing surface and must not drift from the implementation.
- **FR-005**: Update the Tier 1a/1b/1c parity tests in `crates/core/tests/ipc_parity.rs` that assert `knowledge_reprocess_relation_types`' response shape to expect `breakdown` on the apply path.
- **FR-006**: Add a test asserting that an apply run whose candidates *all* abstain returns a `breakdown` containing `UNCLASSIFIED` with a count equal to the full number of reclassified relations — the specific case the reporter identified as indistinguishable from an all-confident run today.
- **FR-007**: Add a test asserting that an apply run with zero in-scope candidates (the early return) returns `breakdown: {}` rather than omitting the field.

### Design Decision (stated deliberately, per issue's Edge Cases)

The dry-run and apply responses are **not** made identical in shape. `dry_run: true` continues to return `would_reclassify_count` and `plan` (a per-edge list of `{edge_id, fact, old_type, new_type}`) in addition to `breakdown`, because those fields describe a *proposed* mutation that has not happened. `dry_run: false` continues to return `reclassified_count`, `unchanged_count`, `success`, and `group_id`, describing a mutation that *did* happen, now with `breakdown` added alongside them. This keeps FR-002's additive guarantee simple (no field is removed, renamed, or repurposed on either path) and avoids sending a `plan` array — which can be large — on a call that already performed the writes it would describe.

### Sibling method note (investigated, not fixed here)

`knowledge_reprocess_entity_types` (`crates/core/src/handlers.rs`, `handle_reprocess_entity_types`) was checked for the same omission. It does **not** have a `breakdown` field on *either* path — dry-run (`handlers.rs:2408–2413`) returns only `would_reclassify_count` and `plan`; apply (`handlers.rs:2542–2548`) returns `success`, `reclassified_count`, `unchanged_count`, `restamped_count`. This is a broader gap than the one this issue fixes (missing on both paths, not just apply), and entity classification's abstain-means-skip semantics (ADR-0037) make the "hidden abstention" argument weaker there — an entity abstention is a no-op, not a write. Fixing it is out of scope for this issue; it is noted here as a candidate follow-up issue rather than silently left unmentioned.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An apply response from `knowledge_reprocess_relation_types` (`dry_run: false`) contains a `breakdown` field with per-type counts, including an `UNCLASSIFIED` entry whenever at least one candidate abstained.
- **SC-002**: An all-abstain apply and an all-confident apply of the same candidate-set size produce responses that are distinguishable by inspecting `breakdown` alone, without a second call.
- **SC-003**: Every apply-path return site in `reprocess_relation_types` includes the `breakdown` field, verified by test (FR-006, FR-007) rather than by code inspection alone.
- **SC-004**: #305's suggested direction is satisfied without requiring a second LLM classification pass to observe the type distribution of an apply run.

## Assumptions

- The `breakdown` computation already exists and is correct for the apply path (`reprocess_relations.rs:239`); this issue is a response-shape change, not a change to classification or abstention logic.
- No changes to `ADR-0037`'s abstention semantics are needed or intended.
- The MCP tool description and published docs site content are updated as part of this change per FR-004; no separate documentation-only follow-up is needed.

## Out of Scope

- Changing abstention semantics (ADR-0037 stays as-is).
- The deprecated `knowledge_backfill_relation_types` (#211) — not touched by this issue.
- Fixing the equivalent gap in `knowledge_reprocess_entity_types` (see "Sibling method note" above) — noted as a candidate follow-up, not fixed here.
- Changing the response shape of the LLM-classification-failure error path (`success: false`) in `reprocess_relation_types` — no `breakdown` data exists at that point in execution.

## Source References

- #305 — the original report, with line-level analysis of the omission
- [ADR-0037](../../docs/adr/0037-relation-classification-abstention-writes-unclassified.md) — relation-classification abstention writes `UNCLASSIFIED`
- #307 / ADR-0307 — token-budget policy, shipping in the same release
- verveguy/liminis#934 — the downstream migration that surfaced this
- `crates/core/src/reprocess_relations.rs` — implementation under change
- `crates/service/src/mcp/tools.rs` — MCP tool description to update (FR-004)
- `docs/ipc-mcp-reference.md` — published docs reference to update (FR-004)
- `crates/core/tests/ipc_parity.rs` — parity tests to update (FR-005)
