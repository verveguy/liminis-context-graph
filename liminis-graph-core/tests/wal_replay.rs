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

    // Lines 4, 6, 7 are MATCH-prefixed mutations; all 9 lines are replayed (seq 8 has apostrophes).
    assert_eq!(stats.lines_replayed, 9, "9 mutation lines replayed");
    assert_eq!(stats.lines_skipped(), 0, "no lines skipped");

    let entity_count = conn.count_nodes("Entity").unwrap();
    let episodic_count = conn.count_nodes("Episodic").unwrap();
    assert_eq!(
        entity_count, 3,
        "expected 3 Entity nodes (including apostrophe-bearing entity-2)"
    );
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
    assert_eq!(entity_count, 3, "expected 3 Entity nodes after WAL rebuild");
    assert_eq!(
        episodic_count, 2,
        "expected 2 Episodic nodes after WAL rebuild"
    );
}

/// MATCH-SET mutations in the fixture update field values; verify they landed (FR-006).
#[test]
fn test_replay_golden_fixture_field_updates() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");

    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let wal_dir_temp = TempDir::new().unwrap();
    fs::copy(
        fixture_path("python_produced.jsonl"),
        wal_dir_temp
            .path()
            .join("20260519_000000_aaa111_0000.jsonl"),
    )
    .unwrap();

    let _stats = WalReplayer::new(wal_dir_temp.path())
        .replay(&conn)
        .expect("replay fixture");

    // Seq 4: MATCH Entity entity-0 SET attributes — verify the attribute value was written.
    let entity = conn
        .get_entity_by_uuid("entity-0")
        .expect("query ok")
        .expect("entity-0 exists");
    assert_eq!(
        entity.attributes, "{\"updated\":true}",
        "entity-0 attributes should reflect MATCH-SET from seq 4"
    );

    // Seq 6: MATCH Episodic episodic-0 SET content_embedding — verify the embedding changed.
    // The original value (seq 2) was [0.5,0.5,0.5,0.5]; the update sets [0.9,0.8,0.7,0.6].
    let emb_rows = conn
        .cypher_query("MATCH (n:Episodic {uuid: 'episodic-0'}) RETURN n.content_embedding")
        .expect("cypher ok");
    assert!(!emb_rows.is_empty(), "episodic-0 must exist");
    let emb_str = &emb_rows[0][0];
    // The updated embedding [0.9,0.8,0.7,0.6] contains "0.9"; the original [0.5,0.5,0.5,0.5] does not.
    assert!(
        emb_str.contains("0.9"),
        "content_embedding should reflect update to [0.9,0.8,0.7,0.6], got: {emb_str:?}"
    );

    // Seq 7: MATCH RelatesToNode_ edge-0 SET fact_embedding — verify the embedding changed.
    // The original value (seq 5 CREATE) was [0.1,0.2,0.3,0.4]; the update sets [0.5,0.4,0.3,0.2].
    let fe_rows = conn
        .cypher_query("MATCH (rn:RelatesToNode_ {uuid: 'edge-0'}) RETURN rn.fact_embedding")
        .expect("cypher ok");
    assert!(!fe_rows.is_empty(), "RelatesToNode_ edge-0 must exist");
    let fe_str = &fe_rows[0][0];
    // The updated embedding starts with 0.5; the original started with 0.1 (no "0.5" in original).
    assert!(
        fe_str.contains("0.5"),
        "fact_embedding should reflect update to [0.5,0.4,0.3,0.2], got: {fe_str:?}"
    );
}

/// Pure MATCH … RETURN queries (no mutation clause) are classified as non-mutations and skipped (SC-006).
#[test]
fn test_replay_skips_pure_match_return() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    let line = r#"{"seq":0,"ts":"2026-05-19T00:00:00.000000+00:00","db":"","cypher":"MATCH (n:Entity {uuid: 'x'}) RETURN n.uuid","params":{}}"#;
    fs::write(
        wal_dir.path().join("20260519_000000_aaa111_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay");

    assert_eq!(
        stats.lines_skipped(),
        1,
        "pure MATCH-RETURN must be skipped"
    );
    assert_eq!(stats.lines_replayed, 0, "no mutations were replayed");
}

/// match_prefixed_replayed counter increments for MATCH-prefixed mutations only (FR-007).
#[test]
fn test_replay_match_prefixed_counter() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    // Two lines: one MERGE (not match-prefixed), one MATCH … SET (match-prefixed).
    let merge_line = make_entity_line(0, "counter-entity");
    let match_line = r#"{"seq":1,"ts":"2026-05-19T00:00:01.000000+00:00","db":"","cypher":"MATCH (n:Entity {uuid: 'counter-entity'}) SET n.summary = 'updated'","params":{}}"#;
    let content = format!("{merge_line}\n{match_line}\n");
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
        .expect("replay");

    assert_eq!(stats.lines_replayed, 2, "both mutations must be replayed");
    assert_eq!(
        stats.match_prefixed_replayed, 1,
        "only the MATCH-prefixed line counts as match_prefixed_replayed"
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

/// progress_fn fires at least once per file and carries correct files_total + counters (SC-001).
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

    #[derive(Clone)]
    struct Snapshot {
        files_processed: u64,
        files_total: u64,
        failed_lines_so_far: u64,
        legacy_skipped_lines_so_far: u64,
    }

    let calls: Arc<Mutex<Vec<Snapshot>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_clone = Arc::clone(&calls);

    let replayer = WalReplayer::new(wal_dir.path());
    let _stats = replayer
        .replay_opts(
            &conn,
            ReplayOptions {
                progress_fn: Some(Box::new(move |p: &ReplayProgress| {
                    calls_clone.lock().unwrap().push(Snapshot {
                        files_processed: p.files_processed,
                        files_total: p.files_total,
                        failed_lines_so_far: p.failed_lines_so_far,
                        legacy_skipped_lines_so_far: p.legacy_skipped_lines_so_far,
                    });
                    true
                })),
                ..Default::default()
            },
        )
        .expect("replay_opts progress");

    let calls_guard = calls.lock().unwrap();

    // Must fire at least once per file (2 files → ≥2 per-file events)
    assert!(
        calls_guard.len() >= 2,
        "progress_fn must fire at least once per file (got {} calls)",
        calls_guard.len()
    );

    // Every event carries the correct denominator (SC-001)
    for snap in calls_guard.iter() {
        assert_eq!(
            snap.files_total, 2,
            "files_total must equal the number of WAL files in the directory"
        );
    }

    // files_processed advances from 1 to N
    assert_eq!(
        calls_guard.first().unwrap().files_processed,
        1,
        "first progress event must have files_processed == 1"
    );
    assert_eq!(
        calls_guard
            .iter()
            .map(|s| s.files_processed)
            .max()
            .unwrap_or(0),
        2,
        "last files_processed must equal files_total"
    );

    // Clean WAL — no failures
    for snap in calls_guard.iter() {
        assert_eq!(
            snap.failed_lines_so_far, 0,
            "clean WAL must have failed_lines_so_far == 0"
        );
        assert_eq!(
            snap.legacy_skipped_lines_so_far, 0,
            "clean WAL must have legacy_skipped_lines_so_far == 0"
        );
    }
}

/// Mid-replay cancel: last progress event has files_processed < files_total (SC-001 scenario 4).
#[test]
fn test_replay_opts_progress_cancel_mid_run() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();

    for i in 0u64..3 {
        fs::write(
            wal_dir
                .path()
                .join(format!("20260522_{:06}_aaa_{:04}.jsonl", i, i)),
            make_entity_line(i, &format!("cancel-{i}")) + "\n",
        )
        .unwrap();
    }

    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    #[derive(Clone)]
    struct Snapshot {
        files_processed: u64,
        files_total: u64,
    }

    let calls: Arc<Mutex<Vec<Snapshot>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_clone = Arc::clone(&calls);

    // Cancel after the first file (return false from progress_fn)
    let cancelled = Arc::new(Mutex::new(false));
    let cancelled_clone = Arc::clone(&cancelled);

    let replayer = WalReplayer::new(wal_dir.path());
    let _stats = replayer
        .replay_opts(
            &conn,
            ReplayOptions {
                progress_fn: Some(Box::new(move |p: &ReplayProgress| {
                    calls_clone.lock().unwrap().push(Snapshot {
                        files_processed: p.files_processed,
                        files_total: p.files_total,
                    });
                    let mut done = cancelled_clone.lock().unwrap();
                    if p.files_processed >= 1 && !*done {
                        *done = true;
                        return false; // abort after first file
                    }
                    true
                })),
                ..Default::default()
            },
        )
        .expect("replay_opts cancel");

    let calls_guard = calls.lock().unwrap();
    assert!(
        !calls_guard.is_empty(),
        "at least one progress event expected before cancel"
    );
    let last = calls_guard.last().unwrap();
    assert_eq!(
        last.files_total, 3,
        "files_total must reflect total WAL files even when cancelled"
    );
    assert!(
        last.files_processed < last.files_total,
        "cancelled replay must have files_processed < files_total"
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
        stats.failed_samples[0].cypher.chars().count() <= 200,
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

/// SC-003: apostrophe in string param must replay without parse error and store correctly.
/// This is the root-cause regression test for issue #128 (84.5% data loss from '' vs \' escaping).
#[test]
fn test_apostrophe_in_params_replays_correctly() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // WAL line with apostrophes in name and summary params — previously caused parse errors.
    let line = r#"{"seq":0,"ts":"2026-05-19T00:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: $uuid}) ON CREATE SET n.name = $name, n.group_id = $group_id, n.labels = $labels, n.created_at = timestamp($created_at), n.name_embedding = $name_embedding, n.summary = $summary, n.attributes = $attributes","params":{"uuid":"apostrophe-entity","name":"Bob's team","group_id":"g","labels":["t"],"created_at":"2026-05-19 00:00:00","name_embedding":[1.0,0.0,0.0,0.0],"summary":"it's Alice's plan","attributes":"{}"}}"#;
    fs::write(
        wal_dir.path().join("20260519_000000_apo111_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.lines_replayed, 1,
        "apostrophe-bearing mutation must replay successfully"
    );
    assert_eq!(stats.failed_lines, 0, "no failures expected");

    // Verify the node was created with the exact apostrophe-bearing name preserved.
    let entity = conn
        .get_entity_by_uuid("apostrophe-entity")
        .expect("query ok")
        .expect("apostrophe-entity must exist in DB");
    assert_eq!(
        entity.name, "Bob's team",
        "name must be stored with apostrophe intact, not doubled or stripped"
    );
}

/// SC-004: fidelity_warning fires when failed_lines / total > 10%.
#[test]
fn test_fidelity_warning_fires_above_threshold() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Pre-insert a node so all subsequent CREATE lines (same UUID) fail with a duplicate PK.
    conn.run_cypher("CREATE (:Entity {uuid: 'fidelity-dup'})")
        .unwrap();

    // 11 failing lines (duplicate PK) + 1 successful MERGE = 11/12 = 91.7% > 10% threshold.
    let failing_line = |seq: u64| -> String {
        format!(
            r#"{{"seq":{seq},"ts":"2026-05-22T00:00:00.000000+00:00","db":"","cypher":"CREATE (:Entity {{uuid: 'fidelity-dup'}})","params":{{}}}}"#
        )
    };
    let good_line = make_entity_line(11, "fidelity-ok");
    let content: String = (0..11u64)
        .map(failing_line)
        .chain(std::iter::once(good_line))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(
        wal_dir.path().join("20260522_000000_fidelity_0000.jsonl"),
        &content,
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay_opts(
            &conn,
            ReplayOptions {
                failure_sample_cap: Some(0),
                ..Default::default()
            },
        )
        .expect("replay must not abort");

    assert_eq!(stats.lines_replayed, 1);
    assert_eq!(stats.failed_lines, 11);
    assert!(
        stats.fidelity_warning.is_some(),
        "fidelity_warning must be set when >10% of mutations fail"
    );
    let warning = stats.fidelity_warning.as_deref().unwrap();
    assert!(
        warning.contains("91.") || warning.contains("91,"),
        "warning must contain the observed ratio (~91.7%), got: {warning}"
    );
    assert!(
        warning.contains("10.0%") || warning.contains("10,0%"),
        "warning must state the threshold (10.0%), got: {warning}"
    );
}

/// SC-005: legacy-schema mutations (Community node) increment legacy_skipped_lines, not failed_lines.
#[test]
fn test_legacy_skip_community_node() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Line 1: Community node CREATE — references a table that doesn't exist in current schema.
    let community_line = r#"{"seq":0,"ts":"2026-05-19T00:00:00.000000+00:00","db":"","cypher":"CREATE (n:Community {uuid: 'comm-1', name: 'Test Community'})","params":{}}"#;
    // Line 2: valid Entity MERGE — should succeed normally.
    let entity_line = make_entity_line(1, "legacy-skip-entity");
    let content = format!("{community_line}\n{entity_line}\n");
    fs::write(
        wal_dir.path().join("20260519_000000_legacy_0000.jsonl"),
        &content,
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(stats.lines_replayed, 1, "valid Entity must be replayed");
    assert_eq!(
        stats.failed_lines, 0,
        "Community must not count as a failure"
    );
    assert_eq!(
        stats.legacy_skipped_lines, 1,
        "Community CREATE must be counted as legacy_skipped_lines"
    );
    assert_eq!(
        stats.lines_skipped(),
        0,
        "lines_skipped() does not include legacy_skipped_lines"
    );
}

/// FR-006 / SC-004: ISO-8601 timestamps in bare $param positions must succeed as TIMESTAMP values.
///
/// Regression guard: if timestamp detection is removed from json_to_cypher_literal,
/// lbug will reject STRING→TIMESTAMP and this test fails immediately in CI.
#[test]
fn test_timestamp_in_params_replays_correctly() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Fixture contains:
    //   Line 0: Episodic MERGE with bare $created_at / $valid_at params (RFC-3339 strings)
    //   Line 1: RelatesToNode_ MERGE with bare $created_at / $valid_at / $invalid_at params
    // Templates use bare $param (no timestamp() wrapper) — the fix must supply the wrapper.
    let wal_dir = TempDir::new().unwrap();
    let fixture = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/wal/timestamps_in_params.jsonl"),
    )
    .unwrap();
    std::fs::write(
        wal_dir.path().join("20260612_100000_ts_test_0000.jsonl"),
        &fixture,
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.failed_lines, 0,
        "timestamp params must not produce STRING→TIMESTAMP binder errors; got {} failures",
        stats.failed_lines
    );
    assert_eq!(
        stats.lines_replayed, 2,
        "both fixture lines must be replayed"
    );

    // Verify the RelatesToNode_ edge is findable by uuid — confirms the CREATE landed and
    // that subsequent MATCHes won't cascade into "Cannot find property" errors.
    let rows = conn
        .cypher_query("MATCH (r:RelatesToNode_ {uuid: 'ts-edge-1'}) RETURN r.uuid")
        .expect("cypher query must succeed");
    assert!(
        !rows.is_empty(),
        "RelatesToNode_ ts-edge-1 must exist after replay"
    );
}

// ---------------------------------------------------------------------------
// #133 regression tests: episodes schema parity + FalkorDB-dialect translation
// ---------------------------------------------------------------------------

/// FR-008a / FR-008d: FalkorDB-era WAL with `episodes STRING[]` + `VECF32($param)` replays
/// correctly. Verifies:
///   - mutations are counted in `mutations_replayed` (not `legacy_skipped_lines`)
///   - `failed_lines == 0`
///   - `RelatesToNode_` node is created (relationship layer is not discarded)
#[test]
fn test_falkordb_episodes_merge_replays_successfully() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let fixture = std::fs::read_to_string(fixture_path("falkordb_era_episodes.jsonl")).unwrap();
    std::fs::write(
        wal_dir.path().join("20260401_100000_episodes_0000.jsonl"),
        &fixture,
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.failed_lines,
        0,
        "episodes + VECF32 mutations must not fail; got {} failures with samples: {:?}",
        stats.failed_lines,
        stats
            .failed_samples
            .iter()
            .map(|s| &s.error)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        stats.legacy_skipped_lines, 0,
        "episodes mutations must not be silently skipped"
    );
    assert_eq!(
        stats.lines_replayed, 2,
        "both fixture lines must be replayed"
    );

    // Relationship layer must exist — the RelatesToNode_ node was created.
    let count = conn.count_nodes("RelatesToNode_").expect("count query");
    assert!(count > 0, "RelatesToNode_ must be non-empty after replay");

    // Verify the specific node is queryable by uuid.
    let rows = conn
        .cypher_query("MATCH (r:RelatesToNode_ {uuid: 'rel-episodes-001'}) RETURN r.uuid")
        .expect("cypher query must succeed");
    assert!(
        !rows.is_empty(),
        "RelatesToNode_ rel-episodes-001 must exist after replay"
    );
}

/// SC-001 regression: `RelatesToNode_` WAL lines carrying `SET r.expired_at = $expired_at`
/// replayed successfully after `expired_at TIMESTAMP` was added to the schema.
///
/// Before the fix, lbug raised `Binder exception: Cannot find property expired_at for r`
/// which failed the MERGE and cascaded into ~74k `Cannot find property uuid` errors.
///
/// Verifies:
///   - `failed_lines == 0`
///   - `legacy_skipped_lines == 0` (not silently skipped)
///   - `lines_replayed == 1`
///   - `expired_at` is stored non-null on the created node (timestamp round-trip)
#[test]
fn test_falkordb_expired_at_merge_replays_successfully() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    let fixture = std::fs::read_to_string(fixture_path("falkordb_era_expired_at.jsonl")).unwrap();
    std::fs::write(
        wal_dir.path().join("20260401_100000_expired_at_0000.jsonl"),
        &fixture,
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.failed_lines,
        0,
        "expired_at mutation must not fail; got {} failures with samples: {:?}",
        stats.failed_lines,
        stats
            .failed_samples
            .iter()
            .map(|s| &s.error)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        stats.legacy_skipped_lines, 0,
        "expired_at mutation must not be silently skipped"
    );
    assert_eq!(stats.lines_replayed, 1, "fixture line must be replayed");

    // Verify expired_at was stored — the column must be non-null for this fixture.
    let rows = conn
        .cypher_query(
            "MATCH (r:RelatesToNode_ {uuid: 'rel-expired-at-001'}) \
             WHERE r.expired_at IS NOT NULL RETURN r.uuid",
        )
        .expect("cypher query must succeed");
    assert!(
        !rows.is_empty(),
        "RelatesToNode_ rel-expired-at-001 must exist with non-null expired_at after replay"
    );
}

/// FR-008b: VECF32([…]) inline array form strips correctly and the containing mutation succeeds.
#[test]
fn test_vecf32_inline_array_strips_correctly() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Uppercase VECF32 with an inline array literal — must be stripped before raw_query.
    let line = r#"{"seq":0,"ts":"2026-04-01T10:00:00.000000+00:00","db":"","cypher":"MERGE (ep:Episodic {uuid: $uuid}) ON CREATE SET ep.name = $name, ep.group_id = $group_id, ep.created_at = $created_at, ep.source = $src, ep.source_description = $sd, ep.content = $content, ep.content_embedding = VECF32([0.1, 0.2, 0.3, 0.4]), ep.valid_at = $valid_at, ep.entity_edges = $entity_edges","params":{"uuid":"ep-inline-vecf32","name":"Inline Vecf32","group_id":"g","created_at":"2026-03-25T10:00:00.000000+00:00","src":"test","sd":"test source","content":"inline test","valid_at":"2026-03-25T10:00:00.000000+00:00","entity_edges":[]}}"#;
    std::fs::write(
        wal_dir
            .path()
            .join("20260401_000000_vecf32_inline_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.failed_lines,
        0,
        "VECF32 inline must not fail; got {} failures with samples: {:?}",
        stats.failed_lines,
        stats
            .failed_samples
            .iter()
            .map(|s| &s.error)
            .collect::<Vec<_>>()
    );
    assert_eq!(stats.lines_replayed, 1);
}

/// FR-008b: vecf32($param) lowercase param-ref form strips correctly.
#[test]
fn test_vecf32_param_ref_strips_correctly() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // Lowercase vecf32 with a param reference — must strip to $name_embedding.
    let line = r#"{"seq":0,"ts":"2026-04-01T10:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: $uuid}) ON CREATE SET n.name = $name, n.group_id = $group_id, n.labels = $labels, n.created_at = $created_at, n.name_embedding = vecf32($name_embedding), n.summary = $summary, n.attributes = $attrs","params":{"uuid":"e-param-vecf32","name":"Param Vecf32","group_id":"g","labels":["person"],"created_at":"2026-03-25T10:00:00.000000+00:00","name_embedding":[0.1,0.2,0.3,0.4],"summary":"test","attrs":"{}"}}"#;
    std::fs::write(
        wal_dir
            .path()
            .join("20260401_000000_vecf32_param_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.failed_lines,
        0,
        "vecf32 param-ref must not fail; got {} failures with samples: {:?}",
        stats.failed_lines,
        stats
            .failed_samples
            .iter()
            .map(|s| &s.error)
            .collect::<Vec<_>>()
    );
    assert_eq!(stats.lines_replayed, 1);
}

/// FR-008c: bulk `SET n = $props` is expanded to individual property assignments.
/// Uses a FalkorDB-style WAL line where all properties come from a single map param.
#[test]
fn test_bulk_set_expands_to_individual_assignments() {
    let wal_dir = TempDir::new().unwrap();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let db = Db::open(db_path.to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    // FalkorDB-style bulk SET: all properties come from a single $props map.
    // After expand_bulk_property_set this becomes individual SET n.k = $props_k assignments.
    let line = r#"{"seq":0,"ts":"2026-04-01T10:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: $uuid}) ON CREATE SET n = $props","params":{"uuid":"bulk-uuid","props":{"name":"Alice","group_id":"g1"}}}"#;
    std::fs::write(
        wal_dir.path().join("20260401_000000_bulk_set_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let stats = WalReplayer::new(wal_dir.path())
        .replay(&conn)
        .expect("replay must succeed");

    assert_eq!(
        stats.failed_lines,
        0,
        "bulk SET must not fail; got {} failures with samples: {:?}",
        stats.failed_lines,
        stats
            .failed_samples
            .iter()
            .map(|s| &s.error)
            .collect::<Vec<_>>()
    );
    assert_eq!(stats.lines_replayed, 1);

    // Entity must exist with the properties from $props.
    let rows = conn
        .cypher_query("MATCH (n:Entity {uuid: 'bulk-uuid'}) RETURN n.name")
        .expect("cypher query must succeed");
    assert!(
        !rows.is_empty(),
        "Entity bulk-uuid must exist after bulk-SET expansion"
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
    assert_eq!(stats.legacy_skipped_lines, 0);
    assert_eq!(stats.lines_skipped(), 0);
    assert!(
        stats.fidelity_warning.is_none(),
        "no warning when total is 0"
    );
    assert!(
        stats.failed_samples.is_empty(),
        "no failures → empty sample vec"
    );
}
