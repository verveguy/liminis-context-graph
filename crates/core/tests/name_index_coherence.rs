// Coherence tests for the in-process NameIndex accelerator behind
// `Conn::get_entity_by_name_ci` (issue #219).
//
// Covers:
//   FR-005/FR-008 (User Story 2): the index stays coherent with the database across every
//     mutation path that can add, change, or remove an entity's name->uuid mapping.
//   SC-003/SC-004: a stale or incorrect index entry never produces a wrong entity — only a
//     correct result or a miss.

use std::fs;

use lcg_core::corrections::{apply_corrections_file, merge_entities, MergeEntitiesParams};
use lcg_core::{Db, EntityRow, WalReplayer};
use tempfile::TempDir;

const DIM: usize = 4;
const TS: &str = "2026-01-01T00:00:00Z";
const GROUP: &str = "liminis";

fn open_db(dir: &TempDir) -> Db {
    let db = Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
    }
    db
}

fn make_entity(uuid: &str, name: &str, created_at: &str) -> EntityRow {
    EntityRow {
        uuid: uuid.to_string(),
        name: name.to_string(),
        group_id: GROUP.to_string(),
        labels: vec!["Entity".to_string()],
        created_at: created_at.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("summary of {name}"),
        attributes: "{}".to_string(),
        ..Default::default()
    }
}

// ── FR-005: entity insert ───────────────────────────────────────────────────────

#[test]
fn insert_entity_is_immediately_lookupable_by_name() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("u1", "Alice", "2026-01-01 00:00:00"))
        .unwrap();

    let found = conn
        .get_entity_by_name_ci("  ALICE  ", GROUP)
        .unwrap()
        .expect("case/whitespace-insensitive lookup must find the inserted entity");
    assert_eq!(found.uuid, "u1");

    assert!(
        conn.get_entity_by_name_ci("Bob", GROUP).unwrap().is_none(),
        "a name that was never inserted must miss"
    );
}

// ── FR-005: merge_entities (canonical created_at reorder) ──────────────────────

#[test]
fn merge_entities_created_at_update_reorders_name_index_winner() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    // Two same-named entities. "zzz-alias" is created first (earlier created_at), so it starts
    // as the deterministic winner regardless of the uuid tiebreak. "aaa-canonical" is created
    // later but has the lexicographically smaller uuid — chosen deliberately so that once
    // merge_entities pulls its created_at back to match the alias's, the uuid ASC tiebreak
    // resolves in its favor, isolating the assertion to "did update_entity_created_at's
    // reorder land in the index" rather than "which uuid happens to sort first".
    conn.insert_entity(&make_entity("zzz-alias", "Brett", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(
        "aaa-canonical",
        "Brett",
        "2026-06-01 00:00:00",
    ))
    .unwrap();
    assert_eq!(
        conn.get_entity_by_name_ci("Brett", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "zzz-alias"
    );

    // Merge "zzz-alias" into "aaa-canonical". merge_entities pulls the canonical's created_at
    // back to the earliest across all merged entities, which changes the deterministic winner
    // for the "brett"/GROUP key without any insert/delete.
    let params = MergeEntitiesParams {
        canonical_uuid: Some("aaa-canonical".to_string()),
        alias_uuids: vec!["zzz-alias".to_string()],
        group_id: GROUP.to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed: {:?}", result.errors);

    // Both entities now share the same created_at; the uuid ASC tiebreak must resolve to the
    // canonical, proving the name index observed update_entity_created_at's reorder rather than
    // still ranking by the pre-merge created_at values.
    let found = conn
        .get_entity_by_name_ci("Brett", GROUP)
        .unwrap()
        .expect("Brett must still resolve after merge");
    assert_eq!(
        found.uuid, "aaa-canonical",
        "the name index must reflect update_entity_created_at, not the pre-merge winner"
    );
}

// ── FR-005: apply_corrections (same_as label-only mutation) ────────────────────

#[test]
fn apply_same_as_label_mutation_does_not_affect_name_lookups() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("canonical", "Robert", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity("alias", "Bob", "2026-01-02 00:00:00"))
        .unwrap();

    let liminis_dir = dir.path().join(".liminis");
    fs::create_dir_all(&liminis_dir).unwrap();
    fs::write(
        liminis_dir.join("knowledge-corrections.yaml"),
        r#"
corrections:
  - id: "c1"
    type: "same_as"
    canonical_uuid: "canonical"
    aliases: ["Bob"]
"#,
    )
    .unwrap();

    let result = apply_corrections_file(&conn, dir.path(), false);
    assert!(
        result.success,
        "apply_corrections should succeed: {:?}",
        result.errors
    );

    // apply_same_as only calls update_entity_labels (adds "Merged") — it never touches
    // name/group_id/created_at, so both name keys must still resolve to their original UUIDs.
    assert_eq!(
        conn.get_entity_by_name_ci("Robert", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "canonical"
    );
    assert_eq!(
        conn.get_entity_by_name_ci("Bob", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "alias",
        "labeling an alias as Merged must not remove it from the name index"
    );
}

// ── FR-005: remove_episode / remove_episodes_by_source (documented no-op) ──────

#[test]
fn removing_episodes_leaves_orphaned_entity_name_lookup_unaffected() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("e1", "Orphan", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_episodic(&lcg_core::types::EpisodicRow {
        uuid: "ep1".to_string(),
        name: "chunk-1".to_string(),
        group_id: GROUP.to_string(),
        created_at: "2026-01-01 00:00:00".to_string(),
        source: "test".to_string(),
        source_description: "src".to_string(),
        content: "content".to_string(),
        content_embedding: vec![1.0, 0.0, 0.0, 0.0],
        valid_at: "2026-01-01 00:00:00".to_string(),
        entity_edges: vec![],
    })
    .unwrap();

    conn.remove_episode("ep1").unwrap();

    // No code path deletes Entity nodes today (see remove_episode's doc comment) — the name
    // index has nothing to invalidate, and the entity must remain lookupable.
    assert!(conn
        .get_entity_by_name_ci("Orphan", GROUP)
        .unwrap()
        .is_some());
}

// ── User Story 2 / SC-004: stale index entry degrades to a miss, never a wrong result ──

#[test]
fn stale_index_entry_after_out_of_band_delete_degrades_to_miss() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("gone", "Ghost", "2026-01-01 00:00:00"))
        .unwrap();
    assert!(conn
        .get_entity_by_name_ci("Ghost", GROUP)
        .unwrap()
        .is_some());

    // No production path ever deletes an Entity node, but this directly exercises the
    // verify-on-hit contract (FR-006): force a stale index entry via a raw DB bypass (which,
    // correctly, does not invalidate the index) and confirm the lookup degrades to a miss
    // rather than returning the now-nonexistent UUID.
    conn.run_cypher("MATCH (e:Entity {uuid: 'gone'}) DETACH DELETE e")
        .unwrap();

    assert!(
        conn.get_entity_by_name_ci("Ghost", GROUP)
            .unwrap()
            .is_none(),
        "a stale index entry must never be returned as if it were a valid entity"
    );
}

// ── FR-004: fresh DB / schema init starts with an empty index ──────────────────

#[test]
fn fresh_db_has_no_entities_lookupable() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    assert!(conn
        .get_entity_by_name_ci("Anything", GROUP)
        .unwrap()
        .is_none());
}

// ── FR-004/FR-005: WAL replay bypasses typed mutation hooks ─────────────────────

#[test]
fn wal_replay_only_populates_name_index_after_explicit_rebuild() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let wal_dir = TempDir::new().unwrap();
    let line = r#"{"seq":0,"ts":"2026-05-19T03:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'replayed-1'}) ON CREATE SET n.name = 'Replayed', n.group_id = 'liminis', n.labels = ['Entity'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{}'","params":{}}"#;
    fs::write(
        wal_dir.path().join("20260519_030000_aaa111_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    WalReplayer::new(wal_dir.path()).replay(&conn).unwrap();
    // Confirm the row landed in the database...
    assert_eq!(conn.count_nodes("Entity").unwrap(), 1);
    // ...but the name index doesn't know about it yet, since replay executes raw Cypher
    // templates and never calls Conn::insert_entity.
    assert!(
        conn.get_entity_by_name_ci("Replayed", GROUP)
            .unwrap()
            .is_none(),
        "replay must not silently populate the name index"
    );

    conn.rebuild_name_index().unwrap();
    assert_eq!(
        conn.get_entity_by_name_ci("Replayed", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "replayed-1",
        "rebuild_name_index must close the replay-bypass gap"
    );
}

// ── FR-004: Db::open_or_rebuild (used by recovery-equivalent paths) rebuilds eagerly ──

#[test]
fn open_or_rebuild_populates_name_index_from_replayed_wal() {
    let db_dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let line = r#"{"seq":0,"ts":"2026-05-19T03:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'rebuilt-1'}) ON CREATE SET n.name = 'Rebuilt', n.group_id = 'liminis', n.labels = ['Entity'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{}'","params":{}}"#;
    fs::write(
        wal_dir.path().join("20260519_030000_aaa111_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let db = Db::open_or_rebuild(
        db_path.to_str().unwrap(),
        wal_dir.path().to_str().unwrap(),
        DIM,
    )
    .unwrap();
    let conn = db.connect().unwrap();

    assert_eq!(
        conn.get_entity_by_name_ci("Rebuilt", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "rebuilt-1",
        "open_or_rebuild must populate the name index from the replayed WAL, not just the DB"
    );
}
