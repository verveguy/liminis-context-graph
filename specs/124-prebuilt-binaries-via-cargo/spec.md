# Feature Specification: Prebuilt-Binary Release Workflow via cargo-dist

**Feature Branch**: `fabrik/issue-124`
**Created**: 2026-06-01
**Status**: Draft
**Input**: 2026-06-01 — with the OSS scaffolding now in place (#120 / PR #123 merged: LICENSE, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, CHANGELOG, README polish), the next OSS-readiness gap is shipping precompiled binaries. Today an OSS user must clone the repo, install rustup, install lbug build-deps, and run `cargo build --release` (which takes 20–30 min on cold cache for the lbug C++ build). That's hostile to "I just want to drop this into Claude Desktop and try it." A tag-push-triggered GitHub Actions release workflow producing prebuilt binaries for three platforms — published via `cargo-dist` with a `curl | sh` installer — closes that gap.

## Background

**Owner decisions confirmed before drafting:**

- **Tooling: `cargo-dist`** (Astral, de facto Rust release standard). Handles multi-platform matrix, archive packaging, SHA-256 checksums, GitHub Release publishing, installer script generation. Configured via `[workspace.metadata.dist]` in `Cargo.toml`.
- **Targets** (three, all in v1):
  - `aarch64-apple-darwin` (Apple Silicon Mac) — primary maintainer platform; dominant Mac architecture
  - `x86_64-unknown-linux-gnu` (Linux x64) — default OSS Linux target
  - `aarch64-unknown-linux-gnu` (Linux ARM64) — AWS Graviton, Raspberry Pi 5, ARM cloud
- **`x86_64-apple-darwin` (Intel Mac) deferred** — Apple winding down Intel Mac; most users now ARM. File a follow-up if a real user reports needing it.
- **Swift sidecar (`native/local-inference/`) NOT in this issue.** Separate concern: needs Xcode 26 (GHA macos-latest still at 15), needs decision on whether to bundle the ~400 MB BGE-base CoreML model in the release artifact or download it on first launch. Mac OSS users continue to build the sidecar locally with `swift build -c release`. A follow-up issue picks this up once the upstream GHA runner image catches up.
- **`curl | sh` installer: yes** — cargo-dist auto-generates one (one-liner `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/verveguy/liminis-graph/releases/download/<TAG>/liminis-context-graph-installer.sh | sh`), picks the right target binary, drops it on `$PATH`.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Mac OSS User Installs via One Command (Priority: P1)

A Mac (Apple Silicon) user who finds the project on GitHub can run a one-line `curl | sh` command and end up with the `liminis-context-graph` binary installed and on their `$PATH`, without needing Rust, Xcode, or any build tooling.

**Why this priority**: this is the headline OSS install UX. The current alternative — `git clone && cargo build --release` — takes 20–30 minutes the first time because of the lbug C++ build, and requires rustup + a C++ toolchain. Unacceptable friction for "let me try this."

**Independent Test**: On a Mac with no Rust toolchain installed, run the documented installer command. Assert that `liminis-context-graph --help` works afterward.

**Acceptance Scenarios**:

1. **Given** a Mac with no Rust toolchain, **When** the user runs the documented installer command, **Then** within 60 seconds the binary is downloaded, extracted, placed on `$PATH`, and `liminis-context-graph --help` succeeds.
2. **Given** the installed binary, **When** the user starts it (with a reachable embedder), **Then** it behaves identically to a `cargo build --release` build of the same git tag.
3. **Given** the installer script content, **When** a security-conscious user inspects it before piping to sh, **Then** the script is readable, makes no out-of-band network calls beyond fetching the documented release artifact, and exits cleanly on user cancellation.

---

### User Story 2 — Linux User Has Tarball + Installer For x64 And ARM64 (Priority: P1)

Linux users on either x86_64 or ARM64 get the same install experience.

**Why this priority**: Linux is the primary deployment target for server-side usage (AWS Graviton, Raspberry Pi 5, ARM cloud).

**Independent Test**: On a Linux x86_64 and ARM64 machine with no Rust toolchain, run the installer. Assert `liminis-context-graph --help` works.

**Acceptance Scenarios**:

1. **Given** a Linux x86_64 or ARM64 machine, **When** the user runs the installer or downloads the tarball, **Then** they get a working binary for their architecture.
2. **Given** the GitHub Release page, **When** the user browses it, **Then** they see clearly-named artifacts for each platform plus a `SHA256SUMS` file (or per-artifact `.sha256` companions).

---

### User Story 3 — Integrity Can Be Verified Independently (Priority: P1)

A security-conscious user (or a downstream packager) can verify the integrity of a downloaded artifact against a published checksum.

**Why this priority**: Supply-chain hygiene is a baseline expectation for any OSS binary distribution.

**Independent Test**: Download an artifact and its checksum from a GitHub Release; run `sha256sum -c` and verify it exits 0.

**Acceptance Scenarios**:

1. **Given** the GitHub Release, **When** the user downloads an artifact and the matching `.sha256` (or the consolidated `SHA256SUMS`), **Then** `sha256sum -c` confirms the hash.
2. **Given** the release workflow, **When** it runs, **Then** the checksum file is generated by the runner *after* the binary is built (not pre-baked) and is uploaded alongside it.

---

### User Story 4 — Tag Push Triggers The Full Release Build (Priority: P1)

The maintainer can cut a release by pushing a single git tag (e.g. `v0.1.0`). No manual workflow dispatch, no manual artifact upload.

**Why this priority**: Release automation is the whole point — manual artifact uploads are error-prone and scale poorly.

**Independent Test**: Push a tag matching `v*.*.*` and confirm a GitHub Release with all three platform artifacts appears within 45 minutes without any further operator action.

**Acceptance Scenarios**:

1. **Given** the maintainer is ready to release, **When** they push a tag matching `v*.*.*`, **Then** the release workflow fires, builds all three platforms, uploads artifacts to a new GitHub Release matching the tag, and the release becomes visible on the repo's Releases page.
2. **Given** a tag push that fails (e.g., one platform's build broke), **When** the workflow finishes, **Then** the GitHub Release is either NOT created (preferred) or is created as a draft so partial-state isn't visible to users. The maintainer can delete the tag, fix, retry.

---

### User Story 5 — README Documents The Install Path (Priority: P2)

A first-time README reader can find the install instructions without hunting.

**Why this priority**: documentation discovery is the last mile — a working installer that nobody can find is wasted.

**Independent Test**: Read README's Quickstart section and verify the `curl | sh` one-liner appears before any build-from-source instructions.

**Acceptance Scenarios**:

1. **Given** the merged release-workflow PR, **When** the README's existing Quickstart section is read, **Then** the first-listed install method is the `curl | sh` one-liner, with the `cargo build --release` path retained as the "build from source" alternative below it.
2. **Given** macOS users, **When** they run the installer or downloaded binary, **Then** the README warns them about Gatekeeper (`xattr -d com.apple.quarantine` workaround) until codesigning lands in a follow-up.

---

### User Story 6 — Cold Tagged Release Completes In Reasonable Time (Priority: P2)

Pushing a release tag should result in a published Release within ~30 minutes wall-clock the first time and faster on subsequent releases (cache hit on lbug for x86_64 Linux at minimum).

**Why this priority**: a 6-hour release workflow would block the OSS launch; 45 minutes is acceptable.

**Independent Test**: Push a tag and measure wall-clock from push to "release visible with all artifacts."

**Acceptance Scenarios**:

1. **Given** a clean release run, **When** it executes, **Then** total wall-clock from tag push to "release visible with all artifacts" is ≤ 45 minutes (P1 lbug build dominates on each platform that doesn't cache-hit).
2. **Given** a follow-up release ≤ 7 days later with no lbug-sys bump, **When** it executes, **Then** the Linux x86_64 job hits the lbug cache from CI (added by #115) and completes in ≤ 15 minutes. Mac and Linux-ARM jobs pay full cost until separate cache infra is added for them (out of scope; follow-up).

---

### Edge Cases

- **lbug fails to build on Linux ARM64.** lbug-sys may not have a precompiled ARM64 binary; the source build path is the only option. If lbug-sys's `build.rs` doesn't cross-compile cleanly, FR-013's "native ARM runner" mitigation kicks in.
- **A tag is pushed but the workspace version doesn't match.** Cargo-dist's default behaviour (fail loudly) is the right one — don't paper over.
- **Tag push fails mid-build on one platform.** Per FR-004 / cargo-dist defaults: the partial Release should not be visible to end users. cargo-dist defaults to either drafting until all platforms complete or failing the whole job; either is acceptable.
- **First release has no `[Unreleased]` content in CHANGELOG.md.** Leave the release body empty; the maintainer fills in retroactively. Or use the literal string "Initial release." Either works.
- **Maintainer pushes a non-semver tag (e.g., `v0.1.0-alpha`).** cargo-dist supports prereleases; ensure the pattern in FR-002 doesn't exclude them.
- **A tagged commit is later force-deleted (`git push --delete tag v0.1.0`).** The GitHub Release lingers unless manually deleted. Document this in the release-runbook section of CONTRIBUTING.md or README — it's an operational note, not a code change.
- **Released binary depends on an embedder sidecar at runtime.** The README's existing Embedder section already covers this — but the per-archive README copy should make the dependency explicit so a user who only reads the archive contents doesn't think `--help`-succeeds means the system is fully usable.
- **`curl | sh` installer is run with a system-default `sh` that's actually `dash` or `ash`.** cargo-dist's installer is typically POSIX-compatible. Verify on Debian (sh=dash) at least once after first release.
- **GHA outage during release.** Treat as transient; maintainer retries. Document the retry pattern (delete tag, re-tag, re-push) in the runbook.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001.** `cargo-dist` MUST be installed and configured via `[workspace.metadata.dist]` in the root `Cargo.toml` per its docs. Use the most recent stable cargo-dist release at issue-implementation time.
- **FR-002.** A `.github/workflows/release.yml` workflow MUST be generated/maintained by `cargo dist init` and committed to the repo. The workflow:
  - Triggers on tag pushes matching `v*.*.*` (or whatever cargo-dist's default trigger is — defer to cargo-dist's idioms unless they conflict with this spec).
  - Includes a `build` matrix for the three targets (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`).
  - Includes a `publish` step that creates the GitHub Release and uploads artifacts.
- **FR-003.** Each platform's archive MUST contain at minimum:
  - The `liminis-context-graph` binary (stripped of debug symbols)
  - `LICENSE`
  - `README.md`
  - `CHANGELOG.md`
  - SHA-256 checksum of the binary (either inside the archive as `.sha256` or as a sibling on the release page)
- **FR-004.** A `SHA256SUMS` file (or per-artifact `.sha256` siblings) MUST appear on the GitHub Release page so users can `sha256sum -c` verify before extracting.
- **FR-005.** A `curl | sh` installer script MUST be auto-generated by cargo-dist and uploaded alongside each release. It MUST be auditable plain shell (no compiled installer), download the correct platform binary, place it on `$PATH` (default `~/.local/bin` or wherever cargo-dist's installer puts it), and print clear next-steps. The installer URL MUST be stable across releases (e.g., `liminis-context-graph-installer.sh`).
- **FR-006.** README.md MUST be updated to:
  - Lead the Quickstart with the `curl | sh` install one-liner (using a versioned URL pattern — exact format defer to cargo-dist's recommendation).
  - Keep the existing `cargo build --release` flow as a labeled "Build from source" section below.
  - Include a brief macOS Gatekeeper note: until binaries are codesigned (separate follow-up), first-launch may require `xattr -d com.apple.quarantine ~/.local/bin/liminis-context-graph` or right-click → Open in Finder.
- **FR-007.** `CHANGELOG.md` MUST be updated under `[Unreleased]` to mention "prebuilt binaries for aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu now published as GitHub Release assets" (or the version section once the first tag is cut).
- **FR-008.** The workspace `Cargo.toml` MUST have a top-level `version` field that drives release naming. Each crate (`liminis-graph-core`, `liminis-graph`) MAY share that version via `version.workspace = true` (the cargo-idiomatic pattern), or keep crate-local versions if cargo-dist's defaults prefer that — defer to cargo-dist's recommendation.
- **FR-009.** The release workflow MUST set `LBUG_BUILD_FROM_SOURCE=1` for each build job (same env var as `ci.yml`). Without it, prebuilt lbug fails to ship its third-party static archives and links fail.
- **FR-010.** The release workflow MUST NOT run on every push or PR — only on tag push (and optionally `workflow_dispatch` for maintainer-initiated test runs). The existing PR-time CI (`ci.yml`) is the gating quality check; the release workflow is a build-and-publish job.
- **FR-011.** If cargo-dist supports it cleanly, the GitHub Release body SHOULD be auto-populated from the `[Unreleased]` (or matching version) section of `CHANGELOG.md`. If this requires meaningful configuration effort or risks misformatting, leave the release body empty and let the maintainer edit it post-publish.
- **FR-012.** Binaries MUST be stripped of debug symbols to keep archive size reasonable. cargo-dist handles this; just verify the resulting archive isn't carrying ~100 MB of debug info.
- **FR-013.** The Linux ARM64 build SHOULD use a native ARM runner (`ubuntu-latest-arm64` or whatever GHA offers; cargo-dist's defaults) rather than cross-compile from x86_64 to avoid lbug C++ cross-compile complications. If a native ARM runner is not available on the user's GHA plan, document the fallback (cross-compile via `cross`).
- **FR-014.** Pre-existing CI gates (`cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings`) MUST continue to pass on the PR that introduces the release workflow. The release workflow itself MUST NOT introduce changes to Rust source code; if cargo-dist asks for any source changes (e.g., a `dist` feature flag), they should be additive only.

## Assumptions

- **A1.** GitHub Actions runners support native ARM64 Linux (`ubuntu-latest-arm64` or similar). Verify during Research; fall back to `cross`-based cross-compile if not.
- **A2.** cargo-dist's defaults are sensible enough that this issue's Implementation stage doesn't need to customize most knobs. If significant customization is required, prefer adding it as a follow-up rather than blocking this issue.
- **A3.** The lbug C++ build succeeds on aarch64-apple-darwin (verified locally; the maintainer's machine is Apple Silicon) and on Linux ARM64 (assumed; verify in Research).
- **A4.** Release builds do not require any secret credentials beyond the default `GITHUB_TOKEN` provided to workflows. No code signing, no upload-to-third-party-CDN, no auth to crates.io.
- **A5.** The first release will be cut by the maintainer pushing a tag like `v0.1.0`. Version-number policy (when to bump major/minor/patch) is the maintainer's discretion; not codified here.
- **A6.** Existing `.github/workflows/ci.yml` and `.github/workflows/swift.yml` are unaffected by adding the release workflow. The release workflow only runs on tag pushes; CI continues to run on `push` and `pull_request`.

## Success Criteria *(mandatory)*

- **SC-001.** Pushing a tag like `v0.0.1` (or whatever the maintainer chooses as the first test version) produces a GitHub Release within 45 minutes containing three platform artifacts (aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu), each accompanied by a SHA-256 checksum.
- **SC-002.** A user on each of the three platforms can run the documented `curl | sh` installer and end up with a working `liminis-context-graph` on `$PATH`.
- **SC-003.** Downloading any artifact and running `sha256sum -c` against the published checksum verifies the bytes.
- **SC-004.** The PR introducing the release workflow passes existing CI (`cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings`) without regressions.
- **SC-005.** README's Quickstart leads with the `curl | sh` install path (per FR-006) so first-time visitors see "one command" before they see "build from source."
- **SC-006.** A second tag pushed after the first succeeds without operator intervention (no manual rerun, no manual upload). Idempotent release process.
- **SC-007.** Released binary is < 50 MB per platform (stripped). lbug + Rust runtime + transitive deps; cargo-dist strips by default; sanity-check the size after the first release.

## Out of Scope

- Code signing / notarization of macOS binaries (requires Apple Developer Program; users will see Gatekeeper warnings and must `xattr -d com.apple.quarantine` or right-click→Open the first time). Document the workaround in README; file a follow-up when there's a budget for the developer account.
- Homebrew tap, Cargo `crates.io` publish, AUR, Nix flakes — package-manager distribution is a separate audience and lift. Cargo-dist supports Homebrew formula generation; can be enabled in a follow-up after the tap repo is set up.
- Windows (`x86_64-pc-windows-msvc`) — lbug has not been tested on Windows; punt until somebody asks for it.
- `x86_64-apple-darwin` (Intel Mac) — Apple winding down Intel Mac; file a follow-up if a real user reports needing it.
- Swift sidecar release artifact — separate concern, needs Xcode 26 and model-bundling decision.
- SBOMs (Software Bill of Materials) — cargo-dist can emit them; nice-to-have; not P1.
- Auto-cutting tags from CHANGELOG state.
- Versioning policy / semver discipline document.
- Publishing to crates.io.
- Adding `cargo-dist` lbug cache for non-x86_64-linux platforms (Mac, ARM Linux) — follow-up once release times are measured.

## Source References

- **`cargo-dist`**: https://opensource.axo.dev/cargo-dist/ — the de facto Rust release-automation tool. Used by uv, ruff, biome, hyperfine, and many others. Maintained by Astral.
- **`#115` (merged)** — added the lbug Ubuntu CI cache. Release-workflow Ubuntu builds get the same speedup automatically (same cache key). Mac and Linux-ARM release builds do NOT yet have the analogous cache; that's a follow-up if release times become painful.
- **`ideas/oss-launch-architecture.md` Question 6** — flagged prebuilt binaries as a separate concern; this issue is the realization of that bullet.
- **`#120` (merged via PR #123)** — OSS scaffolding. The LICENSE / README / CHANGELOG / etc. files this issue extends are now in place.
- **Existing Cargo workspace**: two crates (`liminis-graph-core` library, `liminis-graph` binary). cargo-dist should treat `liminis-graph` (the bin) as the release artifact; `liminis-graph-core` is library code that doesn't ship as its own binary.
- **`Cargo.toml` workspace `license = "MIT"`** — confirmed compatible with bundling third-party deps (most are MIT or Apache-2.0).
- **`.github/workflows/ci.yml`** — the current CI workflow; its `LBUG_BUILD_FROM_SOURCE=1` env var must be mirrored in the release workflow (FR-009).
