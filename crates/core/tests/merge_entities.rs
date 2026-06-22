/// Integration tests for knowledge_merge_entities — collapse duplicate entities.
///
/// Covers SC-001 through SC-006 and the user stories in the spec.
use lcg_core::{
    corrections::{merge_entities, MergeEntitiesParams},
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
    RelatesToEdge {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        source_node_uuid: src.to_string(),
        target_node_uuid: dst.to_string(),
        group_id: "liminis".to_string(),
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
