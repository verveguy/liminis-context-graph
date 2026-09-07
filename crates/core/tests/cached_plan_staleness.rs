// User Story 3 / FR-008 (issue #561): re-executing the same parameterized query within one
// session, after an intervening write that changes the result, must return fresh, non-stale
// data. This exercises the cached-plan fast path directly -- ladybug#877, fixed by #878 and
// shipped in lbug 0.20.2 -- rather than relying on the full suite's incidental repeated calls to
// demonstrate the fix.
//
// `db.rs`'s `query_params`/`exec_params` are `pub(crate)`, so this integration test (a separate
// compilation unit outside the crate) exercises the cached-plan path through public API instead:
// `get_entity_by_uuid` issues a fixed parameterized Cypher template internally, and re-invoking
// it on the same `Conn` after an intervening write re-executes that identical template -- the
// normal calling pattern `crates/core/src/db.rs`'s doc comments describe as the primary
// motivation for this upgrade.

use lcg_core::db::Db;
use lcg_core::types::EntityRow;

#[test]
fn reexecuted_parameterized_query_reflects_intervening_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    conn.insert_entity(&EntityRow {
        uuid: "cached-plan-entity".to_string(),
        name: "Original".to_string(),
        group_id: "g".to_string(),
        labels: vec![],
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: vec![0.0f32; 4],
        summary: "before write".to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
    })
    .unwrap();

    // First execution: plans and caches "MATCH (e:Entity {uuid: $uuid}) RETURN ..." internally.
    let before = conn
        .get_entity_by_uuid("cached-plan-entity")
        .unwrap()
        .expect("entity should exist before the write");
    assert_eq!(before.attributes, "{}");

    // Intervening write via a different query template.
    conn.update_entity_attributes("cached-plan-entity", r#"{"updated":true}"#)
        .unwrap();

    // Second execution: identical Cypher text/params as the first call -- this is the exact
    // re-execution shape ladybug#877's cached-plan fast path returned stale rows for.
    let after = conn
        .get_entity_by_uuid("cached-plan-entity")
        .unwrap()
        .expect("entity should still exist after the write");
    assert_eq!(
        after.attributes, r#"{"updated":true}"#,
        "re-executed parameterized query returned a stale (pre-write) result"
    );
}

#[test]
fn many_reexecutions_all_reflect_the_latest_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    conn.insert_entity(&EntityRow {
        uuid: "cached-plan-entity-2".to_string(),
        name: "N".to_string(),
        group_id: "g".to_string(),
        labels: vec![],
        created_at: "2026-01-01 00:00:00".to_string(),
        name_embedding: vec![0.0f32; 4],
        summary: "s".to_string(),
        attributes: "{}".to_string(),
        ..Default::default()
    })
    .unwrap();

    for i in 0..20 {
        let attrs = format!(r#"{{"iteration":{i}}}"#);
        conn.update_entity_attributes("cached-plan-entity-2", &attrs)
            .unwrap();
        let row = conn
            .get_entity_by_uuid("cached-plan-entity-2")
            .unwrap()
            .expect("entity should exist across every iteration");
        assert_eq!(
            row.attributes, attrs,
            "iteration {i}: re-executed query returned a stale result"
        );
    }
}

/// Background item 4 / issue #561: confirms `enable_cached_prepared_statement` (present in the
/// 0.20.2 bundle, absent in 0.20.1) is a settable runtime pragma, giving operators an escape
/// hatch for ladybug#883 if it's ever hit in production. Out of Scope: this issue does not enable
/// it by default, so this only proves the lever exists and works -- it does not change
/// `Db::open`'s `SystemConfig::default()`.
#[test]
fn enable_cached_prepared_statement_setting_exists_and_is_settable() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();

    conn.run_cypher("CALL enable_cached_prepared_statement='NONE'")
        .expect("enable_cached_prepared_statement should be a recognized, settable pragma");
}
