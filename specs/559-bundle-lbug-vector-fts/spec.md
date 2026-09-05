# Feature Specification: Bundle lbug vector/fts extensions with the release so startup never downloads from the CDN

**Feature Branch**: `fabrik/issue-559`
**Created**: 2026-09-05
**Status**: Specified
**Input**: User description: "Bundle lbug vector/fts extensions with the release so startup never downloads from the CDN"

## Background

Every `Db::open` (`crates/core/src/db.rs`) runs `INSTALL vector` / `INSTALL fts`, and on a machine that has not previously run lcg, those statements download the extension binaries from `https://extension.ladybugdb.com/` at runtime. That imposes a third-party CDN dependency on our users that we do not control and did not choose:

- An air-gapped or egress-restricted deployment cannot start at all.
- A CDN outage or a stale cache becomes an lcg outage — not hypothetical: LadybugDB/ladybug#903 was a Cloudflare caching layer serving stale/mismatched extension artifacts.
- The download happens inside `Db::open` while holding the process-global `OPEN_LOCK` (`crates/core/src/db.rs`), with no timeout, so a slow or stalled fetch serializes and blocks every other concurrent open in the process.

Offline failure today looks like:

```
IO exception: Failed to download extension: vector at URL
https://extension.ladybugdb.com/v0.18.0/linux_amd64/vector/libvector.lbug_extension
(ERROR: Could not establish connection)
```

**Correction to the original report**: the issue as filed additionally cited `ci.yml` as already documenting this CDN-download stall as "the mechanism behind #555." That connection does not hold up — #555 is a distinct, already-diagnosed lbug 0.20.1 threading deadlock in the cached-prepared-statement path (upstream LadybugDB/ladybug#883), unrelated to extension downloads, and `ci.yml` does not mention CDN downloads or `OPEN_LOCK` anywhere. The CDN-dependency risk described above stands on its own merits (verified directly against the vendored lbug C++ source and reproduced under `--network none`) and does not need #555 as supporting evidence. This spec drops that citation; it does not change the scope of the work.

**Correction to the version constraint**: the original report's constraints section was written against `lbug = "=0.18.1"` (extension directory `v0.18.0`). The workspace pin has since moved — `Cargo.toml` currently pins `lbug = "=0.20.1"` and `.cargo/config.toml` currently pins `LBUG_VERSION = "0.20.1"` to match. The general constraint still holds (the extension directory is keyed to the lbug *minor* version, not the crate's patch version, and moves whenever the pin moves) — only the concrete example values are stale. The Research stage must re-verify the exact `LBUG_EXTENSION_VERSION` directory name against the *current* pin, not assume it is still `v0.18.0` or extrapolate to `v0.20.0` without checking.

### Mechanism (from the original report, verified against the vendored lbug C++ source)

`INSTALL` short-circuits when the file is already present — `src/extension/extension_installer.cpp`:

```cpp
if (vfs->fileOrPathExists(localLibFilePath) && !info.forceInstall) {
    // The extension has been installed, skip downloading from the repo.
    return false;
}
```

and the lookup location is fully determined by `ClientContext::getExtensionDir()`:

```cpp
extensionDir = "{homeDirectory}/.lbdb/extension/{LBUG_EXTENSION_VERSION}/{platform}/"
```

`homeDirectory` defaults to the user's home directory but is settable at runtime with `CALL home_directory='<dir>'`.

So pre-placing the two extension files under a directory this project controls, and setting `home_directory` before the existing `INSTALL` statements run, is sufficient. No change is required to the existing `INSTALL` / `LOAD EXTENSION` statements themselves (`crates/core/src/db.rs`) — they simply stop reaching the network once the files are already present at the resolved location.

### Evidence (from the original report)

Verified in a `linux/amd64` container with `--network none`:

| configuration | result |
|---|---|
| no staged files (control) | fails: `Failed to download extension: vector … Could not establish connection` |
| files staged, `HOME` pointed at them | 529 passed / 2 failed (lib), 50 binaries / 1186 passed (integration) — the 2 failures are pre-existing container artifacts, unrelated |
| files staged elsewhere, `$HOME` empty, redirected via `CALL home_directory` | 2 passed; 0 failed |

The third row is the shippable shape: it does not depend on the user's home directory.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Fresh install on an egress-restricted host starts without network access (Priority: P1)

An operator deploys the `liminis-context-graph` release binary (from the official release archive) onto a host with no outbound access to `extension.ladybugdb.com` — an air-gapped environment, a locked-down container, or a CI runner with network egress disabled. They start the service for the first time on that host (no prior `.lbdb` cache exists). The service opens its database and becomes ready without attempting any network call.

**Why this priority**: This is the core problem statement — today this scenario fails outright with a fatal error, and it's the scenario the issue exists to fix.

**Independent Test**: Extract the official release archive for a target platform into an empty directory on a host with no route to `extension.ladybugdb.com` (or under `--network none` in a container), run the binary, and confirm `Db::open` succeeds and the vector/fts extensions load, with no outbound connection attempted.

**Acceptance Scenarios**:

1. **Given** a release archive extracted intact (binary + bundled `.lbdb/extension/...` tree at the layout produced by this feature) on a host with no network access to the extension CDN, **When** the service starts and calls `Db::open` for the first time on that host, **Then** `Db::open` succeeds, `INSTALL vector` / `INSTALL fts` resolve from the bundled files, and no network call to `extension.ladybugdb.com` is attempted.
2. **Given** the same setup, **When** an operator inspects the resolved extension directory (e.g. via a log line or diagnostic), **Then** it points at the bundled path derived from the running executable's location, not the user's home directory.

---

### User Story 2 - Operator overrides the extension location for a non-standard layout (Priority: P2)

An operator stages the extension files at a location of their own choosing (e.g., a shared read-only mount, a custom packaging layout that doesn't preserve the archive's directory structure) and needs lcg to use that location instead of the one derived from the binary's own path.

**Why this priority**: Named explicitly in the original report as a supported use case ("operators who stage their own"), and is the natural escape hatch for the layouts item 3 doesn't anticipate. Independent of Story 1's default-path behavior.

**Independent Test**: Set the override environment variable to a directory containing a validly-laid-out extension tree, in an environment with no other bundled files present, and confirm `Db::open` resolves extensions from that directory.

**Acceptance Scenarios**:

1. **Given** the `LCG_LBUG_HOME` environment variable is set to a directory containing a valid `.lbdb/extension/<version>/<platform>/{vector,fts}/...` tree, **When** the service calls `Db::open`, **Then** it resolves extensions from that directory, taking precedence over the binary-derived path.
2. **Given** `LCG_LBUG_HOME` is unset, **When** the service calls `Db::open`, **Then** resolution falls through to the binary-derived path (Story 1), and then to legacy behavior (Story 3) if that also yields nothing usable.

---

### User Story 3 - Existing deployments see no regression when the bundle is absent (Priority: P1)

An operator running lcg today (from source, from a dev build, or from a release predating this feature) continues to work exactly as before: `Db::open` falls back to the current behavior (user home directory, download-on-demand from the CDN) when no bundled or overridden extension directory is found.

**Why this priority**: Explicitly required by the original report ("fall back to current behaviour ... so nothing regresses when the bundle is absent"). This is a correctness/non-regression requirement, not a nice-to-have — it is what makes this change safe to ship without a breaking-change migration.

**Independent Test**: Run the binary with neither `LCG_LBUG_HOME` set nor a bundled `.lbdb` tree next to the executable, on a host with network access, and confirm `Db::open` behaves exactly as it does today (downloads on first use, succeeds).

**Acceptance Scenarios**:

1. **Given** no override env var is set and no bundled extension directory exists relative to the running executable, **When** `Db::open` runs on a host with network access, **Then** behavior is unchanged from today: lbug installs extensions via its default download path and `Db::open` succeeds.

---

### Edge Cases

- **Partial bundle** (one of the two extension files is missing from an otherwise-resolved bundled/override directory, e.g. a corrupted archive extraction or a hand-staged directory that only copied one extension): resolution is directory-level, not file-level. If a candidate directory is chosen (env override present and pointing at an existing directory, or a binary-derived directory that exists), that directory is used for *both* `INSTALL vector` and `INSTALL fts`; a missing file inside a chosen directory must surface as a clear, actionable error from the `INSTALL` statement itself (e.g., naming the resolved path and the missing file) rather than silently falling through to the next precedence tier and reaching the network unexpectedively in what the operator believes is an offline deployment.
- **Extension version drift**: if the running binary's `LBUG_EXTENSION_VERSION` (tied to the pinned lbug crate version) doesn't match the version segment baked into a bundled or overridden directory's path, the versioned subdirectory simply won't exist at the expected path — resolution naturally falls through to the next precedence tier (see FR-001) exactly as if no bundle were present. No special-case detection is required beyond normal path resolution.
- **Path containing a quote character**: `home_directory` is interpolated into a Cypher string literal in the `CALL home_directory='<path>'` statement. A resolved path containing a single quote (env override supplied by an operator, or an unusual `current_exe()` install location) must not be able to break out of the string literal or otherwise mis-execute. See FR-006.
- **Windows**: not a currently-configured cargo-dist release target (`workspace.metadata.dist.targets` in `Cargo.toml` lists only `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`). Windows-specific extension file naming/layout is out of scope for this issue and only becomes relevant if a Windows target is added to the release matrix in the future.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: `Db::open` MUST resolve an "extension home" directory before issuing `INSTALL vector` / `INSTALL fts`, in this precedence order, using the first candidate that resolves to an existing, usable directory:
  1. An explicit environment variable override (`LCG_LBUG_HOME`).
  2. A directory derived from `std::env::current_exe()`, matching the layout the release archive places bundled files under (FR-004).
  3. No override — fall back to current behavior (lbug's own default: user home directory, download on demand).
- **FR-002**: When a resolved extension home directory is found (precedence tiers 1 or 2), `Db::open` MUST issue `CALL home_directory='<resolved path>'` on the setup connection, before the existing `INSTALL vector` / `INSTALL fts` statements, so those statements resolve against the bundled files instead of the network.
- **FR-003**: When no extension home directory resolves (precedence tier 3), `Db::open`'s behavior MUST be unchanged from today — no new statement is issued, and lbug's existing default install/download behavior applies.
- **FR-004**: The release build process MUST include `libvector.lbug_extension` and `libfts.lbug_extension`, per released target platform, in the release archive, laid out as `.lbdb/extension/<LBUG_EXTENSION_VERSION>/<platform>/{vector,fts}/lib{vector,fts}.lbug_extension` (matching the layout lbug itself expects under a home directory), at a location `current_exe()`-derived resolution (FR-001 tier 2) can find.
- **FR-005**: The bundled extension files MUST be the correct build for each released target platform — the files differ per target triple, so this is a per-target packaging step, not a single shared asset copied into every archive.
- **FR-006**: The path interpolated into the `CALL home_directory='<path>'` statement MUST be escaped/quoted such that a path containing a single-quote character cannot break out of the Cypher string literal or alter the statement's meaning.
- **FR-007**: The codebase MUST include a test that fails (rather than silently passing or falling back to network behavior) when the lbug crate's pinned version changes without the corresponding `LBUG_EXTENSION_VERSION` directory mapping being updated, so a future version bump cannot silently reintroduce the CDN dependency.
- **FR-008**: The README MUST document the offline/air-gapped startup story: what is bundled, how `LCG_LBUG_HOME` can be used to stage extensions at a custom location, and what happens if neither is available (fallback to download).
- **FR-009**: Release notes for the release that first ships this feature MUST call out the bundled-extensions change and the offline startup capability it enables.
- **FR-010**: CI's test/build workflow MUST NOT depend on reaching `extension.ladybugdb.com` at runtime — it must use one of FR-001's resolved-directory mechanisms (an override or a binary-derived bundle) so CI runs are insulated from CDN availability. This replaces (does not merely supplement) `ci.yml`'s current mitigation, which is a build-artifact cache keyed on the lbug version, not a CDN-avoidance mechanism.

### Key Entities

- **Extension home directory**: A filesystem path, resolved once per `Db::open` call, whose `.lbdb/extension/<version>/<platform>/` subtree lbug consults for pre-installed extension binaries before attempting a network fetch.
- **Bundled extension files**: The two per-platform `.lbug_extension` binaries (`vector`, `fts`) shipped inside each release archive, replacing a runtime download of the same artifacts.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A release binary extracted from its official archive and started on a host with zero network route to `extension.ladybugdb.com` reaches a ready state (successful `Db::open`, vector and fts extensions loaded) on first run, with no prior cache and no manual staging.
- **SC-002**: An operator can redirect extension resolution to an arbitrary directory via a single environment variable, without modifying the binary's own install location.
- **SC-003**: A pre-existing deployment (no bundle present, no override set) exhibits identical `Db::open` behavior before and after this change, on a host with network access.
- **SC-004**: CI's required checks (`test`, `build-release` in `ci.yml`) complete without any step making a network request to `extension.ladybugdb.com`.
- **SC-005**: Bumping the pinned lbug crate version without updating the corresponding extension-directory mapping causes an existing test to fail, rather than a silent fallback to the CDN.

## Assumptions

- `LCG_LBUG_HOME` is the name adopted for the override environment variable named as an example (`e.g. LCG_LBUG_HOME`) in the original report; it is treated here as decided, not open, since no alternative was proposed and it follows this project's existing `LCG_`-prefixed environment-variable convention (e.g. `LBUG_VERSION`, `LBUG_LIBRARY_DIR`, `LBUG_INCLUDE_DIR` for the related build-time overrides, though those are unprefixed since they configure `lbug`'s own `build.rs` rather than this project's runtime).
- The exact current value of `LBUG_EXTENSION_VERSION` for the pinned lbug version (`=0.20.1` as of this writing) is not asserted by this spec and must be established empirically in Research, the same way the original report established it for `0.18.1` (by inspecting the vendored source / probing the CDN's actual directory layout) — see the "Correction to the version constraint" note in Background.
- The mechanism (`CALL home_directory=...` before `INSTALL`) is assumed correct as verified in the original report's evidence table; Research should re-confirm it still holds at the current `0.20.1` pin (mechanisms verified against `0.18.x`/vendored source could in principle have shifted across minor versions), but there is no specific reason from current information to expect it has.
- No new supported target platform is introduced by this work — bundling applies to the three targets already in `workspace.metadata.dist.targets` (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`). Windows is explicitly out of scope until a Windows target exists in that list.
- "CI's critical path" (FR-010) refers to `ci.yml`'s `test` and `build-release` jobs, the ones that call `Db::open` (directly or via the built binary) and would otherwise reach the CDN; it does not require every CI job in the repo to be network-isolated.

## Out of Scope

- The lbug 0.20.x-vs-later upgrade path (tracked separately in #555, which is an unrelated threading deadlock, not a CDN-dependency issue — see the Background correction above). This work applies at whatever lbug version is currently pinned and should not wait on or be blocked by that issue.
- Windows packaging/layout, since no Windows release target currently exists for this project.
- Changing the existing `INSTALL vector` / `INSTALL fts` / `LOAD EXTENSION` statements themselves in `crates/core/src/db.rs` — the mechanism relies on those statements being unchanged and simply resolving locally instead of over the network.
- Vendoring or mirroring the extension CDN itself, or negotiating anything with the upstream LadybugDB project — this is purely about this project's own release packaging and startup resolution logic.

## Source References

- `crates/core/src/db.rs` — `Db::open`, `OPEN_LOCK`, existing `INSTALL`/`LOAD EXTENSION` statements.
- `Cargo.toml` — `workspace.metadata.dist` (release targets, includes), `lbug` version pin.
- `.cargo/config.toml` — `LBUG_VERSION` pin (must track the `lbug` crate pin — see the project's `LBUG_VERSION` comment and its precedent in `CLAUDE.md`).
- `.github/workflows/ci.yml` — current lbug build-artifact cache (distinct from, and not a substitute for, this feature's CDN avoidance).
- Issue #555 — unrelated lbug 0.20.1 threading deadlock; incorrectly cited by the original report as evidence for this issue's motivation (see Background correction).
- Issue #546 / LadybugDB/ladybug#903 — the Cloudflare stale-cache incident cited as motivating evidence.
- LadybugDB/ladybug#903 — upstream extension CDN caching-layer incident.
