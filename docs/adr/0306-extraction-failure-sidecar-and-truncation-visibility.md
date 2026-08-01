# ADR-0306: Extraction-Failure Sidecar and Truncation Visibility

**Status**: Accepted
**Date**: 2026-08-01
**Issues**: #306

## Context

ADR-0044 established that `RecordingExtractor`/`ReplayingExtractor` operate strictly at the
`Extractor` trait boundary — `extract`/`classify_entities`/`classify_relations` — and never look
inside `AnthropicExtractor`/`OaiExtractor`. That boundary is why cassette record/replay works
uniformly across both providers and `LlmRouter` with zero provider-specific logic.

That same boundary is also why a failed extraction call left no trace at all. Three defects,
all downstream of it:

- `RecordingExtractor::extract` was `self.inner.extract(opts).await?` — the `?` returned before
  `record()`, so a failed call produced no cassette entry and no other record either.
- Even a successful call recorded only the parsed `ExtractionResult`, never the raw model
  response — there was no `.text()`/`raw_response` capture anywhere in `extractor.rs`.
- `TelemetryEvent::ExtractionTruncated` carried no chunk identity, and edge-budget exhaustion
  (deliberately non-fatal — `extractor.rs` returns `Ok(vec![])`) was byte-identical in the
  cassette to a model genuinely emitting `{"edges": []}`. The eval report could not tell the two
  apart, which was actively blocking a real quality conclusion (see the issue's Background).

The data a failure record needs — `call_type` (`entities`/`edges`), `finish_reason`,
`completion_tokens`, and the complete raw response body — exists only inside
`AnthropicExtractor`/`OaiExtractor`, strictly below the boundary ADR-0044 drew. Closing this gap
means deliberately reaching below that boundary, not extending it.

## Decisions

### 1. A new telemetry event, not a new `Error` variant

Two shapes were considered: (a) a new `TelemetryEvent::ExtractionFailure` emitted from inside
both provider extractors, consumed by a new sidecar-writing `TelemetrySink`; (b) carrying a rich
failure payload on a new `Error` variant, caught in `RecordingExtractor::extract`'s error path.

**We chose (a).** It reuses the existing `NoopSink`/`CaptureSink`/`CountingSink` pattern and
needs no change to `Extractor`'s trait signature or to `Error`. Option (b) would have needed
`Error` to carry structured, non-string metadata for the first time, and would still have lost
`call_type` granularity — `RecordingExtractor::extract` only ever sees one `Result` for the
whole `extract()` call, never a per-sub-call result, so entities-vs-edges attribution would need
to be reconstructed some other way regardless.

`ExtractionFailure` is deliberately a separate, heavier event from `ExtractionTruncated`/
`StructuredOutputParse`, which keep their existing lightweight, counting-only shape (used by
`lcg_eval::runner::CountingSink`). `ExtractionTruncated` gains only a `chunk_key: Option<String>`
field (FR-004); it does not gain the raw body. Two events, not one merged mega-event — the
report's aggregate counters stay decoupled from the sidecar's heavy per-record payload.

**Consequence, stated explicitly**: this deliberately extends ADR-0044's Decision 1. Both
provider extractors now reach below the trait boundary for failure telemetry — the boundary
still holds for what `RecordingExtractor`/`ReplayingExtractor` themselves touch, but
`AnthropicExtractor`/`OaiExtractor` are no longer purely "the concrete implementation behind the
trait" with respect to observability. Anyone editing either provider's error handling must keep
emitting `ExtractionFailure` at all three failure sites, or the sidecar silently stops covering
that path.

### 2. `ExtractOptions.chunk_key: Option<&'a str>`, not a content hash

FR-004/User Story 3 need a chunk identity that a human can correlate against the eval report's
own chunk labels. `chunk_key` was already an established term in this codebase
(`pairwise.rs`'s `chunk_pair_seed`, keyed on `chunk.title`), but nothing threaded an identifier
into `ExtractOptions`/the extractor layer at all.

**We added `ExtractOptions.chunk_key: Option<&'a str>`**, populated with `chunk.title` in
`lcg_eval::runner::run_backend` and the episode `name` in production's `episode::add_episode`.
A hash of `episode_body` would have avoided a signature change, but would have produced an
opaque key that can never be read back against a human-meaningful label — exactly what User
Story 3 needs to reconstruct a failing call. The blast radius was small (6 real construction
sites, all in this repo, all now updated) and well-precedented by `pairwise.rs`'s existing
convention.

`chunk_key` is excluded from `cassette.rs`'s `request_key` hash input — it is observational
metadata, not semantic request content. Two calls differing only in `chunk_key` must still hash
identically for replay-matching purposes, and including it would needlessly invalidate every
cassette recorded before this change.

### 3. The sidecar is installed at every `RecordingExtractor` construction site

The issue's Out of Scope text calls the sidecar "a capture/eval-tooling artifact tied to
cassette recording (`RecordingExtractor`)". Read literally, that includes not just
`lcg-eval`'s `--record-cassette` path but also `crates/service/src/main.rs`'s three
`LCG_RECORD_LLM` construction arms (Anthropic/Http/Uds) — the same `RecordingExtractor`/
`CassetteWriter` types, used for a live production capture rather than an eval run.

**We installed it at all four sites**, rather than special-casing `lcg-eval` vs. production.
This keeps one uniform wiring pattern: wherever a `CassetteWriter` is opened for a cassette
path, an `ExtractionFailureWriter`/`ExtractionFailureSink` is opened for the same path, fed into
the *leaf* extractor's constructor via a `TeeSink` alongside whatever sink was already there.
The combined sink must go to the leaf, not to `RecordingExtractor` — `ExtractionFailure` is
emitted from inside `AnthropicExtractor`/`OaiExtractor` themselves, a layer
`RecordingExtractor` (which only ever observes the whole `extract()` call's return value)
structurally cannot see.

Replay mode (`LCG_REPLAY_LLM`, or an eval `cassette:path=` backend) never constructs a
`RecordingExtractor` at all, so "no sidecar in replay mode" (the spec's Edge Case) falls out of
this wiring for free — there is no code path in which a sidecar writer is ever opened for a
replaying backend.

### 4. HTTP-error body capture required restructuring, not just an added `sink.emit()` call

`.error_for_status()` (Anthropic's retry loop, `OaiExtractor::send_chat`'s HTTP branch) discarded
the response body on a non-2xx status before any caller ever saw it; the OAI UDS path
(`send_and_read_uds`) drained the body on non-success but threw the bytes away, only logging the
status. Capturing "HTTP error" as a named failure class (FR-001) is impossible without reading
the body before checking status.

**We restructured both providers' send paths to read the full body first, then check status.**
`AnthropicExtractor` gained a shared `send_with_retry` helper (replacing duplicated inline
send/retry/status-check blocks in `do_extract_entities`/`do_extract_edges`) returning a
`SendOutcome` enum: `Ok(Value)`, `HttpFailure { status, body }` (a response was received but
wasn't usable — non-2xx, or 2xx with a body that isn't valid JSON), or `Transport(Error)` (no
response was ever received — nothing to capture as a body). `OaiExtractor::send_chat`/
`send_chat_uds` gained the analogous `ChatFailure` enum. This was scoped narrowly to "read body
before checking status" — not a broader request-handling refactor — and a transport-level
failure (connection refused, dial failure) still propagates directly with no sidecar record,
since there genuinely is no body to store in that case.

### 5. `resp.clone()` before Anthropic's destructive parse, not a parse-function rework

`parse_entity_response`/`parse_edge_response` take `resp: Value` by value and mutate it
destructively on the success path (`arr.remove(idx)`, `block["input"].take()`). Preserving the
raw body for a failure record needed the original value to survive past that call.

**We clone `resp` once, immediately before the parse call**, and use the clone only on the
failure branches (`BudgetExhausted`, `ParseError`) to build the `ExtractionFailure` record. This
is a smaller, more localized diff than reworking both parse functions to hand the original
`Value` back out on every variant. The clone is bounded by `max_tokens` (~64KB post-retry) and
failures are rare — the same cost/rarity argument FR-002 already makes for storing the body
whole rather than a prefix.

### 6. Sidecar rotation mirrors `WalWriter`'s byte-size model

FR-003 requires bounding the aggregate (a long-running service's sidecar) without ever
truncating an individual record. `WalWriter`'s `max_bytes_per_file`/rotation pattern
(Mutex-guarded file, append + flush per write, rotate when the next write would exceed the cap)
is the direct in-repo precedent.

**`ExtractionFailureWriter` rotates at 20MB/file** (`DEFAULT_MAX_BYTES_PER_FILE`, matching
SC-003's arithmetic), keeping the first file at the unnumbered `<cassette>.failures.jsonl` path
— matching User Story 1's acceptance scenario literally — and rotating to numbered
`<cassette>.failures.N.jsonl` files thereafter. `max_bytes_per_file = 0` disables rotation,
matching `WalWriter::new`'s own convention. Old rotated files are never deleted — rotation and
retention/cleanup are separate concerns, exactly as in the WAL. The sidecar file is created
eagerly on writer construction (empty if no failures ever occur), matching
`CassetteWriter::open`'s own eager-creation behavior.

## Consequences

- `AnthropicExtractor`/`OaiExtractor` now reach below the `Extractor` trait boundary for failure
  telemetry — a deliberate, documented extension of ADR-0044 Decision 1, not a violation of it.
  `RecordingExtractor`/`ReplayingExtractor` themselves are unchanged and still touch only the
  trait boundary.
- The cassette's success-only invariant (FR-007) and the `#279` duplicate-key/identical-backend
  guards are unaffected: failures never reach `RecordingExtractor::record`, only the sidecar.
- `CountingSink`'s new `truncated` tally (FR-005) is aggregate-per-candidate only; chunk-level
  attribution lives solely in the sidecar (FR-001/FR-004), never in the eval report itself.
- No live golden-corpus re-run of the qwen3.6-35b-a3b capture referenced in the issue's
  Background ships with this change (SC-001) — no such cassette exists in-repo, and
  `docs/history/extraction-eval-2026-07.md` (cited by the spec) does not exist either. This is a
  manual maintainer follow-up, mirroring ADR-0044's own precedent for its golden-corpus
  cassette.
- This issue is observability-only (SC-004): no change to `ExtractionResult`, to edge-budget-
  exhaustion semantics, or to any existing cassette's judged/strict F1.

## Related

- ADR-0044: LLM Cassette Record/Replay Seam — the trait-boundary-only seam this issue
  deliberately extends for failure telemetry specifically, while leaving the seam itself
  (`RecordingExtractor`/`ReplayingExtractor`) unchanged.
- ADR-0010: `tool_use` structured-output extraction — why Anthropic's raw response body is a
  tool-call JSON blob (`resp["content"]`) rather than free text, unlike the OAI-compatible path's
  `choices[0].message.content` string. Both are stored as the full serialized response JSON in
  the sidecar, keeping the record schema uniform across providers.
- ADR-0048: Rust Extraction-Quality Eval Harness — the `CountingSink`/`BackendRunResult`/
  `CandidateReport` architecture this issue's `truncated` count extends.
- `crates/core/src/extraction_failures.rs`: `ExtractionFailureRecord`, `ExtractionFailureWriter`,
  `ExtractionFailureSink`.
- `crates/core/src/telemetry.rs`: `TelemetryEvent::ExtractionFailure`, `TeeSink`.
- `crates/core/src/extractor.rs`: `SendOutcome`/`ChatFailure`, `send_with_retry`, both providers'
  `do_extract_entities`/`do_extract_edges`.
- `crates/eval/src/main.rs`, `crates/service/src/main.rs`: the four `RecordingExtractor`
  construction sites where the sidecar is wired in.
- `crates/core/tests/cassette_record_replay.rs`: integration coverage for all three failure
  classes, the no-sidecar-in-replay-mode edge case, and the chunk-key-excluded-from-hash
  property.
- README.md's "Record/replay cassettes" section: user-facing documentation of the sidecar path
  convention, schema, and rotation behavior.
