# Feature Specification: WAL Replayer Must Accept MATCH-Prefixed Mutation Queries

**Feature Branch**: `fabrik/issue-109`
**Created**: 2026-05-28
**Status**: Draft
**Input**: Live observation 2026-05-28 — replaying a 14.6 MB / 29-file / 1361-line historical WAL from demo-notebook (originally written by Python graphiti, pre-Rust cutover) reconstructed only 103 mutations into the graph. Histogram of WAL Cypher first-tokens:

```
  463  MATCH Episodic          ← skipped by replayer
  390  MERGE Entity            ✓ accepted
  379  MATCH Entity            ← skipped by replayer
   45  MERGE Episodic          ✓ accepted
   30  MATCH RelatesToNode_    ← skipped by replayer
   29  CREATE Entity           ✓ accepted
   21  CREATE RelatesToNode_   ✓ accepted
    4  CREATE Episodic         ✓ accepted
```

872 of 1361 WAL lines (64%) are MATCH-prefixed and were silently dropped as "unrecognised mutation" before even being attempted.

## Background

`liminis-graph-core/src/replay.rs:156-169` decides whether a WAL line is a mutation by looking at the **first uppercase token** of the Cypher string:

```rust
let upper = wal_line.cypher.to_uppercase();
let first = upper.split_whitespace().next().unwrap_or("");
if !matches!(first, "CREATE" | "MERGE" | "SET" | "DELETE" | "DETACH" | "DROP" | "REMOVE") {
    eprintln!("[WAL WARN] skipping unrecognised mutation: {}", …);
    stats.lines_skipped += 1;
    continue;
}
```

This implicit assumption — *"all mutations start with one of these verbs"* — does not hold for graphiti-authored WALs, which use the standard Cypher idiom `MATCH (n) WHERE … SET n.x = …` for any update to an existing node or edge. Concretely, the Python writer uses MATCH-prefixed mutations for:

- **Embedding enrichment** — content/name/fact embeddings are written in a separate post-extraction pass via `MATCH (n:Episodic {uuid: $uuid}) SET n.content_embedding = $embedding`. This is the bulk of the 463 `MATCH Episodic` lines.
- **Entity attribute updates** — summary refinement, attribute merges, label additions after dedup-and-merge: `MATCH (n:Entity {uuid: $uuid}) SET n.summary = $summary, n.attributes = $attributes`.
- **Edge fact updates** — fact paraphrase refinement, fact_embedding writes: `MATCH ()-[r:RelatesToNode_ {uuid: $uuid}]->() SET r.fact_embedding = $embedding`.

The result: every workspace whose WAL was written by Python graphiti — i.e., every workspace that existed before the Rust cutover — cannot be fully reconstructed from its WAL. The current replayer captures only the initial CREATE/MERGE shapes and discards every subsequent enrichment.

This was discovered during Check G (WAL-replay-from-git-commit test) of the 2026-05-26/28 cutover test sequence.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Historical WAL Replays Faithfully (Priority: P1)

A user with a workspace whose WAL was written by Python graphiti (pre-cutover) can trigger `knowledge_rebuild_from_wal` and obtain a graph whose entity/edge counts and embeddings match the pre-cutover state, within the bounds of what the WAL captured.

**Why this priority**: WAL replay is the migration story for every pre-cutover workspace. If 64% of WAL lines are silently dropped, the migration story is broken. This blocks any real-world adoption of the Rust replacement for users with existing graphs.

**Independent Test**: Take demo-notebook's 2026-04-11 WAL (committed in the demo-notebook repo). Trigger replay against an empty `.lcg/db/liminis.db`. Assert entity count, edge count, and content_embedding population rate within 5% of the source workspace's pre-cutover snapshot.

**Acceptance Scenarios**:

1. **Given** a workspace with a graphiti-written WAL containing MATCH-then-SET embedding writes, **When** the user triggers `knowledge_rebuild_from_wal`, **Then** content/name/fact embeddings appear on the replayed nodes/edges (not null).
2. **Given** the same WAL, **When** replay completes, **Then** the `mutations_replayed` count is materially higher than the count produced by the current first-token whitelist (for demo-notebook: ≥ 800, vs the current 103).
3. **Given** a workspace whose WAL contains attribute-update MATCHes for entities (summary refinement, label additions), **When** replay completes, **Then** the replayed entities carry the post-refinement summary and labels, not the pre-refinement initial-extraction values.

---

### User Story 2 — Unsupported Lines Are Recognised, Not Silently Dropped (Priority: P2)

When the WAL contains Cypher shapes the replayer genuinely cannot handle (e.g. pure read queries with no SET/DELETE/REMOVE, or shapes added by a future graphiti version), the replayer skips them but distinguishes them in stats from lines it executed unsuccessfully.

**Why this priority**: This is operator visibility. Once US-1 lands, we still want to know if a replay leaves anything on the table. Conflating "unrecognised" with "raw_query errored" hides real issues. (See sibling issue: `replay: surface failed mutations distinctly from skipped` for the stats split.)

**Independent Test**: Synthesise a WAL with one CREATE, one MATCH-SET, one pure `MATCH (n) RETURN n.uuid` read, and one CREATE that references a missing node UUID. Replay. Assert: 2 mutations replayed, 1 unrecognised, 1 failed.

---

### User Story 3 — Defensive Detection, Not Naïve Substring Match (Priority: P1, paired with US-1)

The MATCH-aware detection MUST NOT treat read-only `MATCH … RETURN` queries as mutations. The detector must inspect the full Cypher shape to confirm a mutation clause (SET / DELETE / REMOVE / DETACH / CREATE / MERGE) is reachable after the MATCH.

**Why this priority**: Without this, the fix in US-1 would happily run read queries via `raw_query` — wasting time, polluting stats, and surfacing the wrong errors.

**Acceptance Scenarios**:

1. **Given** a Cypher string `MATCH (n:Episodic {uuid: $uuid}) RETURN n.uuid`, **When** the replayer inspects it, **Then** it is classified as unrecognised (not as a mutation).
2. **Given** a Cypher string `MATCH (n:Episodic {uuid: $uuid}) SET n.content_embedding = $emb RETURN n.uuid`, **When** the replayer inspects it, **Then** it is classified as a mutation and executed.
3. **Given** a Cypher string `MATCH (n:Entity {uuid: $uuid}) DETACH DELETE n`, **When** the replayer inspects it, **Then** it is classified as a mutation and executed.

### Edge Cases

- **Cypher with comments containing keywords**: `// SET this later` followed by a pure MATCH-RETURN should NOT be classified as a mutation. **Known limitation**: the current implementation does NOT strip Cypher line (`//`) or block (`/* … */`) comments — a mutation keyword appearing only inside a comment will produce a false-positive classification. This is an accepted trade-off: neither graphiti nor liminis-graph emits commented Cypher in WAL lines, so the gap is benign in practice. See `strip_quoted_literals` in `wal.rs` for the documented constraint.
- **`MATCH … MERGE …` patterns** (insert-if-missing-against-an-existing-anchor): MUST be classified as mutation (the MERGE writes if not present).
- **Embedding writes with `$embedding` placeholder of unexpected dim**: If a WAL was written under a different embedding dimension than the current model produces, `raw_query` will fail at execution. This is correctly handled today (counted as a failed line). No change needed; WAL-from-different-embedder is a known limitation.
- **`MATCH (n) RETURN count(n)`** and similar aggregation reads: classified as non-mutation, skipped (no execution overhead).
- **Cypher containing only whitespace or a comment**: classified as non-mutation, skipped.
- **Cypher where the mutation keyword appears only inside a string literal** (e.g., `MATCH (n) WHERE n.label = "DELETE_ME" RETURN n`): naïve token-scan would false-positive. Acceptable trade-off — graphiti does not generate this pattern, and the worst case is wasted `raw_query` that returns 0 rows. Document the limitation in the function header.

## Requirements *(mandatory)*

- **FR-001.** The mutation-detection function MUST recognise as mutations any Cypher whose top-level structure contains at least one of: a `CREATE`, `MERGE`, `SET`, `DELETE`, `DETACH DELETE`, `REMOVE`, `DROP`, or `MERGE … ON CREATE / ON MATCH` clause — regardless of whether the query begins with `MATCH`.
- **FR-002.** The mutation-detection function MUST classify as non-mutations Cypher that contains only `MATCH`, `WHERE`, `WITH`, `UNWIND`, `RETURN`, `CALL` (without a mutation inside the call), and `ORDER BY` / `LIMIT` clauses.
- **FR-003.** Detection MUST tokenise the Cypher beyond the first whitespace-delimited word. A single-pass scan over uppercased tokens (or a small regex matching mutation keywords as whole words, not substrings of identifiers) is acceptable. Naïve `cypher.to_uppercase().contains("SET")` is NOT acceptable — it would match identifiers like `Asset` or property keys like `last_set`.
- **FR-004.** When a MATCH-prefixed mutation fails at `raw_query` execution (e.g., target UUID does not exist), the replayer MUST log the failure with the offending UUID extracted from the Cypher (best-effort) and continue with the next line, exactly as it does for CREATE/MERGE failures today.
- **FR-005.** The replayer MUST execute mutations in their persisted `seq` order (already true today via the JSONL line order). MATCH-then-SET writes that update a node created by an earlier CREATE/MERGE depend on this ordering and MUST replay after the creation, never before.
- **FR-006.** A regression test fixture MUST be added: a minimal but representative WAL extract covering all the patterns observed in demo-notebook's 2026-04-11 WAL — at least one of each of `MATCH Episodic SET content_embedding`, `MATCH Entity SET summary/attributes`, `MATCH ()-[r:RelatesToNode_] SET fact_embedding`. The test asserts post-replay graph state matches an expected snapshot.
- **FR-007.** Telemetry: `mutations_replayed` and `lines_skipped` counters MUST distinguish "first-token mismatch" (now rare — only true non-mutations) from "MATCH-prefixed mutation accepted and executed." Use distinct telemetry phases or a dedicated counter so operators can verify the fix is working in production.

## Success Criteria *(mandatory)*

- **SC-001.** Replaying demo-notebook's 2026-04-11 WAL (committed at `demo-notebook` repo, branch/commit TBD) into an empty `.lcg/db/` reconstructs a graph whose entity, edge, and episode counts are within 5% of the source workspace's pre-cutover `knowledge_status` snapshot.
- **SC-002.** After replay, `content_embedding` is non-null on ≥ 90% of `Episodic` nodes (matching the post-enrichment state of the source workspace, not the pre-enrichment empty state).
- **SC-003.** `mutations_replayed` after demo-notebook replay is ≥ 800 (compared to today's 103). The exact target number is derived from the WAL histogram: ~489 CREATE/MERGE + ~872 MATCH-prefixed = ~1361 attempted, minus genuine failures. ≥ 800 is a conservative floor.
- **SC-004.** New regression test passes: the fixture WAL containing MATCH-SET embedding write, MATCH-SET attribute update, and MATCH-SET edge-fact update produces the expected post-replay graph state.
- **SC-005.** Existing replay tests pass unchanged — CREATE/MERGE paths are not regressed.
- **SC-006.** No false-positive mutation classifications: a fixture containing pure `MATCH … RETURN` reads is correctly classified as non-mutation (Story 3 acceptance scenario 1).

## Assumptions

- **A1.** The full set of mutation Cypher shapes produced by Python graphiti is finite and observable from the demo-notebook WAL plus a survey of `graphiti_core/utils/maintenance/*` and the graphiti fork's writer paths. We do not need to handle shapes only theoretically possible in Cypher.
- **A2.** `Conn::raw_query` correctly handles MATCH-then-SET mutations against lbug today. The replay path's only gap is *deciding to call it*; once called, lbug executes the mutation. (Verification step: spot-check by manually invoking one of the skipped Cypher strings via `Conn::raw_query` against a partial-replay DB and confirming the embedding gets written.)
- **A3.** It is acceptable for the first-pass implementation to mis-classify exotic Cypher edge cases (e.g., keyword-in-string-literal). The regression test covers the cases we actually see.
- **A4.** The schema gap (Python graphiti vs Rust lbug — column naming, type coercions, vector indices) has already been resolved for CREATE/MERGE paths and applies equally to MATCH-then-SET paths. If schema discrepancies emerge for the MATCH paths, they are out of scope for this issue and should be filed separately.

## Out of Scope

- Re-implementing a Cypher parser. Sub-clause detection should be a small targeted scan, not a full grammar.
- Splitting `lines_skipped` into `unrecognised` vs `failed` — that's the sibling Fabrik issue.
- Supporting Cypher shapes graphiti has never written (e.g., `CALL { … } IN TRANSACTIONS`, `FOREACH`).
- Re-writing existing WAL files in any format — replay is read-only against the persisted WAL.

## Source References

- **`liminis-graph-core/src/replay.rs`** — the current implementation; the first-token whitelist lives at lines 156-169.
- **demo-notebook 2026-05-28 Check G** — the live test that surfaced this. See session notes in liminis-graph chat history.
- **graphiti fork** at `~/dev/liminis-project/graphiti` (`liminis` branch) — source of truth for what Cypher shapes the Python writer emits. Grep for `MATCH (` followed by `SET ` in `graphiti_core/utils/maintenance/`.
- **Sibling issue**: `replay: surface failed mutations distinctly from skipped` (to be filed) — splits `lines_skipped` into `unrecognised_lines` and `failed_lines`. This issue can land independently; the sibling improves observability of whatever residual misses remain.
- **Project-level context**: this is part of the broader `liminis-graph` cutover from Python `graphiti_service.py`. WAL replay fidelity is the migration story for pre-cutover workspaces. See `liminis-graph/ideas/cutover-plan.md`.
