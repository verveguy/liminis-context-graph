# ADR-0593: Pin lbug extension bytes; treat embedded version strings as a diagnostic, not an oracle

**Status:** Accepted
**Date:** 2026-09-16
**Issue:** #593

## Context

[ADR-0559](0559-bundle-lbug-extensions.md) made `scripts/stage-lbug-extensions.sh` the sole
downloader of the lbug `vector`/`fts` extension binaries, staged at build time from
`https://extension.ladybugdb.com/v<LBUG_EXTENSION_VERSION>/<platform>/<name>/` so a user's
`Db::open` never reaches the CDN. That script verified only that a download succeeded and the
resulting file was non-empty — it never checked **which** bytes it got.

That is unsound because upstream publishes at a per-minor path and overwrites it on patch
releases: `LBUG_EXTENSION_VERSION` resolves every 0.20.x core to the same `v0.20.0/<platform>/`
path, and whatever was most recently built there is what the next build receives. The extension
ABI a build picks up therefore depends on *when* the build runs, not on anything pinned in this
repository.

This had observed, concrete consequences on Windows, discovered while investigating #561's
0.20.3 core retarget:

| core | CDN extensions (0.20.2-era) |
|---|---|
| 0.20.0 | crashes at `LOAD EXTENSION` |
| 0.20.2 | works |
| 0.20.3 | works |
| 0.20.4 | loads, then crashes at `CREATE_VECTOR_INDEX` / `CREATE_FTS_INDEX` |

The currently-shipping v0.15.0 release depends on this exact accident of upstream's publishing
schedule: 0.20.2-era extensions happen to be ABI-compatible with the pinned 0.20.3 core, loop
tested at 110/110 (vector) and 25/25 (fts) against an official 0.20.3 CLI. Upstream is expected
to rebuild and republish that per-minor path again when it cuts 0.21.x (tracked upstream as
LadybugDB/ladybug#971), which is exactly the silent-overwrite scenario this ADR defends against.
A rebuild at that path would turn a working bundle into a process-crashing one with **no change
on this repository's side and no build failure** — staging succeeds, Linux/macOS CI passes, and
only Windows users would find out.

### Why a version-string read can't be the whole fix

The natural-seeming alternative — read the extension binary's embedded version and assert it
against the pinned lbug core — does not work, for two independent reasons:

1. **MSVC pools identical string literals.** The embedded version table's entries do not appear
   in order or contiguously in the binary. A cluster in the official 0.20.3 CLI *appeared* to end
   at `"0.20.2"` when read positionally, even though the true 0.20.3 source table contains
   `"0.20.3"`. Reading "where a string cluster ends" produces artifacts, not answers. The only
   reliable read is: scan for every `MAJOR.MINOR.PATCH`-shaped token and take the maximum —
   `grep -a -o -E '\b[0-9]+\.[0-9]+\.[0-9]+\b' "$bin" | sort -uV | tail -1`. Verified against official
   CLI binaries for 0.20.0/0.20.2/0.20.3/0.20.4 (each reports its own version), the CDN's
   `v0.20.0/win_amd64` extensions (reports `0.20.2`), and an extension built from the 0.20.4 tag
   (reports `0.20.4`).
2. **Version equality is not ABI compatibility.** Even a correct read cannot be the compatibility
   test, because the pairing this repo already ships *fails* that test: 0.20.2-era extensions run
   clean against a 0.20.3 core. A strict equality assertion would reject the exact bytes already
   proven safe. The check has no access to the actual ABI — it can only tell a human that the
   bytes changed, not whether the change is safe.

So a version-string check can be a **staleness and mismatch alarm** — it detects that the bytes
at the CDN path differ from what was last verified — but it cannot be a **compatibility oracle**.

## Decision

Two independent checks inside `stage-lbug-extensions.sh`'s existing per-extension download loop,
with different jobs.

### 1. Pin the expected sha256 (the guarantee)

A new checked-in file, `LBUG_EXTENSION_HASHES` (repo root, alongside `LBUG_EXTENSION_VERSION`),
maps `<platform> <extension> -> <sha256>` in a flat, greppable text format — no JSON/YAML
dependency, matching the plain style `LBUG_EXTENSION_VERSION` and this script already use, and
avoiding a `jq`/`yq` requirement on every runner shape cargo-dist might select.

After each download, the script hashes the received bytes into a temp file and compares against
the pin **before** moving the file into its staged location:

- No pin entry for a `platform`/`extension` pair → fail the build. A newly added platform can
  never silently ship unpinned bytes.
- Hash mismatch → fail the build, before any file is staged. The temp file is removed; nothing
  lands in the destination tree.
- Hash match → move the temp file into place; staging proceeds exactly as before.

This alone closes the hole: an upstream overwrite at the shared CDN path can no longer silently
change what ships. Re-pinning after a legitimate upstream republish is a manual, human-verified
action (re-run the same loop-testing process #561 used) — the script never updates its own pins.

### 2. Report the embedded version on mismatch (the diagnostic)

On a hash mismatch, the script also runs the max-over-matches scan above against the received
bytes and includes the result in the failure message, explicitly labeled informational — not a
claim of ABI compatibility or incompatibility. This turns a bare "bytes changed" failure into an
actionable one: a human can tell a benign upstream patch rebuild from a suspicious change without
downloading and inspecting the binary by hand. It naturally produces no result on stripped
binaries (macOS, Linux) without any platform branching — the scan runs unconditionally, and a
stripped binary simply has no matching strings to find.

**Sequencing matters**: (1) is what makes the arrangement safe; (2) only explains why (1)
tripped. Implementing (1) alone would already close the hole; (2) alone would not.

### Initial pin values

The `win_amd64` pins are the exact bytes recorded during #561/#572's loop testing and re-verified
by re-hashing the extensions bundled inside the published v0.15.0 Windows release archive. The
other three platforms (`osx_arm64`, `linux_amd64`, `linux_arm64`) were not loop-tested the same
way — they were computed by extracting the same v0.15.0 release archives and hashing the
`.lbdb/extension/0.20.0/<platform>/{vector,fts}/lib*.lbug_extension` files already bundled there,
since that is the only concrete record of what was actually shipped (only file sizes, not
hashes, had been recorded for those three during the issue's discussion). All three platforms'
computed sizes matched the previously-recorded sizes exactly.

## Consequences

- A build that receives exactly the pinned bytes behaves identically to before (SC-001) — no
  behavior change for the common case.
- A build where the CDN serves different bytes than pinned, for any platform/extension pair,
  fails before any file is staged (SC-002), with the mismatch identified by platform and
  extension name.
- On a non-stripped platform, the failure additionally names the highest embedded version string
  found in the received bytes (SC-003) — informational only.
- A re-pin is a checked-in commit to `LBUG_EXTENSION_HASHES`; the mechanism cannot be satisfied by
  simply re-running a build against newly-served bytes (SC-004). `ci.yml`'s `build-release` job
  and `windows.yml` widen their cached-bundle keys to also hash this file, so a re-pin can't be
  served from a stale cache entry that skips the check that would exercise it.
- The pin file covers only `vector`/`fts`, the two extensions `stage-lbug-extensions.sh` bundles.
  It does not extend to the OpenSSL DLLs bundled by `scripts/stage-openssl-rpath.sh`, which
  already guards its own supply chain via `scripts/assert-openssl-linkage.sh`.
- This is build-time supply-chain pinning, not test coverage. #587 (Windows CI extension-load
  test coverage) closes a different hole and neither substitutes for the other.

## Alternatives considered

- **Assert the embedded version equals the pinned lbug core's version.** Rejected — see "Why a
  version-string read can't be the whole fix" above. The currently-shipping pairing (0.20.2-era
  extensions on a 0.20.3 core) is itself a counterexample: a strict equality assertion would
  reject bytes already proven safe by loop testing.
- **Read the version from a fixed position in `storage_version_info.h`'s embedded table.**
  Rejected — MSVC's string-literal pooling makes positional reads unreliable; only a
  max-over-all-matches scan is stable.
- **JSON/YAML pin file.** Rejected — needs `jq`/`yq`, whose presence isn't guaranteed across every
  runner shape cargo-dist's `build-local-artifacts` job might select, for no benefit over a flat
  `platform extension sha256` format this script's own `case` statement already matches in style.
- **Vendor/mirror the extension CDN, or pin per-patch upstream artifacts.** Out of scope for this
  repo — the per-patch-artifact fix belongs upstream (LadybugDB/ladybug#971); this ADR pins what
  this repo receives, not how upstream publishes.
