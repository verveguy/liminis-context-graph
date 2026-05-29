use liminis_graph_core::WalLine;
use serde_json::json;
use std::path::Path;

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wal")
        .join(name)
}

fn make_line(seq: u64) -> WalLine {
    WalLine {
        seq,
        ts: "2026-05-19T00:00:00.000000+00:00".to_string(),
        db: "graphiti".to_string(),
        cypher: "MERGE (n:Entity {uuid: $uuid})".to_string(),
        params: json!({"uuid": "test-uuid-1"}),
    }
}

/// serde_json must serialize WalLine fields in declaration order: seq, ts, db, cypher, params.
#[test]
fn test_walline_serializes_field_order() {
    let line = make_line(1);
    let json_str = serde_json::to_string(&line).expect("serialize");

    let seq_pos = json_str.find("\"seq\"").expect("seq key");
    let ts_pos = json_str.find("\"ts\"").expect("ts key");
    let db_pos = json_str.find("\"db\"").expect("db key");
    let cypher_pos = json_str.find("\"cypher\"").expect("cypher key");
    let params_pos = json_str.find("\"params\"").expect("params key");

    assert!(seq_pos < ts_pos, "seq must precede ts");
    assert!(ts_pos < db_pos, "ts must precede db");
    assert!(db_pos < cypher_pos, "db must precede cypher");
    assert!(cypher_pos < params_pos, "cypher must precede params");
}

/// Round-trip: serialize then deserialize, all fields are preserved.
#[test]
fn test_walline_roundtrip() {
    let original = WalLine {
        seq: 42,
        ts: "2026-05-19T12:34:56.789000+00:00".to_string(),
        db: "mydb".to_string(),
        cypher: "MERGE (n:Entity {uuid: $uuid}) SET n.name = $name".to_string(),
        params: json!({"uuid": "abc", "name": "Alice"}),
    };

    let json_str = serde_json::to_string(&original).expect("serialize");
    let recovered: WalLine = serde_json::from_str(&json_str).expect("deserialize");

    assert_eq!(recovered.seq, original.seq);
    assert_eq!(recovered.ts, original.ts);
    assert_eq!(recovered.db, original.db);
    assert_eq!(recovered.cypher, original.cypher);
    assert_eq!(recovered.params, original.params);
}

/// Every line of python_produced.jsonl must deserialize into WalLine with the expected seq value.
#[test]
fn test_walline_deserializes_python_fixture() {
    let content =
        std::fs::read_to_string(fixture_path("python_produced.jsonl")).expect("read fixture");

    let expected_seqs = [0u64, 1, 2, 3, 4, 5, 6, 7];
    let mut idx = 0;
    for raw in content.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let line: WalLine = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("failed to parse line {idx}: {e}\nContent: {raw}"));
        assert_eq!(line.seq, expected_seqs[idx], "seq mismatch at line {idx}");
        idx += 1;
    }
    assert_eq!(
        idx,
        expected_seqs.len(),
        "expected {} lines",
        expected_seqs.len()
    );
}

/// Rust serialization must produce byte-for-byte output matching rust_produced_expected.jsonl.
#[test]
fn test_walline_pins_rust_output() {
    let expected = std::fs::read_to_string(fixture_path("rust_produced_expected.jsonl"))
        .expect("read fixture");
    let expected = expected.trim_end_matches('\n');

    let line = make_line(1);
    let actual = serde_json::to_string(&line).expect("serialize");

    assert_eq!(actual, expected, "Rust WalLine serialization changed");
}
