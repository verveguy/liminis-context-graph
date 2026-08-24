//! Migration-path coverage for the #144 MENTIONS schema-parity fix.
//!
//! The PR's other tests run against a fresh `init_schema` (the CREATE path). This test exercises
//! the *other* branch — `schema::migrate`'s probe-then-`ALTER TABLE MENTIONS ADD …` on an
//! existing pre-#144 database — which is the part that runs on real upgrades and was previously
//! uncovered. (Rel-table `ALTER ADD` is novel here; node-table ALTER was the only prior form.)

use lcg_core::{schema, Db};
use tempfile::TempDir;

/// Simulates a pre-#144 DB (MENTIONS has only `group_id`, with an existing uuid-less edge), runs
/// the real `schema::migrate`, and asserts: the `uuid`/`created_at` columns are added, the WAL's
/// MENTIONS MERGE round-trips, the pre-existing edge survives, and a second migrate is a no-op.
#[test]
fn migrate_adds_mentions_uuid_and_created_at_on_existing_db() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();

    // Pre-#144 schema. RelatesToNode_ carries the columns `migrate()` probes for, so only
    // MENTIONS needs migrating (keeps the test focused and noise-free).
    conn.run_cypher("CREATE NODE TABLE Episodic (uuid STRING PRIMARY KEY, name STRING)")
        .unwrap();
    conn.run_cypher("CREATE NODE TABLE Entity (uuid STRING PRIMARY KEY, name STRING)")
        .unwrap();
    conn.run_cypher(
        "CREATE NODE TABLE RelatesToNode_ (uuid STRING PRIMARY KEY, relation_type STRING, \
         episodes STRING[], expired_at TIMESTAMP)",
    )
    .unwrap();
    conn.run_cypher("CREATE REL TABLE MENTIONS (FROM Episodic TO Entity, group_id STRING)")
        .unwrap();
    conn.run_cypher("CREATE (:Episodic {uuid:'ep1', name:'e'})")
        .unwrap();
    conn.run_cypher("CREATE (:Entity {uuid:'en1', name:'x'})")
        .unwrap();
    conn.run_cypher(
        "MATCH (s:Episodic {uuid:'ep1'}), (d:Entity {uuid:'en1'}) \
         CREATE (s)-[:MENTIONS {group_id:'g'}]->(d)",
    )
    .unwrap();

    // Precondition: MENTIONS.uuid is absent (binder error on the probe).
    assert!(
        conn.run_cypher("MATCH ()-[r:MENTIONS]->() RETURN r.uuid LIMIT 0")
            .is_err(),
        "precondition: MENTIONS.uuid must be absent before migrate"
    );

    schema::migrate(&conn, 4);

    // The columns now bind (probe succeeds for both).
    conn.run_cypher("MATCH ()-[r:MENTIONS]->() RETURN r.uuid, r.created_at LIMIT 0")
        .expect("MENTIONS.uuid/created_at must bind after migrate");

    // The WAL's MENTIONS MERGE (sets r.uuid + r.created_at) must now execute.
    conn.run_cypher(
        "MATCH (s:Episodic {uuid:'ep1'}) MATCH (d:Entity {uuid:'en1'}) \
         MERGE (s)-[r:MENTIONS {uuid:'m1'}]->(d) \
         SET r.created_at = timestamp('2026-03-25T16:58:57+00:00')",
    )
    .expect("MENTIONS MERGE with uuid/created_at must execute after migrate");

    let new_edge = conn
        .cypher_query("MATCH ()-[r:MENTIONS]->() WHERE r.uuid = 'm1' RETURN r.uuid")
        .unwrap();
    assert!(
        new_edge
            .iter()
            .any(|row| row.first().map(|c| c == "m1").unwrap_or(false)),
        "new MENTIONS edge uuid must round-trip"
    );

    // The pre-existing uuid-less edge is preserved across the ALTER (its uuid is NULL).
    let count = conn
        .cypher_query("MATCH ()-[r:MENTIONS]->() RETURN count(r)")
        .unwrap();
    assert_eq!(
        count[0][0], "2",
        "the pre-existing uuid-less mention must survive the ALTER"
    );

    // Idempotent: a second migrate is a clean no-op (columns already present → no re-ALTER).
    schema::migrate(&conn, 4);
    conn.run_cypher("MATCH ()-[r:MENTIONS]->() RETURN r.uuid, r.created_at LIMIT 0")
        .expect("columns still present after a second migrate");
}

/// Simulates a pre-#470 DB (Entity has no `summary_embedding` column, with an existing row),
/// runs the real `schema::migrate`, and asserts: the column is added, the pre-existing row is
/// zero-filled (not left NULL) so a later `CREATE_VECTOR_INDEX` never has to tolerate NULLs, and
/// a second migrate is a no-op that leaves the value untouched.
#[test]
fn migrate_adds_and_zero_fills_entity_summary_embedding_on_existing_db() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();

    // Pre-#470 Entity schema: no summary_embedding column.
    conn.run_cypher("CREATE NODE TABLE Entity (uuid STRING PRIMARY KEY, name STRING)")
        .unwrap();
    conn.run_cypher("CREATE (:Entity {uuid:'en1', name:'x'})")
        .unwrap();

    // Precondition: Entity.summary_embedding is absent (binder error on the probe).
    assert!(
        conn.run_cypher("MATCH (n:Entity) RETURN n.summary_embedding LIMIT 0")
            .is_err(),
        "precondition: Entity.summary_embedding must be absent before migrate"
    );

    schema::migrate(&conn, 4);

    // The column now binds.
    conn.run_cypher("MATCH (n:Entity) RETURN n.summary_embedding LIMIT 0")
        .expect("Entity.summary_embedding must bind after migrate");

    // The pre-existing row is zero-filled, not left NULL.
    let rows = conn
        .cypher_query("MATCH (n:Entity {uuid:'en1'}) RETURN n.summary_embedding")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0][0].is_empty(),
        "pre-existing row's summary_embedding must be zero-filled, not NULL; got: {:?}",
        rows[0][0]
    );

    // Idempotent: a second migrate is a clean no-op and does not disturb the value.
    schema::migrate(&conn, 4);
    let rows_again = conn
        .cypher_query("MATCH (n:Entity {uuid:'en1'}) RETURN n.summary_embedding")
        .unwrap();
    assert_eq!(
        rows[0][0], rows_again[0][0],
        "a second migrate must not change an already-migrated row's summary_embedding"
    );
}

/// Simulates a pre-#221 DB (`Entity` has no `lookup_key` column, with existing rows), runs the
/// real `schema::migrate`, and asserts: the column is added, every pre-existing row is
/// backfilled with the correct composite key (not left NULL), `get_entity_by_name_ci` resolves
/// via the backfilled column, and a second migrate is a no-op that leaves values untouched
/// (User Story 2, FR-005, SC-004).
#[test]
fn migrate_adds_and_backfills_entity_lookup_key_on_existing_db() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();

    // Pre-#221 Entity schema: every column `get_entity_by_name_ci` and the backfill query need
    // except `lookup_key` itself, so this test isolates the lookup_key migration specifically.
    conn.run_cypher(
        "CREATE NODE TABLE Entity (uuid STRING PRIMARY KEY, name STRING, group_id STRING, \
         labels STRING[], created_at TIMESTAMP, name_embedding FLOAT[4], summary STRING, \
         attributes STRING)",
    )
    .unwrap();
    conn.run_cypher(
        "CREATE (:Entity {uuid:'en1', name:'Alice', group_id:'g1', labels:['Entity'], \
         created_at: timestamp('2026-01-01 00:00:00'), name_embedding: [1.0, 0.0, 0.0, 0.0], \
         summary:'s', attributes:'{}'})",
    )
    .unwrap();

    // Precondition: Entity.lookup_key is absent (binder error on the probe).
    assert!(
        conn.run_cypher("MATCH (n:Entity) RETURN n.lookup_key LIMIT 0")
            .is_err(),
        "precondition: Entity.lookup_key must be absent before migrate"
    );

    schema::migrate(&conn, 4);

    // The column now binds.
    conn.run_cypher("MATCH (n:Entity) RETURN n.lookup_key LIMIT 0")
        .expect("Entity.lookup_key must bind after migrate");

    // The pre-existing row is backfilled with the correct composite key, not left NULL.
    let rows = conn
        .cypher_query("MATCH (n:Entity {uuid:'en1'}) RETURN n.lookup_key")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0], "g1\u{1f}alice",
        "pre-existing row's lookup_key must be backfilled from its group_id/name, not left NULL"
    );

    // Subsequent lookups use the backfilled column.
    assert_eq!(
        conn.get_entity_by_name_ci("Alice", "g1")
            .unwrap()
            .unwrap()
            .uuid,
        "en1",
        "get_entity_by_name_ci must resolve via the freshly backfilled lookup_key"
    );

    // Idempotent: a second migrate is a clean no-op and does not disturb the value.
    schema::migrate(&conn, 4);
    let rows_again = conn
        .cypher_query("MATCH (n:Entity {uuid:'en1'}) RETURN n.lookup_key")
        .unwrap();
    assert_eq!(
        rows[0][0], rows_again[0][0],
        "a second migrate must not change an already-migrated row's lookup_key"
    );

    // FR-002/SC-001, verified specifically for the migrated (not fresh-schema) path: the
    // build_indices_and_constraints step a real upgrade runs after migrate() must produce an
    // ART-indexed access path here too, not just on a DB created post-#221.
    conn.create_entity_lookup_key_index().unwrap();
    let plan = conn
        .cypher_query("EXPLAIN MATCH (e:Entity) WHERE e.lookup_key = 'g1\u{1f}alice' RETURN e.uuid")
        .unwrap();
    let plan_text = plan.into_iter().flatten().collect::<Vec<_>>().join("\n");
    assert!(
        plan_text.contains("ART"),
        "the migrated schema's lookup_key column must be served by the ART index, got: {plan_text}"
    );
}
