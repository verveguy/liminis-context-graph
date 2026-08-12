/// Integration tests for knowledge_merge_entities — collapse duplicate entities.
///
/// Covers SC-001 through SC-006 and the user stories in the spec.
use lcg_core::{
    corrections::{apply_corrections_file, merge_entities, MergeEntitiesParams},
    pointer::read_merged_into,
    Db, EntityRow, RelatesToEdge, WalReplayer, WalWriter,
};
use tempfile::TempDir;
use uuid::Uuid;

const DIM: usize = 4;
const TS: &str = "2026-01-01T00:00:00Z";

// ── helpers ───────────────────────────────────────────────────────────────────

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
        group_id: "liminis".to_string(),
        labels: vec!["Entity".to_string()],
        created_at: created_at.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("summary of {name}"),
        attributes: "{}".to_string(),
        ..Default::default()
    }
}

fn make_edge(src: &str, dst: &str, name: &str, created_at: &str) -> RelatesToEdge {
    make_edge_in_group(src, dst, name, created_at, "liminis")
}

fn make_edge_in_group(
    src: &str,
    dst: &str,
    name: &str,
    created_at: &str,
    group_id: &str,
) -> RelatesToEdge {
    RelatesToEdge {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        source_node_uuid: src.to_string(),
        target_node_uuid: dst.to_string(),
        group_id: group_id.to_string(),
        fact: format!("{src} {name} {dst}"),
        fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
        created_at: created_at.to_string(),
        valid_at: None,
        invalid_at: None,
        attributes: "{}".to_string(),
        relation_type: None,
        episode_uuids: vec![],
        source_descriptions: vec![],
    }
}

/// Returns the number of entities with the given name that are NOT merged.
fn count_active_entities_named(db: &Db, name: &str) -> usize {
    let conn = db.connect().unwrap();
    conn.get_entities_by_name_all(name, "liminis")
        .unwrap()
        .into_iter()
        .filter(|e| !e.labels.contains(&"Merged".to_string()))
        .count()
}

// ── Test 1: merge all identical-name entities ─────────────────────────────────

/// SC-001, SC-003: 5 entities named "Brett", each with distinct edges.
/// After merge: 1 active "Brett", 4 marked merged, all edges on canonical.
#[test]
fn test_merge_by_name_all_identical() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    // Seed 5 "Brett" entities, each connected to a distinct other node
    let other_uuid = "other-001";
    conn.insert_entity(&make_entity(other_uuid, "Other", "2026-01-01 00:00:00"))
        .unwrap();

    let brett_uuids: Vec<String> = (1..=5).map(|i| format!("brett-{i:03}")).collect();
    for (i, uuid) in brett_uuids.iter().enumerate() {
        conn.insert_entity(&make_entity(
            uuid,
            "Brett",
            &format!("2026-01-01 00:0{}:00", i),
        ))
        .unwrap();
        // Each Brett has a distinct outgoing edge to Other
        conn.insert_relates_to_edge(&make_edge(
            uuid,
            other_uuid,
            &format!("knows_{i}"),
            "2026-01-01 00:00:00",
        ))
        .unwrap();
    }

    let params = MergeEntitiesParams {
        canonical_name: Some("Brett".to_string()),
        merge_all_by_name: true,
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "merge should succeed");
    assert_eq!(result.merged_count, 4, "4 aliases should be merged");
    assert_eq!(result.skipped, 0);
    assert_eq!(result.errors, Vec::<String>::new());

    // Exactly 1 active "Brett" remains
    assert_eq!(count_active_entities_named(&db, "Brett"), 1);

    // Canonical has edges (at least 4 — one per alias, plus potential dedup)
    let canonical = conn
        .get_entities_by_name_all("Brett", "liminis")
        .unwrap()
        .into_iter()
        .find(|e| !e.labels.contains(&"Merged".to_string()))
        .expect("one active Brett must remain");
    let edges = conn.get_full_edges_for_entity(&canonical.uuid).unwrap();
    let active_edges: Vec<_> = edges.iter().filter(|e| e.invalid_at.is_none()).collect();
    assert!(
        active_edges.len() >= 4,
        "canonical should have at least 4 active edges, got {}",
        active_edges.len()
    );
}

// ── Test 2: merge by explicit UUID set ───────────────────────────────────────

/// User Story 2: merge 2 aliases into 1 canonical via explicit alias_uuids.
#[test]
fn test_merge_by_uuid_explicit() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let canonical_uuid = "canonical-001";
    let alias1_uuid = "alias-001";
    let alias2_uuid = "alias-002";
    let other_uuid = "other-001";

    conn.insert_entity(&make_entity(
        canonical_uuid,
        "Brett Adam",
        "2026-01-01 00:00:00",
    ))
    .unwrap();
    conn.insert_entity(&make_entity(
        alias1_uuid,
        "Brett Adam",
        "2026-01-01 00:01:00",
    ))
    .unwrap();
    conn.insert_entity(&make_entity(
        alias2_uuid,
        "Brett Adam",
        "2026-01-01 00:02:00",
    ))
    .unwrap();
    conn.insert_entity(&make_entity(other_uuid, "Other", "2026-01-01 00:00:00"))
        .unwrap();

    // alias1 has an outgoing edge to Other
    conn.insert_relates_to_edge(&make_edge(
        alias1_uuid,
        other_uuid,
        "knows",
        "2026-01-01 00:00:00",
    ))
    .unwrap();
    // alias2 has an incoming edge from Other
    conn.insert_relates_to_edge(&make_edge(
        other_uuid,
        alias2_uuid,
        "likes",
        "2026-01-01 00:00:00",
    ))
    .unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(canonical_uuid.to_string()),
        alias_uuids: vec![alias1_uuid.to_string(), alias2_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(result.merged_count, 2);
    assert_eq!(result.canonical_uuid, canonical_uuid);

    // Aliases are marked merged
    let alias1 = conn.get_entity_by_uuid(alias1_uuid).unwrap().unwrap();
    let alias2 = conn.get_entity_by_uuid(alias2_uuid).unwrap().unwrap();
    assert!(
        alias1.labels.contains(&"Merged".to_string()),
        "alias1 should be Merged"
    );
    assert!(
        alias2.labels.contains(&"Merged".to_string()),
        "alias2 should be Merged"
    );

    // Canonical has both edges rewritten to it
    let canonical_edges = conn.get_full_edges_for_entity(canonical_uuid).unwrap();
    let active: Vec<_> = canonical_edges
        .iter()
        .filter(|e| e.invalid_at.is_none())
        .collect();
    assert_eq!(active.len(), 2, "canonical should have 2 active edges");
}

// ── Test 3: dry run ───────────────────────────────────────────────────────────

/// SC-004, User Story 3: dry_run=true must not mutate anything.
#[test]
fn test_dry_run_no_mutations() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let other_uuid = "other-001";
    conn.insert_entity(&make_entity(other_uuid, "Other", "2026-01-01 00:00:00"))
        .unwrap();
    for i in 1..=3 {
        let uuid = format!("brett-{i:03}");
        conn.insert_entity(&make_entity(
            &uuid,
            "Brett",
            &format!("2026-01-01 00:0{i}:00"),
        ))
        .unwrap();
        conn.insert_relates_to_edge(&make_edge(
            &uuid,
            other_uuid,
            "knows",
            "2026-01-01 00:00:00",
        ))
        .unwrap();
    }

    // Clear any pending mutations from seeding
    conn.drain_mutations();

    let params = MergeEntitiesParams {
        canonical_name: Some("Brett".to_string()),
        merge_all_by_name: true,
        group_id: "liminis".to_string(),
        dry_run: true,
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "dry_run should succeed");
    assert_eq!(result.merged_count, 2, "should report 2 merged");

    // Plan must be present with aliases
    let plan = result.plan.expect("plan must be present on dry_run");
    assert_eq!(plan.aliases.len(), 2, "plan must list 2 aliases");
    // Each alias's edges are accounted for (rewritten or deduped)
    for alias_info in &plan.aliases {
        let total = alias_info.active_edges + alias_info.duplicate_edges;
        assert_eq!(total, 1, "each alias has exactly 1 edge to account for");
    }

    // No mutations were captured (dry_run must not call exec_params)
    let mutations = conn.drain_mutations();
    assert!(
        mutations.is_empty(),
        "dry_run must not produce any mutations, got {}",
        mutations.len()
    );

    // Entity and edge counts unchanged
    assert_eq!(count_active_entities_named(&db, "Brett"), 3);
}

// ── Test 4: idempotent re-run ─────────────────────────────────────────────────

/// SC-006: second call on same merge returns merged_count=0, skipped=N-1.
#[test]
fn test_idempotent_rerun() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let other_uuid = "other-001";
    conn.insert_entity(&make_entity(other_uuid, "Other", "2026-01-01 00:00:00"))
        .unwrap();
    for i in 1..=3 {
        let uuid = format!("brett-{i:03}");
        conn.insert_entity(&make_entity(
            &uuid,
            "Brett",
            &format!("2026-01-01 00:0{i}:00"),
        ))
        .unwrap();
    }

    let params = MergeEntitiesParams {
        canonical_name: Some("Brett".to_string()),
        merge_all_by_name: true,
        group_id: "liminis".to_string(),
        ..Default::default()
    };

    let first = merge_entities(&conn, &params, TS);
    assert!(first.success, "first merge should succeed");
    assert_eq!(first.merged_count, 2);

    let second = merge_entities(&conn, &params, TS);
    assert!(second.success, "second merge should succeed (idempotent)");
    assert_eq!(second.merged_count, 0, "no new merges on second call");
    assert_eq!(
        second.skipped, 2,
        "already-merged aliases should be skipped"
    );
}

// ── Test 5: self-UUID in alias list ──────────────────────────────────────────

/// SC-005: canonical UUID appearing in alias_uuids is silently skipped.
#[test]
fn test_self_uuid_in_alias_list() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let canonical_uuid = "canonical-001";
    let alias_uuid = "alias-001";
    let other_uuid = "other-001";

    conn.insert_entity(&make_entity(canonical_uuid, "Brett", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(
        alias_uuid,
        "BrettAlias",
        "2026-01-01 00:01:00",
    ))
    .unwrap();
    conn.insert_entity(&make_entity(other_uuid, "Other", "2026-01-01 00:00:00"))
        .unwrap();

    // Edge between canonical and alias — would create self-loop after merge
    conn.insert_relates_to_edge(&make_edge(
        canonical_uuid,
        alias_uuid,
        "connected_to",
        "2026-01-01 00:00:00",
    ))
    .unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(canonical_uuid.to_string()),
        // Include canonical UUID in alias list — must be silently skipped
        alias_uuids: vec![canonical_uuid.to_string(), alias_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "should succeed: {:?}", result.errors);
    // Self-UUID is skipped; alias_uuid should be merged
    assert_eq!(result.merged_count, 1, "only alias_uuid should be merged");

    // No self-loop on canonical
    let canonical_edges = conn.get_full_edges_for_entity(canonical_uuid).unwrap();
    for edge in &canonical_edges {
        assert_ne!(
            edge.source_node_uuid, edge.target_node_uuid,
            "self-loop edge found: {}",
            edge.uuid
        );
    }
}

// ── Test 6: WAL replay reproduces merged state ────────────────────────────────

/// SC-002: WAL dump + full replay from scratch reproduces post-merge entity counts.
#[test]
fn test_wal_replay_reproduces_merged_state() {
    let db_dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();

    let db = open_db(&db_dir);
    let conn = db.connect().unwrap();

    // Seed 3 same-name entities
    for i in 1..=3 {
        conn.insert_entity(&make_entity(
            &format!("brett-{i:03}"),
            "Brett",
            &format!("2026-01-01 00:0{i}:00"),
        ))
        .unwrap();
    }

    // Capture seed mutations → WAL
    let seed_mutations = conn.drain_mutations();
    let mut wal = WalWriter::new(wal_dir.path(), 10_000, 0).unwrap();
    wal.with_chunk(|w| {
        for (cypher, params) in &seed_mutations {
            let p = if params.is_null() {
                serde_json::json!({})
            } else {
                params.clone()
            };
            w.log_mutation(cypher, p, "")?;
        }
        Ok(())
    })
    .unwrap();

    // Run merge and capture those mutations too
    let params = MergeEntitiesParams {
        canonical_name: Some("Brett".to_string()),
        merge_all_by_name: true,
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed");
    assert_eq!(result.merged_count, 2);

    let merge_mutations = conn.drain_mutations();
    assert!(
        !merge_mutations.is_empty(),
        "merge must emit WAL mutations (FR-014)"
    );
    for (cypher, params_v) in &merge_mutations {
        let p = if params_v.is_null() {
            serde_json::json!({})
        } else {
            params_v.clone()
        };
        wal.with_chunk(|w| w.log_mutation(cypher, p, "")).unwrap();
    }
    drop(wal);

    // Count post-merge state in original DB
    let post_merge_active = count_active_entities_named(&db, "Brett");
    assert_eq!(
        post_merge_active, 1,
        "original DB should have 1 active Brett"
    );

    // Replay WAL on fresh DB
    let db2_dir = TempDir::new().unwrap();
    let db2 = Db::open(db2_dir.path().join("replay.db").to_str().unwrap()).unwrap();
    {
        let conn2 = db2.connect().unwrap();
        conn2.init_schema(DIM).unwrap();
        let stats = WalReplayer::new(wal_dir.path()).replay(&conn2).unwrap();
        assert!(
            stats.lines_replayed > 0,
            "WAL replay must process some lines"
        );
    }

    // Count active Bretts in replayed DB
    let replayed_active = count_active_entities_named(&db2, "Brett");
    assert_eq!(
        replayed_active, post_merge_active,
        "replayed DB must have same active Brett count as original"
    );
}

// ── Test 7: canonical already merged ─────────────────────────────────────────

/// FR-017 / edge case: canonical entity is marked "Merged" → success: false.
#[test]
fn test_canonical_already_merged_error() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let canonical_uuid = "canonical-001";
    let alias_uuid = "alias-001";

    let mut canonical = make_entity(canonical_uuid, "Brett", "2026-01-01 00:00:00");
    canonical.labels.push("Merged".to_string());
    conn.insert_entity(&canonical).unwrap();
    conn.insert_entity(&make_entity(alias_uuid, "Brett", "2026-01-01 00:01:00"))
        .unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(canonical_uuid.to_string()),
        alias_uuids: vec![alias_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(
        !result.success,
        "should fail when canonical is already merged"
    );
    assert!(
        result.errors.iter().any(|e| e.contains("already merged")),
        "error should mention 'already merged', got: {:?}",
        result.errors
    );
}

// ── Test 7b: UUID-specified canonical/alias must belong to the declared merge group ───

/// A canonical or alias resolved by explicit UUID is not otherwise scoped to `group_id` (unlike
/// the by-name paths, which already query within group_id) — a caller could name an entity
/// belonging to a different group's WAL stream. Reject rather than tombstone/write merged_into
/// onto a foreign-group entity (issue #371's core invariant applied at the entity level).
#[test]
fn test_canonical_uuid_foreign_group_rejected() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let mut foreign_canonical =
        make_entity("foreign-canonical-001", "Brett", "2026-01-01 00:00:00");
    foreign_canonical.group_id = "group-F".to_string();
    conn.insert_entity(&foreign_canonical).unwrap();
    conn.insert_entity(&make_entity("alias-001", "Brett", "2026-01-01 00:01:00"))
        .unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some("foreign-canonical-001".to_string()),
        alias_uuids: vec!["alias-001".to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(
        !result.success,
        "must reject a canonical belonging to a foreign group"
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("foreign to the requested merge group")),
        "error should explain the foreign-group rejection, got: {:?}",
        result.errors
    );
    assert_eq!(result.merged_count, 0, "nothing should be merged");

    // The foreign canonical must not have been touched.
    let untouched = conn
        .get_entity_by_uuid("foreign-canonical-001")
        .unwrap()
        .unwrap();
    assert!(
        !untouched.labels.contains(&"Merged".to_string()),
        "foreign canonical must not be tombstoned"
    );
}

#[test]
fn test_alias_uuid_foreign_group_rejected() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity(
        "canonical-001",
        "Brett",
        "2026-01-01 00:00:00",
    ))
    .unwrap();
    let mut foreign_alias = make_entity("foreign-alias-001", "Brett", "2026-01-01 00:01:00");
    foreign_alias.group_id = "group-F".to_string();
    conn.insert_entity(&foreign_alias).unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some("canonical-001".to_string()),
        alias_uuids: vec!["foreign-alias-001".to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(
        !result.success,
        "must reject an alias belonging to a foreign group"
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("foreign to the requested merge group")),
        "error should explain the foreign-group rejection, got: {:?}",
        result.errors
    );
    assert_eq!(result.merged_count, 0, "nothing should be merged");

    // The foreign alias must not have been touched.
    let untouched = conn
        .get_entity_by_uuid("foreign-alias-001")
        .unwrap()
        .unwrap();
    assert!(
        !untouched.labels.contains(&"Merged".to_string()),
        "foreign alias must not be tombstoned"
    );
    assert!(
        read_merged_into(&untouched.attributes).is_none(),
        "foreign alias must not have merged_into written"
    );
}

// ── Test 8: single entity, no aliases ────────────────────────────────────────

/// Edge case: only 1 entity with given name → merged_count: 0, success: true.
#[test]
fn test_single_entity_no_aliases_noop() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("brett-001", "Brett", "2026-01-01 00:00:00"))
        .unwrap();

    let params = MergeEntitiesParams {
        canonical_name: Some("Brett".to_string()),
        merge_all_by_name: true,
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "should succeed even with no aliases");
    assert_eq!(result.merged_count, 0, "no aliases to merge");
    assert_eq!(result.skipped, 0);
    assert_eq!(count_active_entities_named(&db, "Brett"), 1);
}

// ── Test 10: cross-group edge survives merge untouched (issue #371, SC-001/SC-006) ──

/// SC-001 / User Story 1 AC3: a foreign group's edge that would otherwise be rewritten onto
/// the canonical must instead be left completely untouched — no replacement edge written, and
/// the original edge's endpoints still reference the alias.
///
/// Setup: entities X1, X2, Y (group "liminis" — stands in for the spec's group A). Edge
/// X2 --[rel]--> Y in group "liminis". Edge X1 --[rel]--> Y in a different group,
/// "group-L" (stands in for the spec's layer group L). Merging X1 into X2 (merge invoked
/// under group "liminis") must leave the group-L edge byte-for-byte unchanged: still
/// pointing X1 --[rel]--> Y, group_id "group-L", invalid_at unset.
#[test]
fn test_cross_group_edge_survives_merge() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let x1_uuid = "x1-001";
    let x2_uuid = "x2-001";
    let y_uuid = "y-001";

    conn.insert_entity(&make_entity(x2_uuid, "X", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(x1_uuid, "X", "2026-01-01 00:01:00"))
        .unwrap();
    conn.insert_entity(&make_entity(y_uuid, "Y", "2026-01-01 00:00:00"))
        .unwrap();

    // Group A's own edge: X2 --[rel]--> Y
    conn.insert_relates_to_edge(&make_edge_in_group(
        x2_uuid,
        y_uuid,
        "rel",
        "2026-01-01 00:00:00",
        "liminis",
    ))
    .unwrap();
    // Group L's edge, same relation name/endpoints-after-merge, different group: X1 --[rel]--> Y
    let group_l_edge = make_edge_in_group(x1_uuid, y_uuid, "rel", "2026-01-01 00:00:00", "group-L");
    let group_l_edge_uuid = group_l_edge.uuid.clone();
    conn.insert_relates_to_edge(&group_l_edge).unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(x2_uuid.to_string()),
        alias_uuids: vec![x1_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(result.merged_count, 1);
    assert_eq!(
        result.edges_deduplicated, 0,
        "group L's edge must not be counted as a same-group duplicate"
    );
    assert_eq!(
        result.foreign_edges_skipped, 1,
        "group L's edge must be counted as skipped, not rewritten"
    );

    // Group L's edge must be completely untouched: same uuid, same group_id, same endpoints
    // (still referencing the alias X1, NOT re-pointed to the canonical), invalid_at unset.
    let untouched = conn.get_edge_by_uuid(&group_l_edge_uuid).unwrap().unwrap();
    assert_eq!(untouched.group_id, "group-L");
    assert_eq!(untouched.name, "rel");
    assert_eq!(
        untouched.source_node_uuid, x1_uuid,
        "group L's edge must still reference the alias — never rewritten onto the canonical"
    );
    assert_eq!(untouched.target_node_uuid, y_uuid);
    assert!(
        untouched.invalid_at.is_none(),
        "group L's edge must not be invalidated by group liminis's merge"
    );

    // No new edge silently conflates group L's assertion into group "liminis" or creates any
    // replacement edge for it in group "group-L".
    let canonical_edges = conn.get_full_edges_for_entity(x2_uuid).unwrap();
    let group_l_edges_on_canonical: Vec<_> = canonical_edges
        .iter()
        .filter(|e| e.group_id == "group-L" && e.invalid_at.is_none())
        .collect();
    assert_eq!(
        group_l_edges_on_canonical.len(),
        0,
        "no replacement group-L edge should be written onto the canonical"
    );
    let liminis_edges: Vec<_> = canonical_edges
        .iter()
        .filter(|e| e.group_id == "liminis" && e.invalid_at.is_none())
        .collect();
    assert_eq!(
        liminis_edges.len(),
        1,
        "group liminis's own edge count must be unaffected by group L's edge"
    );
}

/// User Story 1 AC1: a foreign edge that would collapse into a self-loop under the merge
/// (`new_src == new_dst`) must be left completely untouched — not invalidated, no replacement.
#[test]
fn test_foreign_self_loop_edge_untouched() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let x1_uuid = "x1-001";
    let x2_uuid = "x2-001";

    conn.insert_entity(&make_entity(x2_uuid, "X", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(x1_uuid, "X", "2026-01-01 00:01:00"))
        .unwrap();

    // Group L's edge: X1 --[rel]--> X2. After X1 merges into X2, this would become a
    // self-loop (X2 --[rel]--> X2) under same-group handling — but it belongs to group-L.
    let group_l_edge =
        make_edge_in_group(x1_uuid, x2_uuid, "rel", "2026-01-01 00:00:00", "group-L");
    let group_l_edge_uuid = group_l_edge.uuid.clone();
    conn.insert_relates_to_edge(&group_l_edge).unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(x2_uuid.to_string()),
        alias_uuids: vec![x1_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(result.foreign_edges_skipped, 1);

    let untouched = conn.get_edge_by_uuid(&group_l_edge_uuid).unwrap().unwrap();
    assert_eq!(untouched.source_node_uuid, x1_uuid);
    assert_eq!(untouched.target_node_uuid, x2_uuid);
    assert!(
        untouched.invalid_at.is_none(),
        "foreign would-be-self-loop edge must not be invalidated"
    );
}

/// User Story 1 AC2: a foreign edge that would duplicate an edge the canonical already has
/// (in the foreign edge's own group) must be left completely untouched — not invalidated as a
/// duplicate.
#[test]
fn test_foreign_duplicate_edge_untouched() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let x1_uuid = "x1-001";
    let x2_uuid = "x2-001";
    let y_uuid = "y-001";

    conn.insert_entity(&make_entity(x2_uuid, "X", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(x1_uuid, "X", "2026-01-01 00:01:00"))
        .unwrap();
    conn.insert_entity(&make_entity(y_uuid, "Y", "2026-01-01 00:00:00"))
        .unwrap();

    // Group L already has X2 --[rel]--> Y (the canonical's own group-L edge).
    conn.insert_relates_to_edge(&make_edge_in_group(
        x2_uuid,
        y_uuid,
        "rel",
        "2026-01-01 00:00:00",
        "group-L",
    ))
    .unwrap();
    // Group L also has X1 --[rel]--> Y — after merge this would duplicate the edge above.
    let dup_edge = make_edge_in_group(x1_uuid, y_uuid, "rel", "2026-01-01 00:00:01", "group-L");
    let dup_edge_uuid = dup_edge.uuid.clone();
    conn.insert_relates_to_edge(&dup_edge).unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(x2_uuid.to_string()),
        alias_uuids: vec![x1_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(
        result.edges_deduplicated, 0,
        "group L's would-be duplicate must not be counted as a same-group dedup"
    );
    assert_eq!(result.foreign_edges_skipped, 1);

    let untouched = conn.get_edge_by_uuid(&dup_edge_uuid).unwrap().unwrap();
    assert_eq!(untouched.source_node_uuid, x1_uuid);
    assert_eq!(untouched.target_node_uuid, y_uuid);
    assert!(
        untouched.invalid_at.is_none(),
        "foreign would-be-duplicate edge must not be invalidated"
    );
}

// ── Test 11: same-group dedup unaffected (issue #368, SC-004) ────────────────

/// SC-004 / User Story 2: fixing the cross-group leak must not change same-group dedup
/// behavior (FR-009): when two edges with the same name/endpoints share the merging alias's
/// own group, one is retained and the other invalidated as a duplicate — same as before.
#[test]
fn test_same_group_dedup_still_collapses() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let x1_uuid = "x1-001";
    let x2_uuid = "x2-001";
    let y_uuid = "y-001";

    conn.insert_entity(&make_entity(x2_uuid, "X", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(x1_uuid, "X", "2026-01-01 00:01:00"))
        .unwrap();
    conn.insert_entity(&make_entity(y_uuid, "Y", "2026-01-01 00:00:00"))
        .unwrap();

    // Both edges in the same group ("liminis")
    conn.insert_relates_to_edge(&make_edge(x2_uuid, y_uuid, "rel", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_relates_to_edge(&make_edge(x1_uuid, y_uuid, "rel", "2026-01-01 00:00:00"))
        .unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(x2_uuid.to_string()),
        alias_uuids: vec![x1_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(result.merged_count, 1);
    assert_eq!(
        result.edges_deduplicated, 1,
        "same-group duplicate must still collapse"
    );
    assert_eq!(
        result.foreign_edges_skipped, 0,
        "no foreign-group activity in this scenario"
    );

    // Exactly one active "rel" edge from canonical to Y remains
    let canonical_edges = conn.get_full_edges_for_entity(x2_uuid).unwrap();
    let active_rel: Vec<_> = canonical_edges
        .iter()
        .filter(|e| e.name == "rel" && e.invalid_at.is_none())
        .collect();
    assert_eq!(
        active_rel.len(),
        1,
        "exactly one active rel edge should remain after dedup"
    );

    // X1's original copy is invalidated (not duplicated)
    let x1_edges = conn.get_full_edges_for_entity(x1_uuid).unwrap();
    for edge in &x1_edges {
        if edge.source_node_uuid == x1_uuid || edge.target_node_uuid == x1_uuid {
            assert!(
                edge.invalid_at.is_some(),
                "X1's original edge copy must be invalidated: {:?}",
                edge
            );
        }
    }
}

// ── Test 9: TIMESTAMP-valued edges survive merge round-trip (regression #169) ─

/// SC-003 / FR-001 / FR-005: edges with non-null valid_at (RFC-3339) are read back from
/// lbug as space-format strings and re-inserted during merge. Without the space-format
/// fallback in json_value_for_param this produces "STRING but expected TIMESTAMP" and
/// merged_count stays 0. This test MUST fail against unfixed code.
#[test]
fn test_merge_with_timestamp_edges() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let other_uuid = "casey-target-001";
    conn.insert_entity(&make_entity(other_uuid, "Target", "2026-06-01 00:00:00"))
        .unwrap();

    let casey_uuids: Vec<String> = (1..=3).map(|i| format!("casey-{i:03}")).collect();
    for (i, uuid) in casey_uuids.iter().enumerate() {
        conn.insert_entity(&make_entity(
            uuid,
            "Casey",
            &format!("2026-06-01 00:0{i}:00"),
        ))
        .unwrap();
        // Edge with explicit RFC-3339 valid_at — stored as TIMESTAMP, read back as space-format
        let edge = RelatesToEdge {
            uuid: Uuid::new_v4().to_string(),
            name: format!("knows_{i}"),
            source_node_uuid: uuid.clone(),
            target_node_uuid: other_uuid.to_string(),
            group_id: "liminis".to_string(),
            fact: format!("{uuid} knows {other_uuid}"),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            created_at: "2026-06-01T00:00:00Z".to_string(),
            valid_at: Some("2026-06-01T12:00:00Z".to_string()),
            invalid_at: None,
            attributes: "{}".to_string(),
            relation_type: None,
            episode_uuids: vec![],
            source_descriptions: vec![],
        };
        conn.insert_relates_to_edge(&edge).unwrap();
    }

    let params = MergeEntitiesParams {
        canonical_name: Some("Casey".to_string()),
        merge_all_by_name: true,
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);

    assert!(
        result.success,
        "merge should succeed; errors: {:?}",
        result.errors
    );
    assert_eq!(
        result.merged_count, 2,
        "2 Casey aliases should be merged; errors: {:?}",
        result.errors
    );
    assert!(
        result.edges_rewritten >= 2,
        "at least 2 edges should be rewritten, got {}",
        result.edges_rewritten
    );
    assert!(
        result.errors.is_empty(),
        "no errors expected, got: {:?}",
        result.errors
    );

    // Exactly 1 active Casey remains
    assert_eq!(count_active_entities_named(&db, "Casey"), 1);

    // Canonical has the rewritten edges with correct timestamps
    let canonical = conn
        .get_entities_by_name_all("Casey", "liminis")
        .unwrap()
        .into_iter()
        .find(|e| !e.labels.contains(&"Merged".to_string()))
        .expect("one active Casey must remain");
    let edges = conn.get_full_edges_for_entity(&canonical.uuid).unwrap();
    let active_edges: Vec<_> = edges.iter().filter(|e| e.invalid_at.is_none()).collect();
    assert!(
        active_edges.len() >= 2,
        "canonical should have at least 2 active edges, got {}",
        active_edges.len()
    );
    // All active edges that were rewritten should have a valid_at value
    assert!(
        active_edges.iter().all(|e| e.valid_at.is_some()),
        "all active rewritten edges should retain valid_at"
    );
}

// ── Test: merged_into forwarding reference recorded on tombstoned alias (issue #371) ──

/// User Story 2 AC1: merging alias X into canonical Y records Y's UUID as the `merged_into`
/// forwarding reference on X's tombstoned row.
#[test]
fn test_merge_entities_records_merged_into() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alias_uuid = "ibm-001";
    let canonical_uuid = "ibm-canonical-001";

    conn.insert_entity(&make_entity(
        canonical_uuid,
        "International Business Machines",
        "2026-01-01 00:00:00",
    ))
    .unwrap();
    conn.insert_entity(&make_entity(alias_uuid, "IBM", "2026-01-01 00:01:00"))
        .unwrap();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(canonical_uuid.to_string()),
        alias_uuids: vec![alias_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(result.merged_count, 1);

    let alias_row = conn.get_entity_by_uuid(alias_uuid).unwrap().unwrap();
    assert!(alias_row.labels.contains(&"Merged".to_string()));
    assert_eq!(
        read_merged_into(&alias_row.attributes),
        Some(canonical_uuid.to_string()),
        "tombstoned alias must record the canonical it became, readable by a reader that \
         doesn't already know the canonical's UUID"
    );
}

// ── Test: merge never produces a mutation attributed to a foreign group (issue #371, FR-003) ──

/// SC-001: verified directly against `Conn::drain_mutations()` output, not only post-merge
/// graph state — a mutation could in principle be authored correctly in graph-state terms
/// while still being attributed to the wrong stream.
#[test]
fn test_merge_produces_no_mutation_for_foreign_edge() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let x1_uuid = "x1-001";
    let x2_uuid = "x2-001";
    let y_uuid = "y-001";

    conn.insert_entity(&make_entity(x2_uuid, "X", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity(x1_uuid, "X", "2026-01-01 00:01:00"))
        .unwrap();
    conn.insert_entity(&make_entity(y_uuid, "Y", "2026-01-01 00:00:00"))
        .unwrap();

    // Same-group edge: will be rewritten.
    conn.insert_relates_to_edge(&make_edge(x1_uuid, y_uuid, "rel", "2026-01-01 00:00:00"))
        .unwrap();
    // Foreign-group edge: must be skipped, and must never appear in drain_mutations() output.
    let group_l_edge = make_edge_in_group(x1_uuid, y_uuid, "rel", "2026-01-01 00:00:01", "group-L");
    let group_l_edge_uuid = group_l_edge.uuid.clone();
    conn.insert_relates_to_edge(&group_l_edge).unwrap();

    // Drain the setup's own mutations so only the merge's mutations remain below.
    conn.drain_mutations();

    let params = MergeEntitiesParams {
        canonical_uuid: Some(x2_uuid.to_string()),
        alias_uuids: vec![x1_uuid.to_string()],
        group_id: "liminis".to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(result.foreign_edges_skipped, 1);

    let mutations = conn.drain_mutations();
    assert!(
        !mutations.is_empty(),
        "the same-group edge rewrite should have produced at least one mutation"
    );
    for (cypher, params) in &mutations {
        let params_str = params.to_string();
        assert!(
            !cypher.contains(&group_l_edge_uuid) && !params_str.contains(&group_l_edge_uuid),
            "no mutation may reference the foreign edge's uuid: {cypher} {params_str}"
        );
    }
}

// ── Test: apply_same_as leaves a foreign-group edge untouched (issue #371, FR-006) ──

/// FR-006: `apply_same_as` (the YAML-corrections-driven merge path) must receive the same
/// foreign-edge-skip treatment as `merge_entities_inner` (FR-001) — a risk the spec calls out
/// explicitly, since `apply_same_as` had zero foreign-edge awareness before this issue. Unlike
/// `test_merge_entities_records_merged_into`/`test_same_as_correction_timestamp_type` (which
/// only check `merged_into` recording), this test exercises the actual skip behavior end to end
/// via `apply_corrections_file`, mirroring `test_cross_group_edge_survives_merge`'s assertions
/// for the `merge_entities` path.
#[test]
fn test_apply_same_as_leaves_foreign_edge_untouched() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let canonical_uuid = "sasl-canonical-001";
    let alias_uuid = "sasl-alias-001";
    let y_uuid = "sasl-y-001";

    conn.insert_entity(&make_entity(
        canonical_uuid,
        "SaslCanonical",
        "2026-01-01 00:00:00",
    ))
    .unwrap();
    conn.insert_entity(&make_entity(alias_uuid, "SaslAlias", "2026-01-01 00:00:01"))
        .unwrap();
    conn.insert_entity(&make_entity(y_uuid, "SaslY", "2026-01-01 00:00:00"))
        .unwrap();

    // Group L's edge, referencing the alias — must survive apply_same_as untouched.
    let group_l_edge =
        make_edge_in_group(alias_uuid, y_uuid, "rel", "2026-01-01 00:00:00", "group-L");
    let group_l_edge_uuid = group_l_edge.uuid.clone();
    conn.insert_relates_to_edge(&group_l_edge).unwrap();

    let liminis_dir = dir.path().join(".liminis");
    std::fs::create_dir_all(&liminis_dir).unwrap();
    std::fs::write(
        liminis_dir.join("knowledge-corrections.yaml"),
        format!(
            "corrections:\n  - id: \"c1\"\n    type: \"same_as\"\n    canonical_uuid: \"{canonical_uuid}\"\n    aliases: [\"SaslAlias\"]\n"
        ),
    )
    .unwrap();

    let result = apply_corrections_file(&conn, dir.path(), false);
    assert!(
        result.success,
        "apply_corrections should succeed: {:?}",
        result.errors
    );
    assert_eq!(result.applied, 1);

    // Group L's edge must be completely untouched: same uuid, same group_id, same endpoints
    // (still referencing the alias, NOT re-pointed to the canonical), invalid_at unset.
    let untouched = conn.get_edge_by_uuid(&group_l_edge_uuid).unwrap().unwrap();
    assert_eq!(untouched.group_id, "group-L");
    assert_eq!(
        untouched.source_node_uuid, alias_uuid,
        "group L's edge must still reference the alias — apply_same_as must never rewrite it"
    );
    assert_eq!(untouched.target_node_uuid, y_uuid);
    assert!(
        untouched.invalid_at.is_none(),
        "group L's edge must not be invalidated by apply_same_as"
    );

    // No replacement group-L edge should have been written onto the canonical.
    let canonical_edges = conn.get_full_edges_for_entity(canonical_uuid).unwrap();
    let group_l_edges_on_canonical: Vec<_> = canonical_edges
        .iter()
        .filter(|e| e.group_id == "group-L" && e.invalid_at.is_none())
        .collect();
    assert_eq!(
        group_l_edges_on_canonical.len(),
        0,
        "no replacement group-L edge should be written onto the canonical by apply_same_as"
    );
}
