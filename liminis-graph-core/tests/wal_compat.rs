use liminis_graph_core::WalLine;
use serde_json::Value;
use std::path::Path;

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wal")
        .join(name)
}

fn read_fixture_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|_| panic!("cannot read fixture {name}"))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Rust WalLine serialization has exactly the five required fields (forward-compat, R-03).
#[test]
fn test_forward_compat_five_fields_present() {
    let line = WalLine {
        seq: 1,
        ts: "2026-05-19T00:00:00.000000+00:00".to_string(),
        db: "graphiti".to_string(),
        cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
        params: serde_json::json!({"uuid": "test"}),
    };
    let json_str = serde_json::to_string(&line).expect("serialize");
    let parsed: Value = serde_json::from_str(&json_str).expect("parse back");

    let obj = parsed.as_object().expect("must be JSON object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["seq", "ts", "db", "cypher", "params"],
        "must have exactly the five WAL fields in order"
    );
}

/// The `ts` field must be a JSON string parseable as an ISO-8601 UTC datetime (R-03).
#[test]
fn test_forward_compat_ts_is_string() {
    let line = WalLine {
        seq: 1,
        ts: "2026-05-19T12:34:56.000000+00:00".to_string(),
        db: "graphiti".to_string(),
        cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
        params: serde_json::json!({}),
    };
    let json_str = serde_json::to_string(&line).unwrap();
    let parsed: Value = serde_json::from_str(&json_str).unwrap();

    let ts = parsed["ts"].as_str().expect("ts must be a JSON string");
    // Must contain a UTC marker.
    assert!(
        ts.ends_with("+00:00") || ts.ends_with('Z'),
        "ts must have UTC timezone marker, got: {ts}"
    );
    // Must be parseable as ISO-8601.
    assert!(ts.contains('T'), "ts must be in ISO-8601 format, got: {ts}");
}

/// The `params` field must be a JSON object, not an array or null (R-03).
#[test]
fn test_forward_compat_params_is_object() {
    let line = WalLine {
        seq: 1,
        ts: "2026-05-19T00:00:00.000000+00:00".to_string(),
        db: "graphiti".to_string(),
        cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
        params: serde_json::json!({"uuid": "x", "name": "Alice"}),
    };
    let json_str = serde_json::to_string(&line).unwrap();
    let parsed: Value = serde_json::from_str(&json_str).unwrap();

    assert!(
        parsed["params"].is_object(),
        "params must be a JSON object, got: {}",
        parsed["params"]
    );
}

/// All lines in the Python-produced fixture must deserialize to WalLine without error (R-04).
#[test]
fn test_backward_compat_python_fixture_parseable() {
    for (i, raw) in read_fixture_lines("python_produced.jsonl")
        .iter()
        .enumerate()
    {
        serde_json::from_str::<WalLine>(raw)
            .unwrap_or_else(|e| panic!("line {i} failed to deserialize: {e}\nContent: {raw}"));
    }
}

/// seq values in the Python-produced fixture are non-decreasing (R-04 structural).
#[test]
fn test_backward_compat_seq_monotonic() {
    let lines: Vec<WalLine> = read_fixture_lines("python_produced.jsonl")
        .iter()
        .map(|raw| serde_json::from_str::<WalLine>(raw).expect("parse WalLine"))
        .collect();

    for window in lines.windows(2) {
        assert!(
            window[1].seq >= window[0].seq,
            "seq must be non-decreasing: {} then {}",
            window[0].seq,
            window[1].seq
        );
    }
}
