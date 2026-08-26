# Feature Specification: Fix missing `weight.bin` in Swift CoreML test fixtures

**Feature Branch**: `fabrik/issue-518`
**Created**: 2026-08-26
**Status**: Specified
**Input**: User description: "Three Swift test fixtures are incomplete in git, and the `CoreMLEmbeddingActor — output schema validation` suite has been failing on `main` as a result. Nobody has seen it because `.github/workflows/swift.yml` is disabled (`if: false`, see #502), so `swift test` never runs in CI."

## Background

`native/local-inference/Tests/LocalInferenceTests/Fixtures/` contains five `.mlpackage`
fixtures used by the Swift test suite. An `.mlpackage` is a directory bundle whose
`Manifest.json` references its contents (including weight blobs) by identifier; CoreML
cannot open the package at all if any referenced file is missing.

Three of the five fixtures — `stub-bge-base-bad-dtype.mlpackage`,
`stub-bge-base-bad-output-name.mlpackage`, and `stub-bge-base-bad-shape.mlpackage` — have a
`Manifest.json` that references a `Data/com.apple.CoreML/weights` item with no backing
content in git, while the other two working fixtures have real weight blobs there. As a
result, the three tests in the `CoreMLEmbeddingActor — output schema validation` suite
(`EmbeddingOutputValidationTests.swift`) that load these fixtures fail with an unrelated
CoreML resource-loading error (`Item does not exist for identifier: ...`) instead of
exercising the schema-validation assertion they exist to make.

This was initially suspected to be a `.gitignore` trap in
`native/local-inference/.gitignore`:

```
*.mlpackage
...
!Tests/LocalInferenceTests/Fixtures/
!Tests/LocalInferenceTests/Fixtures/**
```

The theory: `*.mlpackage` excludes the fixture *directories* themselves, and git does not
descend into an excluded directory, so no `!` re-inclusion pattern beneath it can bring
files back in. **Research investigated this and found it does not reproduce** with the git
version in use (2.50.1) — a file added inside an already-tracked or brand-new `.mlpackage`
directory under `Tests/LocalInferenceTests/Fixtures/` is fully trackable via plain
`git add`, negation and all. The `.gitignore` pattern is still fragile and worth
root-anchoring defensively (see FR-002/FR-003), but it is not what dropped the weight
blobs.

The actual root cause, confirmed by regenerating the fixtures and bisecting the resulting
`swift test` pass/fail: these three stubs have zero learnable parameters by design, but
coremltools' `.mlpackage` writer still emits a manifest `weights` item pointing at an
*empty* `Data/com.apple.CoreML/weights/` directory. Git cannot track an empty directory, so
the directory silently disappears on checkout while `Manifest.json` still promises it's
there — the same class of defect as a missing file, but the git mechanism is "git drops
empty directories," not the `.gitignore` re-inclusion trap. See `Fixtures/README.md`'s
"Bug 785" retrospective for the full account.

This has been invisible because `.github/workflows/swift.yml` currently runs with
`if: false` (see #502 — GitHub's `macos-latest` runner image doesn't yet support the
Swift 6.2 tools version this package requires), so `swift test` has not run in CI at all
during the period this regression was introduced. The breakage was found only through
local verification (macOS 26.5.1 / Swift 6.3.3) while checking an unrelated PR (#516).

Because the tests fail before reaching their real assertion, the behavior they were written
to cover — that `CoreMLEmbeddingActor` raises a specific, named `LocalInferenceError` for
each kind of malformed model output schema (wrong dtype, wrong shape, missing output),
rather than a generic catch-all — is currently **entirely unverified**.

The project already has fixture-generation and freshness-check tooling for this directory
(`generate-bad-stub-models.py`, `check-fixture-freshness.sh`, `refresh-test-fixtures.sh`,
documented in `Fixtures/README.md`). `check-fixture-freshness.sh` already performs a
lightweight existence check per fixture, but today it only checks for `Manifest.json` —
it does not check for the weight blob a `Manifest.json` may reference, which is exactly
the gap that let this regression land undetected.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Schema-validation suite passes on a clean checkout (Priority: P1)

A contributor clones the repository fresh (or a CI runner checks it out) and runs
`swift test` inside `native/local-inference/` on a supported macOS/Swift toolchain. All
tests pass, including the three tests in the `CoreMLEmbeddingActor — output schema
validation` suite, and those three tests exercise the actual schema-mismatch assertions
they were written for (not a resource-loading error).

**Why this priority**: This is the actual bug. Until it's fixed, the three tests provide
zero coverage of the error-classification behavior they exist to verify, and the suite
reports a false failure that will block re-enabling Swift CI (#502).

**Independent Test**: From `native/local-inference/`, run `swift test`. All 45 tests
across all 10 suites pass, with 0 issues.

**Acceptance Scenarios**:

1. **Given** a fresh checkout of the fixed branch, **When** `swift test` is run on
   macOS with a supported Swift toolchain, **Then** `unsupportedOutputDtypeThrowsSpecificError`,
   `wrongOutputShapeThrowsSpecificError`, and `missingOutputThrowsSpecificError` all pass.
2. **Given** the fixed `stub-bge-base-bad-dtype.mlpackage` fixture, **When**
   `CoreMLEmbeddingActor` is initialized with it, **Then** initialization throws
   `LocalInferenceError.embeddingOutputDtypeUnsupported` (not a resource-loading error).
3. **Given** the fixed `stub-bge-base-bad-shape.mlpackage` fixture, **When**
   `CoreMLEmbeddingActor` is initialized with it, **Then** initialization throws
   `LocalInferenceError.embeddingOutputShapeMismatch`.
4. **Given** the fixed `stub-bge-base-bad-output-name.mlpackage` fixture, **When**
   `CoreMLEmbeddingActor` is initialized with it, **Then** initialization throws
   `LocalInferenceError.embeddingOutputMissing`.

---

### User Story 2 - `.gitignore` can no longer silently drop fixture files (Priority: P1)

A contributor adds or regenerates a fixture under
`Tests/LocalInferenceTests/Fixtures/*.mlpackage` and runs `git add` / `git status`. Every
file belonging to that fixture is trackable; none is silently swallowed by an unanchored
`*.mlpackage` ignore rule paired with a fragile `!` re-inclusion negation.

**Why this priority**: As the Background section explains, this ignore-pattern trap was
the originally suspected cause but did not turn out to be what dropped the three broken
fixtures' content (the actual cause was an empty `weights/` directory git cannot track —
see Bug 785). The pattern is nonetheless fragile: git's behavior for negating a pattern
inside an excluded directory is version- and path-dependent, so leaving it unanchored
still risks the same class of failure recurring for a different fixture in the future.
Root-anchoring it removes that risk as defense in depth, independent of the fix for the
actual defect.

**Independent Test**: After the fix, run `git check-ignore -v` (or equivalent) against
every file physically present under `Tests/LocalInferenceTests/Fixtures/**`; none should
report as ignored.

**Acceptance Scenarios**:

1. **Given** the fixed `.gitignore`, **When** a new file is added anywhere under
   `Tests/LocalInferenceTests/Fixtures/`, **Then** `git status` shows it as untracked
   (trackable), not silently ignored.
2. **Given** the fixed `.gitignore`, **When** `*.mlpackage` files exist outside
   `Tests/LocalInferenceTests/Fixtures/` (e.g. locally generated production model
   artifacts), **Then** they remain ignored as before — the fix must not broaden what's
   tracked outside the fixtures directory.

---

### User Story 3 - A missing fixture file fails loudly and specifically (Priority: P2)

A future change accidentally omits a file an `.mlpackage` fixture needs (the same class of
mistake that caused this issue). Running the existing fixture-freshness guard
(`check-fixture-freshness.sh`) — locally or in CI — fails with a targeted error naming the
missing file, before `swift test` runs and produces a confusing, unrelated failure.

**Why this priority**: This directly addresses "what this masks" from the original
report — distinguishing "fixture incomplete" from "assertion failed" is what would have
caught this regression immediately instead of it lying dormant, invisible, until someone
happened to run the suite locally. It extends guard logic that already exists in this
directory for a related but distinct purpose (script/fixture drift), rather than
introducing a new mechanism.

**Independent Test**: Deliberately delete a required file from within one fixture's
`Data/com.apple.CoreML/` tree and run `check-fixture-freshness.sh`; it exits non-zero and
names the specific missing file and fixture.

**Acceptance Scenarios**:

1. **Given** a fixture missing a file required for CoreML to open it, **When**
   `check-fixture-freshness.sh` runs, **Then** it exits non-zero and its error output
   names both the fixture and the specific missing file.
2. **Given** all five fixtures complete and correct, **When**
   `check-fixture-freshness.sh` runs, **Then** it exits zero.

---

### Edge Cases

- The three fixtures needing repair are hand-rolled, deliberately-invalid stubs (per
  `Fixtures/README.md`, they "cannot be produced by any valid production conversion"),
  generated by `generate-bad-stub-models.py`. Any fix must preserve their documented
  purpose: exercising exactly one schema-validation failure mode each, with the same
  input contract as the production model.
- The fixture-generation tooling (`generate-bad-stub-models.py`) is macOS-only
  (depends on `coremltools`' CoreML conversion, which requires macOS). The chosen fix must
  work within that constraint — it does not need to run on Linux.
- The existing `*.script-hash` sentinel mechanism guards against a fixture drifting from
  its *generator script*; it does not, today, guard against a fixture that matches its
  generator script but is nonetheless missing a required file in git (e.g., a partial
  `git add -f`). These are distinct failure modes and both must be caught.
- `.github/workflows/swift.yml` is disabled (`if: false`) pending #502 and stays that way;
  no CI job runs `swift test` or `check-fixture-freshness.sh` automatically on this branch
  as a direct result of this issue. Verification here is necessarily local/manual until
  #502 lands.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: All five `.mlpackage` fixtures under
  `native/local-inference/Tests/LocalInferenceTests/Fixtures/` MUST contain every file
  their `Manifest.json` references (including weight blobs), such that CoreML can open
  each package successfully from a fresh git checkout.
- **FR-002**: `native/local-inference/.gitignore` MUST NOT rely on a pattern that excludes
  a directory under `Tests/LocalInferenceTests/Fixtures/` and then attempts to re-include
  files within it via `!` negation, since git cannot honor negations inside an excluded
  directory. Files physically present under `Tests/LocalInferenceTests/Fixtures/**` MUST
  be trackable by git.
- **FR-003**: The `.gitignore` fix MUST NOT change ignore behavior for `.mlpackage` (or
  other currently-ignored) files outside `Tests/LocalInferenceTests/Fixtures/` — e.g.
  locally-generated production model artifacts under `native/local-inference/` must
  remain ignored.
- **FR-004**: `unsupportedOutputDtypeThrowsSpecificError`, `wrongOutputShapeThrowsSpecificError`,
  and `missingOutputThrowsSpecificError` in `EmbeddingOutputValidationTests.swift` MUST
  pass when run via `swift test` on a supported macOS/Swift toolchain, and each MUST reach
  and satisfy its documented `LocalInferenceError` case assertion (not fail earlier on a
  resource-loading error).
- **FR-005**: The fixture-freshness guard (`check-fixture-freshness.sh` or its logical
  successor) MUST detect when an `.mlpackage` fixture is missing a file required for
  CoreML to load it (not only a missing `Manifest.json`), and MUST report which fixture
  and which file is missing when it fails.
- **FR-006**: Any fixture regeneration performed to satisfy FR-001 MUST preserve each
  fixture's documented single-purpose rationale in `Fixtures/README.md` (bad dtype, bad
  shape, bad output name respectively) and MUST NOT change which `LocalInferenceError`
  case each fixture is expected to trigger.

### Key Entities

- **`.mlpackage` fixture**: A directory-structured CoreML model bundle used as test input;
  identified in git by tracked files including `Manifest.json` and
  `Data/com.apple.CoreML/weights/weight.bin`.
- **Fixture freshness guard**: The existing `check-fixture-freshness.sh` script (and its
  `.script-hash` sentinel files) that verifies committed fixtures are complete and match
  their generator scripts.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Running `swift test` from `native/local-inference/` on a fresh checkout
  (macOS 26.5.1 / Swift 6.3.3 or later supported toolchain) reports 0 failures across all
  suites, including the `CoreMLEmbeddingActor — output schema validation` suite (3/3
  passing).
- **SC-002**: `git check-ignore -v` reports no matches for any file physically present
  under `native/local-inference/Tests/LocalInferenceTests/Fixtures/**`.
- **SC-003**: Deliberately removing a required file from a fixture and re-running the
  fixture-freshness guard produces a non-zero exit and an error message naming the
  specific missing fixture file, verifying the new guard behavior added under FR-005.

## Assumptions

- The three broken fixtures should be repaired by regenerating them with the existing
  `generate-bad-stub-models.py` (the documented, tested source of these exact stubs) and
  force-adding the resulting files, consistent with how the two working fixtures were
  evidently produced — rather than inventing a new fixture format. The precise mechanics
  (e.g. whether to also bump `.script-hash` sentinels) are a Research/Plan-stage decision.
- "Fails loudly" (FR-005) extends the existing `check-fixture-freshness.sh` script rather
  than introducing a new, separate tool, since that script already performs a related
  per-fixture existence check (`Manifest.json`) in the exact place this gap exists. The
  specific implementation (e.g., checking for `weights/weight.bin` explicitly vs. parsing
  `Manifest.json` to check referenced files generically) is a Research/Plan-stage decision.
- Re-enabling `.github/workflows/swift.yml` (tracked separately as #502) is explicitly
  out of scope for this issue; see "Out of Scope" below.

## Out of Scope

- Re-enabling `.github/workflows/swift.yml` (`if: false`) — tracked by #502. This issue
  only needs to ensure that when Swift CI *is* eventually enabled (or when a contributor
  runs `swift test` locally), the output-schema validation suite is not broken by
  incomplete fixtures.
- Wiring `check-fixture-freshness.sh` into `swift.yml` as a CI step — moot while that
  workflow is disabled; can be revisited alongside #502.
- Any change to the production model conversion pipeline (`convert-embedding-model.py`)
  or the two working stub fixtures (`stub-bge-base.mlpackage`,
  `stub-bge-base-fp16.mlpackage`), which are unaffected by this bug.
- Broader `.gitignore` review beyond `native/local-inference/.gitignore`'s fixture
  directory trap.

## Source References

- `native/local-inference/.gitignore`
- `native/local-inference/Tests/LocalInferenceTests/Fixtures/README.md`
- `native/local-inference/Tests/LocalInferenceTests/Fixtures/generate-bad-stub-models.py`
- `native/local-inference/Tests/LocalInferenceTests/EmbeddingOutputValidationTests.swift`
- `native/local-inference/check-fixture-freshness.sh`
- `native/local-inference/refresh-test-fixtures.sh`
- `.github/workflows/swift.yml`
- #502 (re-enable Swift CI), #501 / #516 (where this was discovered)
