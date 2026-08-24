// Coherence tests for the `Entity.lookup_key` secondary ART index behind
// `Conn::get_entity_by_name_ci` (issue #221), replacing `name_index_coherence.rs`'s coverage of
// the in-process `NameIndex` accelerator it supersedes (ADR-0038 -> ADR-0221).
//
// Covers:
//   SC-001 (User Story 1): the lookup is answered by an ART-indexed scan, not a full table scan.
//   User Story 3 / FR-007 / FR-010: resolution passes through `Merged` tombstones exactly as
//     before, and the endpoint-authority scan-fallback site (episode.rs Phase C) preserves an
//     equivalent guarantee to ADR-0283's bounded scan fallback + trust state (SC-007).
//   FR-006: WAL replay leaves `lookup_key` NULL until an explicit backfill pass runs — the
//     replacement for the old "replay bypasses insert_entity" gap `rebuild_name_index` closed.

use std::fs;

use lcg_core::corrections::{apply_corrections_file, merge_entities, MergeEntitiesParams};
use lcg_core::{schema, Db, EntityRow, WalReplayer};
use tempfile::TempDir;

const DIM: usize = 4;
const TS: &str = "2026-01-01T00:00:00Z";
const GROUP: &str = "liminis";

fn open_db(dir: &TempDir) -> Db {
    let db = Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
        // Build the ART index so these coherence tests exercise the indexed access path
        // itself (SC-001), not just the underlying Cypher predicate via an unindexed scan.
        conn.create_entity_lookup_key_index().unwrap();
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

// ── FR-001/FR-003: entity insert writes lookup_key, queried by equality ────────────────

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

/// A name shared across two different groups must not cross-resolve — the composite key
/// (`group_id + '\x1f' + lower(name)`) scopes every lookup to its own group by construction.
#[test]
fn lookup_is_scoped_to_group_id() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("g1-alice", "Alice", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&EntityRow {
        group_id: "other-group".to_string(),
        ..make_entity("g2-alice", "Alice", "2026-01-01 00:00:00")
    })
    .unwrap();

    assert_eq!(
        conn.get_entity_by_name_ci("Alice", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "g1-alice"
    );
    assert_eq!(
        conn.get_entity_by_name_ci("Alice", "other-group")
            .unwrap()
            .unwrap()
            .uuid,
        "g2-alice"
    );
}

// ── FR-001: update_entity_core (knowledge_assert_entity's rename path) also writes lookup_key ──

#[test]
fn update_entity_core_rename_updates_lookup_key() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let existing = make_entity("u1", "OldName", "2026-01-01 00:00:00");
    conn.insert_entity(&existing).unwrap();
    assert!(conn
        .get_entity_by_name_ci("OldName", GROUP)
        .unwrap()
        .is_some());

    conn.update_entity_core(&existing, "NewName", &["Entity".to_string()], "s", "{}")
        .unwrap();

    assert!(
        conn.get_entity_by_name_ci("OldName", GROUP)
            .unwrap()
            .is_none(),
        "the old name's lookup_key must no longer resolve after a rename"
    );
    assert_eq!(
        conn.get_entity_by_name_ci("NewName", GROUP)
            .unwrap()
            .expect("the new name must resolve immediately")
            .uuid,
        "u1"
    );
}

// ── User Story 3 #1 / FR-007: resolution passes through Merged tombstones ──────────────

/// Two same-named entities; merging the earlier-created one into the later-created one
/// (`merge_entities` pulls the canonical's `created_at` back to the earliest) reorders the
/// deterministic `ORDER BY created_at ASC, uuid ASC` winner. The lookup must reflect the new
/// winner, matching pre-#221 `NameIndex` behavior.
#[test]
fn merge_entities_created_at_update_reorders_winner() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

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

    let params = MergeEntitiesParams {
        canonical_uuid: Some("aaa-canonical".to_string()),
        alias_uuids: vec!["zzz-alias".to_string()],
        group_id: GROUP.to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed: {:?}", result.errors);

    // "zzz-alias" is now Merged-tombstoned but both rows share the same created_at, so the
    // uuid ASC tiebreak resolves to the canonical — proving the lookup reflects
    // update_entity_created_at's reorder, and that a lookup for a name whose current winner is
    // NOT a tombstone still resolves correctly post-merge.
    let found = conn
        .get_entity_by_name_ci("Brett", GROUP)
        .unwrap()
        .expect("Brett must still resolve after merge");
    assert_eq!(
        found.uuid, "aaa-canonical",
        "the lookup must reflect update_entity_created_at, not the pre-merge winner"
    );
}

/// ADR-0283's requirement, ported: a lookup for a name that resolves to a `Merged`-tombstoned
/// row must still return that row as the winner (not skip it and either fall through or miss) —
/// `get_entity_by_name_ci`'s query applies no label filter, matching `scan_entity_by_name_ci`.
/// Cross-group pointer resolution (#369) depends on this exact behavior.
///
/// Both entities share the same `created_at`, and the alias's uuid ("aaa-alias") sorts before
/// the canonical's ("zzz-canonical") — so `ORDER BY created_at ASC, uuid ASC` picks the alias
/// as the winner both before *and* after the merge tombstones it, making the assertion
/// deterministic (no reliance on which side `merge_entities` reorders).
#[test]
fn lookup_resolves_to_a_merged_tombstoned_winner() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("aaa-alias", "Dana", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity("zzz-canonical", "Dana", "2026-01-01 00:00:00"))
        .unwrap();
    assert_eq!(
        conn.get_entity_by_name_ci("Dana", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "aaa-alias",
        "precondition: the alias must be the winner before the merge"
    );

    let params = MergeEntitiesParams {
        canonical_uuid: Some("zzz-canonical".to_string()),
        alias_uuids: vec!["aaa-alias".to_string()],
        group_id: GROUP.to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed: {:?}", result.errors);

    let alias_labels = conn
        .get_entity_by_uuid("aaa-alias")
        .unwrap()
        .unwrap()
        .labels;
    assert!(
        alias_labels.contains(&"Merged".to_string()),
        "precondition: the alias must be Merged-tombstoned after the merge; got {alias_labels:?}"
    );

    let found = conn
        .get_entity_by_name_ci("Dana", GROUP)
        .unwrap()
        .expect("a name whose winner is Merged-tombstoned must still resolve");
    assert_eq!(
        found.uuid, "aaa-alias",
        "the lookup must resolve through the Merged tombstone to the same winner \
         ORDER BY created_at ASC, uuid ASC would pick, not skip it or miss"
    );
}

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
        "labeling an alias as Merged must not remove it from the lookup"
    );
}

// ── ART self-maintenance across delete (issue #221; formerly the sole group_purge exception) ──

#[test]
fn stale_entry_after_out_of_band_delete_degrades_to_miss() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("gone", "Ghost", "2026-01-01 00:00:00"))
        .unwrap();
    assert!(conn
        .get_entity_by_name_ci("Ghost", GROUP)
        .unwrap()
        .is_some());

    conn.run_cypher("MATCH (e:Entity {uuid: 'gone'}) DETACH DELETE e")
        .unwrap();

    assert!(
        conn.get_entity_by_name_ci("Ghost", GROUP)
            .unwrap()
            .is_none(),
        "a deleted entity must never be returned as if it were still valid"
    );
}

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

/// Falling through to the next-best surviving same-named candidate (pre-#221: `NameIndex`'s
/// FR-005 "verify-on-hit, fall through to the next candidate" logic) now emerges for free from
/// a single `ORDER BY ... LIMIT 1` query over live data, with no candidate loop needed.
#[test]
fn deleting_the_winner_falls_through_to_the_next_same_named_row() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("winner", "Dana", "2026-01-01 00:00:00"))
        .unwrap();
    conn.insert_entity(&make_entity("runner-up", "Dana", "2026-02-01 00:00:00"))
        .unwrap();
    assert_eq!(
        conn.get_entity_by_name_ci("Dana", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "winner"
    );

    conn.run_cypher("MATCH (e:Entity {uuid: 'winner'}) DETACH DELETE e")
        .unwrap();

    let found = conn
        .get_entity_by_name_ci("Dana", GROUP)
        .unwrap()
        .expect("the surviving same-named candidate must still resolve");
    assert_eq!(found.uuid, "runner-up");
}

// ── FR-006: WAL replay bypasses insert_entity's lookup_key write ───────────────────────

#[test]
fn wal_replay_leaves_lookup_key_null_until_explicit_backfill() {
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
    assert_eq!(conn.count_nodes("Entity").unwrap(), 1);
    assert!(
        conn.get_entity_by_name_ci("Replayed", GROUP)
            .unwrap()
            .is_none(),
        "replay must not silently populate lookup_key — it executes raw Cypher templates and \
         never calls Conn::insert_entity"
    );

    schema::backfill_entity_lookup_keys(&conn).unwrap();
    assert_eq!(
        conn.get_entity_by_name_ci("Replayed", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "replayed-1",
        "backfill_entity_lookup_keys must close the replay-bypass gap"
    );
}

/// `backfill_entity_lookup_keys` must surface a genuine underlying query failure, not silently
/// no-op — mirrors the old `rebuild_name_index_surfaces_a_genuine_query_failure` coverage.
#[test]
fn backfill_entity_lookup_keys_surfaces_a_genuine_query_failure() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("e1", "Alice", "2026-01-01 00:00:00"))
        .unwrap();
    assert!(schema::backfill_entity_lookup_keys(&conn).is_ok());

    // Drop a column backfill_entity_lookup_keys' own scan selects. `group_id` (unlike `name`)
    // isn't used by any FTS/vector index, so this ALTER succeeds cleanly and the resulting
    // Binder exception is solely due to backfill's own query referencing a now-missing column.
    conn.run_cypher("ALTER TABLE Entity DROP group_id").unwrap();

    assert!(
        schema::backfill_entity_lookup_keys(&conn).is_err(),
        "backfill_entity_lookup_keys must surface the underlying query failure rather than \
         silently no-op'ing"
    );
}

// ── FR-006: Db::open_or_rebuild backfills lookup_key from replayed WAL ─────────────────

#[test]
fn open_or_rebuild_backfills_lookup_key_from_replayed_wal() {
    let db_dir = TempDir::new().unwrap();
    let wal_dir = TempDir::new().unwrap();
    let line = r#"{"seq":0,"ts":"2026-05-19T03:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'rebuilt-1'}) ON CREATE SET n.name = 'Rebuilt', n.group_id = 'liminis', n.labels = ['Entity'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{}'","params":{}}"#;
    fs::write(
        wal_dir.path().join("20260519_030000_aaa111_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let db_path = db_dir.path().join("test.db");
    let (db, _stats) = Db::open_or_rebuild(
        db_path.to_str().unwrap(),
        wal_dir.path().to_str().unwrap(),
        DIM,
        None,
    )
    .unwrap();
    let conn = db.connect().unwrap();

    assert_eq!(
        conn.get_entity_by_name_ci("Rebuilt", GROUP)
            .unwrap()
            .unwrap()
            .uuid,
        "rebuilt-1",
        "open_or_rebuild must backfill lookup_key from the replayed WAL, not just populate the DB"
    );
}

// ── User Story 3 #3 / FR-010 / SC-007: Site 1 authority guarantee survives a stale/missing \
//    lookup_key ──────────────────────────────────────────────────────────────────────────────

/// `get_entity_by_name_ci_with_scan_fallback` backs `episode.rs`'s Phase C authority lookup
/// (ADR-0283's Site 1). A row whose `lookup_key` was never written (e.g. WAL replay before an
/// explicit backfill) must not report a false "entity does not exist": the scan fallback must
/// find it, self-heal its `lookup_key`, and be counted (SC-004/FR-012) — an equivalent
/// guarantee to ADR-0283's bounded scan fallback plus trust state.
#[test]
fn scan_fallback_resolves_and_self_heals_a_row_with_no_lookup_key() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let wal_dir = TempDir::new().unwrap();
    let line = r#"{"seq":0,"ts":"2026-05-19T03:00:00.000000+00:00","db":"","cypher":"MERGE (n:Entity {uuid: 'replayed-2'}) ON CREATE SET n.name = 'ReplayedTwo', n.group_id = 'liminis', n.labels = ['Entity'], n.created_at = timestamp('2026-05-19 00:00:00'), n.name_embedding = [1.0, 0.0, 0.0, 0.0], n.summary = 's', n.attributes = '{}'","params":{}}"#;
    fs::write(
        wal_dir.path().join("20260519_030000_bbb222_0000.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();
    WalReplayer::new(wal_dir.path()).replay(&conn).unwrap();

    // Simulate the FR-012 failure posture directly: a post-replay backfill attempt failed, so
    // the caller marks the migration status failed rather than leaving callers to trust it.
    conn.mark_lookup_key_migration_failed();
    assert!(!conn.lookup_key_migrated());

    // The plain, index-only lookup misses — replay bypassed insert_entity and no backfill ran.
    assert!(conn
        .get_entity_by_name_ci("ReplayedTwo", GROUP)
        .unwrap()
        .is_none());

    // The endpoint-authority scan-fallback lookup must resolve the entity regardless of the
    // migration-failed state (User Story 3 #3 / FR-010).
    let found = conn
        .get_entity_by_name_ci_with_scan_fallback("ReplayedTwo", GROUP)
        .unwrap()
        .expect("scan fallback must resolve the replayed entity despite the missing lookup_key");
    assert_eq!(found.uuid, "replayed-2");
    assert_eq!(
        conn.lookup_key_fallback_scan_count(),
        1,
        "the fallback scan must be counted for SC-004 telemetry"
    );

    // Self-healing: the scan hit above must have written lookup_key, so a second lookup
    // resolves via the plain index-only path with no additional scan.
    assert!(conn
        .get_entity_by_name_ci("ReplayedTwo", GROUP)
        .unwrap()
        .is_some());
    assert_eq!(
        conn.lookup_key_fallback_scan_count(),
        1,
        "a self-healed row must not require a second fallback scan"
    );
}

/// `scan_entity_by_name_ci` (the authority-site fallback) must match a stored `name` carrying
/// incidental whitespace against a trimmed query, exactly as `compute_lookup_key`/the index
/// already do. A row written out-of-band (raw Cypher, no `lookup_key`) with leading/trailing
/// whitespace in its `name` is precisely the case this fallback exists to catch (FR-010) — it
/// must not be the one case the fallback itself fails to find.
#[test]
fn scan_fallback_matches_a_stored_name_with_incidental_whitespace() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    // Bypasses insert_entity entirely (raw Cypher), so lookup_key stays NULL and the stored
    // name keeps its whitespace verbatim — unlike compute_lookup_key's writers, which always
    // trim before writing.
    conn.run_cypher(
        "CREATE (:Entity {uuid: 'raw-ws', name: '  Whitespace Co  ', group_id: 'liminis', \
         labels: ['Entity'], created_at: timestamp('2026-01-01 00:00:00'), \
         name_embedding: [1.0, 0.0, 0.0, 0.0], summary: 's', attributes: '{}'})",
    )
    .unwrap();

    let found = conn
        .get_entity_by_name_ci_with_scan_fallback("Whitespace Co", GROUP)
        .unwrap()
        .expect(
            "the scan fallback must match a stored name with incidental whitespace against a \
             trimmed query, not just an untrimmed lower(e.name)",
        );
    assert_eq!(found.uuid, "raw-ws");
}

// ── SC-001: EXPLAIN shows the ART-indexed access path, not a full table scan ───────────

#[test]
fn explain_shows_art_indexed_scan_for_lookup_query() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    conn.insert_entity(&make_entity("u1", "Alice", "2026-01-01 00:00:00"))
        .unwrap();

    let key = format!("{GROUP}\u{1f}alice");
    let plan = conn
        .cypher_query(&format!(
            "EXPLAIN MATCH (e:Entity) WHERE e.lookup_key = '{key}' RETURN e.uuid"
        ))
        .unwrap();
    let plan_text = plan.into_iter().flatten().collect::<Vec<_>>().join("\n");

    assert!(
        plan_text.contains("ART"),
        "EXPLAIN output for the lookup_key equality query must show the ART secondary index \
         being used, got: {plan_text}"
    );
}
