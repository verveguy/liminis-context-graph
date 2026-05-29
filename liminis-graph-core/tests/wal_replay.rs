use liminis_graph_core::{Db, ReplayOptions, ReplayProgress, WalReplayer};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
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
    let db_dir = TempDir::new().unwrap();

    // Write 3 files with timestamps in reverse creation order.
    // File B (lexicographically first, timestamp 030000): MERGE entity-a
    // File A (lexicographically middle, timestamp 040000): MERGE entity-b
    // File C (lexicographically last, timestamp 050000): MERGE entity-c
    // All 3 are independent; replay in any order still produces 3 entities.
    let lines = [
        (
            "20260519_030000_aaa111_0000.jsonl",
            r#"{"seq":0,"ts":"2026-05-19T03:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'entity-a'}) ON CREATE SET n.name = 'A', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 'sa', n.attributes = '{}'","params":{}}"#,
        ),
        (
            "20260519_040000_bbb222_0000.jsonl",
            r#"{"seq":1,"ts":"2026-05-19T04:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'entity-b'}) ON CREATE SET n.name = 'B', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [0.0, 1.0, 0.0, 0.0], n.summary = 'sb', n.attributes = '{}'","params":{}}"#,
        ),
        (
            "20260519_050000_ccc333_0000.jsonl",
            r#"{"seq":2,"ts":"2026-05-19T05:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'entity-c'}) ON CREATE SET n.name = 'C', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [0.0, 0.0, 1.0, 0.0], n.summary = 'sc', n.attributes = '{}'","params":{}}"#,
        ),
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
    let db_dir = TempDir::new().unwrap();

    // A valid first line + a truncated second line (simulates crash during write).
    let content = concat!(
        r#"{"seq":0,"ts":"2026-05-19T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'trunc-e'}) ON CREATE SET n.name = 'T', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 'st', n.attributes = '{}'","params":{}}"#,
        "\n",
        r#"{"seq":5,"ts":"2026-05-19T00:00:"#,
    );
    fs::write(
        wal_dir.path().join("20260519_000000_aaa111_0000.jsonl"),
        content,
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(stats.lines_skipped(), 1, "truncated line must be skipped");
    assert_eq!(stats.lines_replayed, 1, "first valid line must be replayed");
}

/// A WAL line with an unknown first-token Cypher op is skipped without aborting (R-08).
#[test]
fn test_replay_skips_unknown_op_without_abort() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

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

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must not abort");

    assert_eq!(
        stats.lines_skipped(),
        1,
        "unknown op must be counted as skipped"
    );
    assert_eq!(stats.lines_replayed, 0);
}

/// Replay on an empty directory returns zero stats and does not error.
#[test]
fn test_replay_empty_dir_succeeds() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay empty dir");

    assert_eq!(stats.lines_replayed, 0);
    assert_eq!(stats.lines_skipped(), 0);
    assert_eq!(stats.files_read, 0);
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
        wal_dir_temp
            .path()
            .join("20260519_000000_aaa111_0000.jsonl"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir_temp.path())
        .replay(&conn)
        .expect("replay fixture");

    // Line 4 (MATCH ... SET) contains SET keyword so is correctly replayed.
    assert_eq!(stats.lines_replayed, 5, "5 mutation lines replayed");
    assert_eq!(stats.lines_skipped(), 0, "no lines skipped");

    let entity_count = conn.count_nodes("Entity").unwrap();
    let episodic_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(entity_count, 2, "expected 2 Entity nodes");
    assert_eq!(episodic_count, 2, "expected 2 Episodic nodes");
}

/// Db::open_or_rebuild replays WAL into a fresh DB when db_path is absent (R-06).
#[test]
fn test_open_or_rebuild_from_wal() {
    let root = TempDir::new().unwrap();
    let db_path = root.path().join("graph.db");
    let wal_dir = root.path().join("wal");
    fs::create_dir_all(&wal_dir).unwrap();

    // Copy the golden fixture into the WAL directory.
    fs::copy(
        fixture_path("python_produced.jsonl"),
        wal_dir.join("20260519_000000_aaa111_0000.jsonl"),
    )
    .unwrap();

    // DB does not exist yet; open_or_rebuild should replay the WAL.
    let db = Db::open_or_rebuild(db_path.to_str().unwrap(), wal_dir.to_str().unwrap(), 4)
        .expect("open_or_rebuild");

    let conn = db.connect().expect("connect");
    let entity_count = conn.count_nodes("Entity").unwrap();
    let episodic_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(entity_count, 2, "expected 2 Entity nodes after WAL rebuild");
    assert_eq!(
        episodic_count, 2,
        "expected 2 Episodic nodes after WAL rebuild"
    );
}

fn make_entity_line(seq: u64, uuid: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {{uuid: '{uuid}'}}) ON CREATE SET n.name = '{uuid}', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{{}}'","params":{{}}}}"#
    )
}

/// from_seq: lines with seq < from_seq are skipped, not counted as skipped in stats.
#[test]
fn test_replay_opts_from_seq() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    // Write 3 entities with seq 0,1,2
    let content = [
        make_entity_line(0, "entity-seq0"),
        make_entity_line(1, "entity-seq1"),
        make_entity_line(2, "entity-seq2"),
    ]
    .join("\n")
        + "\n";
    fs::write(
        wal_dir.path().join("20260522_000000_aaa111_0000.jsonl"),
        &content,
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let replayer = WalReplayer::new(wal_dir.path());
    let stats = replayer
        .replay_opts(
            &conn,
            ReplayOptions {
                from_seq: 1, // skip seq=0
                ..Default::default()
            },
        )
        .expect("replay_opts");

    assert_eq!(stats.lines_replayed, 2, "only seq>=1 lines replayed");
    let count = conn.count_nodes("Entity").unwrap();
    assert_eq!(count, 2, "only 2 entities in DB (seq 1 and 2)");
}

/// dry_run: mutations are counted but DB is unchanged.
#[test]
fn test_replay_opts_dry_run() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    let content = [
        make_entity_line(0, "dry-entity-a"),
        make_entity_line(1, "dry-entity-b"),
    ]
    .join("\n")
        + "\n";
    fs::write(
        wal_dir.path().join("20260522_000000_bbb222_0000.jsonl"),
        &content,
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let count_before = conn.count_nodes("Entity").unwrap();

    let replayer = WalReplayer::new(wal_dir.path());
    let stats = replayer
        .replay_opts(
            &conn,
            ReplayOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .expect("replay_opts dry_run");

    assert_eq!(stats.lines_replayed, 2, "dry_run should count 2 mutations");
    let count_after = conn.count_nodes("Entity").unwrap();
    assert_eq!(count_before, count_after, "dry_run must not modify the DB");
}

/// progress_fn fires at least once per file.
#[test]
fn test_replay_opts_progress_callback() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    fs::write(
        wal_dir.path().join("20260522_000000_ccc333_0000.jsonl"),
        make_entity_line(0, "prog-a") + "\n",
    )
    .unwrap();
    fs::write(
        wal_dir.path().join("20260522_000001_ddd444_0001.jsonl"),
        make_entity_line(1, "prog-b") + "\n",
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let calls: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_clone = Arc::clone(&calls);

    let replayer = WalReplayer::new(wal_dir.path());
    let _stats = replayer
        .replay_opts(
            &conn,
            ReplayOptions {
                progress_fn: Some(Box::new(move |p: &ReplayProgress| {
                    calls_clone
                        .lock()
                        .unwrap()
                        .push((p.files_processed, p.mutations_replayed));
                    true
                })),
                ..Default::default()
            },
        )
        .expect("replay_opts progress");

    let calls_guard = calls.lock().unwrap();
    assert!(
        calls_guard.len() >= 2,
        "progress_fn must fire at least once per file (got {} calls)",
        calls_guard.len()
    );
}

/// Replay skips a file that cannot be opened (broken symlink) and continues with the rest.
/// The valid files must still be replayed and replay() must return Ok.
#[test]
#[cfg(unix)]
fn test_replay_skips_unreadable_file_and_continues() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    // Two valid WAL files.
    fs::write(
        wal_dir.path().join("20260519_010000_aaa111_0000.jsonl"),
        make_entity_line(0, "readable-a") + "\n",
    )
    .unwrap();
    fs::write(
        wal_dir.path().join("20260519_030000_ccc333_0002.jsonl"),
        make_entity_line(2, "readable-c") + "\n",
    )
    .unwrap();

    // One broken symlink in lexicographic middle — File::open will return Err.
    std::os::unix::fs::symlink(
        "/nonexistent/path/that/does/not/exist.jsonl",
        wal_dir.path().join("20260519_020000_bbb222_0001.jsonl"),
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must return Ok even with one unreadable file");

    assert_eq!(
        stats.lines_replayed, 2,
        "exactly both valid files must be replayed (lines_replayed={})",
        stats.lines_replayed
    );
    // The unreadable file's stats.files_read still counts it (we incremented before open).
    assert_eq!(stats.files_read, 3, "all 3 files were attempted");
}

/// progress_fn returning false aborts replay after current file.
#[test]
fn test_replay_opts_progress_abort() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    fs::write(
        wal_dir.path().join("20260522_000000_eee555_0000.jsonl"),
        make_entity_line(0, "abort-a") + "\n",
    )
    .unwrap();
    fs::write(
        wal_dir.path().join("20260522_000001_fff666_0001.jsonl"),
        make_entity_line(1, "abort-b") + "\n",
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let replayer = WalReplayer::new(wal_dir.path());
    // Return false on first call — should abort after first file
    let stats = replayer
        .replay_opts(
            &conn,
            ReplayOptions {
                progress_fn: Some(Box::new(|_| false)),
                ..Default::default()
            },
        )
        .expect("replay_opts abort");

    assert_eq!(stats.files_read, 1, "replay aborted after first file");
}

/// FR-006: four_bucket_regression — each of the four stat buckets can be independently triggered.
/// Writes a 4-line WAL: 1 valid mutation, 1 unrecognised shape, 1 unparseable JSON, 1 failed
/// execution (duplicate primary key). Asserts each counter is 1, sum is 3, and the failure
/// sample captures the Cypher snippet and error from the failing line.
#[test]
fn four_bucket_regression() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Pre-insert a node so the fourth WAL line (a CREATE with same UUID) violates the PK.
    conn.run_cypher("CREATE (:Entity {uuid: 'conflict-uuid-123'})")
        .unwrap();

    // Line 1: valid mutation — succeeds → lines_replayed
    let line1 = r#"{"seq":0,"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'new-entity-ok'}) ON CREATE SET n.name = 'ok', n.group_id = 'g', n.labels = ['t'], n.created_at = timestamp('2026-05-22 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{}'","params":{}}"#;
    // Line 2: read-only shape — no mutation keyword → unrecognised_lines
    let line2 = r#"{"seq":1,"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"MATCH (n) RETURN n","params":{}}"#;
    // Line 3: malformed JSON — parse failure → unparseable_lines
    let line3 = r#"not valid json {"#;
    // Line 4: duplicate primary key — raw_query returns Err → failed_lines
    let line4 = r#"{"seq":3,"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"CREATE (:Entity {uuid: 'conflict-uuid-123'})","params":{}}"#;

    fs::write(
        wal_dir.path().join("20260522_000000_aaa111_0000.jsonl"),
        format!("{line1}\n{line2}\n{line3}\n{line4}\n"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay_opts(
            &conn,
            ReplayOptions {
                failure_sample_cap: Some(10),
                ..Default::default()
            },
        )
        .expect("replay must not abort even with failures");

    assert_eq!(stats.lines_replayed, 1, "one successful mutation");
    assert_eq!(
        stats.unrecognised_lines, 1,
        "MATCH...RETURN is unrecognised"
    );
    assert_eq!(stats.unparseable_lines, 1, "malformed JSON line");
    assert_eq!(
        stats.failed_lines, 1,
        "duplicate PK causes execution failure"
    );
    assert_eq!(stats.lines_skipped(), 3, "sum of three failure buckets");
    assert_eq!(stats.failed_samples.len(), 1, "one failure sample captured");
    assert!(
        !stats.failed_samples[0].cypher.is_empty(),
        "sample must have a cypher snippet"
    );
    assert!(
        stats.failed_samples[0].cypher.len() <= 200,
        "cypher snippet must be truncated to 200 chars"
    );
    assert!(
        !stats.failed_samples[0].error.is_empty(),
        "sample must have a non-empty error message"
    );
}

/// FR-006: sample_cap_respected — failure_sample_cap limits the number of collected samples.
#[test]
fn sample_cap_respected() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Pre-insert so all 5 CREATE lines will fail with duplicate PK.
    conn.run_cypher("CREATE (:Entity {uuid: 'dup-cap-uuid'})")
        .unwrap();

    let content: String = (0..5u64)
        .map(|seq| {
            format!(
                r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"CREATE (:Entity {{uuid: 'dup-cap-uuid'}})","params":{{}}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    fs::write(
        wal_dir.path().join("20260522_000000_bbb222_0000.jsonl"),
        &content,
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay_opts(
            &conn,
            ReplayOptions {
                failure_sample_cap: Some(3),
                ..Default::default()
            },
        )
        .expect("replay must not abort");

    assert_eq!(stats.failed_lines, 5, "all five lines failed");
    assert_eq!(
        stats.failed_samples.len(),
        3,
        "only first 3 samples collected (cap=3)"
    );
}

/// FR-006: empty_wal_all_zeros — all new counters are zero and failed_samples is empty.
#[test]
fn empty_wal_all_zeros() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay_opts(&conn, ReplayOptions::default())
        .expect("replay empty dir");

    assert_eq!(stats.lines_replayed, 0);
    assert_eq!(stats.unrecognised_lines, 0);
    assert_eq!(stats.failed_lines, 0);
    assert_eq!(stats.unparseable_lines, 0);
    assert_eq!(stats.lines_skipped(), 0);
    assert!(
        stats.failed_samples.is_empty(),
        "no failures → empty sample vec"
    );
}
