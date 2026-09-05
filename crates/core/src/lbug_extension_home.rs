//! Resolves the "extension home" directory `Db::open` should point lbug at before issuing
//! `INSTALL vector` / `INSTALL fts`, so those statements resolve from pre-staged files instead
//! of downloading from `extension.ladybugdb.com` (issue #559, ADR-0559).

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
/// moves, and updated by hand. See ADR-0559 and the `extension_version_matches_lbug_crate_version`
/// test below for the tripwire this file's staleness trips.
pub(crate) const LBUG_EXTENSION_VERSION: &str = include_str!("../../../LBUG_EXTENSION_VERSION");

const EXTENSION_NAMES: [&str; 2] = ["vector", "fts"];

/// Maps the running binary's OS/arch to lbug's own extension-directory platform string.
/// Empirically confirmed (issue #559 Research) against the three release targets configured
/// in `Cargo.toml`'s `workspace.metadata.dist.targets`. Any other OS/arch combination
/// (including Windows, not currently a release target) returns `None`, which makes
/// `resolve_extension_home` a no-op there — falling straight through to lbug's own default
/// behavior, same as if no bundle were found.
fn platform_string() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("osx_arm64"),
        ("linux", "x86_64") => Some("linux_amd64"),
        ("linux", "aarch64") => Some("linux_arm64"),
        _ => None,
    }
}

/// Checks whether `root/.lbdb/extension/<LBUG_EXTENSION_VERSION>/<platform>/` is a usable
/// candidate.
///
/// lbug's own `INSTALL` does not distinguish "this location was never staged" from "this
/// location was staged incompletely" — it silently downloads whatever file is missing
/// (confirmed empirically in issue #559 Research). Resolution here is therefore
/// directory-level: if the versioned platform directory doesn't exist at all, this tier simply
/// doesn't apply (`Ok(false)` — the caller falls through to the next precedence tier, exactly
/// as if no bundle were present here, e.g. because the lbug pin has moved since this directory
/// was staged). But once that directory exists, it's a chosen candidate, and a missing file
/// inside it is a loud, actionable error (`Err`) rather than a silent fall-through that would
/// let an operator's "offline" deployment reach the network unexpectedly.
fn check_candidate(root: &Path, platform: &str) -> Result<bool, Error> {
    let versioned_dir = root
        .join(".lbdb")
        .join("extension")
        .join(LBUG_EXTENSION_VERSION)
        .join(platform);
    if !versioned_dir.is_dir() {
        return Ok(false);
    }

    let missing: Vec<PathBuf> = EXTENSION_NAMES
        .iter()
        .map(|name| {
            versioned_dir
                .join(name)
                .join(format!("lib{name}.lbug_extension"))
        })
        .filter(|file| !file.is_file())
        .collect();

    if missing.is_empty() {
        Ok(true)
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
/// depending on real env vars or `std::env::current_exe()`. `resolve_extension_home` is the
/// production entry point that supplies those.
fn resolve_from(
    env_root: Option<&Path>,
    exe_root: Option<&Path>,
    platform: &str,
) -> Result<Option<PathBuf>, Error> {
    for root in [env_root, exe_root].into_iter().flatten() {
        if check_candidate(root, platform)? {
            return Ok(Some(root.to_path_buf()));
        }
    }
    Ok(None)
}

/// Resolves the "extension home" directory `Db::open` should pass to
/// `CALL home_directory='<path>'` before issuing `INSTALL vector` / `INSTALL fts` (FR-001,
/// FR-002), in precedence order:
///
/// 1. `LCG_LBUG_HOME` env var override (Story 2) — the operator's escape hatch for a
///    non-standard layout.
/// 2. A directory derived from `std::env::current_exe()`'s parent — the layout a release
///    archive bundles files under (Story 1, FR-004): the binary sits at the archive's top
///    level, with a `.lbdb/` sibling directory.
/// 3. Neither resolves: `Ok(None)`. `Db::open` issues no `CALL home_directory=...` and lbug
///    falls back to its own default (user home directory, download on demand) — Story 3,
///    the required non-regression path.
///
/// Returns the *root* directory, not the deeper `.lbdb/extension/<version>/<platform>/` path —
/// `home_directory` expects the root and lbug appends the rest itself.
pub(crate) fn resolve_extension_home() -> Result<Option<PathBuf>, Error> {
    let Some(platform) = platform_string() else {
        return Ok(None);
    };

    let env_root = std::env::var("LCG_LBUG_HOME").ok().map(PathBuf::from);
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

    #[test]
    fn env_root_wins_over_exe_root_when_both_resolve() {
        let env_dir = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        write_stub_bundle(env_dir.path(), LBUG_EXTENSION_VERSION, "linux_amd64");
        write_stub_bundle(exe_dir.path(), LBUG_EXTENSION_VERSION, "linux_amd64");

        let resolved =
            resolve_from(Some(env_dir.path()), Some(exe_dir.path()), "linux_amd64").unwrap();
        assert_eq!(resolved, Some(env_dir.path().to_path_buf()));
    }

    #[test]
    fn exe_root_resolves_when_env_root_absent() {
        let exe_dir = tempfile::tempdir().unwrap();
        write_stub_bundle(exe_dir.path(), LBUG_EXTENSION_VERSION, "linux_amd64");

        let resolved = resolve_from(None, Some(exe_dir.path()), "linux_amd64").unwrap();
        assert_eq!(resolved, Some(exe_dir.path().to_path_buf()));
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
            .join(LBUG_EXTENSION_VERSION)
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

    #[test]
    fn version_drift_falls_through_cleanly_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        // A bundle exists, but staged under a different version than the pinned one — as if
        // the lbug pin moved since this directory was last populated.
        write_stub_bundle(dir.path(), "9.9.9-not-the-pinned-version", "linux_amd64");

        let resolved = resolve_from(Some(dir.path()), None, "linux_amd64").unwrap();
        assert_eq!(resolved, None);
    }

    /// FR-007/SC-005: fails loudly if the `lbug` crate pin moves without a matching update to
    /// the `LBUG_EXTENSION_VERSION` file, so a future version bump cannot silently reintroduce
    /// the CDN dependency this issue removes. Passes today because the current pin (`0.18.1`)
    /// happens to resolve to the identically-named extension directory — see the module-level
    /// doc comment for why that equality is not guaranteed to hold after the next bump, and why
    /// this assertion is still the best available tripwire.
    #[test]
    fn extension_version_matches_lbug_crate_version() {
        assert_eq!(
            LBUG_EXTENSION_VERSION,
            lbug::VERSION,
            "LBUG_EXTENSION_VERSION (repo root) is out of sync with the pinned lbug crate \
             version. If you just bumped the `lbug` workspace dependency pin, re-run the \
             empirical probe from issue #559's Research (INSTALL vector against the new pin \
             with an empty HOME, or check https://extension.ladybugdb.com/v<N>/...) to find the \
             new extension-directory version — it is not always identical to the crate version \
             — and update the LBUG_EXTENSION_VERSION file to match. See ADR-0559."
        );
    }
}
