# Feature Specification: Cover embedder cassette recordings with API-key leak tests

**Feature Branch**: `fabrik/issue-509`
**Created**: 2026-09-01
**Status**: Specified
**Input**: User description: "Cassette recordings are not covered by #497's API-key leak tests"

## Background

#497 added Bearer-token auth to the embedder's HTTP transport (merged in #506). Its leak coverage is good at the process level — `crates/service/tests/embedder_auth.rs::stderr_never_contains_configured_key_value` spawns the real binary with a sentinel key and asserts the key value appears nowhere in the child process's stderr.

Cassettes are a separate leak surface, and they were not covered by that work. `crates/core/tests/cassette_record_replay.rs` already asserts, in its `RecordingExtractor` coverage, that a recorded cassette contains neither an `Authorization` header nor the API key value. That assertion predates #497 and was written when no code path could produce an `Authorization` header at all: the embedder previously sent no auth, and the LLM extractor authenticates via `x-api-key`, not `Authorization`. It guarded against a hypothetical.

As of #506, the embedder emits `Authorization: Bearer <key>` on every HTTP request. Inspection of `crates/core/src/embedder.rs` confirms it has no cassette/recording integration of its own, and `crates/core/tests/cassette_record_replay.rs` has no embedder-related coverage — only `RecordingExtractor` (which wraps the LLM extractor, not the embedder) is exercised there. So the existing "cassette must never contain an Authorization header" assertion has never actually been evaluated against an embedder-recorded interaction, the one code path that can currently produce that header. The assertion now protects against something real, but nothing verifies it fires for the embedder specifically.

Cassettes matter more than the stderr case because they are recorded fixtures that get committed to the repository. A leaked key in stderr is transient and local to a single process run. A leaked key baked into a cassette is pushed, code-reviewed, merged, and permanently retained in git history — and disclosed if the repository is public. This gap is not urgent on its own (recording a new cassette against an authenticated endpoint is not part of any routine workflow today), but it is exactly the kind of gap that stays invisible until the day someone does trigger that workflow, at which point the leak is already permanent.

This is a follow-up to #497 / #506, raised during review of #506 and deliberately not held against that PR, which was otherwise clean.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Embedder cassette recording never leaks the API key (Priority: P1)

A developer records a new cassette fixture for a test that exercises the embedder against an authenticated HTTP endpoint (real or stubbed). The recording machinery writes the cassette file to disk. Before that cassette can be committed, an automated test proves the resulting file contains neither the `Authorization` header nor the configured API key's value, anywhere in the file — not just absent from an obvious header field.

**Why this priority**: This is the entire subject of the issue. Without it, a leaked key in a committed cassette is a real, unguarded possibility rather than a hypothetical.

**Independent Test**: Run a new automated test that configures a sentinel API key, drives an embedder call through the cassette-recording path against a stub HTTP endpoint, and inspects the raw bytes of the resulting cassette file for both the header name and the key value. The test can be written and run entirely independently of any other change.

**Acceptance Scenarios**:

1. **Given** an embedder configured with a sentinel API key and a stub HTTP endpoint that requires `Authorization: Bearer <key>`, **When** a request is recorded through the cassette-writing path, **Then** the stub confirms it actually received the `Authorization: Bearer <key>` header (so the subsequent "never leaks" assertion is not vacuously true).
2. **Given** the cassette file produced by that recording, **When** its raw contents are inspected, **Then** the file does not contain the literal API key value anywhere.
3. **Given** the cassette file produced by that recording, **When** its raw contents are inspected, **Then** the file does not contain the string `authorization` (case-insensitive) anywhere — not only absent as a JSON field name, so that a redaction which relocates the value under a different field name is still caught.

---

### Edge Cases

- If the embedder's HTTP transport does not currently pass through any cassette-recording wrapper at all (i.e., embedder calls are never recorded to a cassette in existing code), the new test must fail loudly (compile error, panic, or explicit assertion failure) rather than silently passing because no cassette was produced — a vacuous pass would recreate the exact blind spot this issue is about.
- The UDS (Unix domain socket) embedder transport never attaches an API key (per #497 FR-005), so this issue's coverage is scoped to the HTTP transport only. No cassette-leak test is required for the UDS path.
- If redaction is later found to rewrite the header under a different name or location, the key-value and case-insensitive `authorization` string checks (Acceptance Scenarios 2 and 3) must still catch it, since they do not depend on where in the file the value or header name would appear.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The test suite MUST include a test that confirms whether `crates/core/tests/cassette_record_replay.rs`'s existing "cassette must never contain an Authorization header" assertion (and its sibling key-value assertion) is actually exercised against an embedder-recorded interaction, as opposed to only extractor-recorded interactions. If the cassette-recording machinery routes the embedder and the extractor through different code paths such that embedder requests never reach that assertion today, this MUST be surfaced explicitly (e.g., a new dedicated test, or an inline note in the existing test documenting the gap) rather than left implicit.
- **FR-002**: A new automated test MUST record an embedder HTTP interaction — via whatever cassette-recording mechanism the embedder uses, or a newly wired one if none currently exists — with a real API key configured (a sentinel value, distinct from any value used elsewhere in the test suite, to keep the assertion unambiguous).
- **FR-003**: That test MUST assert the resulting cassette file's raw contents contain neither the `Authorization` header name (case-insensitive) nor the configured sentinel key's literal value, anywhere in the file.
- **FR-004**: That test MUST include a sanity check proving the sentinel key was actually transmitted over the wire during the recording (mirroring the existing extractor test's pattern of asserting the stub embedder received the expected header) before asserting on the cassette contents — this prevents the leak assertion from ever being vacuously true because the key never appeared anywhere to begin with.
- **FR-005**: The new coverage MUST be scoped to the embedder's HTTP transport. The UDS transport is out of scope per the existing #497 FR-005 guarantee that it never attaches a key.
- **FR-006**: If implementing this coverage reveals that the embedder has no cassette-recording integration at all (i.e., embedder calls are never written to a cassette in current code), that finding MUST be documented as part of this issue's resolution (e.g., in the PR description or a code comment), since it changes the shape of the fix — the test may then need to wire minimal recording support for the embedder rather than exercising an existing path.

### Key Entities

- **Cassette**: A recorded fixture file capturing an HTTP interaction (request/response), committed to the repository and replayed in tests without hitting a live endpoint.
- **Sentinel API key**: A synthetic, clearly-fake credential value used only within a test, chosen so its presence in output is unambiguous evidence of a leak.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A test run against the current codebase (prior to any embedder-side redaction fix, if one turns out to be needed) either fails — demonstrating the leak is real and unguarded — or passes only because embedder cassette recording already correctly redacts the key, with the sanity check (FR-004) proving the check is not vacuous either way.
- **SC-002**: After this issue's changes land, `cargo test` in `crates/core` includes at least one test whose failure would be caused specifically by an embedder-recorded cassette containing an `Authorization` header or the raw API key value.
- **SC-003**: The relationship between the existing `cassette_record_replay.rs:516`-area assertions and embedder coverage is no longer ambiguous — a future reader can determine, from the test file alone, whether embedder interactions are covered by those assertions.

## Assumptions

- The fix is test-only unless investigation (in scope for the Research/Implement stages, not this spec) finds that embedder cassette recording is missing entirely or actively leaks the key, in which case minimal production code changes to redact the header/value before writing the cassette are in scope as a consequence of FR-006, not as separately negotiated scope.
- "Cassette-recording machinery" refers to whatever mechanism currently backs `crates/core/tests/cassette_record_replay.rs` (e.g., a wrapper analogous to `RecordingExtractor`) — the exact mechanism and whether an embedder equivalent exists is left to Research, per the Specify-stage boundary of not prescribing implementation.
- A stub HTTP endpoint (as used in `crates/service/tests/embedder_auth.rs`) is an acceptable stand-in for a real authenticated endpoint; no live third-party service needs to be contacted.

## Out of Scope

- Any change to the UDS embedder transport's auth behavior.
- Any change to the extractor's existing cassette leak coverage, which is already adequate per the issue.
- Broader redaction-mechanism refactoring beyond what is needed to close this specific gap.

## Source References

- `crates/core/tests/cassette_record_replay.rs` (existing "cassette must never contain an Authorization header" assertion, and the `RecordingExtractor`-based test it lives in)
- `crates/service/tests/embedder_auth.rs` (`stderr_never_contains_configured_key_value` — the process-level leak test this issue's cassette-level test is modeled after)
- `crates/core/src/embedder.rs` (embedder HTTP transport, Bearer-token attachment added by #497/#506)
- #497 (Bearer-token auth on the embedder's HTTP transport, FR-005/FR-006/FR-007/FR-009)
- #506 (merge of #497)
