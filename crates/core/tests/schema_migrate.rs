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

    schema::migrate(&conn);

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
    schema::migrate(&conn);
    conn.run_cypher("MATCH ()-[r:MENTIONS]->() RETURN r.uuid, r.created_at LIMIT 0")
        .expect("columns still present after a second migrate");
}
