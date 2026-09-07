//! Resolves the absolute paths to pre-staged `vector`/`fts` extension files so `Db::open` can
//! `LOAD EXTENSION '<path>'` directly, bypassing `INSTALL` (and its network download) entirely
//! (issue #559, ADR-0559).
//!
//! An earlier version of this mechanism pointed lbug at a directory via
//! `CALL home_directory='<dir>'` and let the existing `INSTALL`/`LOAD EXTENSION vector` (bare
//! name) statements resolve locally. That was abandoned after issue #559's Validate stage found
//! it caused silent row loss in an unrelated `RELATES_TO` dump/rebuild round trip whenever
//! `home_directory` was redirected to any value other than the process's real home directory —
//! reproduced deterministically, isolated to the redirect itself (not the extension bytes, not
//! the avoided download, not the filesystem the redirect target lived on). Loading the extension
//! by absolute path sidesteps `home_directory` entirely: `ExtensionManager::loadExtension`
//! (vendored lbug C++) only special-cases a name that matches its `OFFICIAL_EXTENSION` table by
//! exact string equality (`"VECTOR"`, `"FTS"`, ...) — a filesystem path never matches, so it
//! `dlopen`s the given path directly and never touches `home_directory` or the CDN.
//!
//! **The versioned extension directory this module resolves must only ever be produced by
//! `scripts/stage-lbug-extensions.sh` — never hand-copied or hand-renamed.** [`check_candidate`]
//! below can only confirm a directory named `<LBUG_EXTENSION_VERSION>/<platform>` exists and
//! contains non-empty files; it has no way to verify those bytes actually correspond to the
//! version the directory name claims (issue #561 Background: staging correct-but-differently-
//! versioned bytes under a directory renamed to match a wrong `LBUG_EXTENSION_VERSION` would
//! resolve and load "successfully" while silently defeating this module's whole purpose, and no
//! test can catch it after the fact — the only ground truth is the CDN, which a unit test can't
//! assume network access to). The safety property here is structural, not test-driven:
//! `stage-lbug-extensions.sh` derives both the download URL and the destination directory name
//! from the same `$version` shell variable, read once from this repo's `LBUG_EXTENSION_VERSION`
//! file, so a script-driven mismatch between a directory's name and its contents is impossible by
//! construction. The dangerous scenario requires a human bypassing the script entirely.

use std::path::{Path, PathBuf};

use crate::error::Error;

/// Single source of truth for the lbug extension-directory version segment. Read both here
/// (`include_str!`) and by `scripts/stage-lbug-extensions.sh` / CI (`cat`), so the value can
/// never drift between what Rust resolves and what release/CI packaging stages.
///
/// **Not derivable from `lbug::VERSION` at the crate-semver level.** Research for issue #559
/// found the mapping isn't always exact-match — a `0.19.1` crate pin resolved to extension
/// directory `0.19.0`, and `0.20.1`/`0.20.2` both resolved to `0.20.0` — so this must be
/// re-verified empirically (probe a real `INSTALL vector` against the pinned version, or check
/// `https://extension.ladybugdb.com/v<N>/...`) every time the `lbug` workspace dependency pin
/// moves, and updated by hand. See ADR-0559 and `extension_version_was_verified_against_current_lbug_pin`
/// below for the tripwire this file's staleness trips.
pub(crate) const LBUG_EXTENSION_VERSION: &str = include_str!("../../../LBUG_EXTENSION_VERSION");

/// [`LBUG_EXTENSION_VERSION`], trimmed. `include_str!` embeds the repo-root file's bytes
/// verbatim, and `str::trim` isn't `const`, so a stray trailing newline (e.g. a routine editor
/// auto-newline on that single-line file) would silently make the *content* of this constant
/// `"0.18.1\n"` while `scripts/stage-lbug-extensions.sh`'s `$(cat ...)` and `ci.yml`'s
/// `$(cat LBUG_EXTENSION_VERSION)` both trim it via command substitution to `"0.18.1"` —
/// `check_candidate` would then look for a directory literally named `0.18.1\n`, find it
/// doesn't exist, and silently fall through both tiers to `None`, reverting to lbug's
/// CDN-downloading default with no error and no test catching it. Always go through this
/// accessor rather than the raw constant when building a path.
fn extension_version() -> &'static str {
    LBUG_EXTENSION_VERSION.trim()
}

/// The `lbug` crate version [`LBUG_EXTENSION_VERSION`] was last empirically verified against —
/// deliberately a plain Rust literal, not read from the same file, and not required to equal
/// [`LBUG_EXTENSION_VERSION`]. Those two values often *do* differ (see the module doc above), so
/// asserting them equal to each other (or either of them to `lbug::VERSION`) would make the
/// FR-007 tripwire fail forever after the first pin bump onto a diverging mapping, even once a
/// human has correctly re-verified and updated `LBUG_EXTENSION_VERSION` — permanently blocking
/// every future lbug upgrade instead of catching only the "nobody looked" case. This constant
/// exists solely so the tripwire test can compare it against the *live* `lbug::VERSION`: bump the
/// `lbug` workspace pin, and this must be hand-updated to match it (independently of whatever
/// `LBUG_EXTENSION_VERSION` ends up being set to), or the test fails.
#[allow(dead_code)] // only read by the `#[cfg(test)]` tripwire below
const LBUG_CRATE_VERSION_VERIFIED_AGAINST: &str = "0.20.2";

const EXTENSION_NAMES: [&str; 2] = ["vector", "fts"];

/// Maps the running binary's OS/arch to lbug's own extension-directory platform string.
/// Empirically confirmed (issue #559 Research) against the three release targets configured
/// in `Cargo.toml`'s `workspace.metadata.dist.targets`. Any other OS/arch combination
/// (including Windows, not currently a release target) returns `None`, which makes
/// `resolve_extension_files` a no-op there — falling straight through to lbug's own default
/// behavior, same as if no bundle were found.
fn platform_string() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("osx_arm64"),
        ("linux", "x86_64") => Some("linux_amd64"),
        ("linux", "aarch64") => Some("linux_arm64"),
        _ => None,
    }
}

/// The two absolute file paths `Db::open` should `LOAD EXTENSION '<path>'` directly, bypassing
/// `home_directory`/`INSTALL` entirely.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExtensionFiles {
    pub(crate) vector: PathBuf,
    pub(crate) fts: PathBuf,
}

/// Checks whether `root/.lbdb/extension/<LBUG_EXTENSION_VERSION>/<platform>/` is a usable
/// candidate, returning the resolved file paths when it is.
///
/// lbug's own `INSTALL` does not distinguish "this location was never staged" from "this
/// location was staged incompletely" — it silently downloads whatever file is missing
/// (confirmed empirically in issue #559 Research); loading a specific absolute path sidesteps
/// that entirely, but this project's own resolution is still directory-level, for the same
/// reason: if the versioned platform directory doesn't exist at all, this tier simply doesn't
/// apply (`Ok(None)` — the caller falls through to the next precedence tier, exactly as if no
/// bundle were present here, e.g. because the lbug pin has moved since this directory was
/// staged). But once that directory exists, it's a chosen candidate, and a missing file inside
/// it is a loud, actionable error (`Err`) rather than a silent fall-through that would let an
/// operator's "offline" deployment reach the network unexpectedly.
fn check_candidate(root: &Path, platform: &str) -> Result<Option<ExtensionFiles>, Error> {
    let versioned_dir = root
        .join(".lbdb")
        .join("extension")
        .join(extension_version())
        .join(platform);
    if !versioned_dir.is_dir() {
        return Ok(None);
    }

    let file_path = |name: &str| {
        versioned_dir
            .join(name)
            .join(format!("lib{name}.lbug_extension"))
    };
    let missing: Vec<PathBuf> = EXTENSION_NAMES
        .iter()
        .map(|name| file_path(name))
        .filter(|file| !file.is_file())
        .collect();

    if missing.is_empty() {
        Ok(Some(ExtensionFiles {
            vector: file_path("vector"),
            fts: file_path("fts"),
        }))
    } else {
        let missing_list = missing
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::Config(format!(
            "partial lbug extension bundle at {}: missing {missing_list}",
            versioned_dir.display(),
        )))
    }
}

/// Core precedence walk, taking already-resolved root candidates so it's testable without
/// depending on real env vars or `std::env::current_exe()`. `resolve_extension_files` is the
/// production entry point that supplies those.
fn resolve_from(
    env_root: Option<&Path>,
    exe_root: Option<&Path>,
    platform: &str,
) -> Result<Option<ExtensionFiles>, Error> {
    for root in [env_root, exe_root].into_iter().flatten() {
        if let Some(files) = check_candidate(root, platform)? {
            return Ok(Some(files));
        }
    }
    Ok(None)
}

/// Resolves the absolute `libvector`/`libfts` `.lbug_extension` file paths `Db::open` should
/// `LOAD EXTENSION '<path>'` directly (FR-001, FR-002), in precedence order:
///
/// 1. `LCG_LBUG_HOME` env var override (Story 2) — the operator's escape hatch for a
///    non-standard layout.
/// 2. A directory derived from `std::env::current_exe()`'s parent — the layout a release
///    archive bundles files under (Story 1, FR-004): the binary sits at the archive's top
///    level, with a `.lbdb/` sibling directory.
/// 3. Neither resolves: `Ok(None)`. `Db::open` falls back to lbug's own default (`INSTALL`,
///    user home directory, download on demand) — Story 3, the required non-regression path.
pub(crate) fn resolve_extension_files() -> Result<Option<ExtensionFiles>, Error> {
    let Some(platform) = platform_string() else {
        return Ok(None);
    };

    // `var` (not `var_os`) deliberately: an operator's explicit LCG_LBUG_HOME override that
    // happens to contain non-UTF-8 bytes must fail loudly, the same way db.rs's non-UTF-8
    // resolved-path check does downstream, rather than being silently treated as absent and
    // falling through to a lower-precedence tier the operator didn't ask for.
    let env_root = match std::env::var("LCG_LBUG_HOME") {
        Ok(value) => Some(PathBuf::from(value)),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(value)) => {
            return Err(Error::Config(format!(
                "LCG_LBUG_HOME is set but is not valid UTF-8: {}",
                PathBuf::from(value).display()
            )));
        }
    };
    let exe_root = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    resolve_from(env_root.as_deref(), exe_root.as_deref(), platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_stub_bundle(root: &Path, version: &str, platform: &str) {
        let dir = root
            .join(".lbdb")
            .join("extension")
            .join(version)
            .join(platform);
        for name in EXTENSION_NAMES {
            let ext_dir = dir.join(name);
            fs::create_dir_all(&ext_dir).unwrap();
            fs::write(ext_dir.join(format!("lib{name}.lbug_extension")), b"stub").unwrap();
        }
    }

    fn versioned_dir(root: &Path, version: &str, platform: &str) -> PathBuf {
        root.join(".lbdb")
            .join("extension")
            .join(version)
            .join(platform)
    }

    #[test]
    fn env_root_wins_over_exe_root_when_both_resolve() {
        let env_dir = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        write_stub_bundle(env_dir.path(), extension_version(), "linux_amd64");
        write_stub_bundle(exe_dir.path(), extension_version(), "linux_amd64");

        let resolved =
            resolve_from(Some(env_dir.path()), Some(exe_dir.path()), "linux_amd64").unwrap();
        let expected_dir = versioned_dir(env_dir.path(), extension_version(), "linux_amd64");
        let files = resolved.expect("expected env root to resolve");
        assert_eq!(
            files.vector,
            expected_dir.join("vector/libvector.lbug_extension")
        );
        assert_eq!(files.fts, expected_dir.join("fts/libfts.lbug_extension"));
    }

    #[test]
    fn exe_root_resolves_when_env_root_absent() {
        let exe_dir = tempfile::tempdir().unwrap();
        write_stub_bundle(exe_dir.path(), extension_version(), "linux_amd64");

        let resolved = resolve_from(None, Some(exe_dir.path()), "linux_amd64").unwrap();
        let expected_dir = versioned_dir(exe_dir.path(), extension_version(), "linux_amd64");
        let files = resolved.expect("expected exe root to resolve");
        assert_eq!(
            files.vector,
            expected_dir.join("vector/libvector.lbug_extension")
        );
        assert_eq!(files.fts, expected_dir.join("fts/libfts.lbug_extension"));
    }

    #[test]
    fn neither_tier_resolves_to_none() {
        let empty = tempfile::tempdir().unwrap();
        let resolved = resolve_from(Some(empty.path()), None, "linux_amd64").unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn partial_bundle_is_a_hard_error_naming_the_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let versioned_dir = dir
            .path()
            .join(".lbdb")
            .join("extension")
            .join(extension_version())
            .join("linux_amd64");
        let vector_dir = versioned_dir.join("vector");
        fs::create_dir_all(&vector_dir).unwrap();
        fs::write(vector_dir.join("libvector.lbug_extension"), b"stub").unwrap();
        // fts deliberately left unstaged.

        let err = resolve_from(Some(dir.path()), None, "linux_amd64").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("libfts.lbug_extension"),
            "error should name the missing file: {msg}"
        );
    }

    /// Proves only that a *missing* versioned directory falls through safely to the next
    /// precedence tier — it does not and structurally cannot prove that a *present* directory's
    /// staged bytes actually correspond to the version its name claims. Distinguishing "bytes
    /// staged under a mismatched directory name" from "bytes staged under a correct directory
    /// name" is impossible from inside this module: both look identical to `check_candidate`
    /// (a directory exists, the files inside it are non-empty). That gap is closed structurally,
    /// not by a test — see this module's doc comment above and `stage-lbug-extensions.sh`, the
    /// only legitimate writer of this directory tree.
    #[test]
    fn version_drift_falls_through_cleanly_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        // A bundle exists, but staged under a different version than the pinned one — as if
        // the lbug pin moved since this directory was last populated.
        write_stub_bundle(dir.path(), "9.9.9-not-the-pinned-version", "linux_amd64");

        let resolved = resolve_from(Some(dir.path()), None, "linux_amd64").unwrap();
        assert_eq!(resolved, None);
    }

    /// FR-007/SC-005: fails loudly if the `lbug` crate pin moves without a human re-verifying
    /// `LBUG_EXTENSION_VERSION` against it, so a future version bump cannot silently reintroduce
    /// the CDN dependency this issue removes. Deliberately does *not* assert
    /// `LBUG_EXTENSION_VERSION == lbug::VERSION` — Research found that equality doesn't always
    /// hold (a `0.19.1` pin resolved to extension directory `0.19.0`), so that assertion would
    /// fail forever after the next diverging bump even once someone had correctly re-verified
    /// and updated `LBUG_EXTENSION_VERSION`, permanently blocking every subsequent lbug upgrade
    /// instead of catching only the "nobody looked" case. Comparing the crate version against
    /// its own dedicated marker constant catches exactly that case and only that case.
    #[test]
    fn extension_version_was_verified_against_current_lbug_pin() {
        assert_eq!(
            LBUG_CRATE_VERSION_VERIFIED_AGAINST,
            lbug::VERSION,
            "LBUG_CRATE_VERSION_VERIFIED_AGAINST (crates/core/src/lbug_extension_home.rs) is out \
             of sync with the pinned lbug crate version. If you just bumped the `lbug` workspace \
             dependency pin, re-run the empirical probe from issue #559's Research (INSTALL \
             vector against the new pin with an empty HOME, or check \
             https://extension.ladybugdb.com/v<N>/...) to find the new extension-directory \
             version — it is not always identical to the crate version — update the \
             LBUG_EXTENSION_VERSION file (repo root) to match what you found, and update \
             LBUG_CRATE_VERSION_VERIFIED_AGAINST to the new lbug::VERSION to record that you did. \
             See ADR-0559."
        );
    }

    /// A stray trailing newline in the repo-root `LBUG_EXTENSION_VERSION` file (e.g. a routine
    /// editor auto-newline) must not change what directory name resolution looks for — otherwise
    /// `check_candidate` would search for a directory literally named `"0.18.1\n"`, never find
    /// it, and silently fall through to the network-downloading default.
    #[test]
    fn extension_version_is_trimmed_even_with_a_trailing_newline() {
        assert_eq!(extension_version(), LBUG_EXTENSION_VERSION.trim());
        assert!(!extension_version().contains('\n'));
    }
}
