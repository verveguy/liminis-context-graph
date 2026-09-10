# Feature Specification: `knowledge_strip_wal_embeddings` — Strip Embedding Vectors from Pre-0.14 WALs

**Feature Branch**: `fabrik/issue-577`
**Created**: 2026-09-09
**Status**: Specified
**Input**: Issue #577 — "Add a WAL maintenance operation to strip embedding vectors from pre-0.14 WALs"

## Background

0.14.0 stopped **writing** embedding vectors to the WAL, and made replay ignore any vector it finds in an older WAL:

> **Embedding vectors are no longer written to the WAL.** They were **89.9% of WAL bytes** on the reference corpus (66.9 MB of 74.4 MB), since each `f32` becomes a JSON decimal literal. Replay now always recomputes from co-located source text through a content-addressed cache, **and a vector found in an older WAL is ignored**. (#526, #440)
> — `docs/releases/0.14.0.md`

The upgrade is explicitly non-rewriting: *"A 0.13.x database (storage 41) opens directly — no export, WAL untouched."*

So every workspace that predates 0.14.0 is carrying a WAL that is ~90% **inert bytes**: vectors that are read, parsed, and discarded on every replay. They shrink only if the whole WAL is regenerated, which means re-running extraction — the expensive thing the WAL exists to avoid.

There is currently no way to reclaim that space short of a full re-ingest.

**Why this matters beyond disk:**

- **WAL replay parses them.** Every recovery, rebuild, and hydration pays JSON-decode cost for data that is then thrown away.
- **Workspaces under version control.** `liminis`'s `demo-notebook` tracks its WAL deliberately — it is the durable record, and rebuilding it from notes costs LLM extraction, whereas the database rebuilds from the WAL for free. Committing a pre-0.14 WAL bakes ~90% dead weight into git history permanently, where it cannot be reclaimed without a history rewrite.
- **It is pure upside.** Because replay already ignores these vectors, removing them cannot change replay's outcome. This is deleting bytes the system has already decided not to read.

Each WAL record (`crates/core/src/wal.rs`'s `WalLine`) is a JSON object with a fixed five-field shape — `seq`, `ts`, `db`, `cypher`, `params` — where `params` is itself a JSON object of bound-parameter values. Embedding vectors appear as JSON number-array values under `params` keys following the naming convention used by the graph schema's embedding columns (`crates/core/src/schema.rs`: `name_embedding`, `summary_embedding`, `fact_embedding`, `content_embedding`, and any future `*_embedding` column). This is a data-shape fact, not an implementation choice — it grounds what "removing embedding vectors" means without prescribing how the stripping code parses or matches them.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator reclaims disk and replay cost on an upgraded workspace (Priority: P1)

An operator has a workspace that predates 0.14.0. They call `knowledge_strip_wal_embeddings` against the running service. The service rewrites every qualifying WAL file in place, removing embedding-vector fields while leaving every other field, record ordering, and sequence number untouched, and reports what it did.

**Why this priority**: This is the entire reason the issue exists — reclaiming the ~90% inert bytes identified in 0.14.0's release notes, on the primary target (a pre-0.14 workspace that has never been stripped).

**Independent Test**: Take a pre-0.14 WAL fixture containing embedding vectors, record its total byte size and a checksum of the DB state produced by replaying it, call `knowledge_strip_wal_embeddings`, then confirm: (a) the reported byte reduction is close to 89.9%, (b) replaying the stripped WAL produces an identical DB state to replaying the original.

**Acceptance Scenarios**:

1. **Given** a pre-0.14 WAL file containing records with embedding-vector params, **When** `knowledge_strip_wal_embeddings` is called, **Then** every embedding-vector field is removed from every record's `params`, every other field (`seq`, `ts`, `db`, `cypher`, non-embedding `params` entries) is preserved exactly, and record order and sequence numbers are unchanged.
2. **Given** the same WAL file, **When** the call completes, **Then** the response reports files processed, bytes before, bytes after, and the number of records rewritten, and the byte reduction is consistent with the ~89.9% figure documented in `docs/releases/0.14.0.md` for a corpus dominated by embedding vectors.
3. **Given** a stripped WAL file and its pre-strip original (both retained for the test), **When** each is independently replayed into a fresh DB via the existing `WalReplayer`, **Then** the two resulting DBs are identical (same entity, edge, and episode counts and content) and neither replay treats an embedding-vector value specially, per 0.14.0's "a vector found in an older WAL is ignored" behavior.

---

### User Story 2 - Operator safely re-runs the operation without side effects (Priority: P1)

An operator is unsure whether a workspace has already been stripped (e.g., it was stripped once but has since received new post-0.14 writes, or the operator is scripting this as a routine post-upgrade step). They call `knowledge_strip_wal_embeddings` again. Because 0.14.x never writes vectors, and a prior run already removed all pre-0.14 vectors, there is nothing left to strip — the call is a safe no-op.

**Why this priority**: Idempotency is what makes this safe to script into an upgrade runbook without the operator having to first determine whether stripping already happened. It is called out explicitly in the issue (FR-003) and is required, not incidental.

**Independent Test**: Run `knowledge_strip_wal_embeddings` twice in succession against the same workspace. Confirm the second run reports zero records rewritten and leaves every file's bytes unchanged (byte-identical, not just semantically equivalent).

**Acceptance Scenarios**:

1. **Given** a WAL that has already been fully stripped (or was always post-0.14 and never had vectors), **When** `knowledge_strip_wal_embeddings` is called, **Then** the response reports zero files rewritten and zero records rewritten, and no file on disk is modified (mtime and bytes unchanged).
2. **Given** a mixed WAL group containing some pre-0.14 files (with vectors) and some post-0.14 files (without vectors) alongside each other, **When** the operation runs, **Then** only the pre-0.14 files are rewritten; the post-0.14 files are reported as already-clean and left untouched.

---

### User Story 3 - Operator trusts the operation because a crash mid-run cannot corrupt the WAL (Priority: P2)

An operator runs the stripping operation against a large workspace. The process is killed (power loss, OOM, manual interrupt) partway through. The operator restarts the service and confirms the WAL still replays correctly — no file was left truncated or half-rewritten.

**Why this priority**: This is what makes the operation trustworthy to run against a workspace's only durable record (per the `demo-notebook` motivation in the Background) without a backup-first ritual. It is a correctness property, not a convenience.

**Independent Test**: Simulate an interruption (e.g., kill the process, or inject a fault) after some files have been rewritten and mid-write on another, then confirm: every already-rewritten file replays correctly, the interrupted file is either the untouched original or the complete rewritten version (never a partial write), and a subsequent run completes the remaining work.

**Acceptance Scenarios**:

1. **Given** the operation is interrupted after writing a temporary replacement for a file but before the atomic rename completes, **When** the service is restarted and the WAL is inspected, **Then** the original file is intact (the rename never happened) and any leftover temporary file is either absent or clearly identifiable as a temp artifact, not mistaken for a WAL record file.
2. **Given** the operation is interrupted between files (file N fully replaced, file N+1 not yet started), **When** the operation is re-run, **Then** it resumes correctly — file N is recognized as already-stripped (per User Story 2) and file N+1 is processed normally.
3. **Given** any point of interruption, **When** the WAL (in whatever state it was left) is replayed, **Then** replay succeeds and produces a valid graph state — never a parse failure or truncated-record error attributable to this operation.

---

### Edge Cases

- **A WAL already fully written by 0.14.x**: nothing to strip; reported as zero files rewritten (see User Story 2).
- **A mixed WAL** — pre-0.14 files alongside post-0.14 files in the same group: only pre-0.14 files are rewritten; each file is evaluated independently (see User Story 2, Acceptance Scenario 2).
- **A record whose embedding-vector value is malformed or truncated** (not a well-formed JSON number array): the operation MUST fail loudly for that specific file — leave that file untouched, report it as a per-file error — rather than guessing at intent or silently dropping unrelated data. It MUST NOT abort the entire run; other files continue to be processed and are reported normally (see FR-009).
- **Very large individual WAL files**: the operation MUST stream — read, transform, and write incrementally — rather than buffering an entire file in memory (see FR-002).
- **A read-only or permission-restricted WAL directory or file**: the operation MUST report a per-file (or per-directory) error for the affected path(s), leave those files untouched, and continue processing the remaining files rather than failing the entire run.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A new MCP tool, `knowledge_strip_wal_embeddings`, MUST be registered in `crates/service/src/mcp/tools.rs` and dispatched via `crates/core/src/handlers.rs`, following the existing pattern for WAL-admin tools (e.g., `knowledge_backfill_summary_embeddings`, `knowledge_dump_wal`). It is exposed only as an MCP tool on the running service — not as a separate standalone binary.
- **FR-002**: For each qualifying WAL file, the operation MUST rewrite it, removing every embedding-vector field from every record's `params` while preserving every other field (`seq`, `ts`, `db`, `cypher`, all non-embedding `params` entries), preserving record ordering and sequence numbers exactly, and preserving the file's line/JSONL structure. Files MUST be processed by streaming (reading, transforming, and writing incrementally) rather than buffering an entire file's contents in memory.
- **FR-003**: An embedding-vector field is a `params` entry whose value is a JSON array of numbers and whose key follows the embedding-column naming convention already established in `crates/core/src/schema.rs` (e.g., `name_embedding`, `summary_embedding`, `fact_embedding`, `content_embedding`, and any future `*_embedding` column). Research/Plan MUST confirm the concrete field list against `schema.rs` at implementation time rather than hardcoding today's list as exhaustive.
- **FR-004**: The operation MUST be idempotent: running it on a WAL that has already been stripped (or was always post-0.14 and never contained vectors) MUST be a no-op — zero files modified, zero records rewritten, files left byte-identical (not just semantically equivalent) — and MUST NOT be treated as an error.
- **FR-005**: The operation MUST be crash-safe per file: for each file that needs rewriting, write the transformed content to a temporary file (in the same directory, so the eventual rename is same-filesystem) and atomically replace the original only once the write is complete and flushed. A failure at any point before the atomic replace MUST leave the original file completely intact; a failure MUST NOT leave a truncated or partially-rewritten file in the original file's place. This applies per file — files already fully replaced before an interruption remain replaced (see User Story 3, Acceptance Scenario 2).
- **FR-006**: On completion, the operation MUST report: number of files processed, number of files rewritten (vs. left untouched because already clean), total bytes before, total bytes after, and total records rewritten (records that had at least one embedding-vector field removed). Per-file errors (malformed vector, permission failure) MUST be included in the response rather than silently swallowed, and MUST NOT prevent the response from reporting successful progress on the rest of the run.
- **FR-007**: The operation MUST correctly discover and process WAL files under both the multi-stream layout (`<wal_root>/<group_dir>/*.jsonl`, per ADR-0378, where `<group_dir>` is the self-name-if-safe or percent-encoded directory per `group_id`) and any legacy flat layout (`*.jsonl` directly under the WAL root, predating ADR-0378). An optional `group_id` parameter MAY be accepted to scope processing to a single group's directory; when omitted, the operation processes every group directory found under the WAL root plus any legacy flat-layout files at the root itself.
- **FR-008**: Because this operation is invoked as an MCP tool against the running service (per FR-001), it MUST NOT be allowed to interleave with a concurrent write to the same WAL stream (e.g., an in-flight `knowledge_process_chunk`, or a second concurrent invocation of this same tool). It MUST use the same class of exclusive-lock discipline the service already applies to other whole-WAL admin operations (e.g., `knowledge_backfill_summary_embeddings`'s exclusive lock for its run's duration) — the specific lock primitive is an implementation decision for Research/Plan, not this spec.
- **FR-009**: If a record's embedding-vector value is present but malformed or truncated (not a well-formed JSON number array), the operation MUST treat this as a per-file error: leave that file completely untouched, report the file and the error in the response (per FR-006), and continue processing other files. It MUST NOT guess at the intended value, silently drop unrelated data, or abort the entire run because of one bad file.
- **FR-010**: The operation MUST accept an optional `dry_run` parameter (default `false`), consistent with other WAL-admin/maintenance MCP tools in this codebase, that reports the same statistics (files that would be rewritten, bytes that would be reclaimed, records that would be touched) without modifying any file.

### Key Entities

- **WAL record** (`WalLine`): the five-field JSON object (`seq`, `ts`, `db`, `cypher`, `params`) that this operation reads and rewrites. Defined in `crates/core/src/wal.rs`.
- **Embedding-vector field**: a `params` entry keyed by an embedding-column name (per `schema.rs`'s naming convention) whose value is a JSON array of numbers. The unit this operation removes.
- **WAL group directory**: a per-`group_id` subdirectory of the WAL root (ADR-0378), or the WAL root itself under the legacy flat layout. The unit of discovery for this operation.
- **Stripped WAL file**: a rewritten `.jsonl` file with the same records, ordering, and sequence numbers as its pre-strip original, minus embedding-vector fields.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a pre-0.14 WAL fixture whose embedding vectors dominate its bytes (matching the reference corpus's composition), the operation reduces total size by a proportion consistent with the documented 89.9% figure.
- **SC-002**: A graph rebuilt (via `WalReplayer`) from a stripped WAL is identical — same entity, edge, and episode counts and content — to one rebuilt from the corresponding original, unstripped WAL.
- **SC-003**: Re-running the operation on an already-stripped WAL reports zero files rewritten and zero records rewritten, and leaves every file byte-identical to before the re-run.
- **SC-004**: An interrupted run (killed mid-file or between files) leaves a WAL that still replays correctly via `WalReplayer` with no parse failures or corruption attributable to this operation, and a subsequent run completes any remaining work.

## Assumptions

- Replay's "ignore any vector found in the WAL" behaviour (0.14.0, #526/#440) is stable and not reverted. If a future version ever reads WAL vectors again, this operation becomes lossy for any WAL it has already stripped — worth a note in the implementation's ADR.
- The tool name is `knowledge_strip_wal_embeddings` (chosen over `knowledge_compact_wal` from the issue's alternatives) because "compact" would suggest the broader compaction operation (dedup, checkpoint truncation) that is explicitly out of scope, and this name follows the same verb-object naming pattern as `knowledge_backfill_summary_embeddings`.
- The operation defaults to processing the entire WAL root (every group directory plus any legacy flat-layout files) when no `group_id` is supplied, rather than requiring one, because the primary motivating use case (a post-upgrade, one-time reclaim across a whole workspace) is workspace-wide, not per-group. This differs deliberately from `knowledge_backfill_summary_embeddings`, which requires `group_id` because it performs costly re-embedding; stripping is comparatively cheap and mechanical.
- The exact set of embedding-column key names is derived from `crates/core/src/schema.rs` at implementation time and is expected to evolve; this spec does not freeze today's list as exhaustive (see FR-003).
- Progress streaming (`_progress_token`) is not required for the initial implementation, even though a large workspace may have many WAL files; it may be added later, following the pattern already used by `knowledge_rebuild_from_wal` and `knowledge_backfill_summary_embeddings`, without changing this spec's requirements.
- The MCP tool's scope bucket (per this project's `Scope::Read`/`Write`/`Admin`/`Cypher` classification) is expected to be `Admin`, consistent with other WAL/lifecycle/index-maintenance operations — this is noted for Research/Plan, not mandated as a spec-level requirement.

## Out of Scope

- Changing what 0.14.x writes to the WAL (already done, per the Background).
- Re-ingest or re-extraction of any kind.
- Compacting the WAL in any other sense — deduplication, checkpoint truncation, or merging multiple files into fewer files. This operation only removes embedding-vector fields; it does not otherwise restructure the WAL.
- Rewriting git history for a workspace that has already committed a pre-0.14 WAL. This operation only rewrites files on disk; making the reduction show up in git history (e.g., a follow-up commit, or a history rewrite) is the operator's decision and workflow, not this operation's concern.
- A standalone CLI/binary form of this operation — it is exposed only as an MCP tool on the running service (per FR-001), matching how `knowledge_backfill_summary_embeddings` is invoked.
- Progress streaming support in the initial implementation (see Assumptions).

## Source References

- `docs/releases/0.14.0.md` — the 89.9%/66.9 MB-of-74.4 MB figure and the "vector found in an older WAL is ignored" replay behavior this operation relies on for safety.
- `crates/core/src/wal.rs` — `WalLine` (five-field record shape), `WalWriter` (existing atomic-append patterns to mirror for atomic-replace).
- `crates/core/src/schema.rs` — canonical list of embedding-column names (`name_embedding`, `summary_embedding`, `fact_embedding`, `content_embedding`, …).
- `crates/service/src/mcp/tools.rs` — existing `ToolSpec` entries for `knowledge_backfill_summary_embeddings`, `knowledge_dump_wal`, `knowledge_wal_mark_*`, `knowledge_rebuild_from_wal` to follow for naming, scope, and response-shape conventions.
- `docs/adr/0378-multi-stream-wal-per-group-directory.md` — multi-stream WAL-root/group-directory layout and `group_id` → directory-name encoding this operation must traverse.
- `specs/161-knowledge-dump-wal-db/spec.md` — a prior WAL-maintenance MCP tool spec with a similar shape (streaming, atomicity, reporting) worth mirroring.
- Issues #526, #440 — the 0.14.0 changes that stopped writing WAL vectors and made replay ignore them, which this operation depends on for its safety guarantee.
