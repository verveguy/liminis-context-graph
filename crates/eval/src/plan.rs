//! Resolves every `--backend` spec into a plan: replay-or-live decision, cassette record
//! count, and any guard that would abort a real run — shared verbatim by `--dry-run` (which
//! prints it and exits 0) and the real run (which aborts before any outbound call if
//! `guard_violations` is non-empty). This is the single code path User Story 2 requires so a
//! preview can never drift from what a real run would actually do — see ADR-0052.
//!
//! What's a guard violation (FR-002/FR-003/FR-004, aborts a real run) versus a note
//! (FR-005, informational only): a corrupt or duplicate-keyed cassette, or two backends
//! resolving to the same cassette content, would invalidate the whole measurement — those
//! are violations. A cassette covering fewer chunks than the requested scope only shrinks
//! the sample and is already accounted for in the report's `error_rate`-adjacent accounting,
//! so it is reported as a coverage shortfall note, never a violation.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use lcg_core::cassette::load_records;

use crate::backend::{parse_backend_spec, BackendKind};
use crate::cli::Args;

/// What a real run would do with a backend — distinct from whether it *should* (that's
/// `guard_violations`). An unparseable spec is neither `Live` nor `Replay`: it isn't a
/// backend a real run could dispatch to at all, so `render()` must not print it as either
/// (see ADR-0052 — a preview that mislabels an aborting backend as `LIVE` is exactly the
/// kind of drift this module exists to prevent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Live,
    Replay,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct BackendPlan {
    pub name: String,
    pub spec: String,
    pub decision: Decision,
    /// `Some` only for a `cassette:` backend that loaded successfully.
    pub cassette_record_count: Option<usize>,
    /// `Some(n)` when a cassette backend's record count is `n` short of the requested
    /// scope (FR-005). `None` when the count meets or exceeds the scope: neither an exact
    /// match nor a superset is a shortfall.
    pub coverage_shortfall: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    pub backends: Vec<BackendPlan>,
    pub requested_scope: usize,
    pub scope_label: String,
    /// Non-empty means a real run would abort before making any outbound call
    /// (FR-002/FR-003/FR-004). `--dry-run` reports these without aborting; the real run
    /// treats a non-empty list as fatal.
    pub guard_violations: Vec<String>,
}

/// SHA-256 hex digest of `path`'s raw bytes — deliberately over the file's bytes, not the
/// parsed records, so two cassettes are "identical" exactly when a naive `cp` would have
/// made them so (matching what the #278 shell guards this supersedes checked).
fn hash_file(path: &str) -> Result<String, std::io::Error> {
    let bytes = std::fs::read(path)?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Resolves `cli`'s backends against `chunks_len` (the already-selected corpus subset —
/// the requested scope). Never fails: guard violations are entries in `guard_violations`,
/// not an early return, so `--dry-run` and the real run can share this one function and
/// differ only in what they do with the result.
pub fn resolve(cli: &Args, chunks_len: usize) -> ResolvedPlan {
    let scope_label = if cli.all {
        format!("full corpus ({chunks_len} chunks)")
    } else {
        format!("--limit {chunks_len}")
    };

    let mut backends = Vec::with_capacity(cli.backends.len());
    let mut guard_violations = Vec::new();
    // FR-004, same path: cassette path -> the first backend name that used it.
    let mut seen_paths: HashMap<String, String> = HashMap::new();
    // FR-004, same content: (hash, backend name, path) for every cassette that loaded.
    let mut seen_hashes: Vec<(String, String, String)> = Vec::new();

    for b in &cli.backends {
        let kind = match parse_backend_spec(&b.spec) {
            Ok(k) => k,
            Err(e) => {
                guard_violations.push(format!("backend '{}': {e}", b.name));
                backends.push(BackendPlan {
                    name: b.name.clone(),
                    spec: b.spec.clone(),
                    decision: Decision::Invalid,
                    cassette_record_count: None,
                    coverage_shortfall: None,
                });
                continue;
            }
        };

        let path = match kind {
            BackendKind::Cassette { path } => path,
            _ => {
                backends.push(BackendPlan {
                    name: b.name.clone(),
                    spec: b.spec.clone(),
                    decision: Decision::Live,
                    cassette_record_count: None,
                    coverage_shortfall: None,
                });
                continue;
            }
        };

        // Same path already flags this pair below via `seen_paths` — skip the content-hash
        // comparison in that case so the pair is reported once, not twice.
        let same_path_already_flagged = seen_paths.contains_key(&path);
        if let Some(prior_name) = seen_paths.get(&path) {
            guard_violations.push(format!(
                "backend '{}' and backend '{prior_name}' both resolve to cassette path '{path}' \
                 — a self-comparison is degenerate",
                b.name
            ));
        } else {
            seen_paths.insert(path.clone(), b.name.clone());
        }

        // Re-reads the same file for every backend alias pointing at `path`, including one
        // already flagged via `seen_paths` above — deliberate, not an oversight: each backend
        // still needs its own `BackendPlan` entry (record count, coverage shortfall) for the
        // `--dry-run` plan, and cassette files are local and small relative to the double-read
        // ADR-0052 already accepts between `resolve` and the real run.
        match load_records(&path) {
            Ok(records) => {
                if !same_path_already_flagged {
                    match hash_file(&path) {
                        Ok(hash) => {
                            if let Some((_, prior_name, prior_path)) =
                                seen_hashes.iter().find(|(h, _, _)| *h == hash)
                            {
                                guard_violations.push(format!(
                                    "backend '{}' (cassette '{path}') and backend \
                                     '{prior_name}' (cassette '{prior_path}') have \
                                     byte-identical cassette content — a self-comparison is \
                                     degenerate",
                                    b.name
                                ));
                            } else {
                                seen_hashes.push((hash, b.name.clone(), path.clone()));
                            }
                        }
                        // A guard that can no-op without saying so is worth avoiding: this
                        // read races nothing in practice (load_records just read the same
                        // file), but if it ever fails, the FR-004 identity check must be
                        // reported as unable to run, not silently skipped.
                        Err(e) => guard_violations.push(format!(
                            "backend '{}': cannot hash cassette '{path}' for the identity \
                             check: {e}",
                            b.name
                        )),
                    }
                }

                let count = records.len();
                let shortfall = chunks_len.saturating_sub(count);
                backends.push(BackendPlan {
                    name: b.name.clone(),
                    spec: b.spec.clone(),
                    decision: Decision::Replay,
                    cassette_record_count: Some(count),
                    coverage_shortfall: (shortfall > 0).then_some(shortfall),
                });
            }
            Err(e) => {
                guard_violations.push(format!("backend '{}': {e}", b.name));
                backends.push(BackendPlan {
                    name: b.name.clone(),
                    spec: b.spec.clone(),
                    decision: Decision::Replay,
                    cassette_record_count: None,
                    coverage_shortfall: None,
                });
            }
        }
    }

    ResolvedPlan {
        backends,
        requested_scope: chunks_len,
        scope_label,
        guard_violations,
    }
}

/// The `--dry-run` human-readable plan (FR-001/User Story 2).
pub fn render(plan: &ResolvedPlan) -> String {
    let mut out = format!(
        "lcg-eval dry-run plan — requested scope: {}\n\n",
        plan.scope_label
    );
    for b in &plan.backends {
        out.push_str(&format!(
            "  backend '{}': spec='{}' decision={}",
            b.name,
            b.spec,
            match b.decision {
                Decision::Replay => "REPLAY",
                Decision::Live => "LIVE",
                Decision::Invalid => "INVALID",
            }
        ));
        if let Some(n) = b.cassette_record_count {
            out.push_str(&format!(" records={n}"));
        }
        out.push('\n');
        if let Some(shortfall) = b.coverage_shortfall {
            out.push_str(&format!(
                "      NOTE: cassette covers {shortfall} fewer chunk(s) than the requested \
                 scope\n"
            ));
        }
    }
    out.push('\n');
    if plan.guard_violations.is_empty() {
        out.push_str("  no guard would abort a real run\n");
    } else {
        out.push_str("  WOULD ABORT a real run:\n");
        for v in &plan.guard_violations {
            out.push_str(&format!("    - {v}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{parse_args, CliMode};

    fn resolve_args(argv: &[&str], chunks_len: usize) -> ResolvedPlan {
        let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        match parse_args(&owned).unwrap() {
            CliMode::Run(a) => resolve(&a, chunks_len),
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    fn write_cassette(dir: &tempfile::TempDir, name: &str, keys: &[&str]) -> String {
        let path = dir.path().join(name);
        let mut content = String::new();
        for k in keys {
            content.push_str(&format!(
                "{{\"key\": \"{k}\", \"call_type\": \"extract\", \"provider\": \"mock\", \
                 \"model\": \"m\", \"timestamp\": \"2026-01-01T00:00:00Z\", \"request\": {{}}, \
                 \"response\": {{}}}}\n"
            ));
        }
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn two_backends_same_cassette_path_is_a_guard_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write_cassette(&dir, "shared.jsonl", &["a", "b"]);
        let plan = resolve_args(
            &[
                "--backend",
                &format!("x=cassette:path={p}"),
                "--backend",
                &format!("y=cassette:path={p}"),
            ],
            2,
        );
        assert_eq!(plan.guard_violations.len(), 1);
        assert!(plan.guard_violations[0].contains('x'));
        assert!(plan.guard_violations[0].contains('y'));
    }

    #[test]
    fn two_distinct_paths_identical_content_is_a_guard_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let p1 = write_cassette(&dir, "one.jsonl", &["a", "b"]);
        let p2 = write_cassette(&dir, "two.jsonl", &["a", "b"]);
        let plan = resolve_args(
            &[
                "--backend",
                &format!("x=cassette:path={p1}"),
                "--backend",
                &format!("y=cassette:path={p2}"),
            ],
            2,
        );
        assert_eq!(
            plan.guard_violations.len(),
            1,
            "{:?}",
            plan.guard_violations
        );
        assert!(plan.guard_violations[0].contains("byte-identical"));
    }

    #[test]
    fn distinct_cassette_content_is_accepted() {
        let dir = tempfile::TempDir::new().unwrap();
        let p1 = write_cassette(&dir, "one.jsonl", &["a", "b"]);
        let p2 = write_cassette(&dir, "two.jsonl", &["c", "d"]);
        let plan = resolve_args(
            &[
                "--backend",
                &format!("x=cassette:path={p1}"),
                "--backend",
                &format!("y=cassette:path={p2}"),
            ],
            2,
        );
        assert!(
            plan.guard_violations.is_empty(),
            "{:?}",
            plan.guard_violations
        );
    }

    #[test]
    fn live_backends_sharing_a_spec_are_never_flagged() {
        // The mandatory noise-floor calibration pattern: same live spec under two names.
        let plan = resolve_args(
            &[
                "--backend",
                "baseline=anthropic:model=claude-haiku-4-5-20251001",
                "--backend",
                "candidate=anthropic:model=claude-haiku-4-5-20251001",
            ],
            10,
        );
        assert!(plan.guard_violations.is_empty());
        assert!(plan.backends.iter().all(|b| b.decision == Decision::Live));
    }

    #[test]
    fn unparseable_spec_is_invalid_not_live() {
        // handarbeit-pruefer's review finding on PR #280: an unresolvable spec used to be
        // given `replay: false`, which `render()` printed as `decision=LIVE` — asserting
        // something false about a backend that can only ever abort the real run. A real run
        // can't dispatch to it as either live or replay, so the dry-run plan must not claim
        // either.
        let plan = resolve_args(&["--backend", "x=not-a-real-kind"], 2);
        assert_eq!(plan.backends[0].decision, Decision::Invalid);
        assert_eq!(plan.guard_violations.len(), 1);
        let out = render(&plan);
        assert!(out.contains("decision=INVALID"), "{out}");
        assert!(!out.contains("decision=LIVE"), "{out}");
    }

    #[test]
    fn coverage_shortfall_reported_when_records_below_scope() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write_cassette(&dir, "c.jsonl", &["a", "b"]);
        let plan = resolve_args(&["--backend", &format!("x=cassette:path={p}")], 5);
        assert_eq!(plan.backends[0].coverage_shortfall, Some(3));
    }

    #[test]
    fn exact_match_is_not_a_shortfall() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write_cassette(&dir, "c.jsonl", &["a", "b"]);
        let plan = resolve_args(&["--backend", &format!("x=cassette:path={p}")], 2);
        assert_eq!(plan.backends[0].coverage_shortfall, None);
        assert_eq!(plan.backends[0].cassette_record_count, Some(2));
    }

    #[test]
    fn superset_is_not_a_shortfall() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write_cassette(&dir, "c.jsonl", &["a", "b", "c"]);
        let plan = resolve_args(&["--backend", &format!("x=cassette:path={p}")], 2);
        assert_eq!(plan.backends[0].coverage_shortfall, None);
    }

    #[test]
    fn duplicate_key_cassette_is_a_guard_violation_naming_duplication() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write_cassette(&dir, "dup.jsonl", &["a", "a"]);
        let plan = resolve_args(&["--backend", &format!("x=cassette:path={p}")], 2);
        assert_eq!(plan.guard_violations.len(), 1);
        assert!(plan.guard_violations[0].contains("duplicate"));
    }

    #[test]
    fn corrupt_cassette_is_a_guard_violation_naming_corruption() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("corrupt.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        let plan = resolve_args(
            &[
                "--backend",
                &format!("x=cassette:path={}", path.to_string_lossy()),
            ],
            2,
        );
        assert_eq!(plan.guard_violations.len(), 1);
        assert!(plan.guard_violations[0].contains("corrupt"));
    }

    #[test]
    fn scope_label_reflects_all_flag() {
        let plan = resolve_args(&["--backend", "x=anthropic", "--all"], 228);
        assert!(plan.scope_label.contains("full corpus"));
        let plan = resolve_args(&["--backend", "x=anthropic", "--limit", "10"], 10);
        assert!(plan.scope_label.contains("--limit 10"));
    }

    #[test]
    fn render_names_would_abort_guards() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write_cassette(&dir, "dup.jsonl", &["a", "a"]);
        let plan = resolve_args(&["--backend", &format!("x=cassette:path={p}")], 2);
        let out = render(&plan);
        assert!(out.contains("WOULD ABORT"));
        assert!(out.contains("duplicate"));
    }

    #[test]
    fn render_reports_no_violations_cleanly() {
        let plan = resolve_args(&["--backend", "x=anthropic"], 10);
        let out = render(&plan);
        assert!(out.contains("no guard would abort"));
    }
}
