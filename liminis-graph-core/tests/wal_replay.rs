use liminis_graph_core::{Db, WalReplayer};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wal")
        .join(name)
}

/// Replay reads files in lexicographic filename order (R-07).
/// Files are named with timestamps in non-creation order; entities must all be present.
#[test]
fn test_replay_files_in_lexicographic_order() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir  = TempDir::new().unwrap();

    // Write 3 files with timestamps in reverse creation order.
    // File B (lexicographically first, timestamp 030000): MERGE entity-a
    // File A (lexicographically middle, timestamp 040000): MERGE entity-b
    // File C (lexicographically last, timestamp 050000): MERGE entity-c
    // All 3 are independent; replay in any order still produces 3 entities.
    let lines = [
        ("20260519_030000_aaa111_0000.jsonl", r#"{"seq":0,"ts":"2026-05-19T03:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'entity-a'}) ON CREATE SET n.name = 'A', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 'sa', n.attributes = '{}'","params":{}}"#),
        ("20260519_040000_bbb222_0000.jsonl", r#"{"seq":1,"ts":"2026-05-19T04:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'entity-b'}) ON CREATE SET n.name = 'B', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [0.0, 1.0, 0.0, 0.0], n.summary = 'sb', n.attributes = '{}'","params":{}}"#),
        ("20260519_050000_ccc333_0000.jsonl", r#"{"seq":2,"ts":"2026-05-19T05:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'entity-c'}) ON CREATE SET n.name = 'C', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [0.0, 0.0, 1.0, 0.0], n.summary = 'sc', n.attributes = '{}'","params":{}}"#),
    ];

    // Write in C, A, B order (non-lexicographic) to stress the sort.
    fs::write(wal_dir.path().join(lines[2].0), format!("{}\n", lines[2].1)).unwrap();
    fs::write(wal_dir.path().join(lines[0].0), format!("{}\n", lines[0].1)).unwrap();
    fs::write(wal_dir.path().join(lines[1].0), format!("{}\n", lines[1].1)).unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let replayer = WalReplayer::new(wal_dir.path());
    let stats = replayer.replay(&conn).expect("replay");

    assert_eq!(stats.files_read, 3);
    assert_eq!(stats.lines_replayed, 3);
    let count = conn.count_nodes("Entity").unwrap();
    assert_eq!(count, 3, "all 3 entities must be replayed");
}

/// Truncated final line is skipped; replay succeeds and reports it in lines_skipped (R-05).
#[test]
fn test_replay_tolerates_truncated_final_line() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir  = TempDir::new().unwrap();

    // A valid first line + a truncated second line (simulates crash during write).
    let content = concat!(
        r#"{"seq":0,"ts":"2026-05-19T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'trunc-e'}) ON CREATE SET n.name = 'T', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 'st', n.attributes = '{}'","params":{}}"#,
        "\n",
        r#"{"seq":5,"ts":"2026-05-19T00:00:"#,
    );
    fs::write(wal_dir.path().join("20260519_000000_aaa111_0000.jsonl"), content).unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path()).replay(&conn).expect("replay must succeed");

    assert_eq!(stats.lines_skipped, 1, "truncated line must be skipped");
    assert_eq!(stats.lines_replayed, 1, "first valid line must be replayed");
}

/// A WAL line with an unknown first-token Cypher op is skipped without aborting (R-08).
#[test]
fn test_replay_skips_unknown_op_without_abort() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir  = TempDir::new().unwrap();

    // EXPLAIN is not in the known-mutation set.
    let content = r#"{"seq":0,"ts":"2026-05-19T00:00:00.000000+00:00","db":"","cypher":"EXPLAIN MATCH (n) RETURN n","params":{}}"#;
    fs::write(
        wal_dir.path().join("20260519_000000_aaa111_0000.jsonl"),
        format!("{content}\n"),
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path()).replay(&conn).expect("replay must not abort");

    assert_eq!(stats.lines_skipped, 1, "unknown op must be counted as skipped");
    assert_eq!(stats.lines_replayed, 0);
}

/// Replay on an empty directory returns zero stats and does not error.
#[test]
fn test_replay_empty_dir_succeeds() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir  = TempDir::new().unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path()).replay(&conn).expect("replay empty dir");

    assert_eq!(stats.lines_replayed, 0);
    assert_eq!(stats.lines_skipped,  0);
    assert_eq!(stats.files_read,     0);
}

/// Replay the golden fixture against a fresh DB and verify entity/episodic counts (R-04).
#[test]
fn test_replay_golden_fixture_counts() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");

    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Copy fixture to a temp WAL dir so other fixture files don't interfere.
    let wal_dir_temp = TempDir::new().unwrap();
    fs::copy(
        fixture_path("python_produced.jsonl"),
        wal_dir_temp.path().join("20260519_000000_aaa111_0000.jsonl"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir_temp.path())
        .replay(&conn)
        .expect("replay fixture");

    // Line 4 (MATCH ... SET) has first token MATCH, skipped by replayer.
    assert_eq!(stats.lines_replayed, 4, "4 mutation lines replayed");
    assert_eq!(stats.lines_skipped,  1, "1 MATCH line skipped");

    let entity_count   = conn.count_nodes("Entity").unwrap();
    let episodic_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(entity_count,   2, "expected 2 Entity nodes");
    assert_eq!(episodic_count, 2, "expected 2 Episodic nodes");
}

/// Db::open_or_rebuild replays WAL into a fresh DB when db_path is absent (R-06).
#[test]
fn test_open_or_rebuild_from_wal() {
    let root = TempDir::new().unwrap();
    let db_path  = root.path().join("graph.db");
    let wal_dir  = root.path().join("wal");
    fs::create_dir_all(&wal_dir).unwrap();

    // Copy the golden fixture into the WAL directory.
    fs::copy(
        fixture_path("python_produced.jsonl"),
        wal_dir.join("20260519_000000_aaa111_0000.jsonl"),
    )
    .unwrap();

    // DB does not exist yet; open_or_rebuild should replay the WAL.
    let db = Db::open_or_rebuild(
        db_path.to_str().unwrap(),
        wal_dir.to_str().unwrap(),
        4,
    )
    .expect("open_or_rebuild");

    let conn = db.connect().expect("connect");
    let entity_count   = conn.count_nodes("Entity").unwrap();
    let episodic_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(entity_count,   2, "expected 2 Entity nodes after WAL rebuild");
    assert_eq!(episodic_count, 2, "expected 2 Episodic nodes after WAL rebuild");
}
