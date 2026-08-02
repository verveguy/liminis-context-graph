//! End-to-end coverage for the #279 guards: cassette::load_records's corrupt/duplicate
//! distinction, plan::resolve's identity guards, and report::validate_recorded_cassette's
//! truncation check — wired together the way `main.rs` actually calls them (via
//! `cli::parse_args` + `plan::resolve`), not just unit-tested in isolation. SC-002's list:
//! a duplicate-keyed cassette, a corrupt cassette, two backends sharing a path, two cassette
//! backends with identical content, and a truncated capture.

use tempfile::TempDir;

use lcg_core::cassette::load_records;
use lcg_core::Error;
use lcg_eval::cli::{parse_args, CliMode};
use lcg_eval::plan::resolve;
use lcg_eval::report::{
    validate_recorded_cassette, CandidateReport, LatencyPercentiles, StructuredOutputReliability,
};

fn write_cassette(dir: &TempDir, name: &str, keys: &[&str]) -> String {
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

fn resolved_plan(argv: &[&str], chunks_len: usize) -> lcg_eval::plan::ResolvedPlan {
    let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    match parse_args(&owned).unwrap() {
        CliMode::Run(a) => resolve(&a, chunks_len),
        other => panic!("expected Run mode, got {other:?}"),
    }
}

// ── a duplicate-keyed cassette ───────────────────────────────────────────────────

#[test]
fn duplicate_keyed_cassette_is_rejected_by_load_records_and_by_the_real_run_path() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("dup.jsonl");
    std::fs::write(
        &path,
        "{\"key\": \"a\", \"call_type\": \"extract\", \"provider\": \"m\", \"model\": \"m\", \
         \"timestamp\": \"t\", \"request\": {}, \"response\": {}}\n\
         {\"key\": \"a\", \"call_type\": \"extract\", \"provider\": \"m\", \"model\": \"m\", \
         \"timestamp\": \"t\", \"request\": {}, \"response\": {}}\n",
    )
    .unwrap();

    let err = load_records(&path).unwrap_err();
    assert!(matches!(err, Error::CassetteDuplicateKey(_)));

    let plan = resolved_plan(
        &["--backend", &format!("x=cassette:path={}", path.display())],
        2,
    );
    assert_eq!(plan.guard_violations.len(), 1);
    assert!(plan.guard_violations[0].contains("duplicate"));
}

// ── a corrupt cassette ────────────────────────────────────────────────────────────

#[test]
fn corrupt_cassette_is_rejected_and_distinguished_from_duplication() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corrupt.jsonl");
    let valid = "{\"key\": \"a\", \"call_type\": \"extract\", \"provider\": \"m\", \
                 \"model\": \"m\", \"timestamp\": \"t\", \"request\": {}, \"response\": {}}";
    std::fs::write(&path, format!("{valid}\nNOT JSON\n")).unwrap();

    let err = load_records(&path).unwrap_err();
    assert!(matches!(err, Error::CassetteCorrupt(_)));
    assert!(!matches!(err, Error::CassetteDuplicateKey(_)));

    let plan = resolved_plan(
        &["--backend", &format!("x=cassette:path={}", path.display())],
        2,
    );
    assert_eq!(plan.guard_violations.len(), 1);
    assert!(plan.guard_violations[0].contains("corrupt"));
}

// ── two backends sharing a cassette path ────────────────────────────────────────

#[test]
fn two_backends_sharing_a_cassette_path_abort_before_any_run() {
    let dir = TempDir::new().unwrap();
    let path = write_cassette(&dir, "shared.jsonl", &["a", "b", "c"]);

    let plan = resolved_plan(
        &[
            "--backend",
            &format!("baseline=cassette:path={path}"),
            "--backend",
            &format!("candidate=cassette:path={path}"),
        ],
        3,
    );
    assert_eq!(
        plan.guard_violations.len(),
        1,
        "{:?}",
        plan.guard_violations
    );
    assert!(plan.guard_violations[0].contains("baseline"));
    assert!(plan.guard_violations[0].contains("candidate"));
}

// ── two cassette backends with identical content, different paths ──────────────

#[test]
fn two_cassette_backends_with_identical_content_abort_before_any_run() {
    let dir = TempDir::new().unwrap();
    let p1 = write_cassette(&dir, "one.jsonl", &["a", "b", "c"]);
    let p2 = write_cassette(&dir, "two.jsonl", &["a", "b", "c"]);

    let plan = resolved_plan(
        &[
            "--backend",
            &format!("baseline=cassette:path={p1}"),
            "--backend",
            &format!("candidate=cassette:path={p2}"),
        ],
        3,
    );
    assert_eq!(
        plan.guard_violations.len(),
        1,
        "{:?}",
        plan.guard_violations
    );
    assert!(plan.guard_violations[0].contains("byte-identical"));
}

// ── a truncated capture ──────────────────────────────────────────────────────────

#[test]
fn truncated_capture_fails_post_report_validation() {
    let dir = TempDir::new().unwrap();
    let path = write_cassette(&dir, "candidate.jsonl", &["a", "b"]); // only 2 records on disk

    let candidate = CandidateReport {
        backend_name: "candidate".to_string(),
        chunks_run: 3, // report claims 3 ran, 0 errored -> expects 3 records
        chunks_scored: 3,
        errors: 0,
        error_rate: 0.0,
        latency: LatencyPercentiles {
            p50_ms: 1,
            p95_ms: 1,
            p99_ms: 1,
        },
        structured_output: StructuredOutputReliability {
            clean: 3,
            recovered: 0,
            malformed: 0,
            malformed_rate: 0.0,
            schema_invalid: 0,
            schema_invalid_rate: 0.0,
        },
        truncated: Default::default(),
        missing_summary: Default::default(),
        vocabulary_compliance: None,
        strict_entity_f1: 1.0,
        strict_edge_f1: 1.0,
        judged_entity_f1: None,
        judged_entity_precision: None,
        judged_entity_recall: None,
        judged_edge_f1: None,
        judged_edge_precision: None,
        judged_edge_recall: None,
        judged_summary_f1: None,
        judge_errors: 0,
    };

    let actual = load_records(&path).unwrap().len();
    assert_eq!(actual, 2);
    let err = validate_recorded_cassette(&candidate, actual).unwrap_err();
    assert!(err.contains("truncated"));
    assert!(err.contains("candidate"));
}

#[test]
fn a_legitimately_errored_capture_is_not_mistaken_for_truncation() {
    let dir = TempDir::new().unwrap();
    // 2 successful extractions recorded; the report says 3 ran with 1 error, so the
    // expected record count (3 - 1 = 2) matches exactly.
    let path = write_cassette(&dir, "candidate.jsonl", &["a", "b"]);

    let candidate = CandidateReport {
        backend_name: "candidate".to_string(),
        chunks_run: 3,
        chunks_scored: 2,
        errors: 1,
        error_rate: 1.0 / 3.0,
        latency: LatencyPercentiles {
            p50_ms: 1,
            p95_ms: 1,
            p99_ms: 1,
        },
        structured_output: StructuredOutputReliability {
            clean: 2,
            recovered: 0,
            malformed: 1,
            malformed_rate: 1.0 / 3.0,
            schema_invalid: 0,
            schema_invalid_rate: 0.0,
        },
        truncated: Default::default(),
        missing_summary: Default::default(),
        vocabulary_compliance: None,
        strict_entity_f1: 1.0,
        strict_edge_f1: 1.0,
        judged_entity_f1: None,
        judged_entity_precision: None,
        judged_entity_recall: None,
        judged_edge_f1: None,
        judged_edge_precision: None,
        judged_edge_recall: None,
        judged_summary_f1: None,
        judge_errors: 0,
    };

    let actual = load_records(&path).unwrap().len();
    assert!(validate_recorded_cassette(&candidate, actual).is_ok());
}
