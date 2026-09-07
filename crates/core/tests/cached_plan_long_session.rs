// User Story 6 / FR-009 (issue #561): ladybug#883 (an open, unfixed SIGSEGV in the
// cached-prepared-statement path) needs "hundreds of parameterized queries in one session" to
// surface, per upstream reports; the existing suite's short-lived sessions never reach that. This
// test issues an order of magnitude more executions than the ~20-iteration deadlock retest cited
// in the issue's Background, against one open session, and confirms no crash, hang, or stale
// result -- risk-characterization evidence for a residual, still-open upstream issue this bump
// does not fix (Out of Scope), not a substitute for a real upstream fix.

use lcg_core::db::Db;
use lcg_core::types::EntityRow;

const ITERATIONS: usize = 1_200;

#[test]
fn thousand_plus_reexecutions_in_one_session_stay_correct_and_dont_crash() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
    let conn = db.connect().unwrap();
    conn.init_schema(4).unwrap();

    conn.insert_entity(&EntityRow {
        uuid: "long-session-entity".to_string(),
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

    // Alternates a write (through a distinct query template) with a re-execution of the same
    // fixed parameterized read template (get_entity_by_uuid's internal Cypher), well past the
    // ~20-iteration scale the upstream deadlock retest used and past the "hundreds" upstream
    // reports say ladybug#883 needs to surface.
    for i in 0..ITERATIONS {
        let attrs = format!(r#"{{"iteration":{i}}}"#);
        conn.update_entity_attributes("long-session-entity", &attrs)
            .unwrap();
        let row = conn
            .get_entity_by_uuid("long-session-entity")
            .unwrap()
            .unwrap_or_else(|| panic!("entity missing at iteration {i}"));
        assert_eq!(
            row.attributes, attrs,
            "stale result at iteration {i} of {ITERATIONS}"
        );
    }

    // A final read after the loop, using the same cached plan one more time, confirms the
    // session is still healthy and correct at the end of the run.
    let final_row = conn
        .get_entity_by_uuid("long-session-entity")
        .unwrap()
        .expect("entity should still be readable after the long session");
    assert_eq!(
        final_row.attributes,
        format!(r#"{{"iteration":{}}}"#, ITERATIONS - 1)
    );
}
