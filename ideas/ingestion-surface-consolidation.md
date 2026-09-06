# Proposal: Consolidate the Ingestion Surface in liminis-context-graph

**Status**: Draft for review. Pre-Spec-Kit — this is the argument an ADR would record.
**Date**: 2026-09-05
**Belongs to**: `liminis-context-graph` (the surface is lcg's; the fossils are in `liminis`)

Confidence tags: **[V]** verified by direct read at a cited `file:line` · **[I]** inference · **[?]** open.

---

## 1. The claim

Ingestion belongs in lcg, end to end. lcg should accept **either**:

- **Chunk mode** — a sequence of chunks prepared by a client with its own chunking strategy, or
- **Document mode** — a document plus metadata, chunked internally with a markdown-oriented default,

converging on the same stored shape and the same extraction machinery: episodes with rich
attributes and the best entity/relationship extraction achievable *given the context provided*.

Section 5 argues one deliberate exception to "same result": supersession.

## 2. A restoration, not an invention

Server-side chunking existed until the Rust rewrite. **[V]** The #689 Research stage (2026-04-24)
records two chunkers coexisting:

1. `graphiti_service.py:chunk_markdown()` — chonkie `RecursiveChunker`, 512 chars, server-side,
   behind `knowledge_index_document`.
2. `canonical-chunker.ts` — AST/heading-based, `HARD_MAX_TOKENS = 2000`, added later for
   queue-based indexing.

The Rust rewrite implemented the chunk-entry half and kept **neither** server-side chunker. The
client still carries the surface it was written against.

## 3. One root cause, seven defects

| Defect | Split-ownership cause |
|---|---|
| #292 foreign-episode delete | `chunk_id` is a *client address*; server infers lineage from a `source_description` string convention |
| #291 unindexed `Episodic.name` scan | `chunk_id` overloaded into `name` because the server has no document model |
| #288 resubmission race | server cannot order or batch work it does not own |
| #284 splitting + idempotency | server reconstructs prior text because the client's `chunk_version_hash` never crosses the wire |
| #689 context blindness | server never sees the document a chunk came from |
| supersession unsoundness | client computes `ChunkDiff` (added/changed/deleted/**unchanged**) and discards it at the wire |
| client-state desync | `chunkState.setEntry` commits at *enqueue*, before the server confirms — two sources of truth |

**[V]** The signal already reaches the server and is discarded three times:
`source_description = "{source_file}:{chunk_id}"`, and `chunk_id = "{doc_id}::{heading_path}::{index}"`.
The heading path physically arrives today. Nothing parses it, nothing embeds it, and the extractor
never sees it — the extraction user-message is only
`<CURRENT_MESSAGE>{body}</CURRENT_MESSAGE>` + `<ENTITIES>` + `<REFERENCE_TIME>`
(`crates/core/src/prompts/mod.rs:228-238`).

## 4. The six unimplemented methods are a broken agent surface, not dead code

**[V]** All six are absent from lcg's dispatch table (`crates/core/src/handlers.rs`), and **all six
are still advertised to agents as MCP tools**, some also wired to the UI:

| Method | Live callers (liminis) |
|---|---|
| `knowledge_index_document` | `closer-agent.ts:540`; `knowledge-writer-provider.ts:131`; `ipc/context-graph-handlers.ts:505` |
| `knowledge_process_document` | `indexing-queue.ts:1611`; `knowledge-writer-provider.ts:145` |
| `knowledge_list_sources` | `knowledge-reader-provider.ts:74` |
| `knowledge_preview_chunks` | `knowledge-reader-provider.ts:186` |
| `knowledge_suggest_duplicates` | `knowledge-reader-provider.ts:199` |
| `knowledge_entity_edge_analysis` | `knowledge-reader-provider.ts:211` |

Every one fails today. **[V]** `knowledge-reader-provider.ts:1-13` says so knowingly, and exposes a
second defect:

> Python-only tools … are included but unimplemented methods are detected by a substring match on
> "Method not found:" in the error message — **not by error code, since the Rust binary uses the
> generic -32000 code for all errors** (DB failures, embedding failures, and unknown methods alike).

A client distinguishes "not implemented" from "your database is broken" by matching an error
string. Changing that string in lcg silently reclassifies real failures. **Fix regardless of this
proposal's outcome: return -32601 for an unknown method.**

## 5. Where the two modes cannot be equivalent

Document mode carries an implicit **completeness claim** — *this is document D at time T* — which is
the precondition absence-based retraction requires: "F is no longer asserted" is only meaningful
over a scope known to be complete.

Chunk mode carries no such claim, and **[V]** the liminis app sends only *changed* chunks; unchanged
ones are filtered by `ChunkStateStore.diffChunks()` and never enqueued. A server receiving chunks
cannot distinguish "unchanged", "deleted", "still queued" and "re-addressed".

**Proposed asymmetry, designed in rather than discovered:**

- **Chunk mode is append-only** — no supersession, no absence inference. This matches the known
  consumer shape: MCP `Scope::Write` agent callers are write-once, with no chunk-state store and no
  notion of a document's full extent.
- **Document mode is revisable** — supersession is sound because completeness is asserted.

No manifest protocol is needed up front, and the modes still converge on identical storage and
extraction machinery, differing only in achievable quality and revision capability.

**[I] Against over-claiming:** document mode does **not** make chunk churn disappear. Measured over
315 real revision pairs, 9.8% orphan at least one `chunk_id`, driven by heading renames — and since
the heading sits *inside* the chunk text, a rename still changes chunk content. What changes is that
the server holds *both* revisions and can choose a matching strategy (paragraph-level, content
similarity, heading-as-metadata). Client addressing offers no choice: a changed address is simply a
different chunk. Matching becomes a server-side decision with full information rather than an
artifact of someone else's cache key.

## 6. Disposition of the six

**Implement (4)**

- **`knowledge_process_document`** — the document-mode entry point. Chunk internally, ingest, return
  per-chunk results; streams progress via the existing `_progress_token` mechanism (ADR-0005).
- **`knowledge_index_document`** — **consolidate into `process_document`.** Two names for one
  operation is a Python-era accident; `process_document` pairs with the existing `process_chunk`.
  Keep as a deprecated alias for one release (three live callers), then remove.
- **`knowledge_list_sources`** — the natural read counterpart to document ingest, and nearly free
  once `doc_id` is a real column (already agreed for 0.14.0). lcg treats "source" as first-class
  (`get_entities_by_source`, `delete_by_source`) but cannot enumerate.
- **`knowledge_preview_chunks`** — **load-bearing, not a nicety.** If lcg owns chunking, the preview
  UI must ask the server what the chunks are or it will show something different from what is
  stored. This is what lets the second chunker be *deleted* rather than kept in sync (§7).

**Deprecate from this scope (2)**

- **`knowledge_suggest_duplicates`** — not ingestion. Belongs with the corrections pipeline
  (liminis #685/#686; lcg `validate_corrections` / `apply_corrections` / `merge_entities`).
  Re-file against that work rather than dropping silently.
- **`knowledge_entity_edge_analysis`** — **[?]** no one has stated what it did that
  `knowledge_status`, `list_entities`, `list_relationships` and `get_entity_neighbors` do not.
  Deprecate unless the gap can be named.

**Deprecation mechanics.** These are advertised MCP tools, so removal is a visible surface change —
the app must stop *registering* the tool, not merely stop calling it. Sequence: (1) lcg returns
-32601 for unknown methods; (2) app removes the tool registration; (3) client methods deleted.

## 7. Chunker ownership

Porting `canonical-chunker.ts` is the main cost: mdast + micromark + `gpt-tokenizer` become a Rust
markdown AST (comrak/pulldown-cmark) plus a tokenizer, with matching semantics.

Two chunkers that disagree is a new bug class. The resolution is to **not have two**: make lcg's
chunker canonical and have the preview UI call `knowledge_preview_chunks`. The app's local chunker
is itself a rewrite-era workaround for a server capability that went missing.

**[?]** Open: must the port be behaviour-identical (existing graphs keep their chunk boundaries), or
is it free to differ and force a re-index? Identical means porting `mergeSmallSections` /
`splitIntoSubBlocks` semantics exactly.

## 8. Interactions with work in flight

- **0.14.0 (chunking).** #284's splitting becomes the *internal chunking strategy* rather than a
  degradation path bolted onto a chunk endpoint. #293's IPC cap becomes a document-size cap. The
  agreed `Episodic` provenance columns (`doc_id`, `chunk_id`, `unit_index`, `unit_count`,
  `chunk_version_hash`) become more natural, because in document mode the server mints them.
- **0.15.0 (temporal).** **[I] May remove a protocol from that scope.** The revision-manifest design
  exists to reconstruct completeness the wire destroys; document mode supplies it by construction.
  Reconcile before either thread specs further.
- **#689 (contextual retrieval).** Stops being an architecture debate — in document mode the context
  is present, so Paths A/B/C/D collapse to an internal quality choice. Two findings carry over:
  `bge-base-en-v1.5`'s 512-token limit against `HARD_MAX_TOKENS = 2000` means chunks over ~412
  tokens are already embedded from a truncated prefix; and **[V]**
  `CREATE_FTS_INDEX('Episodic','episode_content')` exists (`schema.rs:568`) but nothing queries it —
  every `QUERY_FTS_INDEX` targets `Entity` or `RelatesToNode_`.

## 9. Risks

- **Wire contract.** lcg is public with tagged releases. Additive methods are safe;
  `knowledge_process_chunk` must stay as the append-only mode. Deprecations follow §6's sequence.
- **Long-running document ingest.** N extractions in one call. Progress streaming exists; failure
  semantics mid-document do not — **[?]** partial-document ingest needs defined behaviour.
- **Re-index.** If chunk boundaries change, existing graphs need rebuilding. Acceptable per prior
  direction, but state it rather than discover it.
- **Scope.** Touches three in-flight workstreams. A reframing, not a competing plan — but it should
  be settled before more specs are written against the current shape.

## 10. Open questions

1. Behaviour-identical chunker port, or free to differ with a re-index? (§7)
2. Partial-failure semantics for document mode. (§9)
3. Does `knowledge_entity_edge_analysis` have a purpose worth keeping? (§6)
4. Non-markdown content: design the content-type seam now, or markdown-only with a fallback later?
5. Does document mode obsolete the 0.15.0 revision manifest, or merely feed it? (§8)
