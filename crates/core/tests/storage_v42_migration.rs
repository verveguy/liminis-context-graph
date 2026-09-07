// User Story 4 / FR-006 (issue #561): a pre-existing database at storage version 42 (created
// under lbug 0.18.1, storage version 42 -- what v0.14.0 and v0.14.1 both shipped) must open
// under the current lbug-0.20.2-pinned binary, migrate in place (42 -> 47), and serve correct
// reads, with no manual operator step.
//
// The fixture (crates/core/tests/fixtures/storage_v42_db/t.db.tar.gz) was generated once by
// opening a fresh Db at this repo's then-current HEAD (already pinned to lbug 0.18.1, before the
// pins in this same commit series moved to 0.20.2) and writing two Entity nodes plus one
// RELATES_TO edge with known, hardcoded values via the engine's own public API, then copying the
// resulting `t.db` out -- no historical checkout was needed this time, since HEAD already wrote
// storage version 42. See storage_v41_migration.rs for the structurally identical precedent.

use std::path::Path;

use lcg_core::db::Db;

fn extract_fixture(dest_dir: &Path) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let archive = Path::new(manifest_dir).join("tests/fixtures/storage_v42_db/t.db.tar.gz");
    assert!(
        archive.exists(),
        "storage-v42 fixture archive not found at {}",
        archive.display()
    );

    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .expect("failed to invoke tar to extract the storage-v42 fixture");
    assert!(
        status.success(),
        "tar extraction of the storage-v42 fixture failed"
    );
}

#[test]
fn storage_v42_database_opens_and_migrates_with_correct_reads() {
    let dir = tempfile::TempDir::new().unwrap();
    extract_fixture(dir.path());
    let db_path = dir.path().join("t.db");
    assert!(
        db_path.exists(),
        "expected t.db to be present after extracting the storage-v42 fixture"
    );

    // Opening a storage-v42 database under the current (0.20.2-pinned) binary must succeed with
    // no manual operator step. The on-disk rewrite to the current storage version happens at the
    // first checkpoint (see CHANGELOG.md), not necessarily at open -- this test doesn't force a
    // checkpoint or assert the storage version, only that open is automatic and reads are correct.
    let db = Db::open(db_path.to_str().unwrap()).expect(
        "storage-v42 database failed to open under the current lbug pin; \
         opening it (the first step of in-place migration) is expected to be automatic",
    );
    let conn = db.connect().unwrap();

    let alice = conn
        .get_entity_by_uuid("v42-fixture-entity-alice")
        .unwrap()
        .expect("Alice entity should be readable after migration");
    assert_eq!(alice.name, "Alice");
    assert_eq!(alice.group_id, "v42-fixture-group");
    assert_eq!(
        alice.summary,
        "Alice is a fixture entity for storage-v42 migration testing."
    );

    let bob = conn
        .get_entity_by_uuid("v42-fixture-entity-bob")
        .unwrap()
        .expect("Bob entity should be readable after migration");
    assert_eq!(bob.name, "Bob");

    let (edges, neighbor_uuids) = conn
        .get_entity_neighbors("v42-fixture-entity-alice", None, 10)
        .unwrap();
    assert_eq!(
        edges.len(),
        1,
        "expected exactly one RELATES_TO edge from Alice"
    );
    let edge = &edges[0];
    assert_eq!(edge.uuid, "v42-fixture-edge-alice-knows-bob");
    assert_eq!(edge.fact, "Alice knows Bob.");
    assert_eq!(edge.source_node_uuid, "v42-fixture-entity-alice");
    assert_eq!(edge.target_node_uuid, "v42-fixture-entity-bob");
    assert_eq!(neighbor_uuids, vec!["v42-fixture-entity-bob".to_string()]);
}
