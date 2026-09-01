# Feature Specification: Vectors are a local cache — stop writing them to the WAL, ignore them on replay

**Feature Branch**: `fabrik/issue-526`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "Vectors are a local cache: stop writing them to the WAL, ignore them on replay"

## Background

Embedding vectors are a **local cache of the database**, not content of the log. Today the WAL writer
still emits them and replay can still bind a stored vector read back from the WAL. This issue makes
that unconditional: the writer stops emitting vector params, replay always recomputes vectors from
co-located source text, and a vector found in an older WAL is ignored outright.

#440 (PR #443, merged) measured the cost on the #217 real-corpus capture (16 files, 12,482 records,
74.4 MB):

| | bytes | share of WAL |
|---|---|---|
| WAL total | 74,396,854 | 100% |
| **embedding vectors** | **66,895,328** | **89.9%** |
| all string content | 3,196,460 | 4.3% |

4,126 vectors, all 768-dim, averaging 16,212 bytes — a 3,072-byte raw vector costing 5.3× because each
`f32` becomes a JSON decimal literal. A `name_embedding` is **958×** the 16-byte name it came from.

#440 made replay recompute from co-located source text, with a content-addressed cache outside the
WAL — but it kept the writer emitting vectors, and kept a fallback (its own FR-002) that binds a
stored vector when source text is absent. This issue removes both: the writer stops emitting vectors
unconditionally, and the fallback that made "no source text" survivable is removed.

**Why this is unconditional, not optional.** `AppState.embedder` is `Arc<dyn Embedder>` — not an
`Option` — and an unreachable embedder is documented as "always fatal at startup"
(`crates/core/src/embedder.rs:474`, raised at `crates/service/src/main.rs:310`). There is no
configuration in which lcg runs without one. That removes the only argument for keeping vectors in
the log: a consumer needs an embedder to embed its own queries, so it always has one; requiring it at
ingest costs nothing that was not already required. It also means #440's stored-vector fallback covers
a state that cannot arise — the same shape as #432's removal of machinery built for a case with no
instance.

**The model going forward:**

- **Vectors are derived, not durable.** They are recomputed by whichever instance holds the database,
  from source text carried in the log.
- **Bit-identity is irrelevant.** Search compares stored vectors against query vectors, both produced
  by the same local embedder. A stream written under model A and hydrated under model B produces a
  database that is entirely model B — correct, not degraded.
- **The invariant is local: one database, one embedding model.** Not a property of the WAL, and not
  shared between instances, because vectors no longer travel.
- **Changing embedder is "rebuild from WAL"**, correct by construction.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator publishes a smaller, faster WAL stream (Priority: P1)

An operator running lcg against a real corpus publishes a group's WAL stream (per
`docs/operations.md`'s stream-publish contract) so another instance can hydrate a database from it.
Today ~90% of that stream's bytes are embedding vectors that the consumer's own embedder will
regenerate anyway. After this change, the published stream carries only source text, and is
correspondingly smaller and faster to transfer and to write.

**Why this priority**: This is the entire point of the issue — the measured 89.9% byte reduction on a
real capture is the concrete value delivered.

**Independent Test**: Write a fresh WAL from an ingest run and confirm it contains no embedding vector
params, and that a plain byte-count comparison against the same run's previous, vector-bearing WAL
shape shows the reduction.

**Acceptance Scenarios**:

1. **Given** a running lcg instance with a live WAL, **When** an entity, episode, or edge is created
   or updated, **Then** the WAL record for that mutation contains its source text fields but no
   `name_embedding`, `fact_embedding`, `content_embedding`, `summary_embedding`, or sibling vector
   param.
2. **Given** the #217 capture's ingest workload re-run against the changed writer, **When** the
   resulting WAL is measured, **Then** its size is smaller than the original capture by
   approximately the previously-measured 89.9%, and the actual figure is reported.

---

### User Story 2 - Consumer hydrates a database from an older, vector-bearing WAL (Priority: P1)

An operator has an existing WAL captured before this change (vectors included) and replays it to build
or rebuild a database. The replay must produce the same graph as it does today, with vectors
recomputed from source text rather than read out of the old records — the presence of stored vectors
in an older WAL must not change replay's behavior at all.

**Why this priority**: Backward compatibility with every WAL already in the wild (including the #217
capture itself) is required — this is not a migration that only applies to newly-written WALs.

**Independent Test**: Replay the existing vector-bearing #217 capture end to end and compare resulting
entity/relationship/episode counts, plus a semantic-search spot check, against today's replay of the
same capture.

**Acceptance Scenarios**:

1. **Given** an older WAL record that carries both source text and a stored embedding vector, **When**
   replay processes that record, **Then** the vector actually bound to the entity/edge/episode is
   computed fresh from the record's source text, and the stored vector value is never read or used.
2. **Given** the full #217 capture, **When** replayed under the changed replay path, **Then** the
   resulting `entity_count`, `relationship_count`, and `episode_count` are identical to today's
   replay of the same capture.
3. **Given** the graph produced by replaying the #217 capture under the changed replay path, **When**
   #217's golden semantic-search queries are run against it, **Then** result quality is measured and
   reported (not merely asserted to be acceptable).

---

### User Story 3 - Operator changes the embedding model (Priority: P2)

An operator swaps the configured embedder (e.g., moving to a new model version). The database's
stored model identity must still detect this at query time and signal that a rebuild is needed —
that detection is the one piece of model-identity machinery this change keeps, specifically because
vectors no longer travel with the log and can no longer be cross-checked in transit.

**Why this priority**: This is the surviving half of the model-identity story (FR-004) and is the
mechanism that makes "changing embedder is rebuild-from-WAL" a safe, detectable operation rather than
a silent correctness gap.

**Independent Test**: Open a database whose stored `embedding_model`/`embedding_dim` differ from the
currently configured embedder and confirm the mismatch is detected at query time, independent of
anything WAL-related.

**Acceptance Scenarios**:

1. **Given** a database whose persisted `WalPositionRecord.embedding_model` /
   `embedding_dim` do not match the currently configured embedder, **When** the database is queried,
   **Then** the mismatch is detected and reported using the existing database-side comparison
   mechanism.
2. **Given** a WAL that itself once carried a per-record or per-stream embedding-model stamp used only
   to warn about mismatches during replay (`EmbedderContext::check_replay_mismatch` /
   `warn_on_embedding_model_mismatch`), **When** that WAL is replayed under the changed code,
   **Then** no such WAL-side stamp or mismatch check exists any more — model-identity enforcement
   lives solely in the database-side stamp from Scenario 1.

---

### Edge Cases

- What happens when a WAL record kind that today carries a vector param turns out **not** to have its
  source text co-located in the same record? This is the one identified open risk (see FR-005): every
  such record kind must be enumerated and, for each, either source text must be added to the record, or
  an explicit, documented decision must be recorded that the vector is unrecoverable for that kind. This
  enumeration and resolution is deferred to the Research/Plan/Implement stages of this issue — Specify
  does not resolve it, but requires it to be resolved before FR-002/FR-003 can be implemented safely.
- What happens to `Conn::insert_entity`'s existing assumption that `name_embedding` is "always
  populated with a real, correctly-sized vector by every caller" (used to size the `summary_embedding`
  zero-vector fallback)? That assumption must be re-established under recompute (ideally trivially true
  again, since recompute always produces a real vector) or explicitly replaced — see FR-007.
- What happens when replaying a WAL record whose vector-bearing kind has co-located source text, but
  that text is empty or otherwise degenerate? Out of scope for this issue to define new behavior here:
  whatever recompute-from-text already does for such input (per #440) is unchanged; this issue only
  removes the option to fall back to a stored vector instead.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The WAL writer MUST NOT emit embedding vector params (`name_embedding`,
  `fact_embedding`, `content_embedding`, `summary_embedding`, and any sibling) in any record. Source
  text stays. This is unconditional — no flag, no mode, no default to choose.
- **FR-002**: Replay MUST recompute every vector from co-located source text and MUST NOT bind a
  vector value found in a WAL record. A vector present in an older WAL is ignored, not used.
- **FR-003**: #440's stored-vector fallback (its own FR-002) MUST be removed, along with the WAL-side
  embedding-model stamp and its mismatch check — `EmbedderContext::check_replay_mismatch` and
  `warn_on_embedding_model_mismatch` (`db.rs:288`). Once no stored vector is ever bound, a WAL's model
  identity has nothing to govern.
- **FR-004**: The database-side model identity — `WalPositionRecord.embedding_model` /
  `embedding_dim` (`db.rs:78-81`) — MUST be retained and MUST remain compared at query time. This is
  the load-bearing stamp: it enforces one-database-one-model and is what detects a database whose
  vectors predate an embedder change.
- **FR-005**: Every record kind that today carries a vector MUST be confirmed to carry its source text
  co-located in the same record. Any kind that does not MUST be identified and resolved — either by
  adding the text or by an explicit, documented decision that the vector is unrecoverable for that
  kind. This is the one open risk in the change: #440's fallback exists precisely for records with a
  vector and no text, and removing it without establishing that no such record kind exists would
  silently lose those vectors.
- **FR-006**: An older, vector-bearing WAL MUST replay to the same graph as before — same entity,
  relationship, and episode counts, with vectors recomputed rather than bound. No migration step.
- **FR-007**: `Conn::insert_entity`'s assumption that `name_embedding` is "always populated with a
  real, correctly-sized vector by every caller" (`db.rs:503-511`, which sizes the `summary_embedding`
  zero-vector fallback off its length) MUST be re-established under recompute, or replaced. Recompute
  should make it true again, but it is load-bearing and currently depends on the writer.
- **FR-008**: `docs/operations.md`'s stream-publish contract MUST state that a published stream
  carries no vectors and that hydrating one requires an embedder — which every instance already has.

### Key Entities

- **WAL record**: A single logged mutation (entity/edge/episode create or update). Carries source text
  fields; after this change, never carries an embedding vector param.
- **Embedding vector** (`name_embedding`, `fact_embedding`, `content_embedding`,
  `summary_embedding`, and siblings): A derived, local-only artifact recomputed from a WAL record's
  source text at replay time. Never persisted in the log; never bound from a stored value.
- **Database-side model identity** (`WalPositionRecord.embedding_model` / `embedding_dim`): The
  surviving stamp of which embedding model produced a database's stored vectors. Compared at query
  time; unaffected by this change other than being the sole surviving identity mechanism (FR-004).
- **WAL-side model stamp** (`EmbedderContext::check_replay_mismatch` /
  `warn_on_embedding_model_mismatch`): The mechanism this issue removes (FR-003) — a check that only
  ever mattered because a stored vector could be bound; with that possibility gone, it has nothing
  left to govern.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A freshly written WAL contains no embedding vector params. Measured against the #217
  capture's shape, size drops by approximately the measured 89.9%; the actual figure is reported.
- **SC-002**: Replaying the existing vector-bearing #217 capture produces `entity_count`,
  `relationship_count`, and `episode_count` identical to today.
- **SC-003**: Semantic search over that recomputed graph is measured against #217's golden queries and
  reported — quality assessed, not asserted.
- **SC-004**: No code path binds a stored vector from a WAL record. Verified by the absence of the
  fallback, not only by test.
- **SC-005**: Replay time and embedder call volume for the #217 capture are reported, with and without
  a warm content-addressed cache, so the recompute cost is known rather than assumed.

## Assumptions

- Every lcg instance has a reachable, correctly configured embedder at all times — this is already an
  enforced startup precondition (`AppState.embedder: Arc<dyn Embedder>`, fatal-at-startup check in
  `crates/service/src/main.rs`), not a new requirement introduced by this issue.
- The content-addressed vector cache introduced by #440 (outside the WAL) continues to exist unchanged;
  this issue does not alter its design, only removes the writer's emission of vectors into the WAL and
  the replay-side fallback to a stored vector.
- "Same graph as before" (FR-006, SC-002) means identical entity/relationship/episode counts and
  identifiers on replay of an existing WAL — it does not mean bit-identical stored vectors, since
  recompute under a possibly different embedder is explicitly expected to differ (see Background: "bit
  identity is irrelevant").
- The set of record kinds that carry a vector today (FR-005) is enumerable from the existing schema and
  writer code; resolving that enumeration and any gaps it surfaces is in scope for this issue's
  implementation, not deferred to a follow-up.

## Out of Scope

- **Vector encoding.** The 5.3× JSON-decimal overhead becomes moot for new WALs once vectors are gone.
  If a future record ever needs to carry one, encoding is a separate question.
- **Re-embedding a database in place.** Changing embedder is "rebuild from WAL" under this model, not
  an in-place re-embed operation.
- **Changes to the content-addressed vector cache's own design** (introduced by #440) — this issue only
  removes the writer's vector emission and the replay-side stored-vector fallback that sits in front of
  that cache.

## Source References

- `crates/core/src/db.rs:517` — WAL writer emitting `name_embedding` (and siblings) today.
- `crates/core/src/db.rs:78-81` — `WalPositionRecord.embedding_model` / `embedding_dim` (retained,
  FR-004).
- `crates/core/src/db.rs:288` — `warn_on_embedding_model_mismatch` (removed, FR-003).
- `crates/core/src/db.rs:503-511` — `Conn::insert_entity`'s `name_embedding`-always-populated
  assumption (FR-007).
- `crates/core/src/embedder.rs:474` — embedder-unreachable-is-fatal-at-startup documentation.
- `crates/service/src/main.rs:310` — where that fatal startup check is raised.
- `docs/operations.md` — stream-publish contract to be updated (FR-008).
- **ADR-0006** — the embedder contract, including the normalization requirement that makes same-model
  recompute safe.
- **#440 / PR #443** — recompute on replay, the content-addressed cache, and both model-identity
  stamps; this issue completes it and removes the parts the unconditional model makes dead.
- **#432** — precedent for removing machinery built for a case with no instance.
- **#217** — the real-corpus capture used for all measurements (16 files, 12,482 records, 74.4 MB).
