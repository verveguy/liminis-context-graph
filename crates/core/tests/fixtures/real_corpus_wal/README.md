# Golden Real-Corpus WAL Fixture

This directory contains the real-data fixture consumed by
`crates/core/tests/real_corpus_e2e.rs` (#217). Unlike `crates/core/tests/fixtures/wal/`
(small, hand-crafted `.jsonl` files exercising WAL *format* edge cases), this fixture is a
**real ingest run** — real `AnthropicExtractor` + real `OaiEmbedder` — over a real public
corpus, large enough to cross the `LIMINIS_DEDUP_HYBRID_THRESHOLD` (default 1,000) with
margin. It exists so CI can catch dedup/index/relation-typing/search regressions that only
show up on real-corpus shape (see the issue's Background), without ever making an LLM,
extractor, or embedder call itself — replay is 100% mechanical Cypher re-execution.

## Corpus

- **Source**: Simple English Wikipedia
- **License**: CC-BY-SA 4.0 (https://simple.wikipedia.org/wiki/Simple_English_Wikipedia:Copyrights)
- **Domain cluster**: the Apollo program (astronauts, missions, spacecraft, agencies —
  chosen for dense, recurring hub-entity cross-referencing, FR-001c)
- **Pinned snapshot**: `corpus_manifest.json` lists 309 articles as `{title, revision_id}`
  pairs (Wikipedia `oldid` values), snapshotted 2026-07-24. Pinning by revision ID (rather
  than a dated dump file) means regeneration doesn't depend on a specific dump still being
  available — only on the individual revisions still existing, which Wikipedia guarantees
  for any non-deleted page.
- **PII/secrets**: none — the corpus is public encyclopedic content, not user data. This is
  a property of the corpus-selection criteria (FR-001a/d), not a scrubbing step, and it
  applies to any future re-capture from the same or a similar public source too.

## The four fixture artifacts

| Artifact | Format | Purpose |
|---|---|---|
| `corpus_manifest.json` | JSON | Provenance: the full 309-article pinned list, plus `last_capture` recording exactly which articles this fixture's WAL was built from |
| `corpus_prose.jsonl` | plain JSONL, uncompressed | The cleaned prose actually fed to the extractor for every consumed article — the input fixture for comparing a different extraction model against this same real corpus (#228, FR-013/FR-014) |
| `wal/*.jsonl` | plain JSONL, one file per WAL rotation, uncompressed | The captured WAL itself — replayed by `real_corpus_e2e.rs` via `knowledge_rebuild_from_wal` to rebuild the graph deterministically, zero LLM calls |
| `expected_results.json` | JSON | Recorded counts, golden queries, a 2-hop traversal path, and relation-type samples — the assertions in `real_corpus_e2e.rs` are checked against this file, not hardcoded |

**Why `corpus_prose.jsonl` and `wal/` are both needed, and neither can substitute for the
other**: the WAL is *post-extraction* (it has entities/edges, not source text), so it can't
be fed to a different extractor for comparison. A future LLM-response cassette (recording
one model's exact request/response exchange, tracked separately — see "Known limitations"
below) records only *one* model's exchange, so it can't test a *different* model either.
Only the raw prose that was actually fed to the extractor supports an apples-to-apples
extraction-model comparison, and refetching from Wikipedia at compare-time is not
equivalent even with pinned revisions — the prose is a derived artifact of
`wikitext_to_prose` in `crates/core/scripts/capture_real_corpus.py`, and that cleanup
function changed three times during this issue (see `CLEANUP_VERSION` in that script).

**Why neither artifact is gzip-compressed, and the WAL isn't concatenated into one file**:
git already compresses blobs for storage/transport, so gzip buys nothing on the wire while
making the fixture non-diffable/non-greppable and defeating delta compression across future
re-captures (a one-byte input change would otherwise make the entire multi-MB blob a
brand-new object in git history, forever). The WAL is committed as the run's individual
`.jsonl` files (original filenames preserved) in `wal/` because `WalReplayer` already reads
a directory and sorts filenames lexicographically (`replay.rs`) — this is both the
production layout and keeps individual committed blobs small (16 files, ~1–5 MB each,
71 MB total) and delta-friendly.

## How this specific fixture was captured

This fixture predates the stage/ingest split described below (see "Regenerating a future
fixture") — it was captured with an earlier, single-phase version of
`capture_real_corpus.py` that fetched, cleaned, and ingested each article in one pass. As a
result, `corpus_prose.jsonl` for this fixture was **derived after the fact directly from
the committed WAL**, not staged before ingest:

- `wal/*.jsonl`: the 16 WAL files from the capture run, committed verbatim.
- `corpus_prose.jsonl`: derived with zero network calls via
  `capture_real_corpus.py --derive-prose-from-wal crates/core/tests/fixtures/real_corpus_wal/wal --output-dir crates/core/tests/fixtures/real_corpus_wal`
  — this reads each `CREATE (:Episodic {...})` record's `content`/`name`/`source_description`
  params directly out of the WAL, which is byte-identical to what was actually fed to the
  extractor (see `derive_prose_from_wal` in the script).
- `expected_results.json`: copied from the capture run's output, with `relation_type_samples`
  backfilled via
  `capture_real_corpus.py --backfill-relation-samples-from-wal crates/core/tests/fixtures/real_corpus_wal/wal --output-dir crates/core/tests/fixtures/real_corpus_wal`
  (see "Known limitations" below for why this was needed).
- `corpus_manifest.json`'s `last_capture` block: reconciled against `expected_results.json`'s
  `consumed_articles`/`skipped_articles` lists.

**Capture stats**: 228/309 manifest articles consumed (7 skipped as genuine stub articles —
recorded in `expected_results.json.skipped_articles`), captured 2026-07-25, real
`claude-haiku-4-5` extraction with real CoreML (`BAAI/bge-base-en-v1.5`, 768-dim) embeddings.

**Resulting graph** (from `expected_results.json`):

| Metric | Value |
|---|---|
| Entities | 1,506 (exceeds the 1,000 hybrid-dedup threshold with ~500 margin, SC-002) |
| Relationships | 2,392 |
| Episodes | 228 |
| Embedding dim | 768 |
| `indices_built` | `true` |

## Known limitations of this fixture

- **No LLM-response cassette.** #232 (recording hook for LLM exchanges) hadn't landed when
  this fixture was captured, so there is no way to replay this run's exact model exchange —
  only its final extracted output (the WAL) and its input (`corpus_prose.jsonl`). Comparing
  a different extraction model against this corpus (#228) means running that model against
  `corpus_prose.jsonl` fresh, not replaying a cassette.
- **`build_expected_results` bug, fixed after this capture**: `capture_real_corpus.py`'s
  `build_expected_results` originally read `knowledge_list_relationships`'s response under
  the key `"edges"`, but `handle_list_relationships` (`handlers.rs`) actually returns
  `{"facts": [...], "count": ...}` — so this fixture's *original* `expected_results.json`
  had an empty `relation_type_samples` despite the graph having 2,392 real relationships.
  Fixed in the script for future captures; this fixture's `relation_type_samples` were
  backfilled from the WAL directly (see above) rather than by re-running the capture.
- **Two-phase staging code is present but unexercised by this fixture.** The
  `--stage-only`/`--ingest-only` split in `capture_real_corpus.py` exists to make the *next*
  capture robust (see below) — this fixture predates it and was captured single-phase.

## Regenerating a future fixture

A future re-capture (e.g. once #232's cassette-recording hook exists, to capture the WAL,
prose, and cassette together in one consistent run) should use the current two-phase
capture script:

### Prerequisites (all one-time/local, never required in CI)

1. `ANTHROPIC_API_KEY` set in the environment (real LLM extraction) — Phase 2 only.
2. The local embedding sidecar (`native/local-inference/`) reachable at the default UDS
   socket (`/tmp/liminis-inference.sock`) or `LCG_EMBEDDING_URL` — Phase 2 only. On a fresh
   checkout the sidecar compiles its `.mlpackage` fine but then fails with
   `offlineModeError("Repository not available locally")` fetching the
   `BAAI/bge-base-en-v1.5` tokenizer — point it at the repo's copy:
   `export LOCAL_INFERENCE_HF_CACHE=<repo>/resources/models/tokenizer`.
3. The compiled `liminis-context-graph` binary (`cargo build --release -p lcg-service`) —
   Phase 2 only.
4. Network access to `simple.wikipedia.org` — Phase 1 only.

### Steps

```bash
# Phase 1 — stage (free, no service, re-runnable at will; catches a systemic wikitext-
# cleanup regression here, before any LLM spend)
python3 crates/core/scripts/capture_real_corpus.py --stage-only \
    --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \
    --output-dir crates/core/tests/fixtures/real_corpus_wal

# Start the service against a fresh, empty DB/WAL dir
export ANTHROPIC_API_KEY=sk-...
LCG_DB_PATH=/tmp/real_corpus_capture/db \
LCG_WAL_DIR=/tmp/real_corpus_capture/wal \
LCG_SOCKET_PATH=/tmp/real_corpus_capture/service.sock \
  ./target/release/liminis-context-graph &

# Phase 2 — ingest (paid; reads corpus_prose.jsonl from Phase 1, zero network calls,
# trivially resumable since it never touches Wikipedia)
python3 crates/core/scripts/capture_real_corpus.py --ingest-only \
    --socket /tmp/real_corpus_capture/service.sock \
    --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \
    --wal-dir /tmp/real_corpus_capture/wal \
    --output-dir crates/core/tests/fixtures/real_corpus_wal \
    --target-entities 1500

# Stop the service, review, commit
kill %1
git add crates/core/tests/fixtures/real_corpus_wal/
git commit -m "test(corpus): recapture golden real-corpus WAL fixture (#217)"
```

`--target-entities` (default 1500) stops ingest as soon as `knowledge_status.entity_count`
crosses it, rather than always consuming the full 309-article manifest — hub-entity dedup
makes unique entity count grow sublinearly with article count, so this avoids paying for
extraction the fixture doesn't need. See `capture_real_corpus.py`'s module docstring for the
full design rationale (staging/ingest split, resumability, stub-article skip logic).

### Regenerating only `corpus_prose.jsonl`

If only the prose needs regenerating (e.g. the committed file was lost, or you're extending
the manifest before a full re-capture), re-run Phase 1 alone — it's free and idempotent:

```bash
python3 crates/core/scripts/capture_real_corpus.py --stage-only \
    --manifest crates/core/tests/fixtures/real_corpus_wal/corpus_manifest.json \
    --output-dir crates/core/tests/fixtures/real_corpus_wal
```

This makes zero LLM/embedding calls — verified in
`crates/core/scripts/test_capture_real_corpus.py`.

### Identifying a stale fixture

If a future schema or WAL-format change breaks compatibility with this fixture, either:

- `cargo test --test real_corpus_e2e` fails outright during replay, or
- replay succeeds but `rebuild_and_assert_all_non_determinism_expectations` fails on a
  count/`indices_built` mismatch against `expected_results.json`.

Either signal means the fixture needs regenerating per the steps above (a schema change may
also require the production schema/WAL-format fix to land first — see the repo's schema
parity discipline in `CLAUDE.md`).

## Test suite (`crates/core/tests/real_corpus_e2e.rs`)

Two `#[tokio::test]` functions, both using `MockExtractor`/`MockEmbedder` (wrapped with atomic
call counters — `CountingExtractor`/`CountingEmbedder`) so nothing in the test file can make an
LLM, extractor, or embedder network call (FR-004, FR-011). This is asserted explicitly, not
just assumed from the wiring: every test checks that the extractor/embedder call counts are
still zero immediately after `knowledge_rebuild_from_wal` returns.

- `rebuild_and_assert_all_non_determinism_expectations`: one rebuild, covering Acceptance
  Scenarios 1–5 — counts (FR-004/005), `indices_built`/hybrid-dedup threshold (FR-006),
  golden entity + relationship queries (FR-007), 2-hop traversal (FR-008), and relation-type
  samples (FR-009). Consolidated into a single rebuild (rather than one rebuild per
  scenario) because a full replay + HNSW/FTS index build over this fixture takes roughly a
  minute — five independent rebuilds would multiply CI cost for no additional coverage.
- `replay_is_deterministic_across_independent_processes`: two independent rebuilds,
  confirming identical counts and an identical traversal result (FR-010, Acceptance
  Scenario 6) — this one genuinely needs two separate rebuilds.

Query-time embedding in both tests uses `MockEmbedder` (a zero vector, dim matched to the
fixture's `embedding_dim`), not a live embedder — this is what makes FR-011's
zero-network requirement possible while still testing against the *real* embeddings baked
into the committed graph at capture time. It makes the vector half of RRF-fused search
deterministic-but-uninformative at test time, so golden-query assertions are written as
top-N set-membership (at least one recorded hub entity/relation-type must still surface),
not exact top-1/ordering — see the Edge Cases section of the spec. Graph traversal
(`knowledge_get_entity_neighbors`) and the raw relationship listing
(`knowledge_list_relationships`) don't involve embeddings or ranking at all, so those
assertions are exact-set equality.

### Measured runtime (SC-005)

On the machine that captured this fixture, `cargo test --release --test real_corpus_e2e`
(both tests, default parallelism) measured **~190–240s (roughly 3–4 minutes) wall-clock**
across repeated runs — each full WAL replay + HNSW/FTS index build over the fixture's 1,506
entities / 2,392 relationships / 228 episodes takes roughly 60–140s, and this file performs
three such rebuilds total (one in `rebuild_and_assert_all_non_determinism_expectations`, two
independent ones in `replay_is_deterministic_across_independent_processes`). This is a real,
non-trivial addition to the `cargo test --release` CI job — the spec does not mandate a
ceiling (SC-005 only asks it be measured and documented), but if CI budget becomes a concern,
the determinism test's second rebuild is the first thing to reconsider dropping.
