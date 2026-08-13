//! Integration tests for resolvable cross-group pointers (issue #369).
//!
//! Covers the spec's four user stories:
//!   US1 — cross-group edges carry pointer fields; intra-group edges don't.
//!   US2 — pointer resolution agrees with the name index, including on ambiguity.
//!   US3 — refresh cycle: purge, rehydrate, re-bind.
//!   US4 — unbound/ambiguous edges are observable and don't break read paths.

use lcg_core::{
    corrections::{merge_entities, MergeEntitiesParams},
    cross_group::{self, CreateCrossGroupEdgeParams, EndpointSpec},
    db::Db,
    pointer::{self, BindingState, EndpointSide},
    types::EntityRow,
};
use tempfile::TempDir;
use uuid::Uuid;

const DIM: usize = 4;
const TS: &str = "2026-01-01T00:00:00Z";
const GROUP_LAYER: &str = "layer";
const GROUP_A: &str = "source-a";

fn open_db(dir: &TempDir) -> Db {
    let db = Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.init_schema(DIM).unwrap();
    }
    db
}

fn make_entity(name: &str, group_id: &str, created_at: &str) -> EntityRow {
    EntityRow {
        uuid: Uuid::new_v4().to_string(),
        name: name.to_string(),
        group_id: group_id.to_string(),
        labels: vec!["Entity".to_string()],
        created_at: created_at.to_string(),
        name_embedding: vec![1.0, 0.0, 0.0, 0.0],
        summary: format!("summary of {name}"),
        attributes: "{}".to_string(),
        ..Default::default()
    }
}

// ── User Story 1: cross-group edges carry pointer fields; intra-group edges don't ─────────────

#[test]
fn intra_group_edge_has_no_pointer_fields() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Uuid(bob.uuid.clone()),
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    assert_eq!(edge.attributes, "{}");
    assert!(pointer::read_pointers(&edge.attributes).is_empty());

    // Both hops exist immediately — no resolution needed for an already-intra-group edge.
    let fetched = conn.get_edge_by_uuid(&edge.uuid).unwrap().unwrap();
    assert_eq!(fetched.source_node_uuid, alice.uuid);
    assert_eq!(fetched.target_node_uuid, bob.uuid);
}

#[test]
fn cross_group_edge_persists_pointer_fields_for_foreign_endpoint() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    let pointers = pointer::read_pointers(&edge.attributes);
    assert!(pointers.get(EndpointSide::Src).is_none());
    let dst_ptr = pointers.get(EndpointSide::Dst).unwrap();
    assert_eq!(dst_ptr.source_group_id, GROUP_A);
    assert_eq!(dst_ptr.endpoint_name, "bob");
    assert_eq!(dst_ptr.resolved_uuid, Some(bob.uuid.clone()));
    assert_eq!(dst_ptr.binding_state, BindingState::Bound);

    // Persisted, not just returned: re-fetch from the DB.
    let refetched = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let refetched_pointers = pointer::read_pointers(&refetched.attributes);
    assert_eq!(
        refetched_pointers
            .get(EndpointSide::Dst)
            .unwrap()
            .resolved_uuid,
        Some(bob.uuid.clone())
    );
    assert_eq!(refetched.source_node_uuid, alice.uuid);
    assert_eq!(refetched.target_node_uuid, bob.uuid);
}

#[test]
fn cross_group_edge_via_foreign_but_no_match_is_unbound_not_dropped() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Nobody".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Nobody".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    let pointers = pointer::read_pointers(&edge.attributes);
    let dst_ptr = pointers.get(EndpointSide::Dst).unwrap();
    assert_eq!(dst_ptr.binding_state, BindingState::Unbound);
    assert_eq!(dst_ptr.resolved_uuid, None);

    // Edge is retained (RelatesToNode_ + src hop exist), but the foreign hop is absent —
    // excluded from normal traversal (US4 AC1), not dropped (FR-004).
    let refetched = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(refetched.source_node_uuid, alice.uuid);
    assert_eq!(refetched.target_node_uuid, "");
    assert!(conn.get_edge_by_uuid(&edge.uuid).unwrap().is_none());
}

#[test]
fn bare_uuid_endpoint_foreign_to_edge_group_is_rejected_with_no_partial_write() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_A, TS); // lives in a different group than the edge
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();

    let before = conn.count_nodes("RelatesToNode_").unwrap();

    let result = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            // Bug: caller passed a bare UUID for an endpoint that is actually foreign —
            // no pointer fields would be recorded. Must be rejected (FR-002/SC-005).
            target: EndpointSpec::Uuid(bob.uuid.clone()),
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    );

    assert!(result.is_err());
    let after = conn.count_nodes("RelatesToNode_").unwrap();
    assert_eq!(
        before, after,
        "no partial edge should be written on rejection"
    );
}

// ── User Story 2: pointer resolution agrees with the name index, including on ambiguity ───────

#[test]
fn resolve_endpoint_unbound_when_zero_matches() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "nobody").unwrap();
    assert_eq!(state, BindingState::Unbound);
    assert_eq!(uuid, None);
    assert!(conn
        .get_entity_by_name_ci_with_scan_fallback("nobody", GROUP_A)
        .unwrap()
        .is_none());
}

#[test]
fn resolve_endpoint_bound_when_exactly_one_match() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let carol = make_entity("Carol", GROUP_A, TS);
    conn.insert_entity(&carol).unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "carol").unwrap();
    assert_eq!(state, BindingState::Bound);
    assert_eq!(uuid, Some(carol.uuid.clone()));
    assert_eq!(
        conn.get_entity_by_name_ci_with_scan_fallback("carol", GROUP_A)
            .unwrap()
            .unwrap()
            .uuid,
        carol.uuid
    );
}

#[test]
fn resolve_endpoint_ambiguous_when_two_active_matches() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let dave1 = make_entity("Dave", GROUP_A, "2026-01-01 00:00:00");
    let dave2 = make_entity("Dave", GROUP_A, "2026-01-02 00:00:00");
    conn.insert_entity(&dave1).unwrap();
    conn.insert_entity(&dave2).unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "dave").unwrap();
    assert_eq!(state, BindingState::Ambiguous);
    assert_eq!(uuid, None);
    // The name index itself would have picked a winner here — that's exactly the silent
    // behavior FR-006 says the pointer resolver must not reproduce.
    assert!(conn
        .get_entity_by_name_ci_with_scan_fallback("dave", GROUP_A)
        .unwrap()
        .is_some());
}

#[test]
fn resolve_endpoint_resolves_through_merged_tombstone_to_canonical() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let canonical = make_entity("Erin", GROUP_A, "2026-01-01 00:00:00");
    let mut alias = make_entity("Erin", GROUP_A, "2026-01-02 00:00:00");
    conn.insert_entity(&canonical).unwrap();
    conn.insert_entity(&alias).unwrap();

    // Simulate corrections::merge_entities tombstoning the alias in place (corrections.rs:1068):
    // label added, row left in place, name unchanged.
    alias.labels.push("Merged".to_string());
    conn.update_entity_labels(&alias.uuid, &alias.labels)
        .unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "erin").unwrap();
    assert_eq!(state, BindingState::Bound);
    assert_eq!(uuid, Some(canonical.uuid));
}

// ── merged_into forwarding (issue #371, User Story 3) ──────────────────────────────────────────

#[test]
fn resolve_endpoint_forwards_through_merged_into_when_name_changes() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let canonical = make_entity(
        "International Business Machines",
        GROUP_A,
        "2026-01-01 00:00:00",
    );
    let mut alias = make_entity("IBM", GROUP_A, "2026-01-02 00:00:00");
    conn.insert_entity(&canonical).unwrap();
    conn.insert_entity(&alias).unwrap();

    // Simulate corrections::merge_entities tombstoning the alias with forwarding recorded
    // (corrections.rs's merged_into write, FR-005).
    alias.labels.push("Merged".to_string());
    conn.update_entity_labels(&alias.uuid, &alias.labels)
        .unwrap();
    let new_attrs = pointer::write_merged_into(&alias.attributes, &canonical.uuid);
    conn.update_entity_attributes(&alias.uuid, &new_attrs)
        .unwrap();

    // Re-resolving "ibm" would, without forwarding, land back on the tombstoned alias (its own
    // name is unchanged) — the exact silent-stale-binding bug this issue closes.
    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "ibm").unwrap();
    assert_eq!(state, BindingState::Bound);
    assert_eq!(uuid, Some(canonical.uuid));
}

#[test]
fn resolve_endpoint_follows_two_hop_merged_into_chain() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let a = make_entity("Alpha", GROUP_A, "2026-01-01 00:00:00");
    let b = make_entity("Beta", GROUP_A, "2026-01-02 00:00:00");
    let c = make_entity("Gamma", GROUP_A, "2026-01-03 00:00:00");
    conn.insert_entity(&a).unwrap();
    conn.insert_entity(&b).unwrap();
    conn.insert_entity(&c).unwrap();

    // A -> B -> C in merged_into terms: A was merged into B, B was later merged into C.
    conn.update_entity_labels(&a.uuid, &["Entity".to_string(), "Merged".to_string()])
        .unwrap();
    conn.update_entity_attributes(&a.uuid, &pointer::write_merged_into("{}", &b.uuid))
        .unwrap();
    conn.update_entity_labels(&b.uuid, &["Entity".to_string(), "Merged".to_string()])
        .unwrap();
    conn.update_entity_attributes(&b.uuid, &pointer::write_merged_into("{}", &c.uuid))
        .unwrap();

    // A pointer that had resolved to A must land on C, not on B or A itself (SC-003).
    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "alpha").unwrap();
    assert_eq!(state, BindingState::Bound);
    assert_eq!(uuid, Some(c.uuid));
}

#[test]
fn resolve_endpoint_unbound_when_merged_into_missing() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    // A pre-existing tombstone with no merged_into recorded — every alias tombstoned before
    // this feature shipped has no forwarding data (User Story 3 AC2 / SC-004).
    let mut orphan = make_entity("Orphan", GROUP_A, TS);
    conn.insert_entity(&orphan).unwrap();
    orphan.labels.push("Merged".to_string());
    conn.update_entity_labels(&orphan.uuid, &orphan.labels)
        .unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "orphan").unwrap();
    assert_eq!(
        state,
        BindingState::Unbound,
        "a dead-end tombstone must never be reported Bound"
    );
    assert_eq!(uuid, None);
}

#[test]
fn resolve_endpoint_unbound_when_merged_into_target_does_not_exist() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    // A tombstone whose merged_into points at a UUID with no corresponding entity row —
    // distinct from the "no merged_into recorded" case above: here forwarding data exists but
    // is dangling (e.g. corrupted, or the target was later hard-deleted). Must still floor to
    // Unbound, never report Bound with a made-up UUID.
    let mut dangling = make_entity("Dangling", GROUP_A, TS);
    conn.insert_entity(&dangling).unwrap();
    dangling.labels.push("Merged".to_string());
    conn.update_entity_labels(&dangling.uuid, &dangling.labels)
        .unwrap();
    conn.update_entity_attributes(
        &dangling.uuid,
        &pointer::write_merged_into("{}", "does-not-exist-uuid"),
    )
    .unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "dangling").unwrap();
    assert_eq!(
        state,
        BindingState::Unbound,
        "a dangling merged_into target must never be reported Bound"
    );
    assert_eq!(uuid, None);
}

#[test]
fn resolve_endpoint_unbound_on_merged_into_cycle() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let a = make_entity("Alpha", GROUP_A, "2026-01-01 00:00:00");
    let b = make_entity("Beta", GROUP_A, "2026-01-02 00:00:00");
    conn.insert_entity(&a).unwrap();
    conn.insert_entity(&b).unwrap();

    // A merged_into B, B merged_into A — a cycle that must never be traversed indefinitely.
    conn.update_entity_labels(&a.uuid, &["Entity".to_string(), "Merged".to_string()])
        .unwrap();
    conn.update_entity_attributes(&a.uuid, &pointer::write_merged_into("{}", &b.uuid))
        .unwrap();
    conn.update_entity_labels(&b.uuid, &["Entity".to_string(), "Merged".to_string()])
        .unwrap();
    conn.update_entity_attributes(&b.uuid, &pointer::write_merged_into("{}", &a.uuid))
        .unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "alpha").unwrap();
    assert_eq!(
        state,
        BindingState::Unbound,
        "the cycle guard must stop the traversal, not hang or panic, and record unbound"
    );
    assert_eq!(uuid, None);
}

/// A `merged_into` reference is a raw UUID lookup, unlike the initial name-based winner (which
/// is already scoped to `source_group_id`) — nothing in `merge_entities`/`apply_same_as` today
/// guarantees a canonical and its alias share a group, so a forwarding reference could in
/// principle point outside `source_group_id`. `resolve_endpoint` must never report `Bound` to an
/// entity outside the group it was asked to resolve within — a cross-group target must floor to
/// `Unbound`, the same as a dangling target (PR #377 review finding).
#[test]
fn resolve_endpoint_unbound_when_merged_into_target_is_foreign_group() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let mut alias = make_entity("Alpha", GROUP_A, "2026-01-01 00:00:00");
    let foreign_target = make_entity("Beta", GROUP_LAYER, "2026-01-02 00:00:00");
    conn.insert_entity(&alias).unwrap();
    conn.insert_entity(&foreign_target).unwrap();

    // Simulate a corrupted/malformed merged_into reference that escaped the source group.
    alias.labels.push("Merged".to_string());
    conn.update_entity_labels(&alias.uuid, &alias.labels)
        .unwrap();
    conn.update_entity_attributes(
        &alias.uuid,
        &pointer::write_merged_into("{}", &foreign_target.uuid),
    )
    .unwrap();

    let (state, uuid) = cross_group::resolve_endpoint(&conn, GROUP_A, "alpha").unwrap();
    assert_eq!(
        state,
        BindingState::Unbound,
        "a merged_into target outside source_group_id must never be reported Bound"
    );
    assert_eq!(uuid, None);
}

// ── Original case regression (issue #371, SC-006) ───────────────────────────────────────────────

/// The scenario that originally motivated this issue: source group A holds X1 and Y; layer
/// group L asserts a cross-group edge X1 --[rel]--> Y. Group A merges X1 into Y — a legitimate
/// consolidation, which from L's perspective would collapse the edge into a self-loop. A's
/// merge must leave L's edge completely untouched; the edge recovers (here, correctly
/// invalidated as a genuine self-loop) only when L's own re-bind pass runs.
#[test]
fn original_self_loop_layer_scenario_recovers_via_rebind_not_via_merge() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    let x1 = make_entity("X1", GROUP_A, "2026-01-01 00:00:00");
    let y = make_entity("Y", GROUP_A, "2026-01-01 00:00:01");
    conn.insert_entity(&x1).unwrap();
    conn.insert_entity(&y).unwrap();

    // Layer group L asserts X1 --[rel]--> Y, both endpoints foreign to L's own group.
    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "rel".to_string(),
            source: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "X1".to_string(),
            },
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Y".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "X1 rel Y".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    let pointers = pointer::read_pointers(&edge.attributes);
    assert_eq!(
        pointers.get(EndpointSide::Src).unwrap().binding_state,
        BindingState::Bound
    );
    assert_eq!(
        pointers.get(EndpointSide::Dst).unwrap().binding_state,
        BindingState::Bound
    );

    // Group A merges X1 into Y.
    let params = MergeEntitiesParams {
        canonical_uuid: Some(y.uuid.clone()),
        alias_uuids: vec![x1.uuid.clone()],
        group_id: GROUP_A.to_string(),
        ..Default::default()
    };
    let result = merge_entities(&conn, &params, TS);
    assert!(result.success, "merge should succeed: {:?}", result.errors);
    assert_eq!(
        result.foreign_edges_skipped, 1,
        "the direct Entity<->Entity compat rel created alongside L's pointer edge carries L's \
         own group_id — it is a foreign edge that merge_entities_inner must skip, not rewrite"
    );

    // L's edge survives A's merge completely untouched: still present, not invalidated.
    assert!(
        conn.get_edge_by_uuid(&edge.uuid).unwrap().is_some(),
        "A's merge must never touch L's edge"
    );

    // L's own re-bind pass now discovers the staleness: X1's name still resolves (to the
    // tombstoned row, since X1's own name is unchanged), and merged_into forwarding routes it
    // to Y — producing a genuine self-loop, invalidated by rebind_pointers's own derived logic,
    // not by anything A's merge did.
    conn.set_wal_position(GROUP_A, 2, None).unwrap();
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.invalidated_self_loop, 1);

    // rebind_pointers's self-loop branch invalidates the row without touching its
    // already-existing hops (unlike the fresh-Unbound-both-sides case, this edge's hops were
    // created at edge-creation time) — `get_edge_by_uuid`'s strict two-hop MATCH would still
    // find it, so check `invalid_at` directly via `get_relates_to_by_uuids` instead.
    let raw = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(
        raw.invalid_at.is_some(),
        "L's edge must be invalidated by L's own rebind pass, as a genuine self-loop"
    );
}

// ── User Story 3: refresh cycle — purge, rehydrate, re-bind ────────────────────────────────────

#[test]
fn rebind_pointers_follows_reextraction_to_new_uuid_generation() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob_v1 = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob_v1).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&edge.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .resolved_uuid,
        Some(bob_v1.uuid.clone())
    );

    // Simulate a group-scoped purge-and-rehydrate: delete the old generation's Entity (which
    // DETACH DELETEs its hop to the layer's RelatesToNode_, but never the RelatesToNode_
    // itself — FR-011), then insert a re-extracted "Bob" under a brand-new UUID.
    conn.run_cypher(&format!(
        "MATCH (e:Entity {{uuid: '{}'}}) DETACH DELETE e",
        bob_v1.uuid
    ))
    .unwrap();
    let bob_v2 = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&bob_v2).unwrap();
    conn.set_wal_position(GROUP_A, 2, None).unwrap();

    // RelatesToNode_(layer) and its pointer attributes must have survived the purge untouched.
    let mid = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(mid.source_node_uuid, alice.uuid);
    assert_eq!(
        mid.target_node_uuid, "",
        "stale hop should be gone after purge"
    );
    assert_eq!(
        pointer::read_pointers(&mid.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .resolved_uuid,
        Some(bob_v1.uuid.clone()),
        "pointer's cached uuid is still stale until rebind runs"
    );

    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.checked, 1);
    assert_eq!(counts.bound, 1);

    let after = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.target_node_uuid, bob_v2.uuid);
    let after_ptr = pointer::read_pointers(&after.attributes);
    assert_eq!(
        after_ptr.get(EndpointSide::Dst).unwrap().resolved_uuid,
        Some(bob_v2.uuid)
    );
    assert_eq!(
        after_ptr.get(EndpointSide::Dst).unwrap().binding_state,
        BindingState::Bound
    );

    // Now visible through the normal two-hop read path again (US4 AC3).
    assert!(conn.get_edge_by_uuid(&edge.uuid).unwrap().is_some());
}

#[test]
fn rebind_pointers_restores_hop_detached_without_uuid_change() {
    // A hop can go missing (e.g. externally detached, or a source entity deleted and
    // recreated under the *same* UUID during a purge/rehydrate cycle) without the cached
    // `resolved_uuid` ever changing — re-resolution finds the exact same winner it found
    // last time. Gating hop (re)creation purely on "did resolved_uuid change" would miss
    // this: the pointer keeps reporting `Bound` while the edge stays invisible to every
    // normal two-hop read. `rebind_pointers` must diff against the hop's actual presence
    // in the graph, not against the cached pointer value.
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert!(conn.get_edge_by_uuid(&edge.uuid).unwrap().is_some());

    // Simulate the hop going missing without touching the Entity or the pointer's cached
    // resolved_uuid at all — e.g. a bug elsewhere, or a purge/rehydrate that happened to
    // reuse the same UUID for the recreated entity.
    conn.delete_relates_to_hop(&edge.uuid, EndpointSide::Dst)
        .unwrap();
    assert!(
        conn.get_edge_by_uuid(&edge.uuid).unwrap().is_none(),
        "edge should be invisible to the two-hop read path once its hop is detached"
    );

    // New WAL activity on the source so the staleness gate lets rebind re-check this pointer.
    conn.set_wal_position(GROUP_A, 2, None).unwrap();
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.checked, 1);
    assert_eq!(
        counts.bound, 1,
        "re-resolution finds the same Bob, unchanged"
    );

    let after_ptr = pointer::read_pointers(
        &conn
            .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .attributes,
    );
    assert_eq!(
        after_ptr.get(EndpointSide::Dst).unwrap().resolved_uuid,
        Some(bob.uuid.clone()),
        "resolved_uuid is unchanged by this rebind — this is exactly the case that must \
         not be mistaken for 'nothing to do'"
    );

    // The hop — and therefore the edge's visibility to normal reads — must be restored.
    let restored = conn.get_edge_by_uuid(&edge.uuid).unwrap();
    assert!(
        restored.is_some(),
        "rebind_pointers must restore a detached hop even when the resolved uuid didn't change"
    );
    assert_eq!(restored.unwrap().target_node_uuid, bob.uuid);
}

#[test]
fn rebind_pointers_is_idempotent_with_no_intervening_change() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();
    // Created with no applied_seq recorded yet (bound_at_seq = None), so the first rebind
    // below is guaranteed to actually process it regardless of the staleness gate.
    cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    conn.set_wal_position(GROUP_A, 1, None).unwrap();
    let (first, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(first.checked, 1);
    assert_eq!(first.bound, 1);

    // No intervening WAL activity — applied_seq unchanged — so the staleness gate should
    // make this a true no-op.
    let (second, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(second.checked, 0);
    assert_eq!(second.bound, 0);
    assert_eq!(second.unbound, 0);
    assert_eq!(second.ambiguous, 0);
}

#[test]
fn rebind_pointers_leaves_not_yet_hydrated_target_unbound() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&edge.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .binding_state,
        BindingState::Unbound
    );

    // Source is only partially rehydrated (Bob hasn't landed yet) — bump the position and
    // rebind anyway; must not corrupt state, must stay Unbound.
    conn.set_wal_position(GROUP_A, 2, None).unwrap();
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.checked, 1);
    assert_eq!(counts.unbound, 1);

    let after = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(after.target_node_uuid, "");
    assert_eq!(
        pointer::read_pointers(&after.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .binding_state,
        BindingState::Unbound
    );
}

#[test]
fn rebind_pointers_invalidates_self_loop_reusing_merge_style_handling() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    // Both endpoints are foreign pointers into the *same* group, under the *same* name — that
    // name doesn't exist yet, so the edge starts fully Unbound on both sides (no self-loop is
    // possible while both hops are absent).
    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Self loop candidate".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    let pointers = pointer::read_pointers(&edge.attributes);
    assert_eq!(
        pointers.get(EndpointSide::Src).unwrap().binding_state,
        BindingState::Unbound
    );
    assert_eq!(
        pointers.get(EndpointSide::Dst).unwrap().binding_state,
        BindingState::Unbound
    );

    // Now "Bob" arrives — both pointers resolve to the same entity within one rebind pass,
    // which is exactly the self-loop shape User Story 3 AC 5 says must reuse
    // `merge_entities_inner`'s handling rather than a new policy.
    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&bob).unwrap();
    conn.set_wal_position(GROUP_A, 2, None).unwrap();

    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.invalidated_self_loop, 1);

    assert!(conn.get_edge_by_uuid(&edge.uuid).unwrap().is_none());
    let raw = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(raw.invalid_at.is_some());
    // Both sides resolve to the same entity in this single pass (Src then Dst, in iteration
    // order). Neither hop must be left dangling: resolving both sides before writing either
    // hop (rather than committing Src's hop, then discovering Dst's resolution collapses the
    // edge) means the self-loop is caught before any hop write happens at all.
    assert_eq!(
        raw.source_node_uuid, "",
        "Src hop must not be left dangling on the invalidated node"
    );
    assert_eq!(
        raw.target_node_uuid, "",
        "Dst hop must not be left dangling on the invalidated node"
    );
}

#[test]
fn rebind_pointers_invalidates_duplicate_reusing_has_directed_edge() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    // A second, currently-unbound edge (Bob doesn't exist yet) whose pointer will end up
    // resolving to the exact same entity a *stable* edge already connects to.
    let dup_edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob (duplicate candidate)".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    // Now Bob shows up, and a *second* create call at the new position binds immediately —
    // this is the "stable" edge, pinned in place by the staleness gate below (bound_at_seq
    // will equal the position rebind runs at, so it's skipped regardless of row-scan order).
    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&bob).unwrap();
    conn.set_wal_position(GROUP_A, 2, None).unwrap();
    let stable_edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&stable_edge.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .binding_state,
        BindingState::Bound
    );

    // applied_seq is still 2 (unchanged since stable_edge's creation), so stable_edge's
    // bound_at_seq(2) >= current(2) skips it entirely — it cannot be touched by this pass,
    // regardless of candidate-scan order. dup_edge's bound_at_seq(1) < current(2) is
    // reprocessed and finds Bob now resolves, colliding with the still-intact stable_edge.
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.invalidated_duplicate, 1);

    // The duplicate is invalidated, not left dangling with a wrong resolution.
    assert!(conn.get_edge_by_uuid(&dup_edge.uuid).unwrap().is_none());
    let raw = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&dup_edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(raw.invalid_at.is_some());

    // The stable edge is untouched and still resolves.
    assert!(conn.get_edge_by_uuid(&stable_edge.uuid).unwrap().is_some());
}

/// Both `src` and `dst` are foreign pointers that resolve *in the same rebind pass* (unlike
/// `rebind_pointers_invalidates_duplicate_reusing_has_directed_edge`, where only one side is a
/// pointer) — the exact shape that previously let the `Src` iteration commit its hop before the
/// `Dst` iteration discovered the pair duplicates an existing edge, leaving `Src`'s hop
/// permanently orphaned on the now-invalidated node. Resolving both sides before writing either
/// hop closes that gap.
#[test]
fn rebind_pointers_invalidates_duplicate_from_two_sided_resolve_with_no_orphaned_hop() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    // dup_edge is created before Bob/Carol exist, so both sides start Unbound (no hop for
    // either) and its bound_at_seq is None (no applied position recorded yet) — guaranteeing
    // it is re-checked on the next rebind regardless of the staleness gate.
    let dup_edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Carol".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Bob knows Carol (duplicate candidate)".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&dup_edge.attributes)
            .get(EndpointSide::Src)
            .unwrap()
            .binding_state,
        BindingState::Unbound
    );

    // Now Bob and Carol arrive, and a *stable* edge binds to them immediately at the current
    // applied position — pinning stable_edge's bound_at_seq so the rebind call below (at the
    // same position) skips it entirely, leaving only dup_edge to be reprocessed.
    let bob = make_entity("Bob", GROUP_A, TS);
    let carol = make_entity("Carol", GROUP_A, TS);
    conn.insert_entity(&bob).unwrap();
    conn.insert_entity(&carol).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();
    let stable_edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Carol".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Bob knows Carol".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&stable_edge.attributes)
            .get(EndpointSide::Src)
            .unwrap()
            .binding_state,
        BindingState::Bound
    );

    // applied_seq is still 1 (unchanged since stable_edge's creation), so stable_edge's
    // bound_at_seq(1) >= current(1) skips it entirely. dup_edge's bound_at_seq is None, so it is
    // always reprocessed: both its Src and Dst pointers resolve to Bound (Bob, Carol) within
    // this single pass, colliding with stable_edge's already-real hops.
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.invalidated_duplicate, 1);

    assert!(conn.get_edge_by_uuid(&dup_edge.uuid).unwrap().is_none());
    let raw = conn
        .get_relates_to_by_uuids(std::slice::from_ref(&dup_edge.uuid))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(raw.invalid_at.is_some());
    assert_eq!(
        raw.source_node_uuid, "",
        "Src hop must not be left dangling on the invalidated node"
    );
    assert_eq!(
        raw.target_node_uuid, "",
        "Dst hop must not be left dangling on the invalidated node"
    );

    // The stable edge is untouched and still resolves.
    assert!(conn.get_edge_by_uuid(&stable_edge.uuid).unwrap().is_some());
}

// ── User Story 4: unbound/ambiguous edges are observable and don't break read paths ───────────

#[test]
fn unbound_edge_excluded_from_two_hop_read_paths_without_erroring() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Ghost".to_string(), // never resolves
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Ghost".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&edge.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .binding_state,
        BindingState::Unbound
    );

    // Every two-hop read path must not error and must not surface the unbound edge.
    assert!(conn.get_edge_by_uuid(&edge.uuid).unwrap().is_none());
    assert!(conn
        .get_full_edges_for_entity(&alice.uuid)
        .unwrap()
        .is_empty());
    assert!(conn.get_edges_for_entity(&alice.uuid).unwrap().is_empty());
    assert!(!conn
        .has_directed_edge(&alice.uuid, "irrelevant-uuid", "KNOWS", GROUP_LAYER)
        .unwrap());
}

#[test]
fn ambiguous_edge_excluded_from_two_hop_read_paths_without_erroring() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();
    let dup1 = make_entity("Dup", GROUP_A, "2026-01-01 00:00:00");
    let dup2 = make_entity("Dup", GROUP_A, "2026-01-02 00:00:00");
    conn.insert_entity(&dup1).unwrap();
    conn.insert_entity(&dup2).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Dup".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Dup".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(
        pointer::read_pointers(&edge.attributes)
            .get(EndpointSide::Dst)
            .unwrap()
            .binding_state,
        BindingState::Ambiguous
    );

    assert!(conn.get_edge_by_uuid(&edge.uuid).unwrap().is_none());
    assert!(conn
        .get_full_edges_for_entity(&alice.uuid)
        .unwrap()
        .is_empty());
    assert!(conn.get_edges_for_entity(&alice.uuid).unwrap().is_empty());
}

#[test]
fn count_cross_group_pointers_reports_correct_state_counts() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_A, TS);
    let dup1 = make_entity("Dup", GROUP_A, "2026-01-01 00:00:00");
    let dup2 = make_entity("Dup", GROUP_A, "2026-01-02 00:00:00");
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();
    conn.insert_entity(&dup1).unwrap();
    conn.insert_entity(&dup2).unwrap();

    // An intra-group edge contributes no pointer counts at all.
    cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Uuid(alice.uuid.clone()),
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Alice".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    // One bound pointer.
    cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    // One unbound pointer.
    cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Ghost".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Ghost".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    // One ambiguous pointer.
    cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Dup".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Dup".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    let counts = conn.count_cross_group_pointers().unwrap();
    assert_eq!(counts.bound, 1);
    assert_eq!(counts.unbound, 1);
    assert_eq!(counts.ambiguous, 1);
}

/// An invalidated cross-group edge (e.g. one `rebind_pointers` invalidated as a self-loop or
/// duplicate — User Story 3 AC 5) is no longer a live assertion and must not inflate
/// `knowledge_status`'s counts, matching `list_cross_group_pointer_candidates`'s own
/// `invalid_at IS NULL` filter.
#[test]
fn count_cross_group_pointers_excludes_invalidated_edges() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&alice).unwrap();
    conn.insert_entity(&bob).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();
    assert_eq!(conn.count_cross_group_pointers().unwrap().bound, 1);

    conn.invalidate_edge(&edge.uuid, TS).unwrap();

    let counts = conn.count_cross_group_pointers().unwrap();
    assert_eq!(counts.bound, 0, "invalidated edge must not count as bound");
    assert_eq!(counts.unbound, 0);
    assert_eq!(counts.ambiguous, 0);
}

#[test]
fn rebind_from_unbound_to_bound_makes_edge_reappear_in_traversal() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    // Not yet visible: Bob doesn't exist yet.
    assert!(conn.get_edges_for_entity(&alice.uuid).unwrap().is_empty());

    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&bob).unwrap();
    conn.set_wal_position(GROUP_A, 2, None).unwrap();
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.bound, 1);

    // Now visible via the same repeated traversal query.
    let edges = conn.get_edges_for_entity(&alice.uuid).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].uuid, edge.uuid);
}

/// `insert_cross_group_edge` creates the direct `Entity→Entity` compat rel only when both
/// endpoints already resolve at creation time. A pointer that starts unbound and is later bound
/// via `rebind_pointers` must not be permanently missing that rel — raw-Cypher consumers of
/// `knowledge_query_cypher` querying the direct pattern (rather than the two-hop shadow-node
/// pattern every internal read path uses) would otherwise silently miss it.
#[test]
fn rebind_from_unbound_to_bound_creates_direct_compat_rel() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let conn = db.connect().unwrap();

    let alice = make_entity("Alice", GROUP_LAYER, TS);
    conn.insert_entity(&alice).unwrap();
    conn.set_wal_position(GROUP_A, 1, None).unwrap();

    let edge = cross_group::create_cross_group_edge(
        &conn,
        CreateCrossGroupEdgeParams {
            name: "KNOWS".to_string(),
            source: EndpointSpec::Uuid(alice.uuid.clone()),
            target: EndpointSpec::Foreign {
                source_group_id: GROUP_A.to_string(),
                endpoint_name: "Bob".to_string(),
            },
            group_id: GROUP_LAYER.to_string(),
            fact: "Alice knows Bob".to_string(),
            fact_embedding: vec![1.0, 0.0, 0.0, 0.0],
            valid_at: None,
            relation_type: None,
        },
        TS,
    )
    .unwrap();

    let direct_rel_count = |conn: &lcg_core::db::Conn, uuid: &str| -> usize {
        conn.query_cypher_raw(&format!(
            "MATCH (src:Entity)-[r:RELATES_TO {{uuid: '{uuid}'}}]->(dst:Entity) RETURN r.uuid"
        ))
        .unwrap()
        .count()
    };
    assert_eq!(
        direct_rel_count(&conn, &edge.uuid),
        0,
        "no direct compat rel yet: target is still unbound"
    );

    let bob = make_entity("Bob", GROUP_A, TS);
    conn.insert_entity(&bob).unwrap();
    conn.set_wal_position(GROUP_A, 2, None).unwrap();
    let (counts, _) = cross_group::rebind_pointers(&conn, GROUP_A, TS).unwrap();
    assert_eq!(counts.bound, 1);

    assert_eq!(
        direct_rel_count(&conn, &edge.uuid),
        1,
        "rebind must create the direct compat rel once both endpoints resolve"
    );
}
