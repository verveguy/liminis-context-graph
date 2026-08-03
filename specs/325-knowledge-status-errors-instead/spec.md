# Feature Specification: `knowledge_status` errors instead of reporting degraded state when a core table is missing

**Feature Branch**: `fabrik/issue-325`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "knowledge_status errors instead of reporting degraded state when a core table is missing"

## Background

`mcp_real_corpus_admin_data_e2e` is the last failing job in `real-corpus-e2e`. Its User Story 6 block deliberately induces a genuine, reversible index-build failure by renaming the `Entity` table away via `knowledge_query_cypher`, then checks that the service reports that state honestly:

```rust
let status1 = structured(&status1_resp, "knowledge_status (US6 index-less)");
assert_eq!(
    status1["indices_built"],
    json!(false),
    "indices_built must accurately report false, not stale-true: {status1}"
);
```

`knowledge_status` does not return a status. It errors:

```
database error: Query execution failed: Binder exception: Table Entity does not exist.
```

The preceding assertion in the same block — that `knowledge_build_indices` *must* genuinely fail while `Entity` is renamed away — passes. So the induced condition is real and the service correctly reports failure for the operation that cannot work. It is specifically the **status endpoint** that falls over.

### Why this matters beyond the test

`knowledge_status` is the health-check surface. The README directs operators to it to confirm search-readiness (`indices_built`), and the degraded-mode model in ADR-0009 is built on the service remaining *answerable* when the database underneath it is damaged — that is why the socket is bound before the database is opened. A status endpoint that throws when the graph is broken fails at exactly the moment it is most needed: the operator asking "what state am I in?" gets an exception instead of an answer.

It is also the second instance this cycle of `knowledge_status` misreporting under duress. #297 fixed `indices_built` staying stale after a runtime recovery. This is the same theme — status truthfulness in degraded conditions — with a harder failure mode.

### Provenance: regression, bisected to `f51c40c`

An earlier framing of this section claimed the failure was pre-existing and merely revealed by #301. That was inferred from the job being red across a long run of merges, not from a bisect. A bisect shows the opposite:

- **Last green run**: `5259bfb`, 2026-07-26T14:14 — the immediately preceding merge.
- **First red run**: `f51c40c` — PR #251 / issue #239 / ADR-0046 — merged 2026-07-26T14:54.
- 38 consecutive failing runs since (roughly one week).

`real-corpus-e2e` is now 4/5 green (from 2/5). This is the remaining job.

### Likely cause (unverified — Research must confirm)

ADR-0046 changed `handle_rebuild_from_wal` to **clear the database before replay** (previously it dropped indexes but never cleared, causing duplicate-PK failures on every node `CREATE`). That fix is sound in intent; the e2e failure looks like a consequence of it. Two candidate mechanisms, with different remedies:

1. The schema is not recreated after the clear, so `Entity` genuinely does not exist rather than merely being renamed away.
2. A handle is left pointing at the replaced database (`ArcSwap<Db>`, ADR-0003) — the status path queries a `Db` the clear swapped out from under it.

It is also possible the product is not at fault at all: the test may be stale relative to the new clear-first semantics. **Research must determine which of these applies before Plan proceeds** — see FR-004.

Observed sequence (second server instance, scope `[Admin, Read, Cypher]`, after a successful replay of 12,482 mutations):

```
knowledge_query_cypher    success: true
knowledge_build_indices   success: false
knowledge_status          success: false
  -> "database error: Query execution failed: Binder exception: Table Entity does not exist."
```

### Repro

```
cargo test --release --test mcp_real_corpus_admin_data_e2e -- --ignored
```

Takes ~2 minutes (the full three-test workflow is ~4 minutes), so this is cheap to iterate on locally.

### Blast radius and the 0.12.0 milestone

Everything merged since 2026-07-26 landed without e2e verification, including #306, #307, #310, #311 and #314 — all in the 0.12.0 milestone. This does not automatically mean anything is wrong with them; the other four `real-corpus-e2e` jobs still pass. It means the assurance the suite exists to provide, the release did not have. Whether this blocks the 0.12.0 tag is a maintainer judgement call and is not decided by this spec.

### Why it went unnoticed

`real-corpus-e2e` runs only on push to `main` and via `workflow_dispatch`, never on pull requests, so nothing surfaced this for a week. #298 (merged in #300) added a failure notifier after these failures began; this issue is its first real test. If notification proves insufficient, moving this suite onto the PR path (at roughly 4 minutes/PR cost) would be a stronger remedy — noted here as a candidate follow-up, out of scope for this fix (see Out of Scope).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Status answers when the graph is damaged (Priority: P1)

An operator whose database is missing or partially damaged calls `knowledge_status` to find out what state the service is in.

**Why this priority**: This is the core defect — the health-check surface itself is what fails, at exactly the moment an operator needs it.

**Independent Test**: With the `Entity` table renamed away on an otherwise-open database, call `knowledge_status` directly and confirm it returns a structured response (not a JSON-RPC error) with `indices_built: false` and a signal distinguishing "not queryable" from "empty."

**Acceptance Scenarios**:

1. **Given** a service whose `Entity` table is absent, **When** `knowledge_status` is called, **Then** it returns a status object rather than a JSON-RPC error.
2. **Given** that state, **When** the status is inspected, **Then** `indices_built` is `false` — accurate, not stale-true.
3. **Given** that state, **When** the status is inspected, **Then** the response conveys that the graph is not queryable, rather than reporting healthy-looking zeros indistinguishable from an empty graph.

---

### User Story 2 - The e2e suite goes green (Priority: P1)

**Why this priority**: This is the release-blocking observable outcome — the suite exists to catch exactly this class of regression, and it must be trustworthy again.

**Independent Test**: Run `real-corpus-e2e` on `main` after the fix lands and confirm all five jobs pass.

**Acceptance Scenarios**:

1. **Given** `main` after this change, **When** `real-corpus-e2e` runs, **Then** all five jobs pass, and User Story 6's assertions are unchanged unless Research determines the test itself needs correcting (see FR-004).

---

### Edge Cases

- Missing table vs. missing *index* are different failures with similar binder exceptions — the fix must not conflate them, since the missing-index path already has auto-heal (ADR-0025/ADR-0036).
- Degraded-mode startup (ADR-0009) already has a path where the database never opened. A missing table on an *open* database is a distinct state; check whether the existing degraded reporting can represent it or needs extending.
- The rename in User Story 6 is reversible and the test renames back. Any caching of schema state must not survive the rename-back and report stale-broken afterwards.
- The clear-before-replay path introduced by ADR-0046 may itself be the source of the missing table (see Likely cause) rather than the test's rename — the fix must address whichever is the true root cause, not just the symptom observed via the rename repro.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `knowledge_status` MUST NOT return a JSON-RPC error because a table it queries is missing. It must degrade to a status response.
- **FR-002**: `indices_built` MUST report `false` in that state.
- **FR-003**: The response MUST distinguish "graph not queryable" from "graph empty". A caller cannot be left unable to tell a broken database from a fresh one — counts of zero with no other signal would be a regression in disguise.
- **FR-004**: User Story 6's assertions MUST NOT be weakened, `#[ignore]`d, or removed, **unless Research determines that the regression is in the test itself** — i.e., that it asserts behavior ADR-0046's clear-first semantics legitimately changed, rather than the product being wrong. In that case this constraint does not apply, and the correct fix is to update the test's expectations (with the reasoning recorded in the PR), not to work around a correct test. Research MUST record which case applies before Plan proceeds.
- **FR-005**: Audit the other read-side methods reachable in this state and state in the PR which ones survive a missing core table and which do not. Fixing only `knowledge_status` while `knowledge_get_episodes` (etc.) still throws would leave the same class of gap one call away.
- **FR-006**: The fix MUST NOT mask genuine errors — a query failing for a reason *other* than a missing table must still surface as an error, not be swallowed into a degraded status.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: `mcp_real_corpus_admin_data_e2e` passes, with User Story 6 unchanged unless Research determines the test itself was stale per FR-004, in which case the PR documents the corrected assertions and why.
- **SC-002**: `real-corpus-e2e` is green on all five jobs — the first fully green run since 2026-07-26.
- **SC-003**: A test asserts that a query failing for a non-missing-table reason still errors, proving FR-006 rather than assuming it.
- **SC-004**: The PR enumerates the read methods audited under FR-005 with their behaviour.
- **SC-005**: The PR states which candidate mechanism from "Likely cause" (schema not recreated after clear, or a stale `Db` handle) is the actual root cause, or names the true root cause if neither applies.

## Assumptions

- The regression is real and dates from `f51c40c` (PR #251 / issue #239 / ADR-0046, merged 2026-07-26T14:54); this was established by bisect, not inferred from job history.
- The exact mechanism (schema not recreated after clear vs. stale `Db` handle vs. something else) is not yet known and is Research's job to determine before Plan chooses a fix.
- Whether this regression should block the 0.12.0 tag is a maintainer decision outside this spec's scope.

## Out of Scope

- The `indices_built`-after-runtime-recovery gap (#297, fixed).
- Missing-index auto-heal (ADR-0036).
- Moving `real-corpus-e2e` onto the PR-triggered path — a candidate follow-up raised by the "why it went unnoticed" analysis, but a separate CI-cost/coverage tradeoff from this fix.

## Source References

- `crates/service/tests/mcp_real_corpus_admin_data_e2e.rs` — User Story 6 block, the failing assertion.
- ADR-0009 — degraded-mode startup and recovery; the socket-before-database ordering this issue extends.
- ADR-0046 / PR #251 / issue #239 — the clear-before-replay change bisected as the regression's origin.
- ADR-0003 — `ArcSwap<Db>`, relevant to the stale-handle candidate mechanism.
- ADR-0025 / ADR-0036 — missing-index auto-heal, the adjacent-but-distinct failure mode called out in Edge Cases.
- #297 — the prior `knowledge_status` truthfulness fix.
- #301 — fixed the earlier failure that had been masking this one.
- #298 / #300 — the CI failure notifier that surfaced this job's failures.
- Commits `5259bfb` (last green) / `f51c40c` (first red) — the bisect endpoints.
