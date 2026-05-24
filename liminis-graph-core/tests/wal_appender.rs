use liminis_graph_core::{WalLine, WalWriter};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

fn open_writer(dir: &TempDir, max_events: usize) -> WalWriter {
    WalWriter::new(dir.path(), max_events).expect("WalWriter::new")
}

fn jsonl_files(dir: &TempDir) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

fn count_lines(path: &std::path::Path) -> usize {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn parse_seq(line: &str) -> u64 {
    serde_json::from_str::<WalLine>(line)
        .expect("parse WalLine")
        .seq
}

/// with_chunk returns Ok: exactly one file with 3 lines.
#[test]
fn test_with_chunk_writes_file_on_success() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    w.with_chunk(|w| {
        for _ in 0..3 {
            w.log_mutation("MERGE (n:Entity {uuid: 'x'})", json!({}), "db")?;
        }
        Ok(())
    })
    .expect("with_chunk");

    let files = jsonl_files(&dir);
    assert_eq!(files.len(), 1, "expected exactly one JSONL file");
    assert_eq!(count_lines(&files[0]), 3, "expected 3 lines");
}

/// with_chunk returns Err: no files written (R-02 chunk-atomicity invariant).
#[test]
fn test_with_chunk_discards_on_error() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    let _: Result<(), liminis_graph_core::Error> = w.with_chunk(|w| {
        w.log_mutation("MERGE (n:Entity {uuid: 'x'})", json!({}), "db")?;
        w.log_mutation("MERGE (n:Entity {uuid: 'y'})", json!({}), "db")?;
        Err(liminis_graph_core::Error::QueryFailed(
            "simulated error".to_string(),
        ))
    });

    assert!(
        jsonl_files(&dir).is_empty(),
        "no files must be written on chunk error"
    );
}

/// seq values in a second chunk are strictly greater than those in the first.
#[test]
fn test_seq_monotonic_across_chunks() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    for _ in 0..2 {
        w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'a'})", json!({}), "db"))
            .unwrap();
    }

    let files = jsonl_files(&dir);
    let mut seqs: Vec<u64> = files
        .iter()
        .flat_map(|f| {
            fs::read_to_string(f)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(parse_seq)
                .collect::<Vec<_>>()
        })
        .collect();
    seqs.sort_unstable();
    let max_seq_chunk1 = seqs[0];
    let min_seq_chunk2 = seqs[1];
    assert!(
        min_seq_chunk2 > max_seq_chunk1,
        "second chunk seqs must be > first: {} vs {}",
        min_seq_chunk2,
        max_seq_chunk1
    );
}

/// WalWriter resumes seq from max_seq found in existing files + 1.
#[test]
fn test_seq_resumes_from_existing_wal() {
    let dir = TempDir::new().unwrap();
    // Write a pre-existing WAL file containing seq=7.
    let existing = r#"{"seq":7,"ts":"2026-05-19T00:00:00.000000+00:00","db":"db","cypher":"MERGE (n:Entity {uuid: 'z'})","params":{}}"#;
    fs::write(
        dir.path().join("20260101_000000_aabbcc_0000.jsonl"),
        format!("{existing}\n"),
    )
    .unwrap();

    let mut w = open_writer(&dir, 50);
    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'new'})", json!({}), "db"))
        .unwrap();

    // Find the new file (the one with a different session_id).
    let all_files = jsonl_files(&dir);
    let new_file = all_files
        .iter()
        .find(|f| {
            f.file_name().unwrap().to_str().unwrap().starts_with("2026")
                && !f.file_name().unwrap().to_str().unwrap().contains("aabbcc")
        })
        .expect("new WAL file should exist");

    let content = fs::read_to_string(new_file).unwrap();
    let seq = parse_seq(content.lines().next().unwrap());
    assert_eq!(seq, 8, "should resume at max_seq+1 = 8");
}

/// Rotation: max_events_per_file = 2, two chunks of 2 lines each → 2 files.
#[test]
fn test_file_rotation_on_max_events() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 2);

    for _ in 0..2 {
        w.with_chunk(|w| {
            w.log_mutation("MERGE (n:Entity {uuid: 'a'})", json!({}), "db")?;
            w.log_mutation("MERGE (n:Entity {uuid: 'b'})", json!({}), "db")?;
            Ok(())
        })
        .unwrap();
    }

    let files = jsonl_files(&dir);
    assert_eq!(files.len(), 2, "expected 2 files after rotation");
}

/// Non-mutation Cypher (MATCH) must not appear in pending buffer.
#[test]
fn test_mutation_filter_excludes_reads() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    w.with_chunk(|w| w.log_mutation("MATCH (n) RETURN n", json!({}), "db"))
        .unwrap();

    assert!(
        jsonl_files(&dir).is_empty(),
        "MATCH must not produce a WAL file"
    );
}

/// Index DDL must not appear in pending buffer.
#[test]
fn test_mutation_filter_excludes_index_ddl() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    w.with_chunk(|w| {
        w.log_mutation(
            "CALL CREATE_VECTOR_INDEX('Entity', 'idx', 'embedding', metric := 'cosine')",
            json!({}),
            "db",
        )
    })
    .unwrap();

    assert!(
        jsonl_files(&dir).is_empty(),
        "CREATE_VECTOR_INDEX must not produce a WAL file"
    );
}

/// rotate() after writes returns (1, 1); second rotate returns (0, 1).
#[test]
fn test_rotate_after_writes() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'x'})", json!({}), "db"))
        .unwrap();

    let (rotated, total) = w.rotate();
    assert_eq!(rotated, 1, "one file should have been open and rotated");
    assert_eq!(total, 1, "one JSONL file exists after rotation");

    // Second rotate: no file open, but the existing file is still there.
    let (rotated2, total2) = w.rotate();
    assert_eq!(rotated2, 0, "no open file to rotate");
    assert_eq!(total2, 1, "still one JSONL file in directory");
}

/// rotate() with no prior writes returns (0, 0).
#[test]
fn test_rotate_no_prior_writes() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    let (rotated, total) = w.rotate();
    assert_eq!(rotated, 0, "nothing to rotate when no writes happened");
    assert_eq!(total, 0, "no files in empty WAL dir");
}

/// After rotate(), new writes open a fresh file (not appending to the old one).
#[test]
fn test_rotate_forces_new_file() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'a'})", json!({}), "db"))
        .unwrap();
    w.rotate();

    // Write again; should produce a second file.
    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'b'})", json!({}), "db"))
        .unwrap();

    let files = jsonl_files(&dir);
    assert_eq!(files.len(), 2, "rotate must force a new file on next write");
}

/// flush_pending self-heals when the WAL directory is deleted mid-process.
#[test]
fn test_flush_pending_recreates_deleted_wal_dir() {
    let dir = TempDir::new().unwrap();
    let wal_dir = dir.path().join("wal");
    let mut w = WalWriter::new(&wal_dir, 50).expect("WalWriter::new");

    // Write one chunk to establish at least one file.
    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'a'})", json!({}), "db"))
        .unwrap();
    assert!(
        wal_dir.exists(),
        "WAL directory should exist after first write"
    );
    let files_before = fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .count();
    assert_eq!(files_before, 1, "expected one file after first write");

    // Delete the WAL directory out from under the running writer.
    fs::remove_dir_all(&wal_dir).unwrap();
    assert!(!wal_dir.exists(), "WAL directory should be gone");

    // The next write must succeed and recreate the directory + file.
    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'b'})", json!({}), "db"))
        .expect("with_chunk must succeed after directory deletion");

    assert!(wal_dir.exists(), "WAL directory must be recreated");
    let files_after = fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .count();
    assert!(
        files_after >= 1,
        "at least one .jsonl file must exist after recovery"
    );
}

/// Filename must match the pattern YYYYMMDD_HHMMSS_<6hex>_0000.jsonl
#[test]
fn test_filename_format() {
    let dir = TempDir::new().unwrap();
    let mut w = open_writer(&dir, 50);

    w.with_chunk(|w| w.log_mutation("MERGE (n:Entity {uuid: 'x'})", json!({}), "db"))
        .unwrap();

    let files = jsonl_files(&dir);
    let name = files[0].file_name().unwrap().to_str().unwrap();
    let re = regex::Regex::new(r"^\d{8}_\d{6}_[0-9a-f]{6}_0000\.jsonl$").unwrap();
    assert!(
        re.is_match(name),
        "filename {name} does not match expected pattern"
    );
}
