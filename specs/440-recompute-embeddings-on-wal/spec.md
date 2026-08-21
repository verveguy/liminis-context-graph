# Feature Specification: Recompute embeddings on WAL replay with a content-addressed cache

**Feature Branch**: `fabrik/issue-440`
**Created**: 2026-08-19
**Status**: Specified
**Input**: User description: "Recompute embeddings on WAL replay with a content-addressed cache, instead of replaying stored vectors"

## Background

The WAL stores embedding vectors alongside the text they were computed from. Measured on the real-corpus WAL capture from #217 (16 files, 12,482 records, 74.4 MB):

| | bytes | share of WAL |
|---|---|---|
| WAL total | 74,396,854 | 100% |
| **embedding vectors** | **66,895,328** | **89.9%** |
| all string content | 3,196,460 | 4.3% |

4,126 vectors, all 768-dim, averaging **16,212 bytes each** — roughly 21 bytes per `f32`, because each float is serialised as a full JSON decimal literal. The raw vector is 3,072 bytes; the encoding costs 5.3x on top.

By vector kind, against the text each was computed from:

| param | n | avg source text | avg vector | ratio |
|---|---|---|---|---|
| `name_embedding` | 1,506 | 16 B | 16,214 B | 958x |
| `fact_embedding` | 2,392 | 72 B | 16,212 B | 223x |
| `content_embedding` | 228 | 1,576 B | 16,209 B | 10.3x |

Most of the WAL is 16 KB vectors attached to entity names of a dozen-odd characters.

**Why this is a correctness problem, not just a size problem.** Size is the symptom. The design defect is that a derived cache is persisted in the durable log. Replay binds stored embedding params verbatim into the Cypher template without ever consulting the embedder. So a graph rebuilt from WAL carries the vectors of whatever embedder was configured at capture time, regardless of what the querying process uses now.

Vector search requires the query-time embedder to match the vectors it searches against. Storing vectors therefore pins every consumer of a WAL to the exact embedding model that wrote it, permanently — no upgrade, no substitution, no improvement in embedding technology, without discarding the log. Recomputing on replay imposes a strictly weaker requirement: replay-time and query-time embedders must agree, which is normally the same process on the same machine. It is also self-healing — upgrade the model, rebuild, and queries stay coherent.

Worse, nothing currently detects the mismatch. The embedding model is probed at startup and surfaced in status output, but nothing compares it against what the WAL was written with. A model change silently degrades every vector search over replayed data, with no error and no warning.

**Why this is cheap.** Every embedding's source text is already in the same WAL record. Verified across the whole #217 capture, with zero misses: `fact_embedding` <- `fact` (2,392/2,392), `name_embedding` <- `name` (1,506/1,506), `content_embedding` <- `content` (228/228). Recomputation needs no LLM, no re-extraction, and no data that is not already present. Dropping the vectors costs zero additional bytes.

**Built-in validation oracle.** Existing WAL captures contain both the source text and the vector computed from it. That makes the #217 fixture a ready-made correctness oracle: recompute each vector from its co-located text and compare against the stored one. This measures embedder reproducibility empirically — across processes, machines, and runtime backends — before anything is discarded. Any drift found here is a pre-existing property of the embedder, not a regression introduced by this change, and it is better surfaced by this work than by degraded search quality in the field.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Replay recomputes vectors from source text (Priority: P1)

An operator rebuilds a graph from an existing WAL. The rebuilt graph's embedding vectors are computed by the embedder currently configured on the running process, not copied verbatim from whatever the WAL happened to store — so vector search over the rebuilt graph is coherent with the embedder actually doing the querying, even if the WAL was captured under a different model.

**Why this priority**: This is the core correctness fix the issue exists for — today, replay silently perpetuates stale vectors from capture time.

**Independent Test**: Replay a WAL captured under one embedder configuration while running a different (but dimension-compatible) embedder; verify the rebuilt graph's vectors reflect the running embedder's output for the same source text, not the stored values.

**Acceptance Scenarios**:

1. **Given** a WAL record carries an embedding vector (`name_embedding`, `fact_embedding`, or `content_embedding`) and its co-located source text (`name`, `fact`, or `content` respectively), **When** the record is replayed, **Then** the vector bound into the graph is derived by invoking the currently configured embedder on that source text, not the stored vector value.
2. **Given** the same source text appears in more than one WAL record replayed in the same session, **When** those records are replayed, **Then** the embedder is invoked once for that (text, model) pair and the cached result is reused for subsequent occurrences.

---

### User Story 2 - Model mismatch is surfaced, never silent (Priority: P1)

An operator (or the system itself, at startup) can tell whether the embedder currently running matches the embedder that produced the vectors currently active in the graph. If it does not, this is visible — in logs and/or status output — rather than manifesting only as degraded, unexplained search quality.

**Why this priority**: The issue explicitly calls this out as "a live latent defect regardless of the rest of this issue" — today there is no detection at all, so a model change degrades search with zero diagnostic signal.

**Independent Test**: Configure a WAL (or graph) known to have been produced under embedder identity A, then start the service configured with embedder identity B, and verify a mismatch signal is emitted (log line and/or status field) rather than the process proceeding as if nothing changed.

**Acceptance Scenarios**:

1. **Given** a WAL is stamped with the embedding model identifier and dimension active when it was written, **When** that WAL is replayed by a process running a different embedder identity, **Then** the mismatch is detected and surfaced visibly.
2. **Given** a graph's currently active vectors were computed under embedder identity A, **When** the service starts up (or is queried) running embedder identity B, **Then** the mismatch is detected and surfaced visibly, independent of whether a replay has occurred in this session.

---

### User Story 3 - Recompute cost is measured, not assumed (Priority: P2)

A maintainer evaluating this change can see, from a benchmark, how much CPU cost recompute-during-replay adds relative to today's I/O-bound stored-vector replay, and how much the embedding cache mitigates that cost.

**Why this priority**: The issue flags this as an open question rather than a settled requirement — replay shifts from purely I/O-bound to partly CPU-bound (4,126 forward passes for the #217 fixture, once per rebuild), and the issue asks that this be measured and reported rather than worked around silently.

**Independent Test**: Run a benchmark that replays the #217 fixture (or an equivalent corpus) with recompute and the cache enabled, and confirm it reports a wall-clock or throughput figure that can be compared against the pre-change baseline.

**Acceptance Scenarios**:

1. **Given** the #217 real-corpus WAL capture (or an equivalent fixture), **When** it is replayed with recompute enabled, **Then** a benchmark reports the measured cost, so the finding is visible whether or not it meets any particular budget.

---

### User Story 4 - Recompute is empirically validated against stored vectors (Priority: P1)

Before relying on recompute in place of stored vectors, the change is validated against real data: every vector in the #217 capture is recomputed from its co-located source text and compared to the vector already stored there.

**Why this priority**: This is the issue's "built-in validation oracle" — it is the primary evidence that recompute is safe to rely on, and it is explicitly called out as something that should exist before vectors are ever dropped (a later, out-of-scope issue).

**Independent Test**: A test recomputes all 4,126 vectors in the #217 fixture from their co-located source text and compares each to its stored counterpart, reporting agreement/drift per embedding kind (`name_embedding`, `fact_embedding`, `content_embedding`).

**Acceptance Scenarios**:

1. **Given** the #217 fixture's stored (text, vector) pairs, **When** each vector is recomputed from its source text using the same embedder, **Then** the comparison result (match or measured drift) is reported for all three embedding kinds, not silently assumed.

---

### Edge Cases

- A WAL record carries an embedding vector param but no co-located source text (e.g., malformed or unexpected record shape) — replay must fall back to binding the stored vector verbatim rather than failing.
- The same source text recurs across many WAL records (e.g., a repeated entity name) — the cache must serve repeats without re-invoking the embedder, within and potentially across replay sessions.
- A WAL was written under a different embedding dimension than the currently configured embedder (e.g., via a dimension override) — this counts as a model-identity mismatch, not just a model-name mismatch.
- A WAL predates this change entirely and carries no model-identity stamp at all — it must still replay without any migration step, using recompute wherever source text is present (see Compatibility below).
- Replay happens concurrently with live writes touching overlapping source text — the cache must not return stale or incorrect vectors under concurrent access.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: During WAL replay, for a record whose embedding vector param (`name_embedding`, `fact_embedding`, `content_embedding`) has co-located source text present in the same record, the system MUST derive the vector by invoking the currently configured embedder on that source text, rather than binding the vector value stored in the WAL.
- **FR-002**: When a WAL record's embedding vector param has no co-located source text available, the system MUST fall back to binding the stored vector value verbatim, preserving today's replay behavior for that record.
- **FR-003**: The system MUST provide a content-addressed embedding cache keyed by (source text, embedding model identity) that returns a previously computed vector when the same source text is re-embedded under the same model, avoiding a duplicate embedder call.
- **FR-004**: The embedding cache MUST be stored outside the WAL (never written into the durable log) and MUST be safe to discard at any time — its absence or deletion must not cause replay to fail or to produce incorrect results, only to recompute at full cost.
- **FR-005**: The system MUST record the embedding model identifier and vector dimension that were active when a WAL was written, associated with that WAL.
- **FR-006**: At replay time, the system MUST compare the running embedder's model identity (identifier + dimension) against the identity recorded for the WAL being replayed.
- **FR-007**: At query time (including startup, independent of whether a replay occurred in the current session), the system MUST compare the running embedder's model identity against the model identity under which the graph's currently active vectors were computed.
- **FR-008**: When the comparison in FR-006 or FR-007 detects a mismatch, the system MUST surface it clearly and visibly (e.g., a warning-level log entry and/or a field in status output) — it MUST NOT proceed silently as it does today.
- **FR-009**: Existing WAL files that predate this change (no model-identity stamp, still carrying vectors) MUST continue to replay without any migration step, applying FR-001/FR-002 exactly as they would to a newly written WAL.
- **FR-010**: Recompute correctness MUST be validated against the existing #217 real-corpus WAL capture: for all three embedding kinds, each vector is recomputed from its co-located source text and compared against the vector already stored in that capture, and the result (agreement or measured drift) is reported.
- **FR-011**: The performance cost of enabling recompute during replay MUST be measured (e.g., via a benchmark exercising the #217 real-corpus WAL capture or an equivalent fixture) and the measured cost reported, so the cache's (FR-003) effectiveness at bounding that cost can be assessed.

### Key Entities *(if the feature involves data)*

- **WAL Record**: An existing entity; for this feature, the records of interest are those carrying one of the recognized embedding vector params (`name_embedding`, `fact_embedding`, `content_embedding`) alongside their co-located source-text param (`name`, `fact`, `content`).
- **Embedding Cache Entry**: Keyed by (source text, embedding model identity); value is the computed vector. Lives outside the WAL and is freely disposable/rebuildable.
- **Model Identity Stamp**: The (embedder identifier, vector dimension) pair recorded against a WAL at write time, and separately, the identity under which a graph's currently active vectors were computed — both compared against the running embedder's identity.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Recomputing every vector in the #217 fixture (4,126 vectors, all three kinds) from its co-located source text and comparing it to the originally stored vector produces a reported agreement/drift result for 100% of vectors — no silent gaps.
- **SC-002**: Replaying a pre-existing WAL capture under a matching embedder identity produces a graph whose vector-search results are unchanged from before this change.
- **SC-003**: In tests exercising a model-identity mismatch (WAL vs. running embedder, or graph vs. running embedder), the mismatch is observably surfaced (log and/or status output) in 100% of exercised cases.
- **SC-004**: A benchmark measuring replay of the #217 fixture (or an equivalent corpus) with recompute and the cache enabled reports the added cost relative to the pre-change stored-vector replay path.
- **SC-005**: Replaying a WAL containing repeated identical (source text, model) pairs invokes the embedder fewer times than the number of matching records, demonstrating the cache bounds redundant computation.

## Assumptions

- A model-identity mismatch (FR-008) is surfaced as a visible warning/status signal, not treated as a hard startup or replay failure. This follows directly from the issue's own framing of recompute as "self-healing" — recompute (FR-001) is the corrective mechanism when source text is present, so blocking startup on a mismatch would contradict that framing. The exact surfacing mechanism (log level, status field name, etc.) is a Research/Plan decision.
- The three recognized embedding kinds are `name_embedding`, `fact_embedding`, `content_embedding`, bound to `name`, `fact`, `content` source-text params respectively, per the issue's measurements on the #217 capture. Research should confirm no other embedding kinds exist in the schema before implementation.
- Cache storage medium, content-hashing algorithm, and where the model-identity stamp is physically recorded (WAL header vs. sidecar metadata vs. elsewhere) are technical decisions left to Research/Plan, not specified here.
- The bench file named in the issue, `crates/core/benches/real_corpus_replay_perf.rs`, does not currently exist in the repository (only `crates/core/benches/wal_replay_bench.rs` does, at 248 bytes). Whether to create the named file, extend the existing one, or use another mechanism to satisfy FR-011 is a Research/Plan decision.

## Out of Scope *(optional)*

- Stripping embedding vectors from newly-written WAL files. Recompute must first be proven correct while vectors are still present to validate against (FR-010/SC-001); this is a deliberate, sequenced follow-up issue.
- Deciding the on-disk encoding for any vectors that remain in the WAL after this change (e.g., base64 `f32`, lossless and ~3x smaller than the current decimal JSON encoding, vs. other options) — belongs to the follow-up issue above, not this one.

## Source References *(optional)*

- Issue #217 — origin of the real-corpus WAL capture used throughout as fixture and validation oracle.
- `crates/core/src/replay.rs`, `crates/core/src/legacy_wal.rs` (`strip_vecf32`, `expand_bulk_property_set`) — current verbatim-bind replay path this feature changes.
- `crates/service/src/main.rs` — current embedder probe/startup logic (embedding model + dimension resolution).
- `crates/core/src/handlers.rs` — current status output surfacing `embedding_model`.
- `crates/core/benches/wal_replay_bench.rs` — existing replay benchmark, closest analog to the one referenced in FR-011.
