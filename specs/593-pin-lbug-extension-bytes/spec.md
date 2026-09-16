# Feature Specification: Pin lbug extension bytes and diagnose ABI staleness in stage-lbug-extensions.sh

**Feature Branch**: `fabrik/issue-593`
**Created**: 2026-09-15
**Status**: Specified
**Input**: User description: "scripts/stage-lbug-extensions.sh trusts whatever the CDN serves — pin the extension bytes and assert their ABI version"

## Background

`scripts/stage-lbug-extensions.sh` downloads the lbug `vector` and `fts` extensions from `extension.ladybugdb.com` at build time and stages them into the release archive. It currently verifies only that the downloaded files exist and are non-empty — it does not verify **which** files it got.

That is unsound because upstream publishes extensions at a per-minor path and overwrites them on patch releases: the files served at `v0.20.0/win_amd64/` are 0.20.2-era builds served under a `v0.20.0` path, and `LBUG_EXTENSION_VERSION` (the lbug minor) means every 0.20.x patch resolves to that same path and gets whatever was most recently built there. The extension ABI a build picks up therefore depends on *when* the build runs, not on anything in this repository.

This has real, observed consequences on Windows:

| core | CDN extensions (0.20.2-era) |
|---|---|
| 0.20.0 | crashes at `LOAD EXTENSION` |
| 0.20.2 | works |
| 0.20.3 | works |
| 0.20.4 | loads, then crashes at `CREATE_VECTOR_INDEX` / `CREATE_FTS_INDEX` |

A silent upstream overwrite can turn a working bundle into a process-crashing one with no change on this repository's side, and critically, no build failure — the staging script succeeds, Linux/macOS CI passes, and only Windows users find out. The currently-shipping v0.15.0 release depends on this exact accident of upstream's publishing schedule (0.20.2-era extensions happen to be ABI-compatible with the pinned 0.20.3 core), and upstream is expected to rebuild and republish that per-minor path again when it cuts 0.21.x.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Pin and verify extension bytes (Priority: P1)

As the maintainer of the release build, I need `stage-lbug-extensions.sh` to fail immediately when the bytes it downloads from the CDN differ from a known-good, checked-in set, so that an upstream overwrite at the shared per-minor CDN path can never silently change what ships without a build failure.

**Why this priority**: This alone closes the vulnerability described in the issue. Without it, a build can silently bundle ABI-incompatible extensions with no CI signal — exactly the 0.20.4-vs-0.20.2-era-extension pairing already observed to crash on Windows.

**Independent Test**: Run the staging script against a pin file with a deliberately wrong sha256 for one platform/extension pair and confirm the build fails with a clear error before staging any files. Run it again with correct pins and confirm the build succeeds unchanged.

**Acceptance Scenarios**:

1. **Given** a checked-in pin file with the expected sha256 for each staged platform+extension pair, **When** `stage-lbug-extensions.sh` downloads an extension from the CDN, **Then** it computes the sha256 of the downloaded file and fails the build with a non-zero exit if it does not match the pinned value.
2. **Given** the downloaded bytes match every pinned hash, **When** the script completes, **Then** staging proceeds exactly as it does today, with the extensions placed in the release archive unchanged.
3. **Given** a new platform is added to the staging matrix, **When** no pin exists yet for that platform+extension pair, **Then** the build fails rather than silently accepting unpinned bytes.

---

### User Story 2 - Diagnose why a pin stopped matching (Priority: P2)

As the person responding to a hash-mismatch failure, I need the script to tell me the embedded release version of the bytes it actually received, so I can tell a benign upstream patch rebuild from a suspicious change without downloading and inspecting the binary myself.

**Why this priority**: This is a diagnostic aid layered on top of the P1 gate — it doesn't close the hole itself, but it turns a mismatch failure into an actionable one, directly addressing the case that hurt: a rebuild that looked identical in shape (file exists, non-empty) to the working build but carried an incompatible ABI.

**Independent Test**: Point the script at a binary with a known embedded version-string set and confirm it reports the maximum version present, verified against a binary where a positional/table read would report the wrong value due to string pooling.

**Acceptance Scenarios**:

1. **Given** a hash mismatch on a platform whose extension binary is not stripped (e.g. Windows), **When** the script reports the failure, **Then** it also prints the highest of the distinct `MAJOR.MINOR.PATCH` version strings found in the downloaded binary, computed by scanning for all matches and taking the maximum — not by reading any embedded version table positionally, since compiler string pooling makes the order of literals in the binary unreliable.
2. **Given** a hash mismatch on a platform whose extension binary is stripped (macOS, Linux), **When** the script reports the failure, **Then** it reports the hash mismatch alone, without a fabricated or misleading version diagnostic.
3. **Given** the version diagnostic reports a version, **When** a human reads the failure, **Then** nothing in the message claims or implies that version equality/inequality equals ABI compatibility/incompatibility — it is presented as informational only, since the currently-shipping pairing intentionally bundles a different-version extension with the pinned core and works.

---

### Edge Cases

- CDN request fails outright (network error, 404) — existing behavior (fail build) is unchanged; this is not a new case introduced by this feature.
- A platform's extension binary is stripped of debug/version strings (macOS, Linux) — the version diagnostic is unavailable there by design; the sha256 check is the only signal.
- A future platform is added to the staging matrix without a corresponding pin entry — the build must fail rather than skip the check.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `stage-lbug-extensions.sh` MUST compare the sha256 of every downloaded extension file against a checked-in expected value before staging it into the release archive.
- **FR-002**: The expected sha256 values MUST be stored in a checked-in file, one entry per platform per extension (`vector`, `fts`), alongside `LBUG_EXTENSION_VERSION`.
- **FR-003**: If a downloaded file's sha256 does not match its pinned value, the script MUST fail the build (non-zero exit) before staging any files, with an error identifying which platform/extension pair mismatched.
- **FR-004**: If a platform+extension pair has no pinned hash entry, the script MUST fail the build rather than silently accepting the download unpinned.
- **FR-005**: On a hash mismatch, if the downloaded binary contains embedded version strings (i.e., is not stripped), the script MUST additionally report the highest of the distinct `MAJOR.MINOR.PATCH` version strings found in the binary, computed as the maximum over every matched version-shaped token — not by a positional or first-match read of any version table.
- **FR-006**: On a hash mismatch for a stripped binary (no embedded version strings found), the script MUST report the hash mismatch without attempting to synthesize a version diagnostic.
- **FR-007**: The version diagnostic from FR-005 MUST be informational only — the script MUST NOT treat it as a pass/fail signal, and any accompanying message MUST NOT assert or imply that a version match/mismatch equals ABI compatibility/incompatibility.
- **FR-008**: The initial set of pinned hashes MUST include, at minimum, the `win_amd64` `vector` and `fts` extensions currently bundled in the published v0.15.0 release — the pairing already verified through loop testing against the pinned core.
- **FR-009**: Updating a pinned hash (re-verifying and re-pinning after a legitimate upstream republish) is a manual, human-driven action — the script MUST NOT auto-update its own pins.

### Key Entities

- **Extension pin file**: checked-in record mapping (platform, extension name) → expected sha256, maintained alongside the existing `LBUG_EXTENSION_VERSION` setting.
- **Embedded version string**: a `MAJOR.MINOR.PATCH` token found literally in an extension binary; used only as a diagnostic on mismatch, never as an authority for ABI compatibility.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A staging run against unmodified, currently-pinned extensions completes with no behavior change from today (files staged, archive built as before).
- **SC-002**: A staging run where the CDN serves different bytes than pinned (simulated) fails the build before any file is staged into the release archive, for every platform/extension pair the matrix covers.
- **SC-003**: For every non-stripped platform's simulated mismatch, the failure output names the highest embedded version string found in the received binary.
- **SC-004**: No pin value changes without a corresponding commit to the checked-in pin file — the mechanism cannot be satisfied by simply re-running the build against newly-served bytes.

## Assumptions

- The pin file covers the two extensions this issue is about — `vector` and `fts` — bundled by `stage-lbug-extensions.sh`. It does not extend to the OpenSSL DLLs bundled by the separate `scripts/stage-openssl-rpath.sh` staging path, which already guards its own supply chain via `scripts/assert-openssl-linkage.sh`.
- The initial pin values for `win_amd64` are the ones recorded and reconfirmed against the published v0.15.0 Windows release archive in the issue discussion. Pin values for the other three staged platforms (`aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`) will be computed from the bytes already bundled in that same v0.15.0 release during implementation, since only their file sizes — not their hashes — were recorded in the issue discussion.
- Re-verifying and re-pinning after a legitimate upstream republish (e.g., the expected 0.21.x cut) is a follow-up action taken by a human when the build fails — it is not automated as part of this issue.

## Out of Scope

- Automating the re-pin/re-verify workflow after a legitimate upstream republish.
- Pinning or verifying the OpenSSL DLLs bundled alongside the Windows extensions.
- Changing how extensions are downloaded, cached, or versioned (`LBUG_EXTENSION_VERSION` itself) — only verification of what comes back.
- Upstream fixes tracked in LadybugDB/ladybug#971 (per-patch artifacts or a load-time ABI check).
- Windows extension functional test coverage, tracked separately in #587.

## Source References

- #559 (introduced `stage-lbug-extensions.sh`)
- #561 (0.20.3 retarget investigation that discovered the CDN overwrite behavior)
- #587 (Windows CI extension-load test coverage — complementary, not overlapping)
- LadybugDB/ladybug#971 (upstream issue tracking a per-patch artifact or load-time ABI check)
