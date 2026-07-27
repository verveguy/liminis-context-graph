//! Pure, unit-testable CLI argument parsing for the `lcg-eval` binary.
//!
//! Hand-rolled (no `clap`), matching the existing convention in
//! `crates/service/src/cli.rs` — a handful of flags doesn't clear the bar for a new
//! dependency, and a pure `parse_args` function is just as testable.

use lcg_core::OntologyMode;

/// Default corpus subset when neither `--limit` nor `--all` is given (Plan: "Default
/// corpus subset = first 50 chunks, not all 228" — keeps a default run affordable).
pub const DEFAULT_LIMIT: usize = 50;
pub const DEFAULT_JUDGE_CACHE: &str = "judge_cache.jsonl";
/// Matches `assets/llm_pricing.json`'s Sonnet entry used elsewhere in this repo.
pub const DEFAULT_JUDGE_MODEL: &str = "claude-sonnet-4-6";
/// FR-002: when `--ontology` is given without `--ontology-mode`, the mode defaults to strict.
pub const DEFAULT_ONTOLOGY_MODE: OntologyMode = OntologyMode::Strict;

fn parse_ontology_mode(v: &str) -> Result<OntologyMode, String> {
    match v {
        "open" => Ok(OntologyMode::Open),
        "strict" => Ok(OntologyMode::Strict),
        other => Err(format!(
            "--ontology-mode must be 'open' or 'strict', got '{other}'"
        )),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackendArg {
    pub name: String,
    pub spec: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CassetteArg {
    pub backend: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub backends: Vec<BackendArg>,
    pub reference: Option<String>,
    pub limit: Option<usize>,
    pub all: bool,
    pub record_cassette: Vec<CassetteArg>,
    pub judge_cache: String,
    pub judge_model: String,
    pub output: Option<String>,
    pub corpus: Option<String>,
    pub ontology: Option<String>,
    pub ontology_mode: OntologyMode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CliMode {
    Run(Box<Args>),
    Help,
}

pub fn usage() -> &'static str {
    "lcg-eval — extraction-quality eval harness (replay + LLM-as-judge) over the public \
Wikipedia corpus

USAGE:
    lcg-eval --backend <NAME>=<SPEC> [--backend <NAME>=<SPEC> ...] [OPTIONS]

BACKEND SPEC (--backend NAME=SPEC, repeatable, at least one required):
    anthropic[:model=<MODEL>]
        Hosted Anthropic baseline. Reads ANTHROPIC_API_KEY from the environment.
    oai-http:url=<URL>[,model=<MODEL>]
        OpenAI-compatible local endpoint reached over HTTP.
    oai-uds:path=<SOCKET_PATH>[,model=<MODEL>]
        OpenAI-compatible local endpoint reached over a Unix domain socket.
    cassette:path=<PATH>
        Replay a previously recorded cassette (#232/#263) instead of making live LLM
        calls. Makes zero outbound requests; a cassette miss fails loudly with
        Error::CassetteMiss. Cannot be combined with --record-cassette for the same
        backend name.

OPTIONS:
    --reference <NAME>          Backend name to use as the scoring reference/baseline.
                                 Defaults to the first --backend given. Set this to the
                                 same spec as a candidate backend (with a different name)
                                 to run a baseline-vs-itself noise-floor check.
    --limit <N>                 Run over only the first N corpus chunks (default: 50).
    --all                       Run over the full corpus (mutually exclusive with --limit).
    --corpus <PATH>              Path to a corpus_prose.jsonl file (default: the #217
                                 fixture at crates/core/tests/fixtures/real_corpus_wal/).
    --ontology <PATH>            Load an Ontology from PATH and pass it through
                                 ExtractOptions.ontology on every extraction call, exercising
                                 the ontology-constrained (Open/Strict) prompt path instead of
                                 freeform extraction. Omit for freeform (the default, unchanged
                                 behavior).
    --ontology-mode <open|strict> Mode to apply to the loaded --ontology, overriding any `mode:`
                                 declared inside the file. Defaults to 'strict' when --ontology
                                 is given. Requires --ontology; rejected as a usage error
                                 otherwise.
    --record-cassette <NAME>=<PATH>
                                 Wrap the named backend in a cassette recorder (#232),
                                 writing to PATH, so this run also captures a cassette.
    --judge-cache <PATH>         On-disk JSONL judge-score cache (default: judge_cache.jsonl).
                                 Re-runs against the same corpus/backends make zero new
                                 judge calls once cached.
    --judge-model <MODEL>        Model used for LLM-as-judge scoring (default: claude-sonnet-4-6).
                                 Judge calls require ANTHROPIC_API_KEY; without it, judged
                                 F1 is omitted from the report and only strict-string F1 is
                                 reported.
    --output <PATH>              Also write the JSON report to PATH (always printed to stdout
                                 as human-readable text).
    -h, --help                   Print this help and exit.

Documentation: https://github.com/verveguy/liminis-context-graph — see README.md's
'Extraction-quality eval harness' section for cost implications before running.
"
}

/// Parses argv (excluding the program name at index 0).
pub fn parse_args(args: &[String]) -> Result<CliMode, String> {
    let mut backends = Vec::new();
    let mut reference = None;
    let mut limit: Option<usize> = None;
    let mut all = false;
    let mut record_cassette = Vec::new();
    let mut judge_cache = DEFAULT_JUDGE_CACHE.to_string();
    let mut judge_model = DEFAULT_JUDGE_MODEL.to_string();
    let mut output = None;
    let mut corpus = None;
    let mut ontology: Option<String> = None;
    let mut ontology_mode: Option<OntologyMode> = None;
    let mut want_help = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => want_help = true,
            "--backend" => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or("--backend requires a NAME=SPEC argument")?;
                let (name, spec) = v
                    .split_once('=')
                    .ok_or_else(|| format!("--backend argument must be NAME=SPEC, got '{v}'"))?;
                backends.push(BackendArg {
                    name: name.to_string(),
                    spec: spec.to_string(),
                });
            }
            "--reference" => {
                i += 1;
                reference = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--reference requires a backend name argument")?,
                );
            }
            "--limit" => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or("--limit requires a number argument")?;
                limit = Some(v.parse::<usize>().map_err(|_| {
                    format!("--limit value must be a non-negative integer, got '{v}'")
                })?);
            }
            "--all" => all = true,
            "--record-cassette" => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or("--record-cassette requires a NAME=PATH argument")?;
                let (backend, path) = v.split_once('=').ok_or_else(|| {
                    format!("--record-cassette argument must be NAME=PATH, got '{v}'")
                })?;
                record_cassette.push(CassetteArg {
                    backend: backend.to_string(),
                    path: path.to_string(),
                });
            }
            "--judge-cache" => {
                i += 1;
                judge_cache = args
                    .get(i)
                    .cloned()
                    .ok_or("--judge-cache requires a path argument")?;
            }
            "--judge-model" => {
                i += 1;
                judge_model = args
                    .get(i)
                    .cloned()
                    .ok_or("--judge-model requires a model name argument")?;
            }
            "--output" => {
                i += 1;
                output = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--output requires a path argument")?,
                );
            }
            "--corpus" => {
                i += 1;
                corpus = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--corpus requires a path argument")?,
                );
            }
            "--ontology" => {
                i += 1;
                ontology = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("--ontology requires a path argument")?,
                );
            }
            "--ontology-mode" => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or("--ontology-mode requires an 'open' or 'strict' argument")?;
                ontology_mode = Some(parse_ontology_mode(&v)?);
            }
            arg => {
                return Err(format!("unknown argument: '{arg}'\n\n{}", usage()));
            }
        }
        i += 1;
    }

    if want_help {
        return Ok(CliMode::Help);
    }

    if backends.is_empty() {
        return Err(format!(
            "at least one --backend NAME=SPEC is required\n\n{}",
            usage()
        ));
    }

    {
        let mut seen = std::collections::HashSet::new();
        for b in &backends {
            if !seen.insert(b.name.as_str()) {
                return Err(format!(
                    "--backend name '{}' is specified more than once; backend names must be unique",
                    b.name
                ));
            }
        }
    }

    if all && limit.is_some() {
        return Err("--limit and --all are mutually exclusive; specify only one".to_string());
    }

    if ontology.is_none() && ontology_mode.is_some() {
        return Err(
            "--ontology-mode requires --ontology; there is no ontology to apply the mode to"
                .to_string(),
        );
    }
    let ontology_mode = ontology_mode.unwrap_or(DEFAULT_ONTOLOGY_MODE);

    let reference_name = reference
        .clone()
        .unwrap_or_else(|| backends[0].name.clone());
    if !backends.iter().any(|b| b.name == reference_name) {
        return Err(format!(
            "--reference '{reference_name}' does not match any configured --backend name"
        ));
    }

    for c in &record_cassette {
        let backend = backends
            .iter()
            .find(|b| b.name == c.backend)
            .ok_or_else(|| {
                format!(
                    "--record-cassette backend '{}' does not match any configured --backend name",
                    c.backend
                )
            })?;
        // Mirrors backend::parse_backend_spec's kind-prefix extraction (kept in sync by
        // hand — see crates/eval/src/backend.rs).
        let kind = backend
            .spec
            .split_once(':')
            .map(|(k, _)| k)
            .unwrap_or(backend.spec.as_str());
        if kind == "cassette" {
            return Err(format!(
                "--record-cassette backend '{}' is a cassette: replay backend; recording a \
                 replay is meaningless",
                c.backend
            ));
        }
    }

    Ok(CliMode::Run(Box::new(Args {
        backends,
        reference: Some(reference_name),
        limit,
        all,
        record_cassette,
        judge_cache,
        judge_model,
        output,
        corpus,
        ontology,
        ontology_mode,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn requires_at_least_one_backend() {
        let err = parse_args(&args(&[])).unwrap_err();
        assert!(err.contains("at least one --backend"));
    }

    #[test]
    fn single_backend_defaults_reference_to_it() {
        match parse_args(&args(&["--backend", "baseline=anthropic"])).unwrap() {
            CliMode::Run(a) => {
                assert_eq!(a.backends.len(), 1);
                assert_eq!(a.reference, Some("baseline".to_string()));
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn two_backends_with_explicit_reference() {
        match parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--backend",
            "local=oai-http:url=http://127.0.0.1:8765/v1/chat/completions",
            "--reference",
            "baseline",
        ]))
        .unwrap()
        {
            CliMode::Run(a) => {
                assert_eq!(a.backends.len(), 2);
                assert_eq!(a.reference, Some("baseline".to_string()));
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn unknown_reference_is_rejected() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--reference",
            "bogus",
        ]))
        .unwrap_err();
        assert!(err.contains("bogus"));
    }

    #[test]
    fn malformed_backend_arg_is_rejected() {
        let err = parse_args(&args(&["--backend", "no-equals-sign"])).unwrap_err();
        assert!(err.contains("NAME=SPEC"));
    }

    #[test]
    fn duplicate_backend_name_is_rejected() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--backend",
            "baseline=oai-http:url=http://127.0.0.1:8765/v1/chat/completions",
        ]))
        .unwrap_err();
        assert!(err.contains("baseline"));
        assert!(err.contains("more than once"));
    }

    /// Regression test: a duplicate backend name where the second definition is a `cassette:`
    /// spec used to slip past FR-005's `--record-cassette` rejection, because the check only
    /// looked up the *first* backend matching the name. Now caught by the general duplicate-name
    /// rejection above, before FR-005's check even runs.
    #[test]
    fn duplicate_backend_name_with_cassette_kind_is_rejected() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--backend",
            "baseline=cassette:path=/tmp/cassette.jsonl",
            "--record-cassette",
            "baseline=/tmp/other.jsonl",
        ]))
        .unwrap_err();
        assert!(err.contains("baseline"));
        assert!(err.contains("more than once"));
    }

    #[test]
    fn limit_and_all_are_mutually_exclusive() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--limit",
            "10",
            "--all",
        ]))
        .unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn limit_parses_as_number() {
        match parse_args(&args(&["--backend", "baseline=anthropic", "--limit", "10"])).unwrap() {
            CliMode::Run(a) => assert_eq!(a.limit, Some(10)),
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn non_numeric_limit_is_rejected() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--limit",
            "abc",
        ]))
        .unwrap_err();
        assert!(err.contains("--limit"));
    }

    #[test]
    fn record_cassette_backend_must_be_configured() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--record-cassette",
            "bogus=/tmp/cassette.jsonl",
        ]))
        .unwrap_err();
        assert!(err.contains("bogus"));
    }

    #[test]
    fn record_cassette_rejects_cassette_backend() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=cassette:path=/tmp/cassette.jsonl",
            "--record-cassette",
            "baseline=/tmp/other.jsonl",
        ]))
        .unwrap_err();
        assert!(err.contains("baseline"));
        assert!(err.contains("cassette"));
    }

    #[test]
    fn record_cassette_parses_name_and_path() {
        match parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--record-cassette",
            "baseline=/tmp/cassette.jsonl",
        ]))
        .unwrap()
        {
            CliMode::Run(a) => {
                assert_eq!(a.record_cassette.len(), 1);
                assert_eq!(a.record_cassette[0].backend, "baseline");
                assert_eq!(a.record_cassette[0].path, "/tmp/cassette.jsonl");
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn defaults_judge_cache_and_model() {
        match parse_args(&args(&["--backend", "baseline=anthropic"])).unwrap() {
            CliMode::Run(a) => {
                assert_eq!(a.judge_cache, DEFAULT_JUDGE_CACHE);
                assert_eq!(a.judge_model, DEFAULT_JUDGE_MODEL);
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn judge_cache_and_model_overridable() {
        match parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--judge-cache",
            "/tmp/cache.jsonl",
            "--judge-model",
            "claude-haiku-4-5-20251001",
        ]))
        .unwrap()
        {
            CliMode::Run(a) => {
                assert_eq!(a.judge_cache, "/tmp/cache.jsonl");
                assert_eq!(a.judge_model, "claude-haiku-4-5-20251001");
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn help_flag_short_circuits() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), CliMode::Help);
        assert_eq!(parse_args(&args(&["-h"])).unwrap(), CliMode::Help);
        // Even alongside an otherwise-invalid invocation (no --backend).
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), CliMode::Help);
    }

    #[test]
    fn unknown_flag_is_rejected_with_usage() {
        let err = parse_args(&args(&["--bogus"])).unwrap_err();
        assert!(err.contains("unknown argument"));
        assert!(err.contains("USAGE:"));
    }

    #[test]
    fn usage_text_lists_key_flags() {
        let u = usage();
        for flag in [
            "--backend",
            "--reference",
            "--limit",
            "--all",
            "--record-cassette",
            "--judge-cache",
            "--judge-model",
            "--output",
            "--ontology",
            "--ontology-mode",
            "--help",
        ] {
            assert!(u.contains(flag), "usage missing {flag}");
        }
    }

    #[test]
    fn no_ontology_flag_defaults_to_none_and_strict_placeholder() {
        match parse_args(&args(&["--backend", "baseline=anthropic"])).unwrap() {
            CliMode::Run(a) => {
                assert_eq!(a.ontology, None);
                assert_eq!(a.ontology_mode, OntologyMode::Strict);
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn ontology_without_mode_defaults_to_strict() {
        match parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--ontology",
            "/tmp/ontology.yaml",
        ]))
        .unwrap()
        {
            CliMode::Run(a) => {
                assert_eq!(a.ontology, Some("/tmp/ontology.yaml".to_string()));
                assert_eq!(a.ontology_mode, OntologyMode::Strict);
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn ontology_with_explicit_open_mode() {
        match parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--ontology",
            "/tmp/ontology.yaml",
            "--ontology-mode",
            "open",
        ]))
        .unwrap()
        {
            CliMode::Run(a) => {
                assert_eq!(a.ontology_mode, OntologyMode::Open);
            }
            other => panic!("expected Run mode, got {other:?}"),
        }
    }

    #[test]
    fn ontology_mode_without_ontology_is_rejected() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--ontology-mode",
            "strict",
        ]))
        .unwrap_err();
        assert!(err.contains("--ontology-mode"));
        assert!(err.contains("--ontology"));
    }

    #[test]
    fn invalid_ontology_mode_value_is_rejected() {
        let err = parse_args(&args(&[
            "--backend",
            "baseline=anthropic",
            "--ontology",
            "/tmp/ontology.yaml",
            "--ontology-mode",
            "bogus",
        ]))
        .unwrap_err();
        assert!(err.contains("--ontology-mode"));
        assert!(err.contains("bogus"));
    }

    #[test]
    fn usage_text_documents_cassette_backend_kind() {
        assert!(usage().contains("cassette:path="));
    }
}
