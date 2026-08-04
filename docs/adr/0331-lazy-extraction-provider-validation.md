# ADR-0331: Validate the Extraction Provider on First Use, Not at Startup

**Status**: Accepted
**Date**: 2026-08-03
**Issues**: #331 (this change); regression reported by a downstream consumer in #330; supersedes
the "fatal at startup" sub-claim of ADR-0041 Decision 2/3 (#212)

## Context

Since 0.11.0, `lcg-service` refused to **start** unless an extraction provider was configured —
`ANTHROPIC_API_KEY`, `--extractor-uds`/`--extractor-http`, or `LCG_EXTRACTION_URL`. This arrived
with #212 ([ADR-0041](0041-local-openai-compatible-extraction-adapter.md)), which fixed a real
problem (#201: "extraction requires `ANTHROPIC_API_KEY` despite docs marking it optional") by
making provider selection explicit — no provider is ever auto-detected or silently substituted.
That fix was correct, and ADR-0041's precedence order is untouched by this ADR.

But ADR-0041 also made the *absence* of any configured provider a startup-time fatal error, and
that over-applied the fix. A downstream read-only consumer ("orac", a laptop MCP reader, reported
in #330) starts `lcg-service` to serve `knowledge_find_*`/`knowledge_status`/
`knowledge_search_passages` and to hydrate a local database from a published WAL via
`knowledge_rebuild_from_wal` — pure Cypher replay, no extraction anywhere in that path. Requiring a
provider to even start forced a real API key, or a deliberately unreachable placeholder endpoint
(`--extractor-http http://127.0.0.1:1/disabled`), onto a path that never calls the extractor.

ADR-0041 Decision 2 already established that startup performs no extraction reachability probe —
nothing dials the configured endpoint before the first real call. The startup check therefore only
ever validated *configuration presence*, never *reachability*, which is precisely why the
reporter's placeholder workaround satisfies it: an unreachable URL is still "present". A check that
cannot distinguish "misconfigured" from "absent" buys nothing for the misconfigured case it would
ideally catch, while actively blocking the legitimate absent case.

## Decision

Move the "no extraction provider configured" check from a startup-time fatal error to a call-time
error, produced independently by each extraction-dependent method for as long as the service
remains unconfigured. Provider *selection* is unaffected: `bootstrap_app_state` still resolves the
provider synchronously, once, at startup, following ADR-0041's exact precedence (explicit CLI flag
> `ANTHROPIC_API_KEY` > `LCG_EXTRACTION_URL`). Only the "nothing at all is configured" outcome
changes — from `return Err(...)` (propagating out of `main()` and exiting the process) to
constructing a stub `Extractor` implementation, `UnconfiguredExtractor`
(`crates/core/src/extractor.rs`), and continuing startup normally.

`UnconfiguredExtractor`'s three trait methods (`extract`, `classify_entities`,
`classify_relations`) each immediately return `Err(Error::Config(NO_EXTRACTION_PROVIDER_MSG))` —
the same message ADR-0041's startup error used to print, moved verbatim. This requires no changes
to `AppState` (still `Arc<dyn Extractor>`, not `Option`), no changes to any of the three real call
sites (`episode.rs`, `handlers.rs`, `reprocess_relations.rs` — all already `Result`-propagating with
no panics), and no changes to IPC/MCP error plumbing (the generic error-response path already turns
any `Error`, including `Error::Config`, into a clean structured response).

A startup log line (`extractor: provider=none, extraction calls will fail until configured`) is
still emitted in this case, mirroring every other resolution outcome's log line, so an operator who
genuinely forgot to configure a provider can still notice it in `stderr` rather than only
discovering it on first extraction attempt.

No cassette-recording wrapper (`RecordingExtractor`) is applied when `LCG_RECORD_LLM` is set
together with no provider configured — there is no live call to record, and wrapping a stub that
never dials out would be meaningless. `LCG_REPLAY_LLM` (cassette replay) is unaffected: it already
bypasses provider resolution entirely (`ReplayingExtractor`), so a replay-only deployment needs
nothing else, both before and after this change.

## What did not change

- **ADR-0041's precedence order** (explicit flag > `ANTHROPIC_API_KEY` > `LCG_EXTRACTION_URL`) is
  byte-for-byte identical. A configured deployment's startup log lines, provider selection, and
  extraction behavior are unaffected.
- **No reachability probe was added.** This ADR only defers the *presence* check; ADR-0041
  Decision 2's "no live probe at startup" reasoning is untouched. An unreachable-but-configured
  provider still only fails at the moment a call is actually attempted, same as today.
- **Explicit-flag validation stays startup-fatal.** `--extractor-uds <path>` pointing at a
  nonexistent socket file, or `--extractor-http <url>` with a malformed URL, are still rejected at
  startup — those checks validate a value the operator explicitly gave, which is a different
  failure mode than nothing being configured at all, and are out of this ADR's scope.
- **The embedder's startup requirement is unchanged.** This ADR is extraction-only; the embedder
  remains a required-at-startup dependency (`--embedder-uds`/`--embedder-http`/`LCG_EMBEDDING_URL`)
  with its existing probe-and-fail-fast behavior.

## Consequences

- `lcg-service --embedder-http <url>` with no extraction provider now starts, serves reads, and
  completes `knowledge_rebuild_from_wal` — the regression in #330 is fixed. `--mcp-stdio
  --scope read` also starts with no provider, since it shares the same `bootstrap_app_state` path.
- `knowledge_process_chunk`, `knowledge_add_episode`, `knowledge_reprocess_entity_types`, and
  `knowledge_reprocess_relation_types` are the methods that require the extractor and now fail at
  call time instead of never being reachable. `knowledge_canonicalize_relations` does **not**
  require the extractor — its ontology-description fallback only calls the embedder, a separate
  dependency (confirmed by inspection of `crates/core/src/canonicalize.rs`, which has no
  `state.extractor` reference).
- The placeholder workaround from #330
  (`--extractor-http http://127.0.0.1:1/disabled`) is no longer necessary for a read-only
  deployment to start.
- This composes cleanly with degraded-mode startup
  ([ADR-0009](0009-degraded-mode-startup-recovery.md)): the two "start anyway" behaviors are set at
  unrelated points in `bootstrap_app_state` (extractor resolution happens before DB open) and do
  not interact.

## Related

- ADR-0041: introduced the startup-fatal check this ADR supersedes, and the precedence order this
  ADR preserves unchanged.
- ADR-0009: precedent for "start anyway, fail clearly at first use of the missing dependency"
  (`ArcSwapOption<Db>` / `Error::DbUnavailable`), mirrored here for the extractor via a stub
  `Extractor` implementation rather than an `Option`.
- `crates/core/src/extractor.rs`: `UnconfiguredExtractor`, `NO_EXTRACTION_PROVIDER_MSG`.
- `crates/service/src/main.rs` (`bootstrap_app_state`): `ResolvedExtractor::Unconfigured`.
- `crates/service/tests/extractor_optional.rs`: regression coverage (SC-001–SC-003).
