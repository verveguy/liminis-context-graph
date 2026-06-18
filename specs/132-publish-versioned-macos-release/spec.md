# Feature Specification: Publish Versioned macOS Release Binary for liminis-context-graph

**Feature Branch**: `fabrik/issue-132`
**Created**: 2026-06-18
**Status**: Draft
**Input**: User description: "Publish a versioned macOS (arm64) release binary of `liminis-context-graph` as a GitHub release artifact (e.g. built and attached on tag), so downstream packaging can download a pinned version instead of building from source."

## Background

`liminis-context-graph` (this repo) is consumed by the Liminis Electron app (`verveguy/liminis`), which bundles the binary via `electron-builder` (`mac.extraFiles` → `resources/bin/liminis-context-graph`). The app's build helper (`scripts/build-liminis-context-graph.sh` in `verveguy/liminis`) currently builds the binary from a sibling source checkout. This fails in the app's release CI, which only checks out `verveguy/liminis` — not this repo.

Issue #124 delivered the release infrastructure: a `release.yml` workflow (cargo-dist v0.32.0) configured with `aarch64-apple-darwin` as a target, which triggers on semver tags and produces per-platform `.tar.gz` archives with SHA-256 checksums and a `curl | sh` installer. However, **no version tag has ever been pushed**, so no binary has actually been published. The macOS arm64 build path has never been exercised end-to-end.

This issue closes the remaining gap: exercise the macOS arm64 release path, verify artifacts appear on GitHub Releases, and ensure consumers (principally `verveguy/liminis`) can discover and download a pinned binary version without building from source.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Release Tag Produces a Downloadable macOS arm64 Binary (Priority: P1)

When the maintainer pushes a semver tag (e.g., `v0.1.0`), the release workflow builds `liminis-context-graph` for macOS arm64 and attaches it to a GitHub Release so any downstream consumer can download it by URL.

**Why this priority**: This is the unblocking action for `verveguy/liminis` release CI — without a published binary, the packaged app ships empty.

**Independent Test**: Push tag `v0.1.0-beta.1` (or any pre-release tag). Verify a GitHub Release appears within 45 minutes containing `liminis-context-graph-aarch64-apple-darwin.tar.gz` and an accompanying `.sha256` checksum. Download the archive; extract and run `./liminis-context-graph-aarch64-apple-darwin/liminis-context-graph --help` on an Apple Silicon Mac. It must exit 0.

**Acceptance Scenarios**:

1. **Given** the maintainer pushes tag `v0.1.0` (or any `v*.*.*` tag), **When** the release workflow completes, **Then** a GitHub Release is visible with at minimum `liminis-context-graph-aarch64-apple-darwin.tar.gz` and its SHA-256 checksum attached.
2. **Given** the published archive, **When** a user extracts it on an Apple Silicon Mac, **Then** `./liminis-context-graph --help` exits 0 and prints a usage line.
3. **Given** the artifact and its checksum file, **When** the user runs `shasum -a 256 -c liminis-context-graph-aarch64-apple-darwin.tar.gz.sha256`, **Then** the output says `OK` and exits 0.

---

### User Story 2 — Consumer Can Pin a Specific Version by URL (Priority: P1)

A downstream consumer (`verveguy/liminis` or any other) can construct a stable download URL for a specific tagged version of the binary, without needing to clone this repo or run Rust tooling.

**Why this priority**: `verveguy/liminis`'s release CI is the unblocking consumer — it must be able to download a pinned binary. "Pinned" means reproducible: the same tag always produces the same URL.

**Independent Test**: Given a published release tag `v0.1.0`, a script that runs `curl -L https://github.com/verveguy/liminis-graph/releases/download/v0.1.0/liminis-context-graph-aarch64-apple-darwin.tar.gz -o /tmp/lcg.tar.gz && tar -xzf /tmp/lcg.tar.gz -C /tmp` should produce a working binary at `/tmp/liminis-context-graph-aarch64-apple-darwin/liminis-context-graph`.

**Acceptance Scenarios**:

1. **Given** a published GitHub Release, **When** a consumer uses the documented URL pattern `https://github.com/verveguy/liminis-graph/releases/download/<TAG>/liminis-context-graph-aarch64-apple-darwin.tar.gz`, **Then** the download succeeds (HTTP 200) and the extracted binary is a valid Mach-O arm64 executable.
2. **Given** the GitHub Releases API endpoint `https://api.github.com/repos/verveguy/liminis-graph/releases/latest`, **When** queried, **Then** it returns the latest release tag so a consumer can discover the latest pinned version programmatically.

---

### User Story 3 — README Documents Consumer Download Pattern (Priority: P2)

A first-time consumer reading the README understands how to download a pinned binary version without building from source.

**Why this priority**: Discoverability is the last mile — the existing Quickstart (from #124) covers the OSS install UX, but the app-bundling consumer use case (download-by-URL for a pinned version, extract, place binary) is a different pattern that needs its own documentation.

**Independent Test**: Read README's Quickstart and verify a "Download a specific version" or "Bundling in downstream apps" section exists explaining the tarball URL pattern and extraction steps.

**Acceptance Scenarios**:

1. **Given** the merged PR, **When** the README's Quickstart or a dedicated "Bundling" section is read, **Then** a consumer can identify the download URL pattern for a pinned tag, the archive structure, and how to extract the binary.
2. **Given** the README, **When** a macOS consumer reads it, **Then** the Gatekeeper quarantine note (`xattr -d com.apple.quarantine`) is present so the binary can be launched without a code-signing certificate.

---

### Edge Cases

- **macOS arm64 lbug build cache miss**: The `build-lbug` CI job only pre-populates the lbug cache for Ubuntu runners. The macOS arm64 release build will cache-miss on first run and recompile lbug's Rust wrapper against the prebuilt lbug 0.17.0 bundle. This is expected to add ~15–25 minutes to the first macOS release run; it is not a failure. (A follow-up can add a macOS lbug cache warm-up job if release times become painful.)
- **Tag pushed but workspace version doesn't match**: cargo-dist validates the tag against `[workspace.package].version` in `Cargo.toml` and fails loudly. Maintainer must bump `Cargo.toml` version before tagging.
- **Pre-release tag (`v0.1.0-beta.1`)**: cargo-dist marks the GitHub Release as pre-release. The binary is still produced and downloadable; consumers pinning pre-release versions do so deliberately.
- **Gatekeeper quarantine on downloaded binary**: macOS quarantines binaries downloaded via `curl` or browser. The binary will prompt for "Allow" on first launch, or fail silently if run from a script. `xattr -d com.apple.quarantine <binary>` is the workaround until the binary is code-signed.
- **Archive structure depends on cargo-dist version**: The inner directory name inside the `.tar.gz` is `<binary-name>-<target>/`. If cargo-dist is upgraded, verify the archive structure hasn't changed before updating consumer instructions.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The existing `release.yml` and cargo-dist `[workspace.metadata.dist]` configuration MUST be confirmed correct for `aarch64-apple-darwin` — no modifications are expected given issue #124's implementation, but Research MUST verify the workflow would succeed on a macOS arm64 runner by inspecting the build matrix and confirming lbug 0.17.0's prebuilt macOS bundle is compatible.
- **FR-002**: A version tag (at minimum a pre-release tag) MUST be pushed and the release workflow MUST execute successfully end-to-end, producing at minimum `liminis-context-graph-aarch64-apple-darwin.tar.gz` and its SHA-256 companion on the GitHub Releases page.
- **FR-003**: The released macOS arm64 archive MUST contain the `liminis-context-graph` binary, and the binary MUST be a valid Mach-O arm64 executable (`file liminis-context-graph` returns `Mach-O 64-bit executable arm64`).
- **FR-004**: The released archive MUST have an accompanying SHA-256 checksum (`.sha256` file or `SHA256SUMS`) that passes `shasum -a 256 -c` verification on macOS.
- **FR-005**: README.md MUST be updated to include a consumer-facing "Bundling / Download by version" section (or equivalent heading) that documents:
  - The tarball download URL pattern: `https://github.com/verveguy/liminis-graph/releases/download/<TAG>/liminis-context-graph-aarch64-apple-darwin.tar.gz`
  - The archive directory structure: `liminis-context-graph-aarch64-apple-darwin/liminis-context-graph`
  - The Gatekeeper quarantine workaround: `xattr -d com.apple.quarantine <binary>` (until code-signing lands)
  - How to discover the latest release tag via the GitHub Releases API (`/releases/latest`)
- **FR-006**: The `Cargo.toml` `[workspace.package].version` field MUST be set to a valid semver version (currently `0.1.0`) that will be used as the release tag when the maintainer is ready to cut the first release. No change is required if it is already correct.
- **FR-007**: The release workflow MUST NOT be modified in ways that would break the Linux x86_64 or Linux ARM64 build paths introduced in #124. All three platform artifacts must still be produced on a tag push.

### Verification Requirements

- **FR-008**: Research MUST confirm that `lbug = "=0.17.0"` (prebuilt fat bundle) builds successfully on `macos-14` (the cargo-dist default macOS arm64 runner) without the `LBUG_BUILD_FROM_SOURCE` env var. The CI `build-lbug` job only runs on Ubuntu; the release's macOS build relies on the prebuilt. If the prebuilt does not include a macOS arm64 artifact, this is a blocking issue that must be surfaced before Plan.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A GitHub Release tagged `v0.1.0` (or a chosen pre-release tag) exists and contains `liminis-context-graph-aarch64-apple-darwin.tar.gz` with a SHA-256 checksum companion. The release was produced by the release workflow without manual artifact upload.
- **SC-002**: Extracting the archive on an Apple Silicon Mac and running `./liminis-context-graph --help` exits 0.
- **SC-003**: Running `shasum -a 256 -c <checksum-file>` against the downloaded archive outputs `OK` and exits 0.
- **SC-004**: The README contains a documented download URL pattern for pinned versions that a consumer can use without reading CI code.
- **SC-005**: Existing CI gates (`cargo fmt --check`, `cargo test`, `cargo clippy --release -- -D warnings`) continue to pass on the PR — no regressions.

## Assumptions

- **A1**: lbug 0.17.0's prebuilt bundle includes a macOS arm64 (`aarch64-apple-darwin`) artifact. If it does not, this blocks FR-003 and must be raised in Research (see FR-008).
- **A2**: cargo-dist's default macOS runner for `aarch64-apple-darwin` is `macos-14` (GitHub-hosted Apple Silicon). No custom runner override is needed beyond what's already in `[workspace.metadata.dist.github-custom-runners]`.
- **A3**: `[workspace.package].version = "0.1.0"` is an acceptable first release version. The maintainer decides when to push the actual `v0.1.0` tag; a pre-release tag (`v0.1.0-beta.1`) is acceptable for verification.
- **A4**: No code signing or notarization is needed for this issue; the Gatekeeper workaround (`xattr -d com.apple.quarantine`) is sufficient until a follow-up funds the Apple Developer account.
- **A5**: The `verveguy/liminis` build script update (switching from source checkout to downloading from GitHub Releases) is a companion change in the `verveguy/liminis` repo, tracked separately. This issue's scope ends at "binary is published and documented."

## Out of Scope

- Code signing / notarization of the macOS binary (follow-up; requires Apple Developer Program membership).
- Intel Mac (`x86_64-apple-darwin`) build — not requested; Apple phasing out Intel Macs; follow-up if a user requests it.
- Windows build — lbug has not been tested on Windows; separate issue.
- Pre-warming the macOS lbug build cache in CI (follow-up if release times are painful; not blocking correctness).
- Updating `verveguy/liminis`'s `scripts/build-liminis-context-graph.sh` to download from releases — that is a change in a different repo, tracked in the companion issue referenced in the issue body.
- Homebrew tap, crates.io publish, or other package-manager distribution (out of scope from #124, still out of scope here).

## Source References

- **`release.yml`** — `.github/workflows/release.yml` — cargo-dist generated workflow; already targets `aarch64-apple-darwin`
- **`build-setup.yml`** — `.github/workflows/build-setup.yml` — lbug cache restore step injected into the release build matrix
- **`Cargo.toml`** — `[workspace.metadata.dist]` — dist configuration: targets, cargo-dist version, include files
- **Issue #124 / spec** — `specs/124-prebuilt-binaries-via-cargo/spec.md` — delivered the release infrastructure this issue builds on
- **Issue #115** — added the lbug Ubuntu CI cache; release builds on Ubuntu already benefit from it
- **`verveguy/liminis#844`, `#847`** — MCP OAuth PRs that surfaced the packaging gap (referenced in issue context)
