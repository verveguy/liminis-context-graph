# Feature Specification: Windows CI must prove the vector/FTS extensions work, not just load

**Feature Branch**: `fabrik/issue-587`
**Created**: 2026-09-15
**Status**: Specified
**Input**: User description: "Windows CI proves extensions load but never that they work — add index create/query to the e2e"

## Background

`windows.yml`'s end-to-end job runs `socket_service_e2e`, which starts the service over
the named pipe and exercises the IPC surface. After #586 it does that with a PATH built
from an allowlist and a hard assertion that no OpenSSL DLL is resolvable, so a pass
genuinely proves the **bundled extension DLLs were found and loaded**.

What it does **not** prove is that the extensions *work*. `socket_service_e2e` never
creates or queries a vector or full-text index, so every code path inside
`libvector`/`libfts` beyond `LOAD EXTENSION` is untested on Windows in CI.

This is not hypothetical. While validating the lbug 0.20.4 upgrade (#561) the published
`win_amd64` extensions for the entire 0.20 line were found to crash the process:

- **v0.20.0 extensions**: access violation *inside* `libvector.lbug_extension` /
  `libfts.lbug_extension` during `LOAD EXTENSION`.
- **v0.20.4 core + v0.20.0 extensions**: `LOAD` succeeds, then `0xC0000005` in
  `MSVCP140.dll` at `CREATE_VECTOR_INDEX` / `CREATE_FTS_INDEX`, followed by a
  `0xC0000409` fail-fast abort.

Bisected: v0.18.1 and v0.19.0 are fine, v0.20.0 is broken — a regression in the
published Windows extension builds. Reproducible with LadybugDB's own `lbug.exe` and no
lcg code involved.

The second failure mode above is invisible to CI today: `LOAD` succeeds, so
`socket_service_e2e` passes, and nothing else in the job touches an index. A `windows`
job going green on a 0.20.x pin would therefore be read as "Windows is fine" when
vector and full-text search both fault the process on first use.

This gap has the same shape as the one that let the original OpenSSL contamination
survive undetected for months (#581): the Windows e2e passed against DLLs that happened
to satisfy imports without proving the *bundled* ones worked. Both failures share a root
cause — the job asserted the easy half (a dependency resolves) and never the hard half
(it functions). Closing this gap is what lets a future `windows` green actually gate a
Windows release, and is a prerequisite for trusting CI on any future 0.20.x Windows
attempt.

This issue adds the missing coverage. It does not fix the upstream lbug extension
regression itself (tracked separately) and does not change which lbug version lcg is
currently pinned to (`0.20.3`, per `.cargo/config.toml`).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Windows CI fails when the extensions load but don't work (Priority: P1)

As someone relying on a green `windows` CI job to mean "Windows works," I need that job
to actually exercise the vector and full-text index extensions — not just load them —
so that a regression like the 0.20.0 `win_amd64` crash (extensions load fine, then fault
the process on first real use) is caught by CI instead of discovered by hand during a
version-upgrade investigation.

**Why this priority**: This is the entire content of the issue. Without it, a passing
Windows job carries a false assurance about vector/FTS functionality specifically —
the exact gap that let a real regression through undetected.

**Independent Test**: Run the Windows e2e job against a working set of extensions and
confirm it exercises index creation and querying (not just load) and passes only because
real rows came back. Separately, confirm (by code inspection or by pointing the job at a
known-broken extension build) that a load-succeeds-then-crashes-on-use regression causes
the job to fail rather than pass or hang.

**Acceptance Scenarios**:

1. **Given** the Windows e2e job running against working vector/FTS extensions,
   **When** the job inserts entities carrying literal embedding values, creates a
   vector index, and queries it, **Then** the query returns at least one row and the
   job passes.
2. **Given** the same working extensions, **When** the job creates a full-text index
   over the inserted entities and queries it, **Then** the query returns at least one
   row matching the inserted content and the job passes.
3. **Given** extensions that load successfully but crash the service process when
   `CREATE_VECTOR_INDEX`/`QUERY_VECTOR_INDEX` (or the FTS equivalents) are invoked — the
   0.20.0 regression shape — **When** the Windows e2e job runs, **Then** the job fails
   within its existing timeout (it does not hang and does not pass), and the failure
   output identifies whether the vector path or the FTS path was implicated.

---

### Edge Cases

- A native crash inside the extension DLL during `CREATE_VECTOR_INDEX` or
  `QUERY_VECTOR_INDEX` takes down the service child process; the pipe client must
  detect this (e.g. a closed pipe / process exit) and fail the test promptly, not hang
  waiting on a response that will never arrive.
- A crash triggered during vector-index testing must not prevent the FTS-index
  coverage from being exercised and reported in the same CI run, and vice versa — the
  two must be attributable independently, not merged into one ambiguous failure.
- A query that returns zero rows (e.g. a silent no-op rather than an error) must be
  treated as a failure — the assertion is on rows actually coming back, not merely on
  the JSON-RPC call completing without an `error` field.
- The new coverage must run inside the same constrained-PATH step (or an equivalently
  isolated one) that #586 already established, so a pass continues to mean the
  *bundled* DLLs did the work — not some other OpenSSL/runtime DLL that happens to be
  resolvable on the runner.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The Windows CI e2e coverage MUST, after the service reports healthy,
  insert a small number of entities carrying literal `FLOAT[]` embedding values —
  no embedding service call and no network dependency required.
- **FR-002**: The e2e coverage MUST create a vector index over the inserted embeddings
  (e.g. via `CREATE_VECTOR_INDEX`) issued through the same out-of-process pipe client
  the existing e2e test already uses to talk to the service, not a lower-level
  in-process API.
- **FR-003**: The e2e coverage MUST run a nearest-neighbor query against that vector
  index (e.g. via `QUERY_VECTOR_INDEX`) and assert that at least one row is returned
  with structurally sane content (e.g. an expected UUID/distance field is present) —
  not merely that the call returned without an `error`.
- **FR-004**: The e2e coverage MUST create a full-text search index over the inserted
  entities (e.g. via `CREATE_FTS_INDEX`) issued through the same pipe client.
- **FR-005**: The e2e coverage MUST run a full-text query against that index (e.g. via
  `QUERY_FTS_INDEX`) and assert that at least one row is returned and that it matches
  the expected inserted content.
- **FR-006**: If the service process crashes or aborts during any of the above
  operations (e.g. a native access violation inside the extension DLL), the test run
  MUST fail with a clear, timely error rather than hang until a CI-level timeout.
- **FR-007**: A failure during vector-index testing MUST be distinguishable in the CI
  output from a failure during FTS-index testing, so a reader can tell which extension
  is implicated without reproducing locally.
- **FR-008**: The new coverage MUST run as part of the existing Windows CI job
  (`windows.yml`'s e2e step, or an equivalently PATH-isolated step per #586) and MUST
  NOT introduce a dependency on network access or an external embedding/extraction
  service.
- **FR-009**: The new coverage MUST continue to run under the constrained,
  allowlist-built PATH established by #586 (no OpenSSL DLL resolvable other than the
  bundled ones), so that a pass keeps meaning the *bundled* extension DLLs — not some
  other DLL on the runner's PATH — did the work.

### Key Entities

- **Test entity row**: A minimal graph entity inserted purely to give the vector/FTS
  indexes something to index — literal embedding vector, name, and summary text
  sufficient for both a vector query and an FTS query to have a deterministic expected
  match.
- **Vector index / FTS index**: The two extension-provided index types under test,
  created and queried via raw Cypher (`CREATE_VECTOR_INDEX`/`QUERY_VECTOR_INDEX`,
  `CREATE_FTS_INDEX`/`QUERY_FTS_INDEX`) issued over the named-pipe JSON-RPC transport.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A Windows CI run against working vector/FTS extensions passes only once
  it has received actual rows back from both a vector-index query and an FTS-index
  query — a job that never got past `LOAD EXTENSION` can no longer pass.
- **SC-002**: A Windows CI run against extensions that crash the process during index
  creation or querying (the 0.20.0 regression shape) fails within the job's existing
  90-minute timeout, with output that identifies whether the vector or FTS path was
  implicated.
- **SC-003**: The new coverage adds no network dependency and no new external service
  requirement to the Windows job.

## Assumptions

- The existing `knowledge_query_cypher` JSON-RPC method (already exposed over the
  named-pipe transport with `Scope::Cypher`) is an acceptable mechanism for issuing the
  `CREATE_VECTOR_INDEX`/`QUERY_VECTOR_INDEX`/`CREATE_FTS_INDEX`/`QUERY_FTS_INDEX`
  statements from the e2e's pipe client — no new JSON-RPC method is required for this
  issue.
- Embedding dimensionality and exact Cypher literal shapes will be chosen to match the
  schema already exercised by `crates/core/tests/ldb_spike_ipc.rs`'s CLI-level
  vector/FTS repro (`test_hnsw_vector_query`, the FTS equivalent), which the issue
  offers as a reasonable shape to copy.
- "Worth running it in its own process from the pipe client" (per the issue's Proposal)
  is interpreted as: exercised through the same out-of-process, real-binary pipe-client
  pattern the existing e2e test already uses, with vector and FTS coverage separated
  enough (e.g. distinct test functions/cases, or distinct service invocations) that a
  crash in one is attributable and does not prevent the other from being exercised and
  reported. The exact test structure is left to the Research/Plan stage.
- This issue is scoped to proving the extensions *function*; it does not need to
  reproduce or characterize the specific 0.20.0 crash for verification — the new
  coverage is validated by running green against known-working extensions, and by
  inspection/reasoning that it would fail against the known-broken 0.20.0 build.

## Out of Scope

- Fixing the actual lbug 0.20.x extension crash upstream (tracked separately, per the
  issue's Notes).
- Changing which lbug version lcg is pinned to.
- Adding equivalent vector/FTS index coverage to the Linux/macOS e2e paths in
  `ci.yml` — this issue is scoped to the Windows job, since that's where the gap and
  the historical regression were found. A cross-platform follow-up may be worth filing
  separately but is not required here.
- Adding an embedder-backed (network-dependent) test of vector search relevance or
  quality — literal `FLOAT[]` vectors are explicitly sufficient per the issue, since the
  goal is proving the extension code paths execute and return data, not evaluating
  search quality.
- Changing the OpenSSL-DLL PATH-isolation mechanism itself (#581/#586) — this issue
  reuses that guarantee, it does not modify it.

## Source References

- `crates/service/tests/socket_service_e2e.rs` — the existing Windows e2e test to
  extend.
- `crates/core/tests/ldb_spike_ipc.rs` (`test_hnsw_vector_query` and its FTS
  counterpart) — the CLI-level vector/FTS index repro the issue references as a
  reasonable shape to copy.
- `crates/core/src/handlers.rs` (`handle_query_cypher`) / the `knowledge_query_cypher`
  JSON-RPC method — the raw-Cypher escape hatch usable from the pipe client.
- `.github/workflows/windows.yml` — the CI job to extend.
- Issue #561 (lbug 0.20.4 upgrade investigation, where the crash was found), #581
  (original OpenSSL contamination), #586 (PATH allowlist fix this issue's coverage
  must run under).
